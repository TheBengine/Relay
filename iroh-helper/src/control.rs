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
use std::{convert::Infallible, path::PathBuf, str::FromStr, sync::Arc, time::Duration};
use tokio::{net::TcpListener, sync::Notify};

const MAX_BODY: usize = 4096;
const HOST_ONLINE_WAIT: Duration = Duration::from_secs(8);

pub struct Control {
    token_digest: [u8; 32],
    state_dir: PathBuf,
    network: Network,
    host: tokio::sync::Mutex<Option<Hosted>>,
    client: tokio::sync::Mutex<Option<Arc<Client>>>,
    /// Route-proof exchanges. Apart from `client`: its own budget and no shared route.
    proofs: crate::route_proof::Prover,
    prepared: crate::prepared_route::Store,
    /// Signalled by `/shutdown`; the program then ends.
    pub stop: Notify,
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
            proofs: crate::route_proof::Prover::new(),
            prepared: crate::prepared_route::Store::new(),
            stop: Notify::new(),
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
    #[serde(default)]
    identity_scope: Option<String>,
    #[serde(default)]
    expected_endpoint: Option<String>,
    #[serde(default)]
    expected_admission: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PrepareIdentity {
    scope: String,
    #[serde(default)]
    expected_endpoint: Option<String>,
}

fn default_media_port() -> u16 {
    7882
}

/// A private app lease, distinct from the community's public identity and the
/// invitation capability. A late stop/rotate cannot affect its replacement.
#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct HostBinding {
    server_id: String,
    lease_id: String,
}
impl HostBinding {
    fn valid(&self) -> bool {
        canonical_id(&self.server_id) && canonical_id(&self.lease_id)
    }
}
fn canonical_id(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(i, b)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                b == b'-'
            } else {
                b.is_ascii_digit() || (b'a'..=b'f').contains(&b)
            }
        })
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OwnedStart {
    binding: HostBinding,
    host: HostInput,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OwnedChange {
    binding: HostBinding,
}
struct HostSpec {
    peer: u16,
    voice: u16,
    media: u16,
    ingress: [u8; 32],
    identity_scope: Option<String>,
    expected_endpoint: Option<String>,
}
impl HostSpec {
    fn new(h: &HostInput) -> Self {
        Self {
            peer: h.peer_port,
            voice: h.udp_port,
            media: h.media_udp_port,
            ingress: protocol::digest(&h.ingress_token),
            identity_scope: h.identity_scope.clone(),
            expected_endpoint: h.expected_endpoint.clone(),
        }
    }
    fn matches(&self, h: &HostInput) -> bool {
        self.peer == h.peer_port
            && self.voice == h.udp_port
            && self.media == h.media_udp_port
            && protocol::token_matches(&self.ingress, &h.ingress_token)
            && self.identity_scope == h.identity_scope
            && self.expected_endpoint == h.expected_endpoint
    }
}
struct Hosted {
    service: HostService,
    binding: Option<HostBinding>,
    spec: HostSpec,
    reply: HostReply,
    identity_dir: PathBuf,
}

#[derive(Clone, Serialize)]
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

/// A host route and a fresh challenge, for `/route-proof`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProofInput {
    route: RouteInput,
    challenge: String,
}

