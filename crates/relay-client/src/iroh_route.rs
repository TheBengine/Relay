//! The account-free route as the rest of Relay sees it.
//!
//! A community reached this way has an address like `http://<key>.iroh:4580`.
//! The name is never looked up: Relay maps it to a loopback gateway run by the
//! `relay-iroh` helper, exactly as it does for the Tailscale route. The helper
//! carries chat and voice; admitted media streams also reach the host's fixed SFU.
use crate::{
    connection_path::{PathKind, PathReport, Transport},
    embedded_network::{blocked_endpoint, loopback_url, verify_route_identity},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Read},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{Mutex, OnceLock},
    time::Duration,
};

/// A host reached by key: what a friend needs to connect, and what a saved
/// community must remember. The token is a secret and is never printed.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedIroh {
    pub endpoint_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay: Option<String>,
    pub token: String,
}

impl std::fmt::Debug for SavedIroh {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SavedIroh")
            .field("endpoint_id", &self.endpoint_id)
            .field("token", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl SavedIroh {
    pub fn from_invitation(route: &crate::invitation_routes::IrohRoute) -> Self {
        Self {
            endpoint_id: route.endpoint_id.clone(),
            relay: route.relay.clone(),
            token: route.token.clone(),
        }
    }

    pub fn to_invitation(&self) -> Result<crate::invitation_routes::IrohRoute, String> {
        crate::invitation_routes::IrohRoute::new(
            &self.endpoint_id,
            self.relay.as_deref(),
            &self.token,
        )
    }

    pub fn address(&self) -> String {
        crate::route_choice::iroh_address(&self.endpoint_id)
    }
}

#[derive(Clone, PartialEq, Eq)]
struct RouteState {
    http_url: String,
    voice_udp_port: u16,
    generation: String,
    server_id: Option<String>,
}

struct Helper {
    child: Child,
    control: String,
    token: String,
    // Do not leave this helper (with the same persistent endpoint key) behind
    // after a hard app exit, where a second copy could race the new owner.
    _group: crate::handover_runtime::ProcessGroup,
    _liveness: crate::helper_liveness::Registration<Hosted>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HostBinding {
    server_id: String,
    lease_id: String,
}
#[derive(Clone, PartialEq, Eq)]
struct HostSpec {
    peer_port: u16,
    ingress: [u8; 32],
    transport: Option<HostTransport>,
}
#[derive(Clone, PartialEq, Eq)]
struct HostTransport { scope: String, endpoint: String }
#[derive(Clone)]
struct Hosted {
    binding: HostBinding,
    generation: [u8; 32],
    spec: HostSpec,
    // A pending lease is retained across lost startup replies, so the next
    // attempt retries the same operation instead of replacing a live route.
    saved: Option<SavedIroh>,
}
fn digest(bytes: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes).into()
}

static HELPER: OnceLock<Mutex<Option<Helper>>> = OnceLock::new();
/// Routes which are or were open. A closed route stays here so that a request
/// after a helper failure ends at a closed loopback port, never on the open internet.
static ROUTES: OnceLock<Mutex<HashMap<String, RouteState>>> = OnceLock::new();
/// What the invitations and saved communities told Relay about each host.
static KNOWN: OnceLock<Mutex<HashMap<String, SavedIroh>>> = OnceLock::new();
/// The community this PC hosts through iroh: its server identity and its route.
static HOSTED: OnceLock<Mutex<Option<Hosted>>> = OnceLock::new();
static LIVENESS: crate::helper_liveness::Registry<Hosted> = crate::helper_liveness::Registry::new();

fn slot() -> &'static Mutex<Option<Helper>> {
    HELPER.get_or_init(|| Mutex::new(None))
}
fn routes() -> &'static Mutex<HashMap<String, RouteState>> {
    ROUTES.get_or_init(|| Mutex::new(HashMap::new()))
}
fn known() -> &'static Mutex<HashMap<String, SavedIroh>> {
    KNOWN.get_or_init(|| Mutex::new(HashMap::new()))
}
fn hosted_slot() -> &'static Mutex<Option<Hosted>> {
    HOSTED.get_or_init(|| Mutex::new(None))
}

/// The key in an address such as `http://<64 hex>.iroh:4580`, if it is one.
pub fn endpoint_id(address: &str) -> Option<String> {
    let url = url::Url::parse(address).ok()?;
    if !matches!(url.scheme(), "http" | "ws")
        || url.port_or_known_default()? != 4580
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }
    let id = url.host_str()?.strip_suffix(".iroh")?;
    (id.len() == 64
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
    .then(|| id.to_owned())
}

pub fn is_iroh(address: &str) -> bool {
    endpoint_id(address).is_some()
}

fn helper_path() -> Option<PathBuf> {
    #[cfg(test)]
    if let Some(path) = std::env::var_os("RELAY_IROH_HELPER") {
        return Some(PathBuf::from(path));
    }
    let exe = std::env::current_exe().ok()?;
    Some(exe.parent()?.join("relay-iroh.exe"))
}

/// This installation includes the helper, so the route can be used.
pub fn available() -> bool {
    cfg!(windows) && helper_path().is_some_and(|path| path.is_file())
        || cfg!(test) && helper_path().is_some_and(|path| path.is_file())
}

/// Remembers a host's route so a later connection can use it.
pub fn register(saved: &SavedIroh) {
    if let Ok(mut map) = known().lock() {
        if map.len() < 64 || map.contains_key(&saved.endpoint_id) {
            let changed = map
                .get(&saved.endpoint_id)
                .is_some_and(|old| old.token != saved.token);
            map.insert(saved.endpoint_id.clone(), saved.clone());
            if changed {
                // Reopen with the new secret, preserving the pinned server identity.
                if let Ok(mut routes) = routes().lock() {
                    if let Some(route) = routes.get_mut(&saved.endpoint_id) {
                        route.generation.clear();
                    }
                }
            }
        }
    }
}

/// Install only an independently proved admission. A live route refreshes only
/// its next handshake, retaining its current connection, HTTP socket and voice port.
pub(crate) fn register_proved(saved: &SavedIroh, server_id: &str) -> Result<(), String> {
    saved.to_invitation()?;
    uuid::Uuid::parse_str(server_id).map_err(|_| "Invalid community identity.")?;
    let (previous, state) = {
        let known = known().lock().map_err(|_| "The route is busy.")?;
        let routes = routes().lock().map_err(|_| "The route is busy.")?;
        if known.len() >= 64 && !known.contains_key(&saved.endpoint_id) {
            return Err("The private connection list is full.".into());
        }
        (
            known.get(&saved.endpoint_id).cloned(),
            routes.get(&saved.endpoint_id).cloned(),
        )
    };
    if state
        .as_ref()
        .and_then(|r| r.server_id.as_deref())
        .is_some_and(|id| id != server_id)
    {
        return Err("This connection belongs to another community.".into());
    }
    if let Some(state) = state.as_ref().filter(|s| live(&s.generation)) {
        let previous = previous
            .as_ref()
            .ok_or("The private invitation changed. Check the new host again.")?;
        if previous != saved {
            let reply: RouteReply = with_helper(false, |helper| {
                if helper.token != state.generation {
                    return Err("The private connection changed. Try again.".into());
                }
                api(
                    helper,
                    "/route-refresh",
                    Some(serde_json::json!({
                        "route":route_body(saved),"expected_token":previous.token,
                    })),
                    Duration::from_secs(2),
                )
            })?;
            if reply.http_url != state.http_url || reply.voice_udp_port != state.voice_udp_port {
                return Err("The refreshed connection did not match the active route.".into());
            }
        }
    }
    let mut known = known().lock().map_err(|_| "The route is busy.")?;
    let routes = routes().lock().map_err(|_| "The route is busy.")?;
    install_proved(
        &mut known,
        &routes,
        saved,
        previous.as_ref(),
        state.as_ref(),
    )
}
fn install_proved(
    known: &mut HashMap<String, SavedIroh>,
    routes: &HashMap<String, RouteState>,
    saved: &SavedIroh,
    previous: Option<&SavedIroh>,
    state: Option<&RouteState>,
) -> Result<(), String> {
    if known.get(&saved.endpoint_id) != previous || routes.get(&saved.endpoint_id) != state {
        return Err("The private invitation changed during the host check. Try again.".into());
    }
    if known.len() >= 64 && !known.contains_key(&saved.endpoint_id) {
        return Err("The private connection list is full.".into());
    }
    known.insert(saved.endpoint_id.clone(), saved.clone());
    // Never invalidate the live generation here: the helper retained its sockets.
    Ok(())
}

