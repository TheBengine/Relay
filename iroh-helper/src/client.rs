//! The friend's side: a loopback address and a loopback voice port which carry
//! everything to the host over iroh.
//!
//! Relay's own network code talks to `127.0.0.1` exactly as it does for the
//! Tailscale route. Nothing here looks at the traffic; the host decides what a
//! friend may reach.
use crate::{
    path::{self, PathInfo},
    protocol::{self, ALPN},
    Network,
};
use anyhow::{bail, Context, Result};
use bytes::Bytes;
use iroh::{endpoint::Connection, Endpoint, EndpointAddr, EndpointId, RelayUrl};
use std::{
    collections::HashMap,
    net::{Ipv4Addr, SocketAddr},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{
    io::AsyncWriteExt,
    net::{TcpListener, TcpStream, UdpSocket},
    task::JoinHandle,
    time::timeout,
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
/// Routes open at once, as many communities as a person could use together.
const MAX_ROUTES: usize = 8;
const MAX_DATAGRAM: usize = 1500;

/// Everything needed to reach one host. The token is a secret.
#[derive(Clone)]
pub struct RouteSpec {
    pub id: EndpointId,
    pub relay: Option<RelayUrl>,
    pub token: String,
    /// Direct socket addresses; used by tests, never by an invitation.
    pub direct: Vec<SocketAddr>,
}

impl RouteSpec {
    fn addr(&self) -> EndpointAddr {
        let mut addr = EndpointAddr::new(self.id);
        if let Some(relay) = &self.relay {
            addr = addr.with_relay_url(relay.clone());
        }
        for direct in &self.direct {
            addr = addr.with_ip_addr(*direct);
        }
        addr
    }
}

/// What Relay's own code connects to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RouteInfo {
    pub http: SocketAddr,
    pub voice_udp_port: u16,
}

pub struct Client {
    endpoint: Endpoint,
    routes: Mutex<HashMap<EndpointId, Arc<Route>>>,
}

/// What the background tasks of one route share. Cloning shares the same
/// connection, voice socket and the application's voice address.
#[derive(Clone)]
struct Link {
    spec: Arc<Mutex<RouteSpec>>,
    endpoint: Endpoint,
    connection: Arc<tokio::sync::Mutex<Option<Connection>>>,
    voice_socket: Arc<UdpSocket>,
    /// Where the application's voice packets come from, learned from the first one.
    voice_app: Arc<Mutex<Option<SocketAddr>>>,
}

struct Route {
    link: Link,
    info: RouteInfo,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

impl Client {
    pub async fn new(network: Network) -> Result<Self> {
        Ok(Self {
            endpoint: crate::bind(&network, None, false).await?,
            routes: Mutex::new(HashMap::new()),
        })
    }

    /// Connects (or reuses the connection), proves the invitation and returns
    /// the time it took, which is about one round trip when already online.
    pub async fn probe(&self, spec: RouteSpec) -> Result<f64> {
        let route = self.route_for(spec).await?;
        let started = Instant::now();
        ensure_connection(&route.link).await?;
        Ok(started.elapsed().as_secs_f64() * 1000.0)
    }

    /// Opens the loopback gateway for a host, or returns the one already open.
    pub async fn route(&self, spec: RouteSpec) -> Result<RouteInfo> {
        let route = self.route_for(spec).await?;
        ensure_connection(&route.link).await?;
        Ok(route.info)
    }

    pub async fn path(&self, id: &EndpointId) -> PathInfo {
        let route = self.routes.lock().unwrap().get(id).cloned();
        let Some(route) = route else {
            return path::unknown();
        };
        let slot = route.link.connection.lock().await;
        match slot.as_ref() {
            Some(connection) if connection.close_reason().is_none() => path::of(connection),
            _ => path::unknown(),
        }
    }

    /// Applies an admission the caller has already verified, to a route which is ALREADY
    /// open, for the handshakes still to come. Nothing else about the route changes: the
    /// same `Route`, live connection, local HTTP listener and voice port, voice address,
    /// stream tasks, endpoint and map entry all stay exactly as they are, and nothing is
    /// dialled, closed, opened or awaited. It never creates a route.
    ///
    /// `expected_token` must be the admission the route holds now. If the route already
    /// holds the new token (a reply was lost and the caller asks again) the answer is the
    /// same and nothing changes. A missing route, a different current admission or an
    /// unusable token changes nothing and is an error. Whether the new admission deserves
    /// trust is the caller's decision; this only applies it. Errors carry no token.
    pub async fn refresh_route(&self, spec: RouteSpec, expected_token: &str) -> Result<RouteInfo> {
        if !protocol::valid_token(&spec.token) || !protocol::valid_token(expected_token) {
            bail!("the route is not valid");
        }
        // The same lock order as `route_for`: the map, then the route's spec.
        let routes = self.routes.lock().unwrap();
        let Some(route) = routes.get(&spec.id) else {
            bail!("the route is not open");
        };
        let mut known = route.link.spec.lock().unwrap();
        if known.token == spec.token {
            return Ok(route.info);
        }
        if !protocol::token_matches(&protocol::digest(&known.token), expected_token) {
            bail!("the route holds a different admission");
        }
        known.token = spec.token;
        // Reachability for the next reconnect, as `route_for` refreshes it.
        if spec.relay.is_some() {
            known.relay = spec.relay;
        }
        if !spec.direct.is_empty() {
            known.direct = spec.direct;
        }
        Ok(route.info)
    }

    /// Which live connection a route has, so a test can tell one that was kept from one
    /// that was replaced. Not used by the control API.
    #[doc(hidden)]
    pub async fn connection_id(&self, id: &EndpointId) -> Option<usize> {
        let route = self.routes.lock().unwrap().get(id).cloned()?;
        let slot = route.link.connection.lock().await;
        slot.as_ref()
            .filter(|connection| connection.close_reason().is_none())
            .map(|connection| connection.stable_id())
    }

    pub async fn close_route(&self, id: &EndpointId) {
        let route = self.routes.lock().unwrap().remove(id);
        if let Some(route) = route {
            route.shutdown().await;
        }
    }

    pub async fn close(&self) {
        let routes: Vec<_> = self
            .routes
            .lock()
            .unwrap()
            .drain()
            .map(|(_, r)| r)
            .collect();
        // Closing the endpoint concurrently interrupts a pending handshake which
        // may hold a route's connection lock. Cleanup must not wait 20 seconds
        // behind that handshake while its loopback listeners remain open.
        tokio::join!(
            async { for route in routes { route.shutdown().await; } },
            self.endpoint.close(),
        );
    }

    async fn route_for(&self, spec: RouteSpec) -> Result<Arc<Route>> {
        if !protocol::valid_token(&spec.token) {
            bail!("the invitation's token is not valid");
        }
        {
            let routes = self.routes.lock().unwrap();
            if let Some(route) = routes.get(&spec.id) {
                let mut known=route.link.spec.lock().unwrap();
                if known.token == spec.token {
                    // Refresh reachability for the next reconnect without
                    // closing a live admission or changing its local sockets.
                    if spec.relay.is_some(){known.relay=spec.relay;}
                    if !spec.direct.is_empty(){known.direct=spec.direct;}
                    return Ok(route.clone());
                }
            }
        }
        // A different token for the same host replaces the old route.
        self.close_route(&spec.id).await;
        if self.routes.lock().unwrap().len() >= MAX_ROUTES {
            bail!("too many communities are open through iroh");
        }
        let route = Arc::new(Route::open(self.endpoint.clone(), spec).await?);
        self.routes
            .lock()
            .unwrap()
            .insert(route.link.spec.lock().unwrap().id, route.clone());
        Ok(route)
    }
}

impl Route {
    async fn open(endpoint: Endpoint, spec: RouteSpec) -> Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let http = listener.local_addr()?;
        let voice_socket = Arc::new(UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await?);
        let voice_udp_port = voice_socket.local_addr()?.port();
        let link = Link {
            spec: Arc::new(Mutex::new(spec)),
            endpoint,
            connection: Arc::new(tokio::sync::Mutex::new(None)),
            voice_socket,
            voice_app: Arc::new(Mutex::new(None)),
        };
        let gateway = tokio::spawn(gateway_loop(listener, link.clone()));
        let voice = tokio::spawn(voice_out_loop(link.clone()));
        Ok(Self {
            link,
            info: RouteInfo {
                http,
                voice_udp_port,
            },
            tasks: Mutex::new(vec![gateway, voice]),
        })
    }

    async fn shutdown(&self) {
        for task in self.tasks.lock().unwrap().drain(..) {
            task.abort();
        }
        if let Some(connection) = self.link.connection.lock().await.take() {
            connection.close(0u32.into(), b"route closed");
        }
    }
}

impl Drop for Route {
    fn drop(&mut self) {
        for task in self.tasks.lock().unwrap().drain(..) {
            task.abort();
        }
    }
}

/// Connects and says hello. A live connection is reused; a closed one is replaced.
async fn ensure_connection(link: &Link) -> Result<Connection> {
    let mut slot = link.connection.lock().await;
    if let Some(connection) = slot.as_ref() {
        if connection.close_reason().is_none() {
            return Ok(connection.clone());
        }
    }
    let spec=link.spec.lock().unwrap().clone();
    let connection = timeout(
        CONNECT_TIMEOUT,
        link.endpoint.connect(spec.addr(), ALPN),
    )
    .await
    .context("the host did not answer in time")??;
    let (mut send, mut recv) = connection.open_bi().await?;
    let hello = protocol::encode_hello(&spec.token)
        .map_err(|_| anyhow::anyhow!("the invitation's token is not valid"))?;
    send.write_all(&hello).await?;
    send.finish()?;
    let accepted = timeout(HELLO_TIMEOUT, protocol::read_reply(&mut recv))
        .await
        .context("the host did not answer the invitation")?
        .map_err(|_| anyhow::anyhow!("the host did not accept this invitation"))?;
    if !accepted {
        connection.close(1u32.into(), b"invitation refused");
        bail!("the host did not accept this invitation");
    }
    tokio::spawn(voice_in_loop(connection.clone(), link.clone()));
    *slot = Some(connection.clone());
    Ok(connection)
}

async fn gateway_loop(listener: TcpListener, link: Link) {
    while let Ok((socket, from)) = listener.accept().await {
        if !from.ip().is_loopback() {
            continue;
        }
        let link = link.clone();
        tokio::spawn(async move {
            let _ = tunnel(socket, link).await;
        });
    }
}

/// One local connection becomes one stream. The bytes are not looked at.
async fn tunnel(socket: TcpStream, link: Link) -> Result<()> {
    let connection = ensure_connection(&link).await?;
    let (mut send, mut recv) = connection.open_bi().await?;
    let (mut read, mut write) = socket.into_split();
    let upstream = async {
        tokio::io::copy(&mut read, &mut send).await?;
        send.finish()?;
        Ok::<_, anyhow::Error>(())
    };
    let downstream = async {
        tokio::io::copy(&mut recv, &mut write).await?;
        write.shutdown().await?;
        Ok::<_, anyhow::Error>(())
    };
    let _ = tokio::try_join!(upstream, downstream);
    Ok(())
}

/// The application's voice packets, sent on to the host. Before the first
/// connection exists, or while it is down, a packet is simply lost, as UDP does.
async fn voice_out_loop(link: Link) {
    let mut drops = crate::DatagramDrops::default();
    let mut buffer = vec![0u8; MAX_DATAGRAM + 1];
    while let Ok((length, from)) = link.voice_socket.recv_from(&mut buffer).await {
        if !from.ip().is_loopback() || length > MAX_DATAGRAM {
            continue;
        }
        *link.voice_app.lock().unwrap() = Some(from);
        let slot = link.connection.lock().await;
        if let Some(connection) = slot.as_ref().filter(|c| c.close_reason().is_none()) {
            // Too large for this path: dropped, never split.
            drops.record(
                connection
                    .send_datagram(Bytes::copy_from_slice(&buffer[..length]))
                    .is_err(),
            );
        }
    }
}

/// The host's voice packets, delivered to the application.
async fn voice_in_loop(connection: Connection, link: Link) {
    while let Ok(datagram) = connection.read_datagram().await {
        let to = *link.voice_app.lock().unwrap();
        if let (Some(to), true) = (to, datagram.len() <= MAX_DATAGRAM) {
            let _ = link.voice_socket.send_to(&datagram, to).await;
        }
    }
}
