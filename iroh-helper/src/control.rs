//! The small local API Relay uses to run this helper.
//!
//! It listens on loopback only and answers only to the secret Relay gave it at
//! startup. A web page cannot use it: any request that carries a browser's
//! `Origin` or `Sec-Fetch-Site` header is refused before the secret is read.
// A refusal is an ordinary HTTP reply, built once and returned at once; boxing it
// would only add noise to a local control API.
#![allow(clippy::result_large_err)]
use crate::{
    client::{Client, RouteSpec},
    host::{HostConfig, HostService},
    path::PathInfo,
    protocol, token, Network,
};
use anyhow::{Context, Result};
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::{body::Incoming, service::service_fn, Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use iroh::{EndpointId, RelayUrl};
use serde::{Deserialize, Serialize};
use std::{
    convert::Infallible,
    path::PathBuf,
    str::FromStr,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{net::TcpListener, sync::Notify};

const MAX_BODY: usize = 4096;
const HOST_ONLINE_WAIT: Duration = Duration::from_secs(8);

pub struct Control {
    token_digest: [u8; 32],
    state_dir: PathBuf,
    network: Network,
    host: tokio::sync::Mutex<Option<HostService>>,
    client: tokio::sync::Mutex<Option<Arc<Client>>>,
    /// Signalled by `/shutdown`; the program then ends.
    pub stop: Notify,
    last_host: Mutex<Option<String>>,
}

impl Control {
    pub fn new(control_token: &str, state_dir: PathBuf, network: Network) -> Result<Arc<Self>> {
        anyhow::ensure!(
            (32..=256).contains(&control_token.len()),
            "the control secret must be 32 to 256 characters"
        );
        Ok(Arc::new(Self {
            token_digest: protocol::digest(control_token),
            state_dir,
            network,
            host: tokio::sync::Mutex::new(None),
            client: tokio::sync::Mutex::new(None),
            stop: Notify::new(),
            last_host: Mutex::new(None),
        }))
    }

    fn authorized(&self, request: &Request<Incoming>) -> bool {
        let headers = request.headers();
        if headers.contains_key(hyper::header::ORIGIN) || headers.contains_key("sec-fetch-site") {
            return false;
        }
        headers
            .get(hyper::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .is_some_and(|supplied| protocol::token_matches(&self.token_digest, supplied))
    }
}

#[derive(Serialize)]
struct Status {
    version: u8,
    phase: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    endpoint_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HostInput {
    peer_port: u16,
    ingress_token: String,
    udp_port: u16,
    #[serde(default = "default_media_port")]
    media_udp_port: u16,
}

fn default_media_port() -> u16 {
    7882
}

#[derive(Serialize)]
struct HostReply {
    endpoint_id: String,
    /// Only in the loopback test mode, where there is no relay to find the host by.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    direct: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    relay_url: Option<String>,
    token: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RouteInput {
    endpoint_id: String,
    #[serde(default)]
    relay: Option<String>,
    token: String,
    /// Socket addresses to try first. Only the loopback test mode accepts them.
    #[serde(default)]
    direct: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IdInput {
    endpoint_id: String,
}

#[derive(Serialize)]
struct RouteReply {
    http_url: String,
    voice_udp_port: u16,
}

#[derive(Serialize)]
struct ProbeReply {
    rtt_ms: f64,
}

type Reply = Response<Full<Bytes>>;

fn json<T: Serialize>(status: StatusCode, value: &T) -> Reply {
    let mut response = Response::new(Full::new(Bytes::from(
        serde_json::to_vec(value).unwrap_or_default(),
    )));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(
        hyper::header::CONTENT_TYPE,
        "application/json".parse().unwrap(),
    );
    headers.insert(hyper::header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
}

fn error(status: StatusCode, message: &str) -> Reply {
    json(status, &serde_json::json!({ "error": message }))
}

async fn body<T: for<'de> Deserialize<'de>>(request: Request<Incoming>) -> Result<T, Reply> {
    let bytes = Limited::new(request.into_body(), MAX_BODY)
        .collect()
        .await
        .map_err(|_| error(StatusCode::BAD_REQUEST, "Invalid request"))?
        .to_bytes();
    serde_json::from_slice(&bytes).map_err(|_| error(StatusCode::BAD_REQUEST, "Invalid request"))
}

fn spec(input: &RouteInput, network: &Network) -> Result<RouteSpec, Reply> {
    let bad = || {
        error(
            StatusCode::BAD_REQUEST,
            "A valid invitation route is required",
        )
    };
    let mut direct = Vec::new();
    for address in &input.direct {
        // An invitation never carries addresses, so a real helper refuses them.
        if *network != Network::Loopback {
            return Err(bad());
        }
        direct.push(address.parse().map_err(|_| bad())?);
    }
    Ok(RouteSpec {
        id: EndpointId::from_str(&input.endpoint_id).map_err(|_| bad())?,
        relay: match &input.relay {
            // A relay sees who talks to whom, so only an encrypted address is used.
            Some(url) if url.len() <= 200 && url.starts_with("https://") => {
                Some(RelayUrl::from_str(url).map_err(|_| bad())?)
            }
            Some(_) => return Err(bad()),
            None => None,
        },
        token: input.token.clone(),
        direct,
    })
    .and_then(|spec| {
        if protocol::valid_token(&spec.token) && input.endpoint_id.len() == 64 {
            Ok(spec)
        } else {
            Err(bad())
        }
    })
}

impl Control {
    async fn client(&self) -> Result<Arc<Client>> {
        let mut slot = self.client.lock().await;
        if let Some(client) = slot.as_ref() {
            return Ok(client.clone());
        }
        let client = Arc::new(Client::new(self.network.clone()).await?);
        *slot = Some(client.clone());
        Ok(client)
    }

    pub async fn handle(self: &Arc<Self>, request: Request<Incoming>) -> Reply {
        if !self.authorized(&request) {
            return error(StatusCode::FORBIDDEN, "Private app control required");
        }
        let method = request.method().clone();
        let path = request.uri().path().to_owned();
        match (method, path.as_str()) {
            (Method::GET, "/status") => {
                let host = self.host.lock().await;
                json(
                    StatusCode::OK,
                    &Status {
                        version: 1,
                        phase: if host.is_some() { "hosting" } else { "idle" },
                        endpoint_id: host.as_ref().map(|h| h.endpoint_id().to_string()),
                    },
                )
            }
            (Method::POST, "/host") => {
                let input: HostInput = match body(request).await {
                    Ok(input) => input,
                    Err(reply) => return reply,
                };
                let mut host = self.host.lock().await;
                if host.is_some() {
                    return error(
                        StatusCode::CONFLICT,
                        "This helper already hosts a community",
                    );
                }
                match self.start_host(input).await {
                    Ok((service, reply)) => {
                        *self.last_host.lock().unwrap() = Some(reply.endpoint_id.clone());
                        *host = Some(service);
                        json(StatusCode::OK, &reply)
                    }
                    Err(_) => error(
                        StatusCode::BAD_GATEWAY,
                        "The account-free route could not start",
                    ),
                }
            }
            (Method::POST, "/unhost") => {
                let stopped = self.host.lock().await.take();
                match stopped {
                    Some(service) => {
                        service.stop().await;
                        json(StatusCode::OK, &serde_json::json!({ "unhosted": true }))
                    }
                    None => error(
                        StatusCode::CONFLICT,
                        "This helper does not host a community",
                    ),
                }
            }
            (Method::POST, "/rotate-token") => {
                let host = self.host.lock().await;
                let Some(service) = host.as_ref() else {
                    return error(StatusCode::CONFLICT, "Start hosting first");
                };
                match token::rotate_token(&self.state_dir).and_then(|fresh| {
                    service.set_token(&fresh)?;
                    Ok(fresh)
                }) {
                    Ok(fresh) => json(StatusCode::OK, &serde_json::json!({ "token": fresh })),
                    Err(_) => error(StatusCode::BAD_GATEWAY, "The token could not be replaced"),
                }
            }
            (Method::POST, "/probe") => {
                let input: RouteInput = match body(request).await {
                    Ok(input) => input,
                    Err(reply) => return reply,
                };
                let spec = match spec(&input, &self.network) {
                    Ok(spec) => spec,
                    Err(reply) => return reply,
                };
                let Ok(client) = self.client().await else {
                    return error(
                        StatusCode::BAD_GATEWAY,
                        "The account-free route is unavailable",
                    );
                };
                match client.probe(spec).await {
                    Ok(rtt_ms) => json(StatusCode::OK, &ProbeReply { rtt_ms }),
                    Err(_) => error(StatusCode::BAD_GATEWAY, "The host did not answer"),
                }
            }
            (Method::POST, "/route") => {
                let input: RouteInput = match body(request).await {
                    Ok(input) => input,
                    Err(reply) => return reply,
                };
                let spec = match spec(&input, &self.network) {
                    Ok(spec) => spec,
                    Err(reply) => return reply,
                };
                let Ok(client) = self.client().await else {
                    return error(
                        StatusCode::BAD_GATEWAY,
                        "The account-free route is unavailable",
                    );
                };
                match client.route(spec).await {
                    Ok(info) => json(
                        StatusCode::OK,
                        &RouteReply {
                            http_url: format!("http://{}", info.http),
                            voice_udp_port: info.voice_udp_port,
                        },
                    ),
                    Err(_) => error(
                        StatusCode::BAD_GATEWAY,
                        "The host did not accept the invitation",
                    ),
                }
            }
            (Method::POST, "/path") => {
                let input: IdInput = match body(request).await {
                    Ok(input) => input,
                    Err(reply) => return reply,
                };
                let Ok(id) = EndpointId::from_str(&input.endpoint_id) else {
                    return error(StatusCode::BAD_REQUEST, "A valid key is required");
                };
                let info: PathInfo = match self.client.lock().await.clone() {
                    Some(client) => client.path(&id).await,
                    None => crate::path::unknown(),
                };
                json(StatusCode::OK, &info)
            }
            (Method::POST, "/close-route") => {
                let input: IdInput = match body(request).await {
                    Ok(input) => input,
                    Err(reply) => return reply,
                };
                let Ok(id) = EndpointId::from_str(&input.endpoint_id) else {
                    return error(StatusCode::BAD_REQUEST, "A valid key is required");
                };
                if let Some(client) = self.client.lock().await.clone() {
                    client.close_route(&id).await;
                }
                json(StatusCode::OK, &serde_json::json!({ "closed": true }))
            }
            (Method::POST, "/shutdown") => {
                self.stop.notify_one();
                json(StatusCode::OK, &serde_json::json!({ "stopped": true }))
            }
            _ => error(StatusCode::NOT_FOUND, "Not available"),
        }
    }

    async fn start_host(&self, input: HostInput) -> Result<(HostService, HostReply)> {
        let key = token::load_or_create_key(&self.state_dir)?;
        let admission = token::load_or_create_token(&self.state_dir)?;
        let service = HostService::start(HostConfig {
            secret_key: key,
            token: admission.clone(),
            peer_port: input.peer_port,
            ingress_token: input.ingress_token,
            udp_port: input.udp_port,
            media_udp_port: input.media_udp_port,
            network: self.network.clone(),
        })
        .await
        .context("start hosting")?;
        // The relay is only known once the endpoint has reached it; an offline
        // host still hands out its key, and friends find it by that.
        if self.network != Network::Loopback {
            let _ = service.wait_online(HOST_ONLINE_WAIT).await;
        }
        let direct = if self.network == Network::Loopback {
            service.addr().ip_addrs().map(|a| a.to_string()).collect()
        } else {
            Vec::new()
        };
        let reply = HostReply {
            endpoint_id: service.endpoint_id().to_string(),
            direct,
            relay_url: service.relay_url(),
            token: admission,
        };
        Ok((service, reply))
    }

    pub async fn shutdown(&self) {
        if let Some(host) = self.host.lock().await.take() {
            host.stop().await;
        }
        if let Some(client) = self.client.lock().await.take() {
            client.close().await;
        }
    }
}

/// Serves the control API until the listener is dropped.
pub async fn serve(listener: TcpListener, control: Arc<Control>) {
    while let Ok((socket, from)) = listener.accept().await {
        if !from.ip().is_loopback() {
            continue;
        }
        let control = control.clone();
        tokio::spawn(async move {
            let service = service_fn(move |request| {
                let control = control.clone();
                async move { Ok::<_, Infallible>(control.handle(request).await) }
            });
            let _ = hyper::server::conn::http1::Builder::new()
                .keep_alive(true)
                .serve_connection(TokioIo::new(socket), service)
                .await;
        });
    }
}