/// What is remembered for an address, so it can be saved with the community.
pub fn saved_for(address: &str) -> Option<SavedIroh> {
    let id = endpoint_id(address)?;
    known().lock().ok()?.get(&id).cloned()
}

fn api<T: for<'de> Deserialize<'de>>(
    helper: &Helper,
    path: &str,
    body: Option<serde_json::Value>,
    wait: Duration,
) -> Result<T, String> {
    api_at(&helper.control, &helper.token, path, body, wait)
}
fn api_at<T: for<'de> Deserialize<'de>>(
    control: &str, token: &str, path: &str, body: Option<serde_json::Value>, wait: Duration,
) -> Result<T, String> {
    let http = reqwest::blocking::Client::builder()
        .no_proxy()
        .timeout(wait)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| "Cannot contact the account-free connection.")?;
    let address = format!("{}{}", control.trim_end_matches('/'), path);
    let request = if let Some(body) = body {
        http.post(address).json(&body)
    } else {
        http.get(address)
    };
    let response = request
        .bearer_auth(token)
        .send()
        .map_err(|_| "The account-free connection did not respond.")?;
    if !response.status().is_success() {
        if response.status() == reqwest::StatusCode::NOT_FOUND && (path.ends_with("-owned") || path.contains("prepared-route")) {
            return Err("Update Relay's account-free helper before hosting. The running connection was left untouched.".into());
        }
        return Err(match response.status().as_u16() {
            502 => {
                "Your friend's computer did not accept this connection. Ask for a new invitation."
                    .to_owned()
            }
            409 => "The account-free connection is busy. Try again.".to_owned(),
            _ => "The account-free connection is not ready. Try again.".to_owned(),
        });
    }
    let mut bytes = Vec::new();
    response
        .take(65_537)
        .read_to_end(&mut bytes)
        .map_err(|_| "Cannot read the connection status.")?;
    if bytes.len() > 65_536 {
        return Err("The connection status was too large.".into());
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| "The account-free connection returned an invalid result.".into())
}

fn launch() -> Result<Helper, String> {
    let binary = helper_path().ok_or("Cannot locate Relay.")?;
    if !binary.is_file() {
        return Err("This copy of Relay does not include the account-free connection.".into());
    }
    let base = std::env::var_os("RELAY_PROFILE_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            directories::ProjectDirs::from("org", "Relay", "Relay")
                .map(|d| d.data_local_dir().to_owned())
        })
        .ok_or("Cannot locate the connection's storage.")?;
    let state = base.join("iroh");
    let mut random = [0u8; 32];
    getrandom::getrandom(&mut random).map_err(|_| "Cannot create a connection token.")?;
    let token: String = random.iter().map(|b| format!("{b:02x}")).collect();
    let mut command = Command::new(binary);
    command
        .arg("--state-dir")
        .arg(state)
        .env("RELAY_IROH_CONTROL_TOKEN", &token)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // Tests run the helper without relays; the helper honours this in debug builds only.
    #[cfg(test)]
    command.env("RELAY_IROH_NETWORK", "loopback");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let group=crate::handover_runtime::ProcessGroup::new()
        .map_err(|_|"Cannot reserve ownership of the account-free helper.")?;
    let mut child = command
        .spawn()
        .map_err(|_| "Cannot start the account-free connection.")?;
    if group.add(&child).is_err() {
        let _=child.kill();let _=child.wait();
        return Err("Cannot reserve ownership of the account-free helper.".into());
    }
    let output = child
        .stdout
        .take()
        .ok_or("Cannot read the connection's startup.")?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let result = BufReader::new(output)
            .take(4097)
            .read_line(&mut line)
            .map(|_| line);
        let _ = tx.send(result);
    });
    let control = rx
        .recv_timeout(Duration::from_secs(10))
        .ok()
        .and_then(Result::ok)
        .filter(|s| s.len() <= 4096)
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v["control_url"].as_str().map(str::to_owned))
        .filter(|s| loopback_url(s));
    match control {
        Some(control) => {
            let observation = match LIVENESS.observe(&child, digest(token.as_bytes())) {
                Ok(observation) => observation,
                Err(error) => { let _ = child.kill(); let _ = child.wait(); return Err(error); }
            };
            Ok(Helper {
            child,
            control,
            token,
            _group:group,
            _liveness: observation,
        })},
        None => {
            let _ = child.kill();
            let _ = child.wait();
            Err("The account-free connection could not start.".into())
        }
    }
}

fn with_helper<T>(
    create: bool,
    action: impl FnOnce(&mut Helper) -> Result<T, String>,
) -> Result<T, String> {
    let mut guard = slot().lock().map_err(|_| "The connection is busy.")?;
    if guard
        .as_mut()
        .is_some_and(|h| h.child.try_wait().ok().flatten().is_some())
    {
        *guard = None;
    }
    if guard.is_none() && create {
        *guard = Some(launch()?);
    }
    action(
        guard
            .as_mut()
            .ok_or("The account-free connection is not running.")?,
    )
}

#[derive(Deserialize)]
struct RouteReply {
    http_url: String,
    voice_udp_port: u16,
}

fn route_body(saved: &SavedIroh) -> serde_json::Value {
    let mut body = serde_json::json!({"endpoint_id": saved.endpoint_id, "token": saved.token});
    if let Some(relay) = &saved.relay {
        body["relay"] = serde_json::json!(relay);
    }
    // Tests have no relay: the helper is told where the host listens on this PC.
    #[cfg(test)]
    if let Some(direct) = tests::DIRECT.lock().unwrap().clone() {
        body["direct"] = serde_json::json!(direct);
    }
    body
}

/// Only fetches an anonymous host proof over a separate admission. It cannot
/// replace a cached invitation, touch route sockets or forward account credentials.
/// The caller must still verify its independently pinned key and fresh challenge.
pub(crate) fn route_proof(
    saved: &SavedIroh,
    challenge: &str,
) -> Result<relay_protocol::handover::RouteProof, String> {
    saved.to_invitation()?;
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    if challenge.len() != 43
        || !URL_SAFE_NO_PAD
            .decode(challenge)
            .is_ok_and(|raw| raw.len() == 32 && URL_SAFE_NO_PAD.encode(&raw) == challenge)
    {
        return Err("The host proof challenge is invalid.".into());
    }
    // Hold the helper process slot only long enough to capture this generation's
    // local control. DNS/QUIC/HTTP waits must not lock normal call route operations.
    let (control, token) = with_helper(true, |helper| {
        Ok((
            helper.control.clone(),
            zeroize::Zeroizing::new(helper.token.clone()),
        ))
    })?;
    let proof = api_at(
        &control,
        token.as_str(),
        "/route-proof",
        Some(serde_json::json!({"route":route_body(saved),"challenge":challenge})),
        Duration::from_secs(14),
    )?;
    if !live(token.as_str()) {
        return Err("The private connection changed during the host check. Try again.".into());
    }
    Ok(proof)
}

/// Makes sure the route is open. `Ok(false)` means this is not an iroh address.
pub fn ensure_route(address: &str) -> Result<bool, String> {
    let Some(id) = endpoint_id(address) else {
        return Ok(false);
    };
    let current=routes().lock().map_err(|_| "The route is busy.")?.get(&id).cloned();
    if current.as_ref().is_some_and(|state|live(&state.generation)){return Ok(true);}
    let saved = known()
        .lock()
        .map_err(|_| "The route is busy.")?
        .get(&id)
        .cloned()
        .ok_or("This community needs its invitation again. Ask your friend for a new one.")?;
    open(&id, &saved, true)?;
    Ok(true)
}

fn open(id: &str, saved: &SavedIroh, verify_health: bool) -> Result<RouteState, String> {
    with_helper(true, |h| {
        let reply: RouteReply = api(
            h,
            "/route",
            Some(route_body(saved)),
            Duration::from_secs(25),
        )?;
        if !loopback_url(&reply.http_url) || reply.voice_udp_port == 0 {
            return Err("The account-free route could not be verified.".into());
        }
        let previous = routes().lock().ok().and_then(|r| r.get(id).cloned());
        let state = RouteState {
            http_url: reply.http_url,
            voice_udp_port: reply.voice_udp_port,
            generation: h.token.clone(),
            // A new helper must prove the known community again before it is used.
            server_id: previous.and_then(|p| p.server_id),
        };
        if let Some(expected) = state.server_id.as_ref().filter(|_|verify_health) {
            verify_route_identity(&state.http_url, expected)?;
        }
        routes()
            .lock()
            .map_err(|_| "The route is busy.")?
            .insert(id.to_owned(), state.clone());
        Ok(state)
    })
}