/// The admission an already open route should use for later handshakes, and the one it
/// is expected to hold now, for `/route-refresh`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RefreshInput {
    route: RouteInput,
    expected_token: String,
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PreparedInput { lease: String }
#[derive(Serialize)]
struct PreparedReply { lease: String, http_url: String, voice_udp_port: u16, expires_in_secs: u64 }
fn prepared_reply(value: crate::prepared_route::Lease) -> Reply {
    json(StatusCode::OK, &PreparedReply { lease: value.id,
        http_url: format!("http://{}", value.route.http),
        voice_udp_port: value.route.voice_udp_port, expires_in_secs: value.expires_in_secs })
}
#[derive(Serialize)]
struct GatewayReply { lease: String, http_url: String, voice_udp_port: u16, expires_in_secs: u64, purpose: &'static str }
fn gateway_reply(value: crate::prepared_route::Lease) -> Reply {
    json(StatusCode::OK, &GatewayReply { lease: value.id,
        http_url: format!("http://{}", value.route.http),
        voice_udp_port: value.route.voice_udp_port, expires_in_secs: value.expires_in_secs,
        purpose: value.purpose.as_str() })
}
fn gateway_error(value: crate::prepared_route::Error) -> Reply {
    use crate::prepared_route::Error;
    match value {
        Error::Invalid => error(StatusCode::BAD_REQUEST, "A valid voice gateway route is required"),
        Error::Busy => error(StatusCode::TOO_MANY_REQUESTS, "Two private routes are already open"),
        Error::Missing => error(StatusCode::CONFLICT, "The voice gateway route expired or was closed"),
        Error::Failed => error(StatusCode::BAD_GATEWAY, "The voice gateway route is unavailable"),
    }
}
fn prepared_error(value: crate::prepared_route::Error) -> Reply {
    use crate::prepared_route::Error;
    match value {
        Error::Invalid => error(StatusCode::BAD_REQUEST, "A valid prepared route is required"),
        Error::Busy => error(StatusCode::TOO_MANY_REQUESTS, "Two prepared routes are already open"),
        Error::Missing => error(StatusCode::CONFLICT, "The prepared route expired or was closed"),
        Error::Failed => error(StatusCode::BAD_GATEWAY, "The prepared route is unavailable"),
    }
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
            (Method::POST, "/prepare-host-identity") => {
                let input: PrepareIdentity = match body(request).await {
                    Ok(input) => input, Err(reply) => return reply,
                };
                // Serialize identity preparation and hosting/rotation. Preparing
                // another namespace does not stop the currently hosted server.
                let _host = self.host.lock().await;
                match token::scoped_host_identity(&self.state_dir,&input.scope,input.expected_endpoint.as_deref(),true) {
                    Ok((_,key,admission)) => json(StatusCode::OK, &HostReply {
                        endpoint_id:key.public().to_string(), direct:Vec::new(),
                        relay_url:None, token:admission,
                    }),
                    Err(_) => error(StatusCode::CONFLICT,"The destination transport identity needs recovery"),
                }
            }
            (Method::GET, "/host-identity") => {
                // Only prepare this machine's established key/capability. No
                // endpoint, listener or forwarding service is started here.
                let host = self.host.lock().await;
                if host.is_some() {
                    return error(StatusCode::CONFLICT, "Another host already owns this helper");
                }
                match token::load_host_identity(&self.state_dir) {
                    Ok((key, admission)) => json(StatusCode::OK, &HostReply {
                        endpoint_id: key.public().to_string(), direct: Vec::new(),
                        relay_url: None, token: admission,
                    }),
                    Err(_) => error(StatusCode::CONFLICT, "The established host identity needs recovery"),
                }
            }
            (Method::GET, "/status") => {
                let host = self.host.lock().await;
                json(
                    StatusCode::OK,
                    &Status {
                        version: 1,
                        phase: if host.is_some() { "hosting" } else { "idle" },
                        endpoint_id: host.as_ref().map(|h| h.service.endpoint_id().to_string()),
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
                let spec = HostSpec::new(&input);
                match self.start_host(input).await {
                    Ok((service, reply, identity_dir)) => {
                        *host = Some(Hosted {
                            service,
                            binding: None,
                            spec,
                            reply: reply.clone(),
                            identity_dir,
                        });
                        json(StatusCode::OK, &reply)
                    }
                    Err(_) => error(
                        StatusCode::BAD_GATEWAY,
                        "The account-free route could not start",
                    ),
                }
            }
            (Method::POST, "/host-owned") => {
                let input: OwnedStart = match body(request).await {
                    Ok(v) => v,
                    Err(r) => return r,
                };
                if !input.binding.valid() {
                    return error(
                        StatusCode::BAD_REQUEST,
                        "Valid community and lease IDs required",
                    );
                }
                let mut host = self.host.lock().await;
                if let Some(current) = host.as_ref() {
                    if current.binding.as_ref() == Some(&input.binding)
                        && current.spec.matches(&input.host)
                    {
                        if input.host.expected_admission.as_ref().is_some_and(|token|
                            !protocol::token_matches(&protocol::digest(&current.reply.token),token)) {
                            return error(StatusCode::CONFLICT,"The prepared admission changed; current host retained");
                        }
                        return json(
                            StatusCode::OK,
                            &serde_json::json!({"binding":input.binding,"route":current.reply}),
                        );
                    }
                    return error(
                        StatusCode::CONFLICT,
                        "Another host or configuration owns this route; it was left running",
                    );
                }
                let spec = HostSpec::new(&input.host);
                match self.start_host(input.host).await {
                    Ok((service, reply, identity_dir)) => {
                        let response = json(
                            StatusCode::OK,
                            &serde_json::json!({"binding":input.binding,"route":reply}),
                        );
                        *host = Some(Hosted {
                            service,
                            binding: Some(input.binding),
                            spec,
                            reply,
                            identity_dir,
                        });
                        response
                    }
                    Err(_) => error(
                        StatusCode::BAD_GATEWAY,
                        "The account-free route could not start",
                    ),
                }
            }
            (Method::POST, "/unhost-owned") => {
                let input: OwnedChange = match body(request).await {
                    Ok(v) => v,
                    Err(r) => return r,
                };
                if !input.binding.valid() {
                    return error(
                        StatusCode::BAD_REQUEST,
                        "Valid community and lease IDs required",
                    );
                }
                let mut host = self.host.lock().await;
                if host
                    .as_ref()
                    .is_some_and(|h| h.binding.as_ref() != Some(&input.binding))
                {
                    return error(
                        StatusCode::CONFLICT,
                        "This lease does not own the current host",
                    );
                }
                if let Some(current) = host.take() {
                    current.service.stop().await;
                }
                json(
                    StatusCode::OK,
                    &serde_json::json!({"unhosted":true,"binding":input.binding}),
                )
            }
            (Method::POST, "/rotate-token-owned") => {
                let input: OwnedChange = match body(request).await {
                    Ok(v) => v,
                    Err(r) => return r,
                };
                if !input.binding.valid() {
                    return error(
                        StatusCode::BAD_REQUEST,
                        "Valid community and lease IDs required",
                    );
                }
                let mut host = self.host.lock().await;
                let Some(current) = host
                    .as_mut()
                    .filter(|h| h.binding.as_ref() == Some(&input.binding))
                else {
                    return error(
                        StatusCode::CONFLICT,
                        "This lease does not own the current host",
                    );
                };
                match token::rotate_token(&current.identity_dir).and_then(|fresh| {
                    current.service.set_token(&fresh)?;
                    Ok(fresh)
                }) {
                    Ok(fresh) => {
                        current.reply.token = fresh.clone();
                        json(
                            StatusCode::OK,
                            &serde_json::json!({"binding":input.binding,"token":fresh}),
                        )
                    }
                    Err(_) => error(StatusCode::BAD_GATEWAY, "The token could not be replaced"),
                }
            }
            (Method::POST, "/unhost") => {
                let mut host = self.host.lock().await;
                if host.as_ref().is_some_and(|h| h.binding.is_some()) {
                    return error(
                        StatusCode::CONFLICT,
                        "Use the owning lease to stop this host",
                    );
                }
                match host.take() {
                    Some(current) => {
                        current.service.stop().await;
                        json(StatusCode::OK, &serde_json::json!({"unhosted":true}))
                    }
                    None => error(
                        StatusCode::CONFLICT,
                        "This helper does not host a community",
                    ),
                }
            }
            (Method::POST, "/rotate-token") => {
                let mut host = self.host.lock().await;
                let Some(current) = host.as_mut().filter(|h| h.binding.is_none()) else {
                    return error(
                        StatusCode::CONFLICT,
                        "Use the owning lease to rotate this host",
                    );
                };
                match token::rotate_token(&current.identity_dir).and_then(|fresh| {
                    current.service.set_token(&fresh)?;
                    Ok(fresh)
                }) {
                    Ok(fresh) => {
                        current.reply.token = fresh.clone();
                        json(StatusCode::OK, &serde_json::json!({"token":fresh}))
                    }
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
            (Method::POST, "/prepared-route") => {
                let input: RouteInput = match body(request).await { Ok(v) => v, Err(reply) => return reply };
                let route = match spec(&input, &self.network) { Ok(v) => v, Err(reply) => return reply };
                match self.prepared.open(&self.network, route).await { Ok(v) => prepared_reply(v), Err(e) => prepared_error(e) }
            }
            (Method::POST, "/prepared-route-status") => {
                let input: PreparedInput = match body(request).await { Ok(v) => v, Err(reply) => return reply };
                match self.prepared.status(&input.lease) { Ok(v) => prepared_reply(v), Err(e) => prepared_error(e) }
            }
            (Method::POST, "/close-prepared-route") => {
                let input: PreparedInput = match body(request).await { Ok(v) => v, Err(reply) => return reply };
                match self.prepared.close(&input.lease).await {
                    Ok(()) => json(StatusCode::OK, &serde_json::json!({"closed":true})), Err(e) => prepared_error(e)
                }
            }
            (Method::POST, "/voice-gateway-route") => {
                let input: RouteInput = match body(request).await { Ok(v) => v, Err(reply) => return reply };
                let route = match spec(&input, &self.network) { Ok(v) => v, Err(reply) => return reply };
                match self.prepared.open_gateway(&self.network, route).await { Ok(v) => gateway_reply(v), Err(e) => gateway_error(e) }
            }
            (Method::POST, "/voice-gateway-route-status") => {
                let input: PreparedInput = match body(request).await { Ok(v) => v, Err(reply) => return reply };
                match self.prepared.status_gateway(&input.lease) { Ok(v) => gateway_reply(v), Err(e) => gateway_error(e) }
            }
            (Method::POST, "/renew-voice-gateway-route") => {
                let input: PreparedInput = match body(request).await { Ok(v) => v, Err(reply) => return reply };
                match self.prepared.renew_gateway(&input.lease) { Ok(v) => gateway_reply(v), Err(e) => gateway_error(e) }
            }
            (Method::POST, "/close-voice-gateway-route") => {
                let input: PreparedInput = match body(request).await { Ok(v) => v, Err(reply) => return reply };
                match self.prepared.close_gateway(&input.lease).await {
                    Ok(()) => json(StatusCode::OK, &serde_json::json!({"closed":true})), Err(e) => gateway_error(e)
                }
            }
            (Method::POST, "/route-proof") => {
                use crate::route_proof::ProofError;
                let input: ProofInput = match body(request).await {
                    Ok(input) => input,
                    Err(reply) => return reply,
                };
                let spec = match spec(&input.route, &self.network) {
                    Ok(spec) => spec,
                    Err(reply) => return reply,
                };
                // Never the shared client, its routes or its connections.
                match self
                    .proofs
                    .prove(&self.network, spec, &input.challenge)
                    .await
                {
                    Ok(proof) => {
                        let mut reply = Response::new(Full::new(proof));
                        let headers = reply.headers_mut();
                        headers.insert(
                            hyper::header::CONTENT_TYPE,
                            "application/json".parse().unwrap(),
                        );
                        headers.insert(hyper::header::CACHE_CONTROL, "no-store".parse().unwrap());
                        reply
                    }
                    Err(ProofError::Invalid) => error(
                        StatusCode::BAD_REQUEST,
                        "A valid route and challenge are required",
                    ),
                    Err(ProofError::Busy) => error(
                        StatusCode::TOO_MANY_REQUESTS,
                        "A route proof is already being checked",
                    ),
                    Err(ProofError::Failed) => error(
                        StatusCode::BAD_GATEWAY,
                        "The host did not give a route proof",
                    ),
                }
            }
            (Method::POST, "/route-refresh") => {
                let input: RefreshInput = match body(request).await {
                    Ok(input) => input,
                    Err(reply) => return reply,
                };
                let spec = match spec(&input.route, &self.network) {
                    Ok(spec) => spec,
                    Err(reply) => return reply,
                };
                if !protocol::valid_token(&input.expected_token) {
                    return error(
                        StatusCode::BAD_REQUEST,
                        "A valid invitation route is required",
                    );
                }
                // Only an existing route: no client is created and nothing is dialled.
                let Some(client) = self.client.lock().await.clone() else {
                    return error(StatusCode::CONFLICT, "The route could not be refreshed");
                };
                match client.refresh_route(spec, &input.expected_token).await {
                    Ok(info) => json(
                        StatusCode::OK,
                        &RouteReply {
                            http_url: format!("http://{}", info.http),
                            voice_udp_port: info.voice_udp_port,
                        },
                    ),
                    Err(_) => error(StatusCode::CONFLICT, "The route could not be refreshed"),
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

    async fn start_host(&self, input: HostInput) -> Result<(HostService, HostReply, PathBuf)> {
        let (identity_dir,key,admission) = if let Some(scope) = input.identity_scope.as_deref() {
            token::scoped_host_identity(&self.state_dir,scope,input.expected_endpoint.as_deref(),false)?
        } else {
            anyhow::ensure!(input.expected_endpoint.is_none(),"A scoped transport identity is required");
            let (key,admission) = token::load_host_identity(&self.state_dir)?;
            (self.state_dir.clone(),key,admission)
        };
        anyhow::ensure!(input.expected_admission.as_ref().is_none_or(|expected|
            protocol::token_matches(&protocol::digest(&admission),expected)),"The prepared admission changed");
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
        Ok((service, reply, identity_dir))
    }

    pub async fn shutdown(&self) {
        self.prepared.shutdown().await;
        if let Some(host) = self.host.lock().await.take() {
            host.service.stop().await;
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
