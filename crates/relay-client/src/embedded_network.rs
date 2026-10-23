//! App-owned userspace networking. Community identity stays independent of routing.
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Read},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

#[derive(Clone, Default, Deserialize)]
pub struct Status {
    pub phase: String,
    #[serde(default)]
    pub message: String,
    pub auth_url: Option<String>,
    pub address: Option<String>,
}
#[derive(Clone, Deserialize, Serialize)]
pub struct Turn {
    pub url: String,
    pub username: String,
    pub password: String,
}
#[derive(Clone, Deserialize)]
struct Route {
    http_url: String,
    turn: Turn,
    voice_udp_port: u16,
    #[serde(skip)]
    generation: String,
    #[serde(skip)]
    server_id: Option<String>,
}
struct Helper {
    child: Child,
    control: String,
    token: String,
    routes: HashMap<String, Route>,
}
static HELPER: OnceLock<Mutex<Option<Helper>>> = OnceLock::new();
// Keep closed local routes after a helper failure: existing requests must fail
// at localhost rather than silently trying an unprotected OS network route.
static ROUTES: OnceLock<Mutex<HashMap<String, Route>>> = OnceLock::new();
static HOSTED: OnceLock<Mutex<Option<(String, String)>>> = OnceLock::new();
static IROH_TURNS: OnceLock<Mutex<HashMap<String, (String, Turn)>>> = OnceLock::new();

fn iroh_turns() -> &'static Mutex<HashMap<String, (String, Turn)>> {
    IROH_TURNS.get_or_init(|| Mutex::new(HashMap::new()))
}
fn slot() -> &'static Mutex<Option<Helper>> {
    HELPER.get_or_init(|| Mutex::new(None))
}
fn routes() -> &'static Mutex<HashMap<String, Route>> {
    ROUTES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn private_host(address: &str) -> Option<String> {
    let url = url::Url::parse(address).ok()?;
    if !matches!(url.scheme(), "http" | "ws")
        || url.port_or_known_default()? != 4580
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }
    let host = url.host_str()?;
    crate::tailscale_connect::valid_address(host).then(|| host.to_owned())
}
pub(crate) fn loopback_url(address: &str) -> bool {
    url::Url::parse(address).ok().is_some_and(|u| {
        u.scheme() == "http"
            && u.host_str() == Some("127.0.0.1")
            && u.port().is_some()
            && u.username().is_empty()
            && u.password().is_none()
            && u.query().is_none()
            && u.fragment().is_none()
            && u.path() == "/"
    })
}
fn api<T: for<'de> Deserialize<'de>>(
    helper: &Helper,
    path: &str,
    body: Option<serde_json::Value>,
) -> Result<T, String> {
    let http = reqwest::blocking::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(8))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| "Cannot contact the private connection.")?;
    let address = format!("{}{}", helper.control.trim_end_matches('/'), path);
    let request = if let Some(body) = body {
        http.post(address).json(&body)
    } else {
        http.get(address)
    };
    let response = request
        .bearer_auth(&helper.token)
        .send()
        .map_err(|_| "The private connection did not respond.")?;
    if !response.status().is_success() {
        return Err("The private connection is not ready. Open its setup and try again.".into());
    }
    let mut bytes = Vec::new();
    response
        .take(65_537)
        .read_to_end(&mut bytes)
        .map_err(|_| "Cannot read connection status.")?;
    if bytes.len() > 65_536 {
        return Err("The connection status was too large.".into());
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| "The private connection returned an invalid result.".into())
}

