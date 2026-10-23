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

#[derive(Clone)]
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
}

static HELPER: OnceLock<Mutex<Option<Helper>>> = OnceLock::new();
/// Routes which are or were open. A closed route stays here so that a request
/// after a helper failure ends at a closed loopback port, never on the open internet.
static ROUTES: OnceLock<Mutex<HashMap<String, RouteState>>> = OnceLock::new();
/// What the invitations and saved communities told Relay about each host.
static KNOWN: OnceLock<Mutex<HashMap<String, SavedIroh>>> = OnceLock::new();
/// The community this PC hosts through iroh: its server identity and its route.
static HOSTED: OnceLock<Mutex<Option<(String, SavedIroh)>>> = OnceLock::new();

fn slot() -> &'static Mutex<Option<Helper>> {
    HELPER.get_or_init(|| Mutex::new(None))
}
fn routes() -> &'static Mutex<HashMap<String, RouteState>> {
    ROUTES.get_or_init(|| Mutex::new(HashMap::new()))
}
fn known() -> &'static Mutex<HashMap<String, SavedIroh>> {
    KNOWN.get_or_init(|| Mutex::new(HashMap::new()))
}
fn hosted_slot() -> &'static Mutex<Option<(String, SavedIroh)>> {
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
    let http = reqwest::blocking::Client::builder()
        .no_proxy()
        .timeout(wait)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| "Cannot contact the account-free connection.")?;
    let address = format!("{}{}", helper.control.trim_end_matches('/'), path);
    let request = if let Some(body) = body {
        http.post(address).json(&body)
    } else {
        http.get(address)
    };
    let response = request
        .bearer_auth(&helper.token)
        .send()
        .map_err(|_| "The account-free connection did not respond.")?;
    if !response.status().is_success() {
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
    let mut child = command
        .spawn()
        .map_err(|_| "Cannot start the account-free connection.")?;
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
        Some(control) => Ok(Helper {
            child,
            control,
            token,
        }),
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

/// Makes sure the route is open. `Ok(false)` means this is not an iroh address.
pub fn ensure_route(address: &str) -> Result<bool, String> {
    let Some(id) = endpoint_id(address) else {
        return Ok(false);
    };
    if let Some(state) = routes().lock().map_err(|_| "The route is busy.")?.get(&id) {
        if live(&state.generation) {
            return Ok(true);
        }
    }
    let saved = known()
        .lock()
        .map_err(|_| "The route is busy.")?
        .get(&id)
        .cloned()
        .ok_or("This community needs its invitation again. Ask your friend for a new one.")?;
    open(&id, &saved)?;
    Ok(true)
}

fn open(id: &str, saved: &SavedIroh) -> Result<RouteState, String> {
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
        if let Some(expected) = &state.server_id {
            verify_route_identity(&state.http_url, expected)?;
        }
        routes()
            .lock()
            .map_err(|_| "The route is busy.")?
            .insert(id.to_owned(), state.clone());
        Ok(state)
    })
}