/// An independent, expiring route for readiness and generated media only. It
/// never enters KNOWN/ROUTES, changes a saved pin or replaces an active admission.
pub(crate) struct PreparedRoute {
    id: String,
    control: String,
    generation: zeroize::Zeroizing<String>,
    lease: String,
    http_url: String,
    voice_udp_port: u16,
    expires: Mutex<std::time::Instant>,
    purpose: RoutePurpose,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum RoutePurpose { Preflight, VoiceGateway }
impl RoutePurpose {
    fn open(self) -> &'static str { match self { Self::Preflight => "/prepared-route", Self::VoiceGateway => "/voice-gateway-route" } }
    fn status(self) -> &'static str { match self { Self::Preflight => "/prepared-route-status", Self::VoiceGateway => "/voice-gateway-route-status" } }
    fn close(self) -> &'static str { match self { Self::Preflight => "/close-prepared-route", Self::VoiceGateway => "/close-voice-gateway-route" } }
    fn matches(self, reply: &PreparedReply) -> bool {
        match self { Self::Preflight => reply.purpose.as_deref().is_none_or(|p|p=="preflight"),
            Self::VoiceGateway => reply.purpose.as_deref()==Some("voice_gateway") }
    }
}
#[derive(Deserialize)]
struct PreparedReply {
    lease: String,
    http_url: String,
    voice_udp_port: u16,
    expires_in_secs: u64,
    #[serde(default)]
    purpose: Option<String>,
}
impl PreparedRoute {
    pub(crate) fn open(saved: &SavedIroh, server_id: &str) -> Result<Self, String> {
        Self::open_purpose(saved, server_id, RoutePurpose::Preflight)
    }
    pub(crate) fn open_voice_gateway(saved: &SavedIroh, server_id: &str) -> Result<Self, String> {
        Self::open_purpose(saved, server_id, RoutePurpose::VoiceGateway)
    }
    fn open_purpose(saved: &SavedIroh, server_id: &str, purpose: RoutePurpose) -> Result<Self, String> {
        saved.to_invitation()?;
        uuid::Uuid::parse_str(server_id).map_err(|_| "Invalid community identity.")?;
        if routes().lock().map_err(|_| "The route is busy.")?
            .get(&saved.endpoint_id).and_then(|s| s.server_id.as_deref())
            .is_some_and(|known| known != server_id)
        { return Err("This connection belongs to another community.".into()); }
        // Capture private control briefly. Connecting to a new host must not hold
        // the normal helper's lock or pause any live voice/control worker.
        let (control, generation) = with_helper(true, |h| Ok((h.control.clone(), zeroize::Zeroizing::new(h.token.clone()))))?;
        Self::open_at(saved, control, generation, purpose)
    }
    fn open_at(saved: &SavedIroh, control: String, generation: zeroize::Zeroizing<String>, purpose: RoutePurpose) -> Result<Self, String> {
        let reply: PreparedReply = api_at(&control, generation.as_str(), purpose.open(),
            Some(route_body(saved)), Duration::from_secs(25))?;
        let correct_purpose = purpose.matches(&reply);
        let route = Self { id: saved.endpoint_id.clone(), control, generation,
            lease: reply.lease, http_url: reply.http_url, voice_udp_port: reply.voice_udp_port,
            expires: Mutex::new(std::time::Instant::now() + Duration::from_secs(reply.expires_in_secs.min(120))), purpose };
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
        if !correct_purpose || !loopback_url(&route.http_url)
            || route.voice_udp_port == 0 || reply.expires_in_secs == 0 || reply.expires_in_secs > 120
            || route.lease.len() != 43 || !URL_SAFE_NO_PAD.decode(&route.lease)
                .is_ok_and(|v| v.len() == 32 && URL_SAFE_NO_PAD.encode(v) == route.lease)
        { return Err("The prepared connection could not be verified.".into()); }
        Ok(route)
    }
    #[cfg(test)]
    pub(crate) fn open_for_test(saved: &SavedIroh, control: &str, token: &str) -> Result<Self, String> {
        saved.to_invitation()?;
        Self::open_at(saved, control.into(), zeroize::Zeroizing::new(token.into()), RoutePurpose::Preflight)
    }
    pub(crate) fn gateway(&self) -> Result<String, String> {
        if *self.expires.lock().map_err(|_| "The prepared route is busy.")? <= std::time::Instant::now() {
            return Err("The prepared connection expired. Check the new host again.".into());
        }
        let current: PreparedReply = api_at(&self.control, self.generation.as_str(), self.purpose.status(),
            Some(serde_json::json!({"lease":self.lease})), Duration::from_secs(2))?;
        if !self.matches(&current) { return Err("The prepared connection changed. Check the new host again.".into()); }
        Ok(self.http_url.clone())
    }
    fn matches(&self, current: &PreparedReply) -> bool {
        self.purpose.matches(current) && current.lease == self.lease && current.http_url == self.http_url
            && current.voice_udp_port == self.voice_udp_port && current.expires_in_secs > 0
            && current.expires_in_secs <= 120 && loopback_url(&current.http_url)
    }
    pub(crate) fn voice_gateway_address(&self) -> Result<std::net::SocketAddr, String> {
        if self.purpose != RoutePurpose::VoiceGateway { return Err("A dedicated voice route is required.".into()); }
        self.gateway()?;
        Ok(std::net::SocketAddr::from(([127,0,0,1], self.voice_udp_port)))
    }
    pub(crate) fn renew_voice_gateway(&self) -> Result<(), String> {
        let requested = std::time::Instant::now();
        if self.purpose != RoutePurpose::VoiceGateway
            || *self.expires.lock().map_err(|_| "The prepared route is busy.")? <= requested
        { return Err("The voice route expired.".into()); }
        let current: PreparedReply = api_at(&self.control, self.generation.as_str(), "/renew-voice-gateway-route",
            Some(serde_json::json!({"lease":self.lease})), Duration::from_secs(2))?;
        if !self.matches(&current) { return Err("The voice route changed.".into()); }
        let next = requested + Duration::from_secs(current.expires_in_secs);
        if next <= std::time::Instant::now() { return Err("The voice route expired.".into()); }
        *self.expires.lock().map_err(|_| "The prepared route is busy.")? = next;
        Ok(())
    }
    pub(crate) fn signaling(&self, raw: &str) -> Result<String, String> {
        let signal = url::Url::parse(raw).map_err(|_| "Invalid private signaling route")?;
        if endpoint_id(raw).as_deref() != Some(&self.id) || signal.scheme() != "ws"
            || signal.path() != "/" || signal.query().is_some() || signal.fragment().is_some()
        { return Err("Private signaling does not match the prepared connection.".into()); }
        let mut gateway = url::Url::parse(&self.gateway()?).map_err(|_| "Invalid private gateway")?;
        gateway.set_scheme("ws").map_err(|_| "Invalid private gateway")?;
        Ok(gateway.to_string())
    }
}
impl Drop for PreparedRoute {
    fn drop(&mut self) {
        // This object lives on the bounded handover job, never the UI/media loop.
        // A lost close reply is harmless: this exact lease expires in the helper.
        let _: Result<serde_json::Value, _> = api_at(&self.control, self.generation.as_str(),
            self.purpose.close(), Some(serde_json::json!({"lease":self.lease})), Duration::from_secs(1));
    }
}
pub(crate) fn bound_media_gateway(address:&str) -> Result<String,String> {
    let id=endpoint_id(address).ok_or("Invalid private media origin")?;
    let state=routes().lock().map_err(|_|"The route is busy.")?.get(&id).cloned()
        .filter(|s|s.server_id.is_some()).ok_or("The private community identity is not confirmed.")?;
    if !live(&state.generation)||!loopback_url(&state.http_url){return Err("The private connection changed.".into());}
    Ok(state.http_url)
}

fn live(generation: &str) -> bool {
    LIVENESS.alive(digest(generation.as_bytes()))
}

pub fn routed(address: &str) -> bool {
    endpoint_id(address).is_some_and(|id| routes().lock().map_or(true, |r| r.contains_key(&id)))
}

/// The loopback address which stands for the community.
pub fn endpoint(address: &str) -> Option<String> {
    let id = endpoint_id(address)?;
    let state = routes().lock().ok().and_then(|r| r.get(&id).cloned());
    let Some(state) = state.filter(|s| live(&s.generation)) else {
        return Some(blocked_endpoint(address));
    };
    let original = url::Url::parse(address).ok()?;
    let mut target = url::Url::parse(&state.http_url).ok()?;
    if original.scheme() == "ws" {
        let _ = target.set_scheme("ws");
    }
    target.set_path(original.path());
    target.set_query(original.query());
    Some(crate::embedded_network::same_form(address, &target))
}

/// Pins the route to the community the invitation was for.
pub fn bind_identity(address: &str, server_id: &str) -> Result<(), String> {
    let Some(id) = endpoint_id(address) else {
        return Ok(());
    };
    uuid::Uuid::parse_str(server_id).map_err(|_| "The community identity is invalid.")?;
    let mut map = routes().lock().map_err(|_| "The route is busy.")?;
    let state = map.get_mut(&id).ok_or("Reconnect the community first.")?;
    if state
        .server_id
        .as_deref()
        .is_some_and(|known| known != server_id)
    {
        return Err("The community identity changed. Ask the host for a new invitation.".into());
    }
    if state.server_id.is_none() {
        verify_route_identity(&state.http_url, server_id)?;
    }
    state.server_id = Some(server_id.to_owned());
    Ok(())
}

/// The loopback address and port which carry voice.
pub fn new_voice_route(address: &str) -> Result<(String, u16), String> {
    let id = endpoint_id(address).ok_or("The voice address is invalid.")?;
    let state = routes()
        .lock()
        .map_err(|_| "The route is busy.")?
        .get(&id)
        .cloned()
        .filter(|s| live(&s.generation))
        .ok_or("The account-free route is not open.")?;
    Ok((state.http_url, state.voice_udp_port))
}

/// One loopback-only QA helper owns a host plus an ordinary caller connection.
/// The production host entry point deliberately uses the desktop's fixed port;
/// this fixture uses the real test server's reserved ephemeral voice port.
#[cfg(test)]
pub(crate) fn media_fixture_host(peer: u16, voice: u16, ingress: &str, server: &str)
    -> Result<(SavedIroh, String, u16), String>
{
    let reply: serde_json::Value = with_helper(true, |h| api(h, "/host",
        Some(serde_json::json!({"peer_port":peer,"ingress_token":ingress,"udp_port":voice,"media_udp_port":0})),
        Duration::from_secs(10)))?;
    *tests::DIRECT.lock().map_err(|_| "The fixture route is busy.")? =
        Some(serde_json::from_value(reply["direct"].clone()).map_err(|_| "Missing loopback-only route.")?);
    let saved = SavedIroh {
        endpoint_id: reply["endpoint_id"].as_str().ok_or("Missing endpoint.")?.into(),
        relay: None,
        token: reply["token"].as_str().ok_or("Missing admission.")?.into(),
    };
    register(&saved);
    ensure_route(&saved.address())?;
    bind_identity(&saved.address(), server)?;
    let (http, voice) = new_voice_route(&saved.address())?;
    Ok((saved, http, voice))
}

/// The address the server's built-in-network mode is given when only the
/// account-free route is used. It is in the private range the launcher accepts,
/// is never routed anywhere and is never shown. Video needs a real one.
pub const ONLY_ADDRESS: &str = "100.127.255.254";

/// What starting the account-free route needs from a server which already runs.
#[derive(Debug, PartialEq, Eq)]
pub enum HostPlan {
    /// The server is already on the placeholder address: only the route is attached.
    Attach,
    /// The server is shared through Tailscale, or not through the built-in network
    /// at all: it restarts on the placeholder, which also lets go of Tailscale.
    Restart,
}

/// `existing` is the private address the running server was started with.
pub fn host_plan(existing: Option<&str>) -> HostPlan {
    if existing == Some(ONLY_ADDRESS) {
        HostPlan::Attach
    } else {
        HostPlan::Restart
    }
}

#[derive(Deserialize)]
struct PathReply {
    kind: String,
    #[serde(default)]
    rtt_ms: f64,
}

pub fn path(address: &str) -> Result<PathReport, String> {
    let id = endpoint_id(address).ok_or("This is not an account-free address.")?;
    let reply: PathReply = with_helper(false, |h| {
        api(
            h,
            "/path",
            Some(serde_json::json!({ "endpoint_id": id })),
            Duration::from_secs(5),
        )
    })?;
    Ok(PathReport {
        transport: Transport::Iroh,
        kind: match reply.kind.as_str() {
            "direct" => PathKind::Direct,
            "relay" => PathKind::Relay,
            _ => PathKind::Unknown,
        },
        region: None,
        rtt_ms: (reply.rtt_ms.is_finite() && reply.rtt_ms > 0.0 && reply.rtt_ms <= 60_000.0)
            .then_some(reply.rtt_ms as f32),
        same_network: false,
    })
}

#[derive(Deserialize)]
struct ProbeReply {
    rtt_ms: f64,
}

/// Connects and proves the invitation without opening a gateway. Used to
/// compare this route with the others before one is chosen.
pub fn probe(saved: &SavedIroh) -> Result<f32, String> {
    let reply: ProbeReply = with_helper(true, |h| {
        api(
            h,
            "/probe",
            Some(route_body(saved)),
            Duration::from_secs(25),
        )
    })?;
    if !reply.rtt_ms.is_finite() || reply.rtt_ms < 0.0 {
        return Err("The measurement was not valid.".into());
    }
    Ok(reply.rtt_ms as f32)
}

#[derive(Deserialize)]
struct HostReply {
    endpoint_id: String,
    #[serde(default)]
    relay_url: Option<String>,
    token: String,
    /// Only the helper's loopback test mode reports this.
    #[cfg(test)]
    #[serde(default)]
    direct: Vec<String>,
}

/// Prepare the destination's own invitation without opening a community route.
pub(crate) fn prepare_host_identity(scope: &str, expected: Option<&str>) -> Result<SavedIroh, String> {
    if !uuid::Uuid::parse_str(scope).is_ok_and(|id| id.to_string() == scope) {
        return Err("The destination transport identity is invalid.".into());
    }
    with_helper(true, |helper| {
        let reply: HostReply = api(helper, "/prepare-host-identity",
            Some(serde_json::json!({"scope":scope,"expected_endpoint":expected})), Duration::from_secs(5))?;
        let saved = SavedIroh { endpoint_id: reply.endpoint_id, relay: reply.relay_url, token: reply.token };
        saved.to_invitation()?;
        if expected.is_some_and(|pin| pin != saved.endpoint_id) {
            return Err("The prepared destination transport key changed.".into());
        }
        Ok(saved)
    })
}

/// Repeating setup retries its private lease without disconnecting calls.
/// A different server is never unhosted.
pub fn host(peer_port: u16, ingress_token: &str, server_id: &str) -> Result<SavedIroh, String> {
    host_bound(peer_port,ingress_token,server_id,None,None).map(|(saved,_,_)|saved)
}
fn host_bound(peer_port:u16,ingress_token:&str,server_id:&str,transport:Option<HostTransport>,expected_admission:Option<&str>)->Result<(SavedIroh,HostBinding,[u8;32]),String>{
    if !uuid::Uuid::parse_str(server_id).is_ok_and(|id| id.to_string() == server_id) {
        return Err("The community identity is invalid.".into());
    }
    let spec = HostSpec {
        peer_port,
        ingress: digest(ingress_token.as_bytes()),
        transport,
    };
    let saved = with_helper(true, |helper| {
        let generation = digest(helper.token.as_bytes());
        let mut slot = hosted_slot().lock().map_err(|_| "The host is busy.")?;
        if slot.as_ref().is_some_and(|h| h.generation != generation) {
            *slot = None;
        }
        if let Some(previous) = slot.as_ref() {
            if previous.binding.server_id != server_id {
                return Err("This PC is already sharing another community. Stop that host before starting this one.".into());
            }
            if previous.spec != spec {
                if previous.spec.transport != spec.transport {
                    return Err("The current host uses another transport identity. Its route was kept.".into());
                }
                // A new native backend has a new ingress capability. Release
                // only our exact old lease, never a helper's unknown occupant.
                release_host(helper, &previous.binding)?;
                *slot = None;
            }
        }
        let pending = slot.get_or_insert_with(|| Hosted {
            binding: HostBinding {
                server_id: server_id.to_owned(),
                lease_id: uuid::Uuid::new_v4().to_string(),
            },
            generation,
            spec,
            saved: None,
        });
        #[derive(Deserialize)]
        struct Reply {
            binding: HostBinding,
            route: HostReply,
        }
        let mut host=serde_json::json!({"peer_port":peer_port,"ingress_token":ingress_token,"udp_port":4581});
        if let Some(transport)=&pending.spec.transport {
            host["identity_scope"]=serde_json::json!(transport.scope);
            host["expected_endpoint"]=serde_json::json!(transport.endpoint);
        }
        if let Some(expected)=expected_admission {
            host["expected_admission"]=serde_json::json!(expected);
        }
        let reply: Reply = api(
            helper,
            "/host-owned",
            Some(serde_json::json!({
                "binding":pending.binding,"host":host
            })),
            Duration::from_secs(25),
        )?;
        if reply.binding != pending.binding {
            return Err("The hosted community changed. Its connection was left untouched.".into());
        }
        #[cfg(test)]
        {
            *tests::DIRECT.lock().unwrap() = Some(reply.route.direct.clone());
        }
        let saved = SavedIroh {
            endpoint_id: reply.route.endpoint_id,
            relay: reply
                .route
                .relay_url
                .filter(|u| u.starts_with("https://") && u.len() <= 200),
            token: reply.route.token,
        };
        saved.to_invitation()?;
        pending.saved = Some(saved.clone());
        LIVENESS.set_hosted(pending.generation, Some(pending.clone()));
        Ok((saved,pending.binding.clone(),pending.generation))
    })?;
    register(&saved.0);
    Ok(saved)
}

pub(crate) struct HostLease { binding:HostBinding,generation:[u8;32] }
impl HostLease {
    pub(crate) fn open(peer_port:u16,ingress:&str,server_id:&str,expected:&SavedIroh,scope:Option<&str>,granted:bool)->Result<Self,String>{
        let transport=scope.map(|scope|HostTransport{scope:scope.to_owned(),endpoint:expected.endpoint_id.clone()});
        let (saved,binding,generation)=host_bound(peer_port,ingress,server_id,transport,(!granted).then_some(expected.token.as_str()))?;
        let lease=Self{binding,generation};
        // A changed/lost local identity cannot be replaced with the source's
        // invitation. Drop releases only this newly created exact lease.
        // Once authority is durably granted, this host may revoke its old
        // invitation. Restart still requires the same private endpoint key;
        // only its locally stored current admission can differ from the card.
        if saved.endpoint_id!=expected.endpoint_id||(!granted&&saved.token!=expected.token) {return Err("This PC's prepared invitation changed. Prepare a new move.".into());}
        Ok(lease)
    }
    pub(crate) fn invitation(&self)->Option<SavedIroh> {
        with_helper(false,|helper| {
            if digest(helper.token.as_bytes())!=self.generation {return Err("The owned host changed.".into());}
            hosted_slot().lock().map_err(|_|"The owned host is busy.".to_owned())?
                .as_ref().filter(|host|host.binding==self.binding&&host.generation==self.generation)
                .and_then(|host|host.saved.clone()).ok_or("The owned host is not shared.".into())
        }).ok()
    }
    pub(crate) fn alive(&self)->Option<bool>{
        let mut helper=slot().try_lock().ok()?;
        let hosted=hosted_slot().try_lock().ok()?;
        Some(helper.as_mut().is_some_and(|h|digest(h.token.as_bytes())==self.generation&&h.child.try_wait().is_ok_and(|s|s.is_none()))
            && hosted.as_ref().is_some_and(|h|h.binding==self.binding&&h.generation==self.generation))
    }
}
impl Drop for HostLease {
    fn drop(&mut self){
        let binding=self.binding.clone();let generation=self.generation;
        let _=std::thread::Builder::new().name("relay-close-host-route".into()).spawn(move||{
            let _=release_owned_binding(&binding,generation);
        });
    }
}
fn release_owned_binding(binding:&HostBinding,generation:[u8;32])->Result<(),String>{
    with_helper(false,|helper|{
                if digest(helper.token.as_bytes())!=generation{return Ok(());}
                let mut current=hosted_slot().lock().map_err(|_|"The host is busy.")?;
                if current.as_ref().is_some_and(|h|h.binding==*binding&&h.generation==generation){
                    release_host(helper,binding)?;*current=None;
                }
                Ok(())
    })
}

fn release_host(helper: &Helper, binding: &HostBinding) -> Result<(), String> {
    let response: serde_json::Value = api(
        helper,
        "/unhost-owned",
        Some(serde_json::json!({"binding":binding})),
        Duration::from_secs(10),
    )?;
    if response["unhosted"] != true
        || response["binding"] != serde_json::to_value(binding).map_err(|_| "Invalid host lease")?
    {
        return Err("The host stop could not be confirmed. Try again; its lease was kept.".into());
    }
    LIVENESS.set_hosted(digest(helper.token.as_bytes()), None);
    Ok(())
}

/// Rotates admissions only for this exact app-owned host. Already admitted
/// calls continue, and repeating host() returns the current invitation token.
pub fn rotate(server_id: &str) -> Result<(), String> {
    let fresh = with_helper(false, |helper| {
        let mut slot = hosted_slot().lock().map_err(|_| "The host is busy.")?;
        let current = slot
            .as_mut()
            .filter(|h| {
                h.binding.server_id == server_id && h.generation == digest(helper.token.as_bytes())
            })
            .ok_or("This PC is not sharing that server through iroh.")?;
        let previous = current
            .saved
            .clone()
            .ok_or("Confirm the host's startup before rotating invitations.")?;
        #[derive(Deserialize)]
        struct Reply {
            binding: HostBinding,
            token: String,
        }
        let reply: Reply = api(
            helper,
            "/rotate-token-owned",
            Some(serde_json::json!({"binding":current.binding})),
            Duration::from_secs(10),
        )?;
        if reply.binding != current.binding {
            return Err("The hosted server changed. Open Invite friends again.".into());
        }
        let fresh = SavedIroh {
            token: reply.token,
            ..previous
        };
        fresh.to_invitation()?;
        current.saved = Some(fresh.clone());
        LIVENESS.set_hosted(current.generation, Some(current.clone()));
        Ok(fresh)
    })?;
    register(&fresh);
    Ok(())
}

pub fn hosted(server_id: &str) -> Option<SavedIroh> {
    // Same lock order as host/stop/rotate; no lease changes between the liveness
    // check and reading the route. Pending startup replies are never advertised.
    with_helper(false, |helper| {
        hosted_slot()
            .lock()
            .map_err(|_| "The host is busy.".to_owned())?
            .as_ref()
            .filter(|h| {
                h.binding.server_id == server_id && h.generation == digest(helper.token.as_bytes())
            })
            .and_then(|h| h.saved.clone())
            .ok_or("This server is not being shared.".into())
    })
    .ok()
}

/// UI-only observation: no helper control request or control-plane mutex.
/// Commands still use hosted() to serialize decisions with host/stop/rotation.
pub fn hosted_status(server_id: &str) -> Option<SavedIroh> {
    LIVENESS.hosted().filter(|host| host.binding.server_id == server_id)
        .and_then(|host| host.saved)
}

pub fn unhost(server_id: &str) -> Result<(), String> {
    // Do not start a helper for a stop, and never forget a lease when its stop
    // reply is lost. The next attempt can safely repeat the same scoped stop.
    with_helper(false, |helper| {
        let mut slot = hosted_slot().lock().map_err(|_| "The host is busy.")?;
        let Some(current) = slot.as_ref() else {
            return Ok(());
        };
        if current.generation != digest(helper.token.as_bytes()) {
            *slot = None;
            LIVENESS.set_hosted(digest(helper.token.as_bytes()), None);
            return Ok(());
        }
        if current.binding.server_id != server_id {
            return Ok(());
        }
        release_host(helper, &current.binding)?;
        *slot = None;
        Ok(())
    })
    .or_else(|error| {
        // An already exited helper owns no route; retain all remembered friend
        // invitations. A live helper's refusal/timeout must remain visible.
        let mut helper = slot().lock().map_err(|_| "The host is busy.")?;
        if helper
            .as_mut()
            .is_none_or(|h| h.child.try_wait().is_ok_and(|exit| exit.is_some()))
        {
            *hosted_slot().lock().map_err(|_| "The host is busy.")? = None;
            Ok(())
        } else {
            Err(error)
        }
    })
}

pub fn shutdown() {
    LIVENESS.retire();
    if let Ok(mut h) = hosted_slot().lock() {
        *h = None;
    }
    if let Ok(mut guard) = slot().lock() {
        if let Some(mut h) = guard.take() {
            LIVENESS.retire();
            let _ = api::<serde_json::Value>(
                &h,
                "/shutdown",
                Some(serde_json::json!({})),
                Duration::from_secs(3),
            );
            let until = std::time::Instant::now() + Duration::from_secs(3);
            while std::time::Instant::now() < until {
                if h.child.try_wait().ok().flatten().is_some() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(30));
            }
            let _ = h.child.kill();
            let _ = h.child.wait();
        }
    }
}