#[cfg(windows)]
fn launch() -> Result<Helper, String> {
    use std::os::windows::process::CommandExt;
    let exe = std::env::current_exe().map_err(|_| "Cannot locate Relay.")?;
    let binary = exe
        .parent()
        .ok_or("Cannot locate Relay.")?
        .join("relay-network.exe");
    if !binary.is_file() {
        return Err(
            "Extract the complete Relay package to use its built-in private connection.".into(),
        );
    }
    let base = std::env::var_os("RELAY_PROFILE_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            directories::ProjectDirs::from("org", "Relay", "Relay")
                .map(|d| d.data_local_dir().to_owned())
        })
        .ok_or("Cannot locate private connection storage.")?;
    let state = base.join("private-network");
    let mut random = [0u8; 32];
    getrandom::getrandom(&mut random).map_err(|_| "Cannot create a private connection token.")?;
    let token = random
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let mut child = Command::new(binary)
        .arg("--state-dir")
        .arg(state)
        .env("RELAY_NETWORK_CONTROL_TOKEN", &token)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(0x08000000)
        .spawn()
        .map_err(|_| "Cannot start the private connection.")?;
    let output = child
        .stdout
        .take()
        .ok_or("Cannot read private connection startup.")?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let result = BufReader::new(output)
            .take(4097)
            .read_line(&mut line)
            .map(|_| line);
        let _ = tx.send(result);
    });
    let result = rx
        .recv_timeout(Duration::from_secs(10))
        .ok()
        .and_then(Result::ok)
        .filter(|s| s.len() <= 4096)
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v["control_url"].as_str().map(str::to_owned))
        .filter(|s| loopback_url(s));
    match result {
        Some(control) => Ok(Helper {
            child,
            control,
            token,
            routes: HashMap::new(),
        }),
        None => {
            let _ = child.kill();
            let _ = child.wait();
            Err("The private connection could not start.".into())
        }
    }
}
#[cfg(not(windows))]
fn launch() -> Result<Helper, String> {
    Err("Built-in private networking currently requires the Windows package.".into())
}

