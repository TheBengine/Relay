//! The host side: lets friends in over iroh and hands their traffic to the
//! community's restricted peer listener.
//!
//! A friend is identified by the key their connection proved, admitted by the
//! token in the invitation, and then reaches only what `policy` allows. The
//! community sees a friend as a private-network peer with an address the host
//! assigned, exactly as it does for Tailscale.
use crate::{policy, protocol, Network};
use anyhow::Result;
use bytes::Bytes;
use http_body_util::{combinators::BoxBody, BodyExt, Full};
use hyper::{
    body::Incoming,
    header::{HeaderName, HeaderValue},
    service::service_fn,
    Request, Response, StatusCode,
};
use hyper_util::{
    client::legacy::{connect::HttpConnector, Client},
    rt::{TokioExecutor, TokioIo, TokioTimer},
};
use iroh::{
    endpoint::{Connection, QuicTransportConfig, VarInt},
    Endpoint, EndpointAddr, EndpointId, SecretKey,
};
use std::{
    collections::HashMap,
    convert::Infallible,
    net::Ipv4Addr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{net::UdpSocket, sync::Semaphore, task::JoinHandle, time::timeout};

/// Friends connected at once, and streams each of them may have open.
const MAX_CONNECTIONS: usize = 64;
const MAX_STREAMS_PER_CONNECTION: u32 = 64;
/// A new connection must say hello within this long.
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
/// Wrong tokens allowed per key before it is refused for a while.
const MAX_FAILURES: u32 = 5;
const FAILURE_WINDOW: Duration = Duration::from_secs(600);
/// The most voice a datagram may carry, as Relay's own limit.
const MAX_DATAGRAM: usize = 1500;

pub type Body = BoxBody<Bytes, hyper::Error>;

pub struct HostConfig {
    pub secret_key: SecretKey,
    /// Friends present this; only its digest is kept.
    pub token: String,
    /// The community's restricted peer listener on this PC.
    pub peer_port: u16,
    pub ingress_token: String,
    /// The community's voice port on this PC.
    pub udp_port: u16,
    pub media_udp_port: u16,
    pub network: Network,
}

pub struct HostService {
    endpoint: Endpoint,
    accept: JoinHandle<()>,
    shared: Arc<Shared>,
}

struct Shared {
    token_digest: Mutex<[u8; 32]>,
    peer_port: u16,
    ingress_token: HeaderValue,
    udp_port: u16,
    media_udp_port: u16,
    media_slots: Arc<Semaphore>,
    connections: Arc<Semaphore>,
    failures: Mutex<HashMap<EndpointId, (u32, Instant)>>,
    client: Client<HttpConnector, Incoming>,
}

impl HostService {
    pub async fn start(config: HostConfig) -> Result<Self> {
        anyhow::ensure!(
            protocol::valid_token(&config.token),
            "the admission token is not valid"
        );
        anyhow::ensure!(
            config.peer_port != 0 && config.udp_port != 0,
            "the community ports are required"
        );
        let ingress_token = HeaderValue::from_str(&config.ingress_token)
            .map_err(|_| anyhow::anyhow!("the ingress capability is not valid"))?;
        let endpoint = crate::bind(&config.network, Some(config.secret_key), true).await?;
        let mut connector = HttpConnector::new();
        connector.set_connect_timeout(Some(Duration::from_secs(5)));
        let client = Client::builder(TokioExecutor::new())
            .pool_idle_timeout(Duration::from_secs(30))
            .build(connector);
        let shared = Arc::new(Shared {
            token_digest: Mutex::new(protocol::digest(&config.token)),
            peer_port: config.peer_port,
            ingress_token,
            udp_port: config.udp_port,
            media_udp_port: config.media_udp_port,
            media_slots: Arc::new(Semaphore::new(128)),
            connections: Arc::new(Semaphore::new(MAX_CONNECTIONS)),
            failures: Mutex::new(HashMap::new()),
            client,
        });
        let accept = tokio::spawn(accept_loop(endpoint.clone(), shared.clone()));
        Ok(Self {
            endpoint,
            accept,
            shared,
        })
    }

    pub fn endpoint_id(&self) -> EndpointId {
        self.endpoint.id()
    }

    /// The host's own address, with whatever the endpoint knows about itself.
    pub fn addr(&self) -> EndpointAddr {
        self.endpoint.addr()
    }

    /// The home relay once the endpoint has found one.
    pub fn relay_url(&self) -> Option<String> {
        self.endpoint
            .addr()
            .relay_urls()
            .next()
            .map(|url| url.to_string())
    }

    /// Waits until the endpoint is reachable through its relay, or gives up.
    pub async fn wait_online(&self, limit: Duration) -> bool {
        timeout(limit, self.endpoint.online()).await.is_ok()
    }

    /// Replaces the token. Friends who connect afterwards need the new one.
    pub fn set_token(&self, token: &str) -> Result<()> {
        anyhow::ensure!(
            protocol::valid_token(token),
            "the admission token is not valid"
        );
        *self.shared.token_digest.lock().unwrap() = protocol::digest(token);
        Ok(())
    }

    pub async fn stop(self) {
        self.accept.abort();
        self.endpoint.close().await;
    }
}

async fn accept_loop(endpoint: Endpoint, shared: Arc<Shared>) {
    while let Some(incoming) = endpoint.accept().await {
        let Ok(permit) = shared.connections.clone().try_acquire_owned() else {
            incoming.refuse();
            continue;
        };
        let shared = shared.clone();
        tokio::spawn(async move {
            let _permit = permit;
            if let Ok(accepting) = incoming.accept() {
                if let Ok(connection) = accepting.await {
                    let _ = serve_connection(connection, shared).await;
                }
            }
        });
    }
}

fn blocked(shared: &Shared, id: &EndpointId) -> bool {
    let mut failures = shared.failures.lock().unwrap();
    failures.retain(|_, (_, since)| since.elapsed() < FAILURE_WINDOW);
    failures
        .get(id)
        .is_some_and(|(count, _)| *count >= MAX_FAILURES)
}

fn note_failure(shared: &Shared, id: EndpointId) {
    let mut failures = shared.failures.lock().unwrap();
    if failures.len() >= 1024 && !failures.contains_key(&id) {
        return;
    }
    let entry = failures.entry(id).or_insert((0, Instant::now()));
    entry.0 += 1;
}

async fn serve_connection(connection: Connection, shared: Arc<Shared>) -> Result<()> {
    let id = connection.remote_id();
    if blocked(&shared, &id) {
        connection.close(2u32.into(), b"too many wrong tokens");
        return Ok(());
    }
    // Nothing else is accepted until the hello has been checked.
    let admitted = timeout(HELLO_TIMEOUT, async {
        let (mut send, mut recv) = connection.accept_bi().await?;
        let token = protocol::read_hello(&mut recv).await.ok();
        let digest = *shared.token_digest.lock().unwrap();
        let accepted = token.is_some_and(|t| protocol::token_matches(&digest, &t));
        protocol::write_reply(&mut send, accepted).await.ok();
        send.finish().ok();
        Ok::<bool, anyhow::Error>(accepted)
    })
    .await;
    if !matches!(admitted, Ok(Ok(true))) {
        note_failure(&shared, id);
        // Let the friend read the refusal and close first; then make sure it is closed.
        let _ = timeout(Duration::from_millis(500), connection.closed()).await;
        connection.close(1u32.into(), b"not admitted");
        return Ok(());
    }
    let peer_ip = policy::synthetic_peer_ip(id.as_bytes());
    let voice = tokio::spawn(voice_bridge(connection.clone(), shared.udp_port));
    let streams = Arc::new(Semaphore::new(MAX_STREAMS_PER_CONNECTION as usize));
    loop {
        let (send, recv) = match connection.accept_bi().await {
            Ok(pair) => pair,
            Err(_) => break,
        };
        let Ok(permit) = streams.clone().try_acquire_owned() else {
            // More streams than allowed: the extra one is simply not served.
            drop((send, recv));
            continue;
        };
        let shared = shared.clone();
        tokio::spawn(async move {
            let _permit = permit;
            serve_http(send, recv, shared, peer_ip).await;
        });
    }
    voice.abort();
    Ok(())
}

async fn serve_http(
    send: iroh::endpoint::SendStream,
    recv: iroh::endpoint::RecvStream,
    shared: Arc<Shared>,
    peer_ip: Ipv4Addr,
) {
    let io = TokioIo::new(tokio::io::join(recv, send));
    let service = service_fn(move |request| {
        let shared = shared.clone();
        async move { Ok::<_, Infallible>(forward(request, &shared, peer_ip).await) }
    });
    let mut http = hyper::server::conn::http1::Builder::new();
    http.timer(TokioTimer::new())
        .header_read_timeout(Duration::from_secs(15))
        .max_buf_size(64 * 1024)
        .keep_alive(true);
    let _ = http.serve_connection(io, service).with_upgrades().await;
}

fn plain(status: StatusCode, text: &'static str) -> Response<Body> {
    let mut response = Response::new(
        Full::new(Bytes::from_static(text.as_bytes()))
            .map_err(|never| match never {})
            .boxed(),
    );
    *response.status_mut() = status;
    response
}

async fn forward(
    mut request: Request<Incoming>,
    shared: &Shared,
    peer_ip: Ipv4Addr,
) -> Response<Body> {
    if request.uri().path() == crate::media::PATH {
        return crate::media::handle(request, shared.media_udp_port, shared.media_slots.clone())
            .await;
    }
    if !policy::request_allowed(request.method(), request.uri(), request.headers()) {
        return plain(StatusCode::FORBIDDEN, "Not available");
    }
    let upgrading = policy::is_upgrade(request.headers());
    let client_upgrade = upgrading.then(|| hyper::upgrade::on(&mut request));
    let (parts, body) = request.into_parts();
    let path = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/");
    let Ok(uri) = format!("http://127.0.0.1:{}{path}", shared.peer_port).parse::<hyper::Uri>()
    else {
        return plain(StatusCode::BAD_REQUEST, "Not available");
    };
    let mut upstream = Request::new(body);
    *upstream.method_mut() = parts.method;
    *upstream.uri_mut() = uri;
    *upstream.headers_mut() = policy::forwardable_headers(&parts.headers);
    upstream.headers_mut().insert(
        hyper::header::HOST,
        HeaderValue::from_str(&format!("127.0.0.1:{}", shared.peer_port)).unwrap(),
    );
    upstream.headers_mut().insert(
        HeaderName::from_static("x-relay-network-ingress"),
        shared.ingress_token.clone(),
    );
    upstream.headers_mut().insert(
        HeaderName::from_static("x-relay-network-peer"),
        HeaderValue::from_str(&peer_ip.to_string()).unwrap(),
    );
    let Ok(Ok(mut response)) =
        timeout(Duration::from_secs(30), shared.client.request(upstream)).await
    else {
        return plain(StatusCode::BAD_GATEWAY, "Connection unavailable");
    };
    if response.status() == StatusCode::SWITCHING_PROTOCOLS {
        if let Some(client_upgrade) = client_upgrade {
            let server_upgrade = hyper::upgrade::on(&mut response);
            tokio::spawn(async move {
                if let (Ok(mut client), Ok(mut server)) =
                    (client_upgrade.await, server_upgrade.await)
                {
                    let _ = tokio::io::copy_bidirectional(
                        &mut TokioIo::new(&mut client),
                        &mut TokioIo::new(&mut server),
                    )
                    .await;
                }
            });
        }
    }
    response.map(|body| body.boxed())
}

/// Carries voice datagrams between a friend and the community's voice port.
/// The community sees one local socket per friend, as it does for Tailscale.
async fn voice_bridge(connection: Connection, udp_port: u16) {
    let Ok(socket) = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await else {
        return;
    };
    if socket
        .connect((Ipv4Addr::LOCALHOST, udp_port))
        .await
        .is_err()
    {
        return;
    }
    let socket = Arc::new(socket);
    let outgoing = {
        let socket = socket.clone();
        let connection = connection.clone();
        tokio::spawn(async move {
            let mut buffer = vec![0u8; MAX_DATAGRAM + 1];
            let mut drops = crate::DatagramDrops::default();
            while let Ok(length) = socket.recv(&mut buffer).await {
                if length > MAX_DATAGRAM {
                    continue;
                }
                // Too large for this path: the packet is dropped, never split.
                drops.record(
                    connection
                        .send_datagram(Bytes::copy_from_slice(&buffer[..length]))
                        .is_err(),
                );
            }
        })
    };
    while let Ok(datagram) = connection.read_datagram().await {
        if datagram.len() <= MAX_DATAGRAM {
            let _ = socket.send(&datagram).await;
        }
    }
    outgoing.abort();
}

pub fn transport() -> QuicTransportConfig {
    QuicTransportConfig::builder()
        .max_concurrent_bidi_streams(VarInt::from_u32(MAX_STREAMS_PER_CONNECTION + 1))
        .max_concurrent_uni_streams(VarInt::from_u32(0))
        .datagram_send_buffer_size(crate::VOICE_SEND_BUFFER_BYTES)
        .datagram_receive_buffer_size(Some(crate::VOICE_RECEIVE_BUFFER_BYTES))
        .build()
}