fn live(generation: &str) -> bool {
    slot().lock().ok().is_some_and(|mut s| {
        s.as_mut().is_some_and(|h| {
            h.token == generation && h.child.try_wait().is_ok_and(|status| status.is_none())
        })
    })
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
    Some(target.to_string())
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

/// Lets friends in by key. `peer_port` and `ingress_token` are the community's
/// restricted peer listener. Safe to repeat: the key and token are kept.
pub fn host(peer_port: u16, ingress_token: &str, server_id: &str) -> Result<SavedIroh, String> {
    uuid::Uuid::parse_str(server_id).map_err(|_| "The community identity is invalid.")?;
    let reply: HostReply = with_helper(true, |h| {
        // A helper which already hosts this community answers "busy"; ask it to stop first.
        let _: Result<serde_json::Value, String> = api(
            h,
            "/unhost",
            Some(serde_json::json!({})),
            Duration::from_secs(10),
        );
        api(
            h,
            "/host",
            Some(serde_json::json!({
                "peer_port": peer_port,
                "ingress_token": ingress_token,
                "udp_port": 4581,
            })),
            Duration::from_secs(25),
        )
    })?;
    #[cfg(test)]
    {
        *tests::DIRECT.lock().unwrap() = Some(reply.direct.clone());
    }
    let saved = SavedIroh {
        endpoint_id: reply.endpoint_id,
        relay: reply
            .relay_url
            .filter(|u| u.starts_with("https://") && u.len() <= 200),
        token: reply.token,
    };
    saved.to_invitation()?;
    register(&saved);
    *hosted_slot().lock().map_err(|_| "The host is busy.")? =
        Some((server_id.to_owned(), saved.clone()));
    Ok(saved)
}

/// Replaces the admission secret for this PC's hosted community. Already admitted
/// connections continue; new connections and reconnects need the fresh invitation.
/// The endpoint key, community data and encryption keys do not change.
pub fn rotate(server_id: &str) -> Result<(), String> {
    #[derive(Deserialize)]
    struct Reply {
        token: String,
    }
    let previous = hosted(server_id).ok_or("This PC is not sharing that server through iroh.")?;
    let reply: Reply = with_helper(false, |h| {
        api(
            h,
            "/rotate-token",
            Some(serde_json::json!({})),
            Duration::from_secs(10),
        )
    })?;
    let fresh = SavedIroh {
        token: reply.token,
        ..previous.clone()
    };
    fresh.to_invitation()?;
    let mut hosted = hosted_slot().lock().map_err(|_| "The host is busy.")?;
    if hosted.as_ref() != Some(&(server_id.to_owned(), previous)) {
        return Err("The hosted server changed. Open Invite friends again.".into());
    }
    *hosted = Some((server_id.to_owned(), fresh.clone()));
    drop(hosted);
    register(&fresh);
    Ok(())
}

/// The route this PC offers for a community, if it hosts it through iroh.
pub fn hosted(server_id: &str) -> Option<SavedIroh> {
    let hosted = hosted_slot().lock().ok()?.clone()?;
    if hosted.0 != server_id {
        return None;
    }
    let mut helper = slot().lock().ok()?;
    if helper.as_mut()?.child.try_wait().ok()?.is_some() {
        return None;
    }
    Some(hosted.1)
}

pub fn unhost(server_id: &str) -> Result<(), String> {
    let hosted = hosted_slot().lock().ok().and_then(|h| h.clone());
    if !hosted.is_some_and(|h| h.0 == server_id) {
        return Ok(());
    }
    let _ = with_helper(false, |h| {
        api::<serde_json::Value>(
            h,
            "/unhost",
            Some(serde_json::json!({})),
            Duration::from_secs(10),
        )
    });
    if let Ok(mut h) = hosted_slot().lock() {
        *h = None;
    }
    Ok(())
}

pub fn shutdown() {
    if let Ok(mut h) = hosted_slot().lock() {
        *h = None;
    }
    if let Ok(mut guard) = slot().lock() {
        if let Some(mut h) = guard.take() {
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

    /// Needs the helper: `RELAY_IROH_HELPER=<path to relay-iroh.exe, a debug build>`.
    #[test]
    #[ignore = "needs the relay-iroh helper (RELAY_IROH_HELPER)"]
    fn a_host_and_a_friend_use_the_route_through_the_apps_own_functions() {
        const SERVER_ID: &str = "4be44aef-8c98-4db6-a9d5-81c2eb311330";
        assert!(
            available(),
            "set RELAY_IROH_HELPER to a debug build of relay-iroh"
        );
        let (peer_port, seen) = community(SERVER_ID);
        let ingress = "an-ingress-capability-for-the-peer-listener-0123456789";
        let saved = host(peer_port, ingress, SERVER_ID).expect("the host starts");
        assert_eq!(hosted(SERVER_ID), Some(saved.clone()));
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

        unhost(SERVER_ID).unwrap();
        assert!(hosted(SERVER_ID).is_none());
        shutdown();
        assert!(!live("anything"));
        assert!(
            endpoint(&address)
                .unwrap()
                .starts_with("http://127.0.0.1:0/"),
            "after shutdown the address ends at a closed port"
        );
    }
}