fn with_helper<T>(
    create: bool,
    action: impl FnOnce(&mut Helper) -> Result<T, String>,
) -> Result<T, String> {
    let mut guard = slot()
        .lock()
        .map_err(|_| "The private connection is busy.")?;
    if guard
        .as_mut()
        .is_some_and(|h| h.child.try_wait().ok().flatten().is_some())
    {
        if let Some(hosted) = HOSTED.get() {
            *hosted.lock().map_err(|_| "The private host is busy.")? = None;
        }
        *guard = None;
    }
    if guard.is_none() && create {
        *guard = Some(launch()?);
    }
    action(
        guard
            .as_mut()
            .ok_or("Open the built-in private connection and sign in first.")?,
    )
}
pub fn status() -> Result<Status, String> {
    with_helper(true, |h| {
        let s: Status = api(h, "/status", None)?;
        if s.phase == "ready"
            && !s
                .address
                .as_deref()
                .is_some_and(crate::tailscale_connect::valid_address)
        {
            return Err("The private connection has no usable address.".into());
        }
        if s.auth_url.as_deref().is_some_and(|v| !valid_auth_url(v)) {
            return Err("The sign-in link could not be verified.".into());
        }
        Ok(s)
    })
}
pub fn connect() -> Result<Status, String> {
    with_helper(true, |h| {
        let _: serde_json::Value = api(h, "/connect", Some(serde_json::json!({})))?;
        api(h, "/status", None)
    })
}
/// How the private network currently reaches a community's host, and how fast.
/// The helper never reports the other PC's address, only the kind of path.
pub fn path(address: &str) -> Result<crate::connection_path::PathReport, String> {
    let host = private_host(address).ok_or("This is not a private network address.")?;
    if !routed(address) {
        return Err("The private route is not open.".into());
    }
    with_helper(false, |h| {
        let reply: crate::connection_path::HelperPath =
            api(h, "/path", Some(serde_json::json!({ "host": host })))?;
        Ok(reply.into_report())
    })
}
pub fn valid_auth_url(value: &str) -> bool {
    url::Url::parse(value).ok().is_some_and(|u| {
        u.scheme() == "https"
            && u.host_str() == Some("login.tailscale.com")
            && u.username().is_empty()
            && u.password().is_none()
            && matches!(u.port(), None | Some(443))
    })
}
pub fn ensure_route(address: &str) -> Result<bool, String> {
    if crate::iroh_route::is_iroh(address) {
        return crate::iroh_route::ensure_route(address);
    }
    let Some(host) = private_host(address) else {
        return Ok(false);
    };
    let present = slot()
        .lock()
        .map_err(|_| "The private connection is busy.")?
        .is_some();
    if !present && routed(address) {
        return Err(
            "Reconnect Relay's built-in private network before using this community.".into(),
        );
    }
    if !present {
        return Ok(false);
    };
    with_helper(false, |h| {
        let s: Status = api(h, "/status", None)?;
        if s.phase == "idle"
            && !routes()
                .lock()
                .map_err(|_| "The private route is busy.")?
                .contains_key(&host)
        {
            return Ok(false);
        }
        if s.phase != "ready" {
            return Err(
                "Sign in through Relay's built-in private connection before joining this server."
                    .into(),
            );
        }
        if !h.routes.contains_key(&host) {
            if h.routes.len() >= 32 {
                return Err(
                    "Too many private communities are open. Close Relay and try again.".into(),
                );
            }
            let mut route: Route = api(
                h,
                "/route",
                Some(serde_json::json!({"host":host,"port":4580})),
            )?;
            if !loopback_url(&route.http_url)
                || !valid_turn(&route.turn)
                || route.voice_udp_port == 0
            {
                return Err("The private route could not be verified.".into());
            }
            route.generation = h.token.clone();
            inherit_identity(&host, &mut route)?;
            routes()
                .lock()
                .map_err(|_| "The private route is busy.")?
                .insert(host.clone(), route.clone());
            h.routes.insert(host, route);
        }
        Ok(true)
    })
}
fn valid_turn(turn: &Turn) -> bool {
    turn.url
        .strip_prefix("turn:127.0.0.1:")
        .and_then(|s| s.strip_suffix("?transport=tcp"))
        .and_then(|s| s.parse::<u16>().ok())
        .is_some_and(|p| p != 0)
        && !turn.username.is_empty()
        && !turn.password.is_empty()
        && turn.username.len() <= 128
        && turn.password.len() <= 256
}
// A new SDK session may use another account or a remapped node address. Confirm
// the known community through the new route before publishing it to workers
// that already hold account credentials.
fn inherit_identity(host: &str, route: &mut Route) -> Result<(), String> {
    let previous = routes()
        .lock()
        .map_err(|_| "The private route is busy.")?
        .get(host)
        .cloned();
    if let Some(previous) = previous {
        if let Some(expected) = previous.server_id {
            if route.generation != previous.generation || route.http_url != previous.http_url {
                verify_route_identity(&route.http_url, &expected)?;
            }
            route.server_id = Some(expected);
        }
    }
    Ok(())
}
pub(crate) fn verify_route_identity(http_url: &str, expected: &str) -> Result<(), String> {
    let http = reqwest::blocking::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| "Cannot verify the private community.")?;
    let response = http
        .get(format!("{}/api/health", http_url.trim_end_matches('/')))
        .send()
        .map_err(|_| "Cannot verify the private community. Reconnect and try again.")?;
    if !response.status().is_success() {
        return Err("The private route did not return a Relay community.".into());
    }
    let mut bytes = Vec::new();
    response
        .take(16_385)
        .read_to_end(&mut bytes)
        .map_err(|_| "Cannot read the private community identity.")?;
    if bytes.len() > 16_384 {
        return Err("The private community identity response was too large.".into());
    }
    let summary: crate::connection_setup::ServerSummary = serde_json::from_slice(&bytes)
        .map_err(|_| "The private route did not return a Relay community.")?;
    summary.validate(Some(expected))?;
    Ok(())
}
pub fn bind_identity(address: &str, server_id: &str) -> Result<(), String> {
    if crate::iroh_route::is_iroh(address) {
        return crate::iroh_route::bind_identity(address, server_id);
    }
    let Some(host) = private_host(address) else {
        return Ok(());
    };
    if !routed(address) {
        return Ok(());
    }
    uuid::Uuid::parse_str(server_id).map_err(|_| "The private community identity is invalid.")?;
    with_helper(false, |h| {
        let route = h
            .routes
            .get_mut(&host)
            .ok_or("Reconnect the private community first.")?;
        if route.server_id.as_deref().is_some_and(|id| id != server_id) {
            return Err(
                "The private community identity changed. Ask the host for a new invitation.".into(),
            );
        }
        if route.server_id.is_none() {
            verify_route_identity(&route.http_url, server_id)?;
        }
        route.server_id = Some(server_id.to_owned());
        routes()
            .lock()
            .map_err(|_| "The private route is busy.")?
            .insert(host, route.clone());
        Ok(())
    })
}
pub fn routed(address: &str) -> bool {
    if crate::iroh_route::is_iroh(address) {
        return crate::iroh_route::routed(address);
    }
    let Some(host) = private_host(address) else {
        return false;
    };
    match routes().lock() {
        Ok(known) => known.contains_key(&host),
        // A failed route registry cannot make a previously private call choose
        // the ordinary OS voice route. Its endpoint remains blocked too.
        Err(_) => true,
    }
}
pub fn endpoint(address: &str) -> String {
    if let Some(mapped) = crate::iroh_route::endpoint(address) {
        return mapped;
    }
    let Some(host) = private_host(address) else {
        return address.to_owned();
    };
    let route = match routes().lock() {
        Ok(r) => r.get(&host).cloned(),
        Err(_) => return blocked_endpoint(address),
    };
    let Some(route) = route else {
        return address.to_owned();
    };
    let live = slot().lock().ok().is_some_and(|mut s| {
        s.as_mut().is_some_and(|h| {
            h.token == route.generation && h.child.try_wait().is_ok_and(|e| e.is_none())
        })
    });
    if !live {
        return blocked_endpoint(address);
    }
    let Ok(original) = url::Url::parse(address) else {
        return address.to_owned();
    };
    let Ok(mut target) = url::Url::parse(&route.http_url) else {
        return address.to_owned();
    };
    if original.scheme() == "ws" {
        let _ = target.set_scheme("ws");
    }
    target.set_path(original.path());
    target.set_query(original.query());
    target.to_string()
}
pub(crate) fn blocked_endpoint(address: &str) -> String {
    let Ok(original) = url::Url::parse(address) else {
        return "http://127.0.0.1:0/".into();
    };
    let mut target = url::Url::parse("http://127.0.0.1:0/").expect("fixed loopback URL");
    if original.scheme() == "ws" {
        let _ = target.set_scheme("ws");
    }
    target.set_path(original.path());
    target.set_query(original.query());
    target.to_string()
}
pub fn rtc_turn(address: &str) -> Option<Turn> {
    if let Some((generation, turn)) = iroh_turns().lock().ok()?.get(address).cloned() {
        let mut guard = slot().lock().ok()?;
        let helper = guard.as_mut()?;
        return (helper.token == generation
            && helper.child.try_wait().is_ok_and(|exit| exit.is_none()))
        .then_some(turn);
    }
    let host = private_host(address)?;
    routes().lock().ok()?.get(&host).map(|r| r.turn.clone())
}