#[cfg(all(test, windows))]
mod liveness_tests {
    use super::*;

    #[test]
    fn exact_process_observation_survives_control_contention_and_retires_generations() {
        use std::{io::Write, os::windows::process::CommandExt, sync::mpsc};
        struct OwnedChild(Child);
        impl Drop for OwnedChild {
            fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); }
        }
        // A hidden, owned input-waiting process. No Relay profile, network,
        // helper setup, media device or system setting is involved.
        let mut child = OwnedChild(Command::new("C:/Windows/System32/cmd.exe")
            .args(["/d", "/q"]).stdin(Stdio::piped()).stdout(Stdio::null())
            .stderr(Stdio::null()).creation_flags(0x08000000).spawn().unwrap());
        let first_token = "owned-liveness-one";
        let first_generation = digest(first_token.as_bytes());
        let first = LIVENESS.observe(&child.0, first_generation).unwrap();
        LIVENESS.set_hosted(first_generation, Some(Hosted {
            binding: HostBinding { server_id: "owned-monitor".into(), lease_id: "owned".into() },
            generation: first_generation,
            spec: HostSpec { peer_port: 4580, ingress: [1; 32], transport: None },
            saved: Some(SavedIroh { endpoint_id: "ab".repeat(32), relay: None, token: "owned-admission".into() }),
        }));
        let control = slot().lock().unwrap();
        let host_control = hosted_slot().lock().unwrap();
        let (reply, result) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            let _ = reply.send((live(first_token), hosted_status("owned-monitor").is_some()));
        });
        let observed = result.recv_timeout(Duration::from_secs(1));
        drop(host_control);
        drop(control);
        reader.join().unwrap();
        assert_eq!(observed.unwrap(), (true, true), "read-only status must not wait for helper HTTP/control locks");

        let replacement_generation = digest(b"owned-liveness-two");
        let replacement = LIVENESS.observe(&child.0, replacement_generation).unwrap();
        drop(first); // A retired owner's later drop must not retire a replacement.
        assert!(!LIVENESS.alive(first_generation));
        assert!(LIVENESS.alive(replacement_generation));
        drop(replacement);
        assert!(!LIVENESS.alive(replacement_generation));
        assert!(child.0.try_wait().unwrap().is_none(), "retirement is independent of process exit");

        let final_observation = LIVENESS.observe(&child.0, replacement_generation).unwrap();
        child.0.stdin.take().unwrap().write_all(b"exit\r\n").unwrap();
        child.0.wait().unwrap();
        assert!(!LIVENESS.alive(replacement_generation), "the exact captured process exit must invalidate status");
        assert!(hosted_status("owned-monitor").is_none());
        drop(final_observation);
    }
}