pub fn rtc_requires_turn(address: &str) -> bool {
    crate::iroh_route::is_iroh(address)
        || private_host(address).is_some()
        || iroh_turns()
            .lock()
            .is_ok_and(|routes| routes.contains_key(address))
}

/// Use the same verified community route for signaling and its fixed SFU for media.
pub fn prepare_iroh_rtc(
    ticket: &mut relay_protocol::RtcTicket,
    origin: &str,
    server_id: &str,
    local: bool,
) -> Result<(), String> {
    let actual = url::Url::parse(&ticket.url).map_err(|_| "Invalid video address")?;
    if actual.scheme() != "ws"
        || actual.host_str() != Some(crate::iroh_route::ONLY_ADDRESS)
        || actual.port_or_known_default() != Some(4580)
        || actual.path() != "/"
        || actual.query().is_some()
        || actual.fragment().is_some()
        || !actual.username().is_empty()
        || actual.password().is_some()
    {
        return Err("The video address does not match this account-free community.".into());
    }
    let (gateway, port) = if local {
        if !crate::local_host::is_local_url(origin)
            || crate::local_host::embedded_address(server_id).as_deref()
                != Some(format!("http://{}:4580", crate::iroh_route::ONLY_ADDRESS).as_str())
        {
            return Err("Cannot confirm your local account-free video server.".into());
        }
        (String::new(), 7882)
    } else {
        if !crate::iroh_route::ensure_route(origin)? {
            return Err("Open the account-free connection first.".into());
        }
        crate::iroh_route::bind_identity(origin, server_id)?;
        let gateway = crate::iroh_route::endpoint(origin)
            .filter(|url| loopback_url(url))
            .ok_or("The account-free video gateway is not ready.")?;
        #[derive(Deserialize)]
        struct MediaInfo {
            version: u32,
            media_udp_port: u16,
        }
        let response = reqwest::blocking::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(5))
            .build()
            .map_err(|_| "Cannot check account-free video.")?
            .get(format!("{}/_relay/media", gateway.trim_end_matches('/')))
            .send()
            .map_err(|_| "Cannot reach your friend's video server.")?;
        if !response.status().is_success() {
            return Err(
                "Update the host's Relay to use video and screen sharing without an extra account."
                    .into(),
            );
        }
        let mut bytes = Vec::new();
        response
            .take(1025)
            .read_to_end(&mut bytes)
            .map_err(|_| "Cannot read video support.")?;
        if bytes.len() > 1024 {
            return Err("Invalid video support response.".into());
        }
        let info: MediaInfo = serde_json::from_slice(&bytes)
            .map_err(|_| "Update the host's Relay for account-free video.")?;
        if info.version != 1 || info.media_udp_port == 0 {
            return Err("Update the host's Relay for account-free video.".into());
        }
        (gateway, info.media_udp_port)
    };
    let mut signaling = url::Url::parse(origin).map_err(|_| "Invalid community address")?;
    signaling
        .set_scheme("ws")
        .map_err(|_| "Invalid video route")?;
    signaling.set_path("/");
    signaling.set_query(None);
    signaling.set_fragment(None);
    let signaling = signaling.to_string();
    with_helper(true, |helper| {
        let turn: Turn = api(
            helper,
            "/iroh-media",
            Some(serde_json::json!({
                "gateway":gateway,"host":crate::iroh_route::ONLY_ADDRESS,"port":port
            })),
        )?;
        if !valid_turn(&turn) {
            return Err("The account-free video connection could not be verified.".into());
        }
        iroh_turns()
            .lock()
            .map_err(|_| "Video connection is busy.")?
            .insert(signaling.clone(), (helper.token.clone(), turn));
        Ok(())
    })?;
    ticket.url = signaling;
    Ok(())
}
pub fn new_voice_route(address: &str) -> Result<(String, u16), String> {
    if crate::iroh_route::is_iroh(address) {
        return crate::iroh_route::new_voice_route(address);
    }
    let host = private_host(address).ok_or("The private voice address is invalid.")?;
    with_helper(false, |h| {
        let mut route: Route = api(
            h,
            "/route",
            Some(serde_json::json!({"host":host,"port":4580})),
        )?;
        if !loopback_url(&route.http_url) || !valid_turn(&route.turn) || route.voice_udp_port == 0 {
            return Err("The private voice route could not be verified.".into());
        }
        route.generation = h.token.clone();
        inherit_identity(&host, &mut route)?;
        let result = (route.http_url.clone(), route.voice_udp_port);
        routes()
            .lock()
            .map_err(|_| "The private route is busy.")?
            .insert(host.clone(), route.clone());
        h.routes.insert(host, route);
        Ok(result)
    })
}
pub fn host(peer_port: u16, ingress_token: &str, server_id: &str) -> Result<Status, String> {
    with_helper(false, |h| {
        let reply: serde_json::Value = api(
            h,
            "/host",
            Some(
                serde_json::json!({"peer_port":peer_port,"ingress_token":ingress_token,"server_id":server_id,"media_udp_port":7882,"voice_udp_port":4581}),
            ),
        )?;
        let status: Status = api(h, "/status", None)?;
        let address = status
            .address
            .as_deref()
            .filter(|s| crate::tailscale_connect::valid_address(s))
            .ok_or("The private host address is unavailable.")?;
        let url = format!("http://{address}:4580");
        if reply["server_id"].as_str() != Some(server_id)
            || reply["http_url"].as_str() != Some(url.as_str())
            || status.phase != "ready"
        {
            return Err("The private host identity could not be confirmed.".into());
        }
        *HOSTED
            .get_or_init(|| Mutex::new(None))
            .lock()
            .map_err(|_| "The private host is busy.")? = Some((server_id.into(), url));
        Ok(status)
    })
}
pub fn shared_address(server_id: &str) -> Option<String> {
    let value = HOSTED.get()?.lock().ok()?.clone()?;
    if value.0 != server_id {
        return None;
    }
    let mut helper = slot().lock().ok()?;
    if helper.as_mut()?.child.try_wait().ok()?.is_some() {
        return None;
    }
    Some(value.1)
}
pub fn unhost(server_id: &str) -> Result<(), String> {
    let hosted = HOSTED
        .get()
        .and_then(|h| h.lock().ok().and_then(|h| h.clone()));
    if !hosted.is_some_and(|h| h.0 == server_id) {
        return Ok(());
    }
    with_helper(false, |h| {
        let _: serde_json::Value = api(
            h,
            "/unhost",
            Some(serde_json::json!({"server_id":server_id})),
        )?;
        Ok(())
    })?;
    if let Some(h) = HOSTED.get() {
        *h.lock().map_err(|_| "The private host is busy.")? = None;
    }
    Ok(())
}
pub fn require_rtc_route(ticket_url: &str, origin: &str, server_id: &str) -> Result<(), String> {
    if crate::iroh_route::is_iroh(origin) {
        return Err("Prepare the account-free video route before connecting.".into());
    }
    let expected =
        url::Url::parse(origin).map_err(|_| "The private community address is invalid.")?;
    let actual =
        url::Url::parse(ticket_url).map_err(|_| "The private video address is invalid.")?;
    if actual.scheme() != "ws"
        || actual.host_str() != expected.host_str()
        || actual.port_or_known_default() != Some(4580)
        || actual.path() != "/"
        || actual.query().is_some()
        || actual.fragment().is_some()
        || !actual.username().is_empty()
        || actual.password().is_some()
    {
        return Err("The video address does not match this private community. Ask the host to check its connection.".into());
    }
    if !ensure_route(origin)? {
        return Err("Connect Relay's built-in private network before starting video.".into());
    }
    bind_identity(origin, server_id)?;
    Ok(())
}
pub fn shutdown() {
    if let Ok(mut turns) = iroh_turns().lock() {
        turns.clear();
    }
    crate::iroh_route::shutdown();
    if let Some(hosted) = HOSTED.get() {
        if let Ok(mut h) = hosted.lock() {
            *h = None;
        }
    }
    if let Ok(mut guard) = slot().lock() {
        if let Some(mut h) = guard.take() {
            let _ = api::<serde_json::Value>(&h, "/shutdown", Some(serde_json::json!({})));
            let until = Instant::now() + Duration::from_secs(3);
            while Instant::now() < until {
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
    fn replacement_gateway_checks_known_community_without_credentials() {
        use std::io::{Read, Write};
        let expected = "60bf83c0-570c-4485-b979-c34175228954";
        for (reported, accepted) in [
            (expected, true),
            ("5e45ecb4-f995-435e-9066-0f0c594c267c", false),
        ] {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let address = format!("http://{}", listener.local_addr().unwrap());
            let body = serde_json::json!({
                "ok":true,"server_name":"Owned identity fixture","server_id":reported,
                "protocol_version":relay_protocol::API_PROTOCOL_VERSION,"rtc_enabled":true
            })
            .to_string();
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = [0u8; 4096];
                let n = stream.read(&mut request).unwrap();
                let request = String::from_utf8_lossy(&request[..n]).to_lowercase();
                assert!(request.starts_with("get /api/health "));
                assert!(!request.contains("authorization:"));
                write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body).unwrap();
            });
            assert_eq!(verify_route_identity(&address, expected).is_ok(), accepted);
            server.join().unwrap();
        }
    }
    #[test]
    fn routing_targets_require_exact_private_service() {
        assert_eq!(
            private_host("http://100.70.1.2:4580/api/bootstrap").as_deref(),
            Some("100.70.1.2")
        );
        for u in [
            "http://127.0.0.1:4580",
            "http://100.70.1.2:80",
            "http://user@100.70.1.2:4580",
            "https://100.70.1.2:4580",
            "http://192.168.1.2:4580",
        ] {
            assert!(private_host(u).is_none());
        }
    }
    #[test]
    fn native_turn_is_authenticated_tcp_on_loopback_only() {
        let mut turn = Turn {
            url: "turn:127.0.0.1:31234?transport=tcp".into(),
            username: "owned-fixture".into(),
            password: "owned-fixture-credential".into(),
        };
        assert!(valid_turn(&turn));
        for url in [
            "turn:192.0.2.1:31234?transport=tcp",
            "turn:127.0.0.1:31234?transport=udp",
            "turn:127.0.0.1:0?transport=tcp",
            "turn:127.0.0.1:31234?transport=tcp&other=1",
        ] {
            turn.url = url.into();
            assert!(!valid_turn(&turn));
        }
        turn.url = "turn:127.0.0.1:31234?transport=tcp".into();
        turn.password.clear();
        assert!(!valid_turn(&turn));
    }
    #[test]
    fn control_and_login_urls_cannot_leave_their_trust_boundaries() {
        assert!(loopback_url("http://127.0.0.1:12345/"));
        assert!(!loopback_url("http://127.0.0.1:12345/api/local"));
        assert!(!loopback_url("http://localhost:12345/"));
        assert!(!loopback_url("http://100.70.1.2:12345/"));
        assert!(valid_auth_url("https://login.tailscale.com/a/example"));
        assert!(!valid_auth_url("http://login.tailscale.com/a/example"));
        assert!(!valid_auth_url(
            "https://login.tailscale.com.evil/a/example"
        ));
    }
    #[test]
    fn dead_private_route_has_no_os_or_reused_port_fallback() {
        assert_eq!(
            blocked_endpoint("http://100.70.1.2:4580/api/login"),
            "http://127.0.0.1:0/api/login"
        );
        assert_eq!(
            blocked_endpoint("ws://100.70.1.2:4580/api/events"),
            "ws://127.0.0.1:0/api/events"
        );
    }
    #[test]
    fn private_rtc_ticket_cannot_change_verified_peer_or_transport() {
        for ticket in [
            "ws://100.70.9.9:4580",
            "ws://100.70.1.2:7880",
            "wss://100.70.1.2:4580",
            "ws://100.70.1.2:4580/rtc",
            "ws://user@100.70.1.2:4580",
        ] {
            assert!(require_rtc_route(
                ticket,
                "http://100.70.1.2:4580",
                "60bf83c0-570c-4485-b979-c34175228954"
            )
            .is_err());
        }
    }
}