#[cfg(test)]
mod verified_admission_tests {
    use super::*;
    #[test]
    fn a_proved_admission_preserves_live_sockets_and_refuses_racing_cache_changes() {
        let id = "a3".repeat(32);
        let old = SavedIroh {
            endpoint_id: id.clone(),
            relay: None,
            token: "a".repeat(43),
        };
        let next = SavedIroh {
            token: "b".repeat(43),
            ..old.clone()
        };
        let state = RouteState {
            http_url: "http://127.0.0.1:12345".into(),
            voice_udp_port: 12346,
            generation: "synthetic-helper-generation".into(),
            server_id: Some(uuid::Uuid::new_v4().to_string()),
        };
        let mut known = HashMap::from([(id.clone(), old.clone())]);
        let routes = HashMap::from([(id.clone(), state.clone())]);
        install_proved(&mut known, &routes, &next, Some(&old), Some(&state)).unwrap();
        assert!(known.get(&id) == Some(&next));
        assert!(routes.get(&id) == Some(&state)); // No generation invalidation or socket replacement.
        assert!(install_proved(&mut known, &routes, &old, Some(&old), Some(&state)).is_err());
        assert!(known.get(&id) == Some(&next));
        let mut changed = routes.clone();
        changed.get_mut(&id).unwrap().generation = "another-helper-generation".into();
        assert!(install_proved(&mut known, &changed, &old, Some(&next), Some(&state)).is_err());
        assert!(known.get(&id) == Some(&next));
        assert!(changed.get(&id).unwrap().generation == "another-helper-generation");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tailscale_server_restarts_on_the_placeholder_and_an_account_free_one_only_attaches() {
        // Shared through Tailscale: its real address is not the placeholder.
        assert_eq!(host_plan(Some("100.76.3.4")), HostPlan::Restart);
        // Not on the built-in network at all (a plain local server).
        assert_eq!(host_plan(None), HostPlan::Restart);
        // Already account-free: nothing restarts.
        assert_eq!(host_plan(Some(ONLY_ADDRESS)), HostPlan::Attach);
        // Lookalikes are not the placeholder.
        assert_eq!(host_plan(Some("100.127.255.253")), HostPlan::Restart);
        assert_eq!(host_plan(Some("")), HostPlan::Restart);
    }

    /// Where the test host listens, so the test friend can find it without a relay.
    pub(super) static DIRECT: Mutex<Option<Vec<String>>> = Mutex::new(None);

    const ID: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn only_a_key_address_on_relays_port_is_an_iroh_address() {
        let ok = format!("http://{ID}.iroh:4580");
        assert_eq!(endpoint_id(&ok).as_deref(), Some(ID));
        assert_eq!(
            endpoint_id(&format!("ws://{ID}.iroh:4580/api/ws")).as_deref(),
            Some(ID)
        );
        // Host names are case-insensitive; the key is always kept in lowercase.
        assert_eq!(
            endpoint_id(&format!("http://{}.IROH:4580", ID.to_uppercase())).as_deref(),
            Some(ID)
        );
        for bad in [
            format!("https://{ID}.iroh:4580"),
            format!("http://{ID}.iroh:4581"),
            format!("http://{ID}.iroh"),
            format!("http://{}.iroh:4580", &ID[..63]),
            format!("http://{ID}.example.org:4580"),
            format!("http://user@{ID}.iroh:4580"),
            "http://100.70.1.2:4580".to_owned(),
            "not an address".to_owned(),
        ] {
            assert!(!is_iroh(&bad), "{bad}");
        }
    }

    #[test]
    fn a_remembered_route_keeps_its_token_out_of_debug_output_and_round_trips() {
        let saved = SavedIroh {
            endpoint_id: ID.into(),
            relay: Some("https://relay.example.org".into()),
            token: "abcdefghijklmnopqrstuvwxyzABCDEF0123456789_-".into(),
        };
        assert!(!format!("{saved:?}").contains("abcdefghijklmnop"));
        let route = saved.to_invitation().unwrap();
        assert_eq!(SavedIroh::from_invitation(&route), saved);
        assert_eq!(saved.address(), format!("http://{ID}.iroh:4580"));
        let json = serde_json::to_string(&saved).unwrap();
        assert_eq!(serde_json::from_str::<SavedIroh>(&json).unwrap(), saved);
        let without_relay: SavedIroh = serde_json::from_str(&format!(
            r#"{{"endpoint_id":"{ID}","token":"abcdefghijklmnopqrstuvwxyz0123456789"}}"#
        ))
        .unwrap();
        assert!(without_relay.relay.is_none());
        let _ = register;
    }

    #[test]
    fn an_address_without_a_route_ends_at_a_closed_loopback_port() {
        let address = format!("http://{ID}.iroh:4580/api/health");
        assert!(!routed(&address) || routes().lock().unwrap().contains_key(ID));
        let mapped = endpoint(&address).expect("an iroh address is always mapped");
        assert!(mapped.starts_with("http://127.0.0.1:0/"), "{mapped}");
        assert!(mapped.ends_with("/api/health"));
        // A base address stays a base: joins must not produce "//api/...".
        let base = endpoint(&format!("http://{ID}.iroh:4580")).unwrap();
        assert_eq!(
            format!("{base}/api/hosting/handover"),
            "http://127.0.0.1:0/api/hosting/handover"
        );
        assert_eq!(
            endpoint(&format!("http://{ID}.iroh:4580/")).unwrap(),
            "http://127.0.0.1:0/"
        );
        assert!(endpoint("http://192.168.1.2:4580").is_none());
        assert_eq!(ensure_route("http://192.168.1.2:4580"), Ok(false));
        assert!(ensure_route(&address).unwrap_err().contains("invitation"));
    }

    /// A stand-in community: answers the health request with its identity and
    /// remembers the ingress capability it was shown.
    fn community(server_id: &'static str) -> (u16, std::sync::Arc<Mutex<Vec<String>>>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen: std::sync::Arc<Mutex<Vec<String>>> = Default::default();
        let log = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let log = log.clone();
                std::thread::spawn(move || {
                    let mut stream = stream;
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
                    let mut buffer = vec![0u8; 8192];
                    let mut filled = 0;
                    loop {
                        let Ok(n) = stream.read(&mut buffer[filled..]) else {
                            return;
                        };
                        if n == 0 {
                            return;
                        }
                        filled += n;
                        let text = String::from_utf8_lossy(&buffer[..filled]).to_lowercase();
                        if !text.contains("\r\n\r\n") {
                            continue;
                        }
                        let ingress = text
                            .lines()
                            .find_map(|l| l.strip_prefix("x-relay-network-ingress: "))
                            .unwrap_or("")
                            .trim()
                            .to_owned();
                        log.lock().unwrap().push(ingress);
                        let body = format!(
                            r#"{{"ok":true,"server_name":"Fixture","server_id":"{server_id}","protocol_version":{},"rtc_enabled":false}}"#,
                            relay_protocol::API_PROTOCOL_VERSION
                        );
                        let reply = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                            body.len()
                        );
                        if stream.write_all(reply.as_bytes()).is_err() {
                            return;
                        }
                        filled = 0;
                    }
                });
            }
        });
        (port, seen)
    }

    /// One owned helper hosts a stand-in and both client routes, all loopback.
    /// No Relay source/target server processes or physical devices are involved.
    #[test]
    #[ignore = "needs the relay-iroh helper (RELAY_IROH_HELPER)"]
    fn an_independent_iroh_gateway_carries_generated_opus_without_stealing_normal_voice() {
        use chacha20poly1305::{aead::{Aead, Payload}, ChaCha20Poly1305, KeyInit, Nonce};
        use std::{net::UdpSocket, sync::{Arc,atomic::{AtomicBool,Ordering}}, time::Instant};
        use crate::local_voice_gateway::{Client as HopClient, Grant, Worker};
        let folder = tempfile::tempdir().unwrap();
        struct Profile(Option<std::ffi::OsString>);
        impl Drop for Profile { fn drop(&mut self) { match self.0.take() {Some(v)=>std::env::set_var("RELAY_PROFILE_DIR",v),None=>std::env::remove_var("RELAY_PROFILE_DIR")} } }
        let _profile = Profile(std::env::var_os("RELAY_PROFILE_DIR"));
        std::env::set_var("RELAY_PROFILE_DIR",folder.path());
        struct StopHelper;
        impl Drop for StopHelper { fn drop(&mut self) { shutdown(); *DIRECT.lock().unwrap()=None; } }
        let _stop = StopHelper;
        let echo = UdpSocket::bind("127.0.0.1:0").unwrap();
        echo.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
        let udp = echo.local_addr().unwrap().port();
        let ending = Arc::new(AtomicBool::new(false)); let finish=ending.clone();
        struct StopEcho(Arc<AtomicBool>,Option<std::thread::JoinHandle<()>>);
        impl Drop for StopEcho {fn drop(&mut self){self.0.store(true,Ordering::Release);if let Some(t)=self.1.take(){let _=t.join();}}}
        let _echo=StopEcho(ending,Some(std::thread::spawn(move||{let mut b=[0;2048];while !finish.load(Ordering::Acquire){if let Ok((n,from))=echo.recv_from(&mut b){let _=echo.send_to(&b[..n],from);}}})));
        const SERVER: &str="321c581a-d27b-47de-84f8-29e817c9ae36";
        let (peer,_) = community(SERVER);
        let reply:serde_json::Value=with_helper(true,|h|api(h,"/host",Some(serde_json::json!({"peer_port":peer,"ingress_token":"gateway-ingress-fixture-012345678901234567890","udp_port":udp,"media_udp_port":0})),Duration::from_secs(10))).unwrap();
        *DIRECT.lock().unwrap()=Some(serde_json::from_value(reply["direct"].clone()).unwrap());
        let saved=SavedIroh{endpoint_id:reply["endpoint_id"].as_str().unwrap().into(),relay:None,token:reply["token"].as_str().unwrap().into()};
        register(&saved);ensure_route(&saved.address()).unwrap();bind_identity(&saved.address(),SERVER).unwrap();
        let ordinary=routes().lock().unwrap().get(&saved.endpoint_id).unwrap().clone();
        let normal=UdpSocket::bind("127.0.0.1:0").unwrap();normal.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let ping = |value:u8|{let body=vec![value;64];normal.send_to(&body,("127.0.0.1",ordinary.voice_udp_port)).unwrap();let mut b=[0;2048];let(n,_)=normal.recv_from(&mut b).unwrap();assert_eq!(&b[..n],body.as_slice());};
        ping(91);
        let lease=PreparedRoute::open_voice_gateway(&saved,SERVER).unwrap();
        let gateway_upstream=lease.voice_gateway_address().unwrap();assert_ne!(gateway_upstream.port(),ordinary.voice_udp_port);
        let worker=Worker::start("127.0.0.1:0".parse().unwrap(),gateway_upstream).unwrap();
        let grant=||Grant{id:[27;16],session:7,key:zeroize::Zeroizing::new([53;32]),valid_for:Duration::from_secs(2)};
        let now=Instant::now();assert!(worker.grant(grant(),now));let mut hop=HopClient::new(grant(),now,now).unwrap();
        let socket=UdpSocket::bind("127.0.0.1:0").unwrap();socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();let mut buffer=[0;2048];
        for _ in 0..3 {if let Some(probe)=hop.probe(Instant::now()){socket.send_to(&probe,worker.address).unwrap();let(n,_)=socket.recv_from(&mut buffer).unwrap();hop.receive(&buffer[..n],Instant::now());}std::thread::sleep(Duration::from_millis(55));}
        assert!(hop.healthy(Instant::now()));
        let samples:Vec<f32>=(0..480).map(|i|(i as f32*0.09).sin()*0.2).collect();let mut opus=[0;512];
        let mut encoder=opus::Encoder::new(48_000,opus::Channels::Mono,opus::Application::Voip).unwrap();let count=encoder.encode_float(&samples,&mut opus).unwrap();
        let mut clear=vec![1];clear.extend(1u64.to_le_bytes());clear.extend(&opus[..count]);
        let mut packet=7u64.to_le_bytes().to_vec();packet.extend(1u64.to_le_bytes());let mut iv=[0;12];iv[4..].copy_from_slice(&1u64.to_le_bytes());
        let cipher=ChaCha20Poly1305::new_from_slice(&[71;32]).unwrap();packet.extend(cipher.encrypt(Nonce::from_slice(&iv),Payload{msg:&clear,aad:&packet}).unwrap());
        let wrapped=hop.datagram(&packet,Instant::now()).unwrap();socket.send_to(&wrapped,worker.address).unwrap();
        let (n,from)=socket.recv_from(&mut buffer).unwrap();assert_eq!(from,worker.address);let returned=hop.receive(&buffer[..n],Instant::now()).unwrap();assert_eq!(returned,packet);
        let decoded=cipher.decrypt(Nonce::from_slice(&iv),Payload{msg:&returned[16..],aad:&returned[..16]}).unwrap();let mut pcm=[0.0;480];
        assert_eq!(opus::Decoder::new(48_000,opus::Channels::Mono).unwrap().decode_float(&decoded[9..],&mut pcm,false).unwrap(),480);
        assert!(worker.take_peer_activity());socket.send_to(b"unauthenticated-noise",worker.address).unwrap();std::thread::sleep(Duration::from_millis(30));assert!(!worker.take_peer_activity());
        lease.renew_voice_gateway().unwrap();assert_eq!(lease.voice_gateway_address().unwrap(),gateway_upstream);ping(92);
        drop(worker);drop(lease);ping(93);
        assert!(routes().lock().unwrap().get(&saved.endpoint_id)==Some(&ordinary));assert_eq!(saved_for(&saved.address()),Some(saved));
    }

    /// Needs the helper: `RELAY_IROH_HELPER=<path to relay-iroh.exe, a debug build>`.
    #[test]
    #[ignore = "needs the relay-iroh helper (RELAY_IROH_HELPER)"]
    fn a_host_and_a_friend_use_the_route_through_the_apps_own_functions() {
        struct StopOwnedHelper;
        impl Drop for StopOwnedHelper {
            fn drop(&mut self) { shutdown(); }
        }
        let _cleanup = StopOwnedHelper;
        const SERVER_ID: &str = "4be44aef-8c98-4db6-a9d5-81c2eb311330";
        assert!(
            available(),
            "set RELAY_IROH_HELPER to a debug build of relay-iroh"
        );
        let (peer_port, seen) = community(SERVER_ID);
        let ingress = "an-ingress-capability-for-the-peer-listener-0123456789";
        let saved = host(peer_port, ingress, SERVER_ID).expect("the host starts");
        assert_eq!(hosted(SERVER_ID), Some(saved.clone()));
        assert_eq!(
            host(peer_port, ingress, SERVER_ID).unwrap(),
            saved,
            "a repeated share retries its lease"
        );
        assert!(host(peer_port, ingress, "11111111-2222-3333-4444-555555555555").is_err());
        unhost("11111111-2222-3333-4444-555555555555").unwrap();
        assert_eq!(
            hosted(SERVER_ID),
            Some(saved.clone()),
            "another community cannot stop the route"
        );
        assert!(hosted("11111111-2222-3333-4444-555555555555").is_none());
        let address = saved.address();

        assert_eq!(ensure_route(&address), Ok(true));
        assert!(routed(&address));
        let mapped = endpoint(&format!("{address}/api/health")).unwrap();
        assert!(
            mapped.starts_with("http://127.0.0.1:") && !mapped.contains(":0/"),
            "{mapped}"
        );
        let http = reqwest::blocking::Client::builder()
            .no_proxy()
            .build()
            .unwrap();
        let summary: crate::connection_setup::ServerSummary =
            http.get(&mapped).send().unwrap().json().unwrap();
        assert!(summary.validate(Some(SERVER_ID)).is_ok());
        assert_eq!(
            seen.lock().unwrap().last().map(String::as_str),
            Some(ingress),
            "the community saw the host's capability"
        );

        assert!(bind_identity(&address, SERVER_ID).is_ok());
        let prepared=PreparedRoute::open(&saved,SERVER_ID).unwrap();
        assert_ne!(prepared.gateway().unwrap(),endpoint(&address).unwrap().trim_end_matches('/'));
        assert!(prepared.signaling(&address.replacen("http://","ws://",1)).is_ok());
        let conflicting=SavedIroh{token:"C".repeat(43),..saved.clone()};
        assert!(PreparedRoute::open(&conflicting,SERVER_ID).is_err());
        assert_eq!(saved_for(&address),Some(saved.clone()));
        let hinted=SavedIroh{relay:Some("https://relay.invalid".into()),..saved.clone()};
        assert!(PreparedRoute::open(&hinted,SERVER_ID).is_ok());
        assert_eq!(saved_for(&address),Some(saved.clone()),"an optional relay hint must not replace the admitted route");
        assert_eq!(http.get(&mapped).send().unwrap().status(),reqwest::StatusCode::OK,"checking a different invitation must not interrupt the admitted route");
        assert!(
            bind_identity(&address, "11111111-2222-3333-4444-555555555555").is_err(),
            "a different community is refused"
        );
        let (gateway, port) = new_voice_route(&address).unwrap();
        assert!(gateway.starts_with("http://127.0.0.1:") && port > 0);
        let path = path(&address).unwrap();
        assert_eq!(path.transport, Transport::Iroh);
        assert_eq!(path.kind, PathKind::Direct);
        assert!(probe(&saved).unwrap() >= 0.0);
        // A wrong token never opens a route.
        let wrong = SavedIroh {
            token: "abcdefghijklmnopqrstuvwxyzABCDEF0123456789_-".into(),
            ..saved.clone()
        };
        assert!(probe(&wrong).is_err());

        assert!(rotate("11111111-2222-3333-4444-555555555555").is_err());
        rotate(SERVER_ID).unwrap();
        let fresh = hosted(SERVER_ID).unwrap();
        assert_eq!(fresh.endpoint_id, saved.endpoint_id);
        assert_ne!(fresh.token, saved.token);
        // Existing admissions continue, but explicitly close the friend's route
        // so this proves a new handshake, not a cached connection.
        with_helper(false, |h| {
            api::<serde_json::Value>(
                h,
                "/close-route",
                Some(serde_json::json!({ "endpoint_id": fresh.endpoint_id })),
                Duration::from_secs(5),
            )
        })
        .unwrap();
        assert!(probe(&saved).is_err(), "the old admission cannot reconnect");
        assert!(probe(&fresh).is_ok(), "the fresh admission reconnects");
        assert_eq!(ensure_route(&address), Ok(true));
        assert!(bind_identity(&address, SERVER_ID).is_ok());
        let mapped = endpoint(&format!("{address}/api/health")).unwrap();
        let summary: crate::connection_setup::ServerSummary =
            http.get(mapped).send().unwrap().json().unwrap();
        assert!(summary.validate(Some(SERVER_ID)).is_ok());

        let lease=HostLease::open(peer_port,ingress,SERVER_ID,&fresh,None,false).unwrap();
        let replacement=host(peer_port,"replacement-ingress-capability-0123456789",SERVER_ID).unwrap();
        release_owned_binding(&lease.binding,lease.generation).unwrap();
        assert_eq!(hosted(SERVER_ID),Some(replacement.clone()),"late cleanup cannot release a replacement lease");
        assert!(probe(&replacement).is_ok());
        drop(lease);
        unhost(SERVER_ID).unwrap();
        assert!(hosted(SERVER_ID).is_none());
        let scope="44444444-4444-8444-8444-444444444444";
        let prepared=prepare_host_identity(scope,None).unwrap();
        assert_ne!(prepared.endpoint_id,fresh.endpoint_id,"a transferred community has its own endpoint");
        let scoped_lease=HostLease::open(peer_port,ingress,SERVER_ID,&prepared,Some(scope),false).unwrap();
        assert_eq!(hosted(SERVER_ID),Some(prepared.clone()));
        assert!(host(peer_port,ingress,SERVER_ID).is_err(),"default sharing cannot replace a scoped host");
        assert!(probe(&prepared).is_ok());
        rotate(SERVER_ID).unwrap();
        let rotated=hosted(SERVER_ID).unwrap();
        assert_eq!(rotated.endpoint_id,prepared.endpoint_id);
        assert_ne!(rotated.token,prepared.token);
        assert!(HostLease::open(peer_port,ingress,SERVER_ID,&prepared,Some(scope),false).is_err());
        assert_eq!(hosted(SERVER_ID),Some(rotated.clone()),"failed standby check must leave an existing host running");
        unhost(SERVER_ID).unwrap();
        let resumed=HostLease::open(peer_port,ingress,SERVER_ID,&prepared,Some(scope),true).unwrap();
        assert_eq!(hosted(SERVER_ID),Some(rotated.clone()),"a granted restart uses the same endpoint and current local admission");
        release_owned_binding(&scoped_lease.binding,scoped_lease.generation).unwrap();
        assert_eq!(hosted(SERVER_ID),Some(rotated));
        drop(scoped_lease);
        unhost(SERVER_ID).unwrap();
        drop(resumed);
        let legacy:HostReply=with_helper(false,|h|api(h,"/host-identity",None,Duration::from_secs(5))).unwrap();
        assert_eq!(legacy.endpoint_id,fresh.endpoint_id);
        assert_eq!(legacy.token,fresh.token,"scoped revocation must not rotate the old community's invitation");
        shutdown();
        assert!(!live("anything"));
        assert!(
            endpoint(&address).unwrap() == "http://127.0.0.1:0",
            "after shutdown the address ends at a closed port"
        );
    }
}
