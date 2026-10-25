//! "Keep a copy on this PC": the desktop side of a community replica.
//!
//! The member chooses this explicitly, after accepting the Replica role on a
//! community that offers copies. The app then runs the installed companion as
//! `relay-server --replica-pull DIR`, hidden, one bounded pull at a time:
//! one JSON object on its standard input, which is then closed, and one JSON
//! line back. The pull token lives only in the OS vault and on that pipe. The
//! app resolves the community's pinned route freshly for every pull and keeps
//! it alive while the child runs; it never passes discovery addresses and never
//! stores a helper port.
//!
//! Nothing here claims that this PC could take over the community. A report
//! says only what the pull itself confirmed.
use crate::network::CommandOrigin;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use zeroize::Zeroizing;

/// The community capability that gates the whole flow. Friends communities do
/// not advertise it.
pub const CAPABILITY: &str = "replica-pull-v1";

/// Time from the end of one pull to the start of the next.
pub const INTERVAL: Duration = Duration::from_secs(30);
/// Transient failures wait longer each time, up to this.
pub const BACKOFF_CAP: Duration = Duration::from_secs(300);
const INPUT_LIMIT: usize = 8192;
const OUTPUT_LIMIT: u64 = 4096;
/// The companion's own bounded pass is 150 s; this is its hard limit.
const PULL_DEADLINE: Duration = Duration::from_secs(180);
const POLL: Duration = Duration::from_millis(100);
const ROUTE_CHECK: Duration = Duration::from_secs(1);
const MAX_COPIES: usize = 8;

// ---------------------------------------------------------------------------
// The persisted choice

/// What preferences remember about a copy: that the member chose it, and which
/// copy it is. The pull token is never part of this.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Choice {
    #[serde(default)]
    pub source_url: String,
    #[serde(default)]
    pub server_id: String,
    #[serde(default)]
    pub user_id: String,
    #[serde(default)]
    pub replica_id: String,
}

impl Choice {
    /// A new copy of the community and account that are connected now.
    pub fn new(origin: &CommandOrigin) -> Result<Self, String> {
        let choice = Self {
            source_url: crate::community::normalize_url(&origin.url)?,
            server_id: origin.server_id.clone(),
            user_id: origin.user_id.clone(),
            replica_id: uuid::Uuid::new_v4().to_string(),
        };
        if choice.valid() {
            Ok(choice)
        } else {
            Err(
                "This community's identity is not confirmed yet, so a copy cannot be started."
                    .into(),
            )
        }
    }

    pub fn valid(&self) -> bool {
        crate::community::normalize_url(&self.source_url).is_ok_and(|url| url == self.source_url)
            && uuid::Uuid::parse_str(&self.server_id).is_ok()
            && uuid::Uuid::parse_str(&self.replica_id).is_ok()
            && !self.user_id.is_empty()
            && self.user_id.len() <= 128
    }

    /// The same community address, community and account. A copy never follows
    /// the app to another one.
    pub fn belongs_to(&self, origin: &CommandOrigin) -> bool {
        crate::community::normalize_url(&origin.url).is_ok_and(|url| url == self.source_url)
            && origin.server_id == self.server_id
            && origin.user_id == self.user_id
    }

    pub fn key(&self) -> crate::credential_vault::ReplicaKey<'_> {
        crate::credential_vault::ReplicaKey {
            source_url: &self.source_url,
            server_id: &self.server_id,
            user_id: &self.user_id,
            replica_id: &self.replica_id,
        }
    }

    fn digest(&self) -> String {
        let mut hash = Sha256::new();
        for part in [
            self.source_url.as_str(),
            self.server_id.as_str(),
            self.user_id.as_str(),
            self.replica_id.as_str(),
        ] {
            hash.update((part.len() as u64).to_le_bytes());
            hash.update(part.as_bytes());
        }
        format!("{:x}", hash.finalize())[..32].to_owned()
    }
}

/// Damaged or foreign entries are dropped, duplicates keep the first, and the
/// list stays small. Called once when preferences are loaded.
pub fn sanitize(choices: &mut Vec<Choice>) {
    let mut seen: Vec<(String, String, String)> = Vec::new();
    choices.retain(|choice| {
        let identity = (
            choice.source_url.clone(),
            choice.server_id.clone(),
            choice.user_id.clone(),
        );
        if !choice.valid() || seen.contains(&identity) {
            return false;
        }
        seen.push(identity);
        true
    });
    choices.truncate(MAX_COPIES);
}

pub fn find<'a>(choices: &'a [Choice], origin: &CommandOrigin) -> Option<&'a Choice> {
    choices.iter().find(|choice| choice.belongs_to(origin))
}

/// Keeps one choice per community and account. A full list refuses a new one
/// instead of silently forgetting an older copy.
pub fn remember(choices: &mut Vec<Choice>, choice: Choice) -> Result<(), String> {
    if !choice.valid() {
        return Err("The copy's identity is invalid.".into());
    }
    let same = |other: &Choice| {
        other.source_url == choice.source_url
            && other.server_id == choice.server_id
            && other.user_id == choice.user_id
    };
    if let Some(slot) = choices.iter_mut().find(|other| same(other)) {
        *slot = choice;
        return Ok(());
    }
    if choices.len() >= MAX_COPIES {
        return Err("This PC already keeps the most copies Relay supports. Stop one first.".into());
    }
    choices.push(choice);
    Ok(())
}

pub fn forget(choices: &mut Vec<Choice>, origin: &CommandOrigin) {
    choices.retain(|choice| !choice.belongs_to(origin));
}

// ---------------------------------------------------------------------------
// Storage

fn storage_root() -> Result<PathBuf, String> {
    if let Some(profile) = std::env::var_os("RELAY_PROFILE_DIR") {
        return Ok(PathBuf::from(profile).join("replica-copies"));
    }
    #[cfg(windows)]
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    #[cfg(not(windows))]
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")));
    base.map(|path| path.join("Relay").join("replica-copies"))
        .ok_or_else(|| "Cannot find your per-user storage folder.".to_string())
}

/// Where this copy lives: next to the profile, never in the folder of a
/// community this PC hosts. The companion keeps only `copy.sqlite3` and its own
/// lock there.
pub fn copy_directory(choice: &Choice) -> Result<PathBuf, String> {
    if !choice.valid() {
        return Err("The copy's identity is invalid.".into());
    }
    Ok(storage_root()?.join(choice.digest()))
}

// ---------------------------------------------------------------------------
// What can go wrong

/// Why a pull did not give a copy. Text is the app's own: nothing a server or
/// the companion said is shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    /// The community refused the copy's token: the role ended or the token did.
    AccessDenied,
    /// The copy belongs to another community or account.
    Binding,
    /// The copy's folder could not be used.
    Storage,
    Network,
    Busy,
    Integrity,
    /// No safe route to the community right now.
    Route,
    /// The address cannot be reached over a connection that is safe for a copy.
    Unsupported,
    /// This PC holds no access for the copy.
    NoAccess,
    /// The Windows credential vault could not be used.
    Vault,
    /// The companion is not installed beside Relay.
    Missing,
    /// The companion answered in a way the contract does not allow.
    Protocol,
}

impl Fault {
    /// A permanent fault stops the schedule until the member presses Retry.
    pub fn permanent(self) -> bool {
        matches!(
            self,
            Fault::AccessDenied
                | Fault::Binding
                | Fault::Storage
                | Fault::Unsupported
                | Fault::NoAccess
                | Fault::Vault
                | Fault::Missing
        )
    }

    fn from_code(code: &str) -> Option<Self> {
        Some(match code {
            "access_denied" => Fault::AccessDenied,
            "binding" => Fault::Binding,
            "storage" => Fault::Storage,
            "network" => Fault::Network,
            "busy" => Fault::Busy,
            "integrity" => Fault::Integrity,
            _ => return None,
        })
    }

    fn exit_code(self) -> Option<i32> {
        match self {
            Fault::AccessDenied => Some(3),
            Fault::Binding | Fault::Storage => Some(4),
            Fault::Network | Fault::Busy | Fault::Integrity => Some(5),
            _ => None,
        }
    }

    pub fn explain(self) -> &'static str {
        match self {
            Fault::AccessDenied => "The community no longer lets this PC copy it. Your role may have ended or the access expired.",
            Fault::Binding => "The copy on this PC belongs to a different community or account, so Relay did not use it.",
            Fault::Storage => "Relay could not use the folder that holds the copy.",
            Fault::Network => "The community could not be reached.",
            Fault::Busy => "The community was busy.",
            Fault::Integrity => "What arrived did not pass its checks, so it was not kept.",
            Fault::Route => "Relay could not open a safe connection to the community.",
            Fault::Unsupported => "A copy needs an HTTPS address, Relay's private connection or this PC. This address is none of those.",
            Fault::NoAccess => "This PC no longer holds access for the copy. Start it again to allow it.",
            Fault::Vault => "Windows could not read the saved access for this copy.",
            Fault::Missing => "The Relay copy component was not found beside Relay. Reinstall Relay.",
            Fault::Protocol => "The copy component answered in an unexpected way.",
        }
    }
}

enum Stop {
    Cancelled,
    Fault(Fault),
}

impl From<Fault> for Stop {
    fn from(fault: Fault) -> Self {
        Stop::Fault(fault)
    }
}

/// Seconds to wait after the nth transient failure in a row: 30, 60, 120, 240,
/// then five minutes.
pub fn backoff(failures: u32) -> Duration {
    let doublings = failures.saturating_sub(1).min(4);
    (INTERVAL * 2u32.pow(doublings)).min(BACKOFF_CAP)
}

// ---------------------------------------------------------------------------
// The companion's contract

/// What one pull confirmed. `head` is how far this PC's copy has applied the
/// community's change log; it is not a statement about readiness to host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Progress {
    pub complete: bool,
    pub lineage: String,
    pub head: u64,
    pub source_head: u64,
    pub files_pending: u64,
    pub bytes_received: u64,
    pub rows_applied: u64,
}

#[derive(Deserialize)]
struct Report {
    schema: u64,
    state: String,
    #[serde(default)]
    code: String,
    #[serde(default)]
    server_id: String,
    #[serde(default)]
    recipient_user_id: String,
    #[serde(default)]
    replica_id: String,
    #[serde(default)]
    lineage: String,
    #[serde(default)]
    head: u64,
    #[serde(default)]
    source_head: u64,
    #[serde(default)]
    files_pending: u64,
    #[serde(default)]
    bytes_received: u64,
    #[serde(default)]
    rows_applied: u64,
    #[serde(default)]
    source_confirmed: bool,
}

#[derive(Serialize)]
struct Input<'a> {
    schema: u8,
    source_url: &'a str,
    transport_url: &'a str,
    server_id: &'a str,
    recipient_user_id: &'a str,
    replica_id: &'a str,
    token: &'a str,
}

/// The one JSON object written to the companion's standard input.
fn input_for(choice: &Choice, transport: &str, token: &str) -> Result<Zeroizing<Vec<u8>>, Fault> {
    let mut bytes = Zeroizing::new(Vec::new());
    serde_json::to_writer(
        &mut *bytes,
        &Input {
            schema: 1,
            source_url: &choice.source_url,
            transport_url: transport,
            server_id: &choice.server_id,
            recipient_user_id: &choice.user_id,
            replica_id: &choice.replica_id,
            token,
        },
    )
    .map_err(|_| Fault::Protocol)?;
    if bytes.len() > INPUT_LIMIT {
        return Err(Fault::Protocol);
    }
    Ok(bytes)
}

/// Reads what the companion printed and how it exited. A success is shown only
/// if it names exactly this community, account and copy.
fn read_report(exit: Option<i32>, stdout: &[u8], choice: &Choice) -> Result<Progress, Fault> {
    if stdout.len() as u64 > OUTPUT_LIMIT {
        return Err(Fault::Protocol);
    }
    let text = std::str::from_utf8(stdout).map_err(|_| Fault::Protocol)?;
    let line = text.trim_end_matches(['\r', '\n']);
    if line.is_empty() || line.contains(['\r', '\n']) {
        return Err(Fault::Protocol);
    }
    let report: Report = serde_json::from_str(line).map_err(|_| Fault::Protocol)?;
    if report.schema != 1 {
        return Err(Fault::Protocol);
    }
    match exit {
        Some(0) => {
            if report.server_id != choice.server_id
                || report.recipient_user_id != choice.user_id
                || report.replica_id != choice.replica_id
            {
                return Err(Fault::Binding);
            }
            let lineage_ok = !report.lineage.is_empty()
                && report.lineage.len() <= 128
                && report.lineage.bytes().all(|b| b.is_ascii_graphic());
            let state_ok = matches!(report.state.as_str(), "current" | "files_pending");
            if !lineage_ok || !state_ok {
                return Err(Fault::Protocol);
            }
            if !report.source_confirmed || report.head > report.source_head {
                return Err(Fault::Integrity);
            }
            Ok(Progress {
                complete: report.state == "current"
                    && report.files_pending == 0
                    && report.head == report.source_head,
                lineage: report.lineage,
                head: report.head,
                source_head: report.source_head,
                files_pending: report.files_pending,
                bytes_received: report.bytes_received,
                rows_applied: report.rows_applied,
            })
        }
        Some(3..=5) => {
            let fault = (report.state == "error")
                .then(|| Fault::from_code(&report.code))
                .flatten()
                .ok_or(Fault::Protocol)?;
            if fault.exit_code() == exit {
                Err(fault)
            } else {
                Err(Fault::Protocol)
            }
        }
        _ => Err(Fault::Protocol),
    }
}

/// The origin the companion may send the token to. It is the community's own
/// HTTPS origin, the numeric loopback origin of a community on this PC, or the
/// numeric loopback endpoint of the helper that carries the pinned private
/// route. Anything else is refused before a token is read.
pub fn transport_origin(source: &str, resolved: &str, routed: bool) -> Result<String, Fault> {
    let source = url::Url::parse(source).map_err(|_| Fault::Route)?;
    let resolved = url::Url::parse(resolved).map_err(|_| Fault::Route)?;
    if !matches!(resolved.scheme(), "http" | "https")
        || !resolved.username().is_empty()
        || resolved.password().is_some()
        || resolved.query().is_some()
        || resolved.fragment().is_some()
        || !matches!(resolved.path(), "" | "/")
    {
        return Err(Fault::Route);
    }
    let loopback = |url: &url::Url| match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        _ => false,
    };
    let origin = resolved.origin().ascii_serialization();
    // A community on this PC reached by the name `localhost` is sent to the
    // numeric loopback address of the same port.
    let local_name = source.scheme() == "http"
        && source.host_str() == Some("localhost")
        && resolved.scheme() == "http"
        && matches!(resolved.host(), Some(url::Host::Ipv4(ip)) if ip.is_loopback())
        && source.port_or_known_default() == resolved.port_or_known_default();
    if local_name {
        return Ok(origin);
    }
    if source.origin() == resolved.origin() {
        return if resolved.scheme() == "https" || loopback(&resolved) {
            Ok(origin)
        } else {
            Err(Fault::Unsupported)
        };
    }
    let helper = routed
        && resolved.scheme() == "http"
        && matches!(resolved.host(), Some(url::Host::Ipv4(ip)) if ip.is_loopback())
        && resolved.port().is_some_and(|port| port != 0);
    if helper {
        Ok(origin)
    } else {
        Err(Fault::Route)
    }
}

/// `localhost` is replaced by the numeric loopback address; nothing else changes.
fn numeric_loopback(address: &str) -> String {
    match url::Url::parse(address) {
        Ok(mut url) if url.scheme() == "http" && url.host_str() == Some("localhost") => {
            let _ = url.set_host(Some("127.0.0.1"));
            url.to_string()
        }
        _ => address.to_owned(),
    }
}

/// An address a copy could ever use, judged without opening any route.
pub fn address_supported(source: &str) -> bool {
    let Ok(url) = url::Url::parse(source) else {
        return false;
    };
    url.scheme() == "https"
        || (url.scheme() == "http" && url.host_str() == Some("localhost"))
        || crate::iroh_route::is_iroh(source)
        || crate::tailscale_connect::requires_private_connection(source)
        || matches!(url.host(), Some(url::Host::Ipv4(ip)) if ip.is_loopback())
        || matches!(url.host(), Some(url::Host::Ipv6(ip)) if ip.is_loopback())
}

// ---------------------------------------------------------------------------
// One pull

/// Where the running child lives. The pull thread uses it, and so can
/// `Controller::stop`, which ends the child at once without waiting for it.
type Slot = Arc<Mutex<Option<Child>>>;

fn lock(slot: &Slot) -> std::sync::MutexGuard<'_, Option<Child>> {
    slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Ends the child in the slot, if there is one. Never waits.
fn end_child(slot: &Slot) {
    if let Some(child) = lock(slot).as_mut() {
        let _ = child.kill();
    }
}

/// Kills and reaps its own child if it is dropped still running. Only the
/// handle this app spawned is ever signalled.
struct Owned(Slot);

impl Owned {
    fn new(slot: &Slot, child: Child) -> Self {
        *lock(slot) = Some(child);
        Self(slot.clone())
    }
    fn with<R>(&self, f: impl FnOnce(&mut Child) -> R) -> Option<R> {
        lock(&self.0).as_mut().map(f)
    }
    fn finish(self) {
        let child = lock(&self.0).take();
        if let Some(mut child) = child {
            let _ = child.wait();
        }
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        let child = lock(&self.0).take();
        if let Some(mut child) = child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// The route as it stands now: the origin to send to, and the raw endpoint so
/// a change of route while the child runs can be noticed.
fn resolve(choice: &Choice) -> Result<(String, String), Fault> {
    crate::tailscale_connect::ensure_private_connection(&choice.source_url)
        .map_err(|_| Fault::Route)?;
    crate::embedded_network::bind_identity(&choice.source_url, &choice.server_id)
        .map_err(|_| Fault::Binding)?;
    let resolved = crate::embedded_network::endpoint(&choice.source_url);
    let routed = crate::embedded_network::routed(&choice.source_url);
    let origin = transport_origin(&choice.source_url, &numeric_loopback(&resolved), routed)?;
    Ok((origin, resolved))
}

fn pull_once(
    choice: &Choice,
    directory: &Path,
    cancel: &AtomicBool,
    slot: &Slot,
) -> Result<Progress, Stop> {
    let cancelled = || cancel.load(Ordering::Acquire);
    let token = match crate::credential_vault::load_replica_token(&choice.key()) {
        Ok(Some(token)) => token,
        Ok(None) => return Err(Fault::NoAccess.into()),
        Err(_) => return Err(Fault::Vault.into()),
    };
    if token.expires != 0 && token.expires <= unix_now() {
        return Err(Fault::NoAccess.into());
    }
    let (transport, resolved) = resolve(choice)?;
    if cancelled() {
        return Err(Stop::Cancelled);
    }
    let input = input_for(choice, &transport, &token.token)?;
    drop(token);
    let exe = crate::local_host::server_binary().ok_or(Fault::Missing)?;
    std::fs::create_dir_all(directory).map_err(|_| Fault::Storage)?;

    let mut command = Command::new(exe);
    command.arg("--replica-pull").arg(directory);
    supervise(
        command,
        &input,
        choice,
        &resolved,
        cancel,
        slot,
        PULL_DEADLINE,
    )
}

/// Runs one prepared child to its end: the input goes in once and the pipe is
/// closed, the output is bounded, and the child is killed at once if the pull
/// is cancelled, the route changes or the deadline passes. Only the handle
/// spawned here is ever signalled.
fn supervise(
    mut command: Command,
    input: &[u8],
    choice: &Choice,
    resolved: &str,
    cancel: &AtomicBool,
    slot: &Slot,
    deadline: Duration,
) -> Result<Progress, Stop> {
    let cancelled = || cancel.load(Ordering::Acquire);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let owned = Owned::new(slot, command.spawn().map_err(|_| Fault::Missing)?);
    // A failed write means the child already ended; its exit explains why.
    if let Some(mut stdin) = owned.with(|child| child.stdin.take()).flatten() {
        let _ = stdin.write_all(input);
    }
    let (sender, receiver) = mpsc::channel();
    let stdout = owned
        .with(|child| child.stdout.take())
        .flatten()
        .ok_or(Fault::Protocol)?;
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout.take(OUTPUT_LIMIT + 1).read_to_end(&mut bytes);
        let _ = sender.send(bytes);
    });

    let started = Instant::now();
    let mut last_route_check = Instant::now();
    let mut output: Option<Vec<u8>> = None;
    let status = loop {
        if cancelled() {
            return Err(Stop::Cancelled);
        }
        if started.elapsed() > deadline {
            return Err(Fault::Network.into());
        }
        if last_route_check.elapsed() >= ROUTE_CHECK {
            last_route_check = Instant::now();
            if crate::embedded_network::endpoint(&choice.source_url) != resolved {
                return Err(Fault::Route.into());
            }
        }
        if output.is_none() {
            if let Ok(bytes) = receiver.try_recv() {
                if bytes.len() as u64 > OUTPUT_LIMIT {
                    return Err(Fault::Protocol.into());
                }
                output = Some(bytes);
            }
        }
        match owned.with(|child| child.try_wait()) {
            Some(Ok(Some(status))) => break status,
            Some(Ok(None)) => thread::sleep(POLL),
            _ => return Err(Fault::Protocol.into()),
        }
    };
    owned.finish();
    let bytes = match output {
        Some(bytes) => bytes,
        None => receiver
            .recv_timeout(Duration::from_secs(2))
            .map_err(|_| Fault::Protocol)?,
    };
    read_report(status.code(), &bytes, choice).map_err(Stop::Fault)
}

// ---------------------------------------------------------------------------
// The schedule

pub enum Event {
    Pulling {
        generation: u64,
    },
    Finished {
        generation: u64,
        result: Result<Progress, Fault>,
        /// When the next pull starts, or None when the schedule has stopped.
        retry_in: Option<Duration>,
    },
}

struct Active {
    choice: Choice,
    cancel: Arc<AtomicBool>,
    now: Arc<AtomicBool>,
    slot: Slot,
    handle: JoinHandle<()>,
}

/// Owns at most one copy schedule. Starting or stopping never waits: a stopped
/// schedule ends its own child on its own thread, and its late answers are
/// dropped by generation.
pub struct Controller {
    ctx: eframe::egui::Context,
    sender: mpsc::Sender<Event>,
    events: mpsc::Receiver<Event>,
    generation: u64,
    active: Option<Active>,
    retiring: Option<JoinHandle<()>>,
}

impl Controller {
    pub fn new(ctx: eframe::egui::Context) -> Self {
        let (sender, events) = mpsc::channel();
        Self {
            ctx,
            sender,
            events,
            generation: 0,
            active: None,
            retiring: None,
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn running(&self) -> Option<&Choice> {
        self.active.as_ref().map(|active| &active.choice)
    }

    /// Begin copying. A previous schedule is cancelled first; the new one waits
    /// for it to finish on its own thread before it touches the folder.
    pub fn start(&mut self, choice: Choice) -> Result<u64, String> {
        let directory = copy_directory(&choice)?;
        self.stop();
        self.generation += 1;
        let generation = self.generation;
        let cancel = Arc::new(AtomicBool::new(false));
        let now = Arc::new(AtomicBool::new(false));
        let slot = Slot::default();
        let previous = self.retiring.take();
        let handle = {
            let (cancel, now, slot) = (cancel.clone(), now.clone(), slot.clone());
            let (sender, ctx) = (self.sender.clone(), self.ctx.clone());
            let choice = choice.clone();
            thread::Builder::new()
                .name("relay-replica-copy".into())
                .spawn(move || {
                    if let Some(previous) = previous {
                        let _ = previous.join();
                    }
                    work(
                        choice, directory, generation, cancel, now, slot, sender, ctx,
                    )
                })
                .map_err(|_| "Relay could not start the copy.".to_string())?
        };
        self.active = Some(Active {
            choice,
            cancel,
            now,
            slot,
            handle,
        });
        Ok(generation)
    }

    pub fn stop(&mut self) {
        // Anything in flight is now stale, even if its thread is still unwinding.
        self.generation += 1;
        if let Some(active) = self.active.take() {
            active.cancel.store(true, Ordering::Release);
            // The child ends now, not when its thread next looks.
            end_child(&active.slot);
            self.retiring = Some(active.handle);
        }
    }

    /// Skip the wait and pull as soon as the current one is done.
    pub fn check_now(&self) {
        if let Some(active) = &self.active {
            active.now.store(true, Ordering::Release);
        }
    }

    /// Answers of the current schedule only.
    pub fn poll(&mut self) -> Vec<Event> {
        let current = self.generation;
        let mut found = Vec::new();
        while let Ok(event) = self.events.try_recv() {
            let generation = match &event {
                Event::Pulling { generation } | Event::Finished { generation, .. } => *generation,
            };
            if generation == current {
                found.push(event);
            }
        }
        found
    }

    /// For leaving the app. The child is ended at once and nothing is waited
    /// for, so closing the window stays fast; its thread finishes on its own.
    pub fn shutdown(&mut self) {
        self.stop();
        self.retiring = None;
    }
}

fn work(
    choice: Choice,
    directory: PathBuf,
    generation: u64,
    cancel: Arc<AtomicBool>,
    now: Arc<AtomicBool>,
    slot: Slot,
    events: mpsc::Sender<Event>,
    ctx: eframe::egui::Context,
) {
    let mut failures = 0u32;
    loop {
        if cancel.load(Ordering::Acquire) {
            return;
        }
        let _ = events.send(Event::Pulling { generation });
        ctx.request_repaint();
        let outcome = pull_once(&choice, &directory, &cancel, &slot);
        // A stopped schedule reports nothing, so a late answer can never be
        // shown for an account or community the app has moved on from.
        if cancel.load(Ordering::Acquire) {
            return;
        }
        let (result, retry_in) = match outcome {
            Err(Stop::Cancelled) => return,
            Ok(progress) => {
                failures = 0;
                (Ok(progress), Some(INTERVAL))
            }
            Err(Stop::Fault(fault)) if fault.permanent() => (Err(fault), None),
            Err(Stop::Fault(fault)) => {
                failures = failures.saturating_add(1);
                (Err(fault), Some(backoff(failures)))
            }
        };
        let _ = events.send(Event::Finished {
            generation,
            result,
            retry_in,
        });
        ctx.request_repaint();
        let Some(wait) = retry_in else {
            return;
        };
        if !sleep(wait, &cancel, &now) {
            return;
        }
    }
}

/// Waits unless cancelled; "check now" ends the wait early.
fn sleep(total: Duration, cancel: &AtomicBool, now: &AtomicBool) -> bool {
    let end = Instant::now() + total;
    while Instant::now() < end {
        if cancel.load(Ordering::Acquire) {
            return false;
        }
        if now.swap(false, Ordering::AcqRel) {
            return true;
        }
        thread::sleep(Duration::from_millis(200));
    }
    !cancel.load(Ordering::Acquire)
}

// ---------------------------------------------------------------------------
// What the page shows

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Started; the first pull has not reported yet.
    Starting,
    Pulling,
    /// Between pulls. `failed` is the reason the last one did not succeed.
    Waiting {
        due: Instant,
        failed: Option<(Fault, u32)>,
    },
    /// The schedule has ended and needs the member.
    Stopped(Fault),
}

/// The state of the copy for one community and account, in this session. The
/// numbers are the last that a pull confirmed; after a restart they are empty
/// until the first pull has reported.
#[derive(Clone, Debug)]
pub struct View {
    pub choice: Choice,
    pub generation: u64,
    pub phase: Phase,
    pub last: Option<(Progress, Instant)>,
    failures: u32,
}

impl View {
    pub fn new(choice: Choice, generation: u64) -> Self {
        Self {
            choice,
            generation,
            phase: Phase::Starting,
            last: None,
            failures: 0,
        }
    }

    /// Takes an answer of this view's schedule. Anything else is refused.
    pub fn apply(&mut self, event: Event, at: Instant) -> bool {
        match event {
            Event::Pulling { generation } if generation == self.generation => {
                self.phase = Phase::Pulling;
                true
            }
            Event::Finished {
                generation,
                result,
                retry_in,
            } if generation == self.generation => {
                match result {
                    Ok(progress) => {
                        self.failures = 0;
                        self.last = Some((progress, at));
                        self.phase = Phase::Waiting {
                            due: at + retry_in.unwrap_or(INTERVAL),
                            failed: None,
                        };
                    }
                    Err(fault) => {
                        self.failures = self.failures.saturating_add(1);
                        // The last good copy and its numbers stay as they were.
                        self.phase = match retry_in {
                            Some(wait) => Phase::Waiting {
                                due: at + wait,
                                failed: Some((fault, self.failures)),
                            },
                            None => Phase::Stopped(fault),
                        };
                    }
                }
                true
            }
            _ => false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Good,
    Info,
    Warn,
    Bad,
}

pub fn age_text(seconds: u64) -> String {
    match seconds {
        0..=4 => "just now".into(),
        5..=59 => format!("{seconds} s ago"),
        60..=3599 => format!("{} min ago", seconds / 60),
        _ => format!("{} h ago", seconds / 3600),
    }
}

fn count(value: u64, one: &str, many: &str) -> String {
    format!("{value} {}", if value == 1 { one } else { many })
}

/// A headline and detail lines for the Hosts page. Completeness is stated only
/// from what the last pull confirmed, and never as readiness to take over.
pub fn describe(view: &View, now: Instant) -> (Tone, String, Vec<String>) {
    let mut lines = Vec::new();
    if let Some((progress, at)) = &view.last {
        let age = now.saturating_duration_since(*at).as_secs();
        lines.push(format!(
            "Last successful check: {}. Changes applied up to {} of {} in the community's log.",
            age_text(age),
            progress.head,
            progress.source_head
        ));
        if progress.files_pending > 0 {
            lines.push(format!(
                "{} still to arrive.",
                count(progress.files_pending, "file is", "files are")
            ));
        }
    } else {
        lines.push("No check has finished since Relay started.".into());
    }
    let (tone, headline) = match &view.phase {
        Phase::Starting => (Tone::Info, "Starting the first check…".to_string()),
        Phase::Pulling => (Tone::Info, "Checking the community now…".to_string()),
        Phase::Waiting { due, failed: None } => {
            let wait = due.saturating_duration_since(now).as_secs();
            let complete = view.last.as_ref().is_some_and(|(p, _)| p.complete);
            (
                if complete { Tone::Good } else { Tone::Info },
                if complete {
                    format!("This PC's copy matched the community at the last check. Next check in {wait} s.")
                } else {
                    format!("The copy is still catching up. Next check in {wait} s.")
                },
            )
        }
        Phase::Waiting {
            due,
            failed: Some((fault, times)),
        } => {
            let wait = due.saturating_duration_since(now).as_secs();
            (
                Tone::Warn,
                format!(
                    "The last check failed ({times} in a row). {} Trying again in {wait} s.",
                    fault.explain()
                ),
            )
        }
        Phase::Stopped(fault) => (
            Tone::Bad,
            format!("Copying has stopped. {}", fault.explain()),
        ),
    };
    if view.last.is_some() {
        lines.push("The earlier copy is kept while checks fail.".into());
    }
    (tone, headline, lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SERVER: &str = "6f6087b7-a4a8-4dba-b4c5-58bf76b59d6b";
    const COPY: &str = "f79a7de8-05d0-4297-b56d-7f34cc03a56d";

    fn choice() -> Choice {
        Choice {
            source_url: "https://one.example".into(),
            server_id: SERVER.into(),
            user_id: "alice".into(),
            replica_id: COPY.into(),
        }
    }

    fn success(extra: &str) -> String {
        format!(
            r#"{{"schema":1,"state":"current","server_id":"{SERVER}","recipient_user_id":"alice","replica_id":"{COPY}","lineage":"L1","head":7,"source_head":7,"files_pending":0,"bytes_received":10,"rows_applied":3,"source_confirmed":true,"promotion_eligible":false{extra}}}"#
        )
    }

    #[test]
    fn the_input_is_one_bounded_object_with_the_token_only_in_it() {
        let bytes = input_for(&choice(), "https://one.example", "synthetic-token").unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["schema"], 1);
        assert_eq!(value["source_url"], "https://one.example");
        assert_eq!(value["transport_url"], "https://one.example");
        assert_eq!(value["server_id"], SERVER);
        assert_eq!(value["recipient_user_id"], "alice");
        assert_eq!(value["replica_id"], COPY);
        assert_eq!(value["token"], "synthetic-token");
        assert_eq!(value.as_object().unwrap().len(), 7);
        assert!(bytes.len() <= INPUT_LIMIT);
        let huge = "x".repeat(INPUT_LIMIT);
        assert!(input_for(&choice(), "https://one.example", &huge).is_err());
    }

    #[test]
    fn a_success_is_shown_only_for_exactly_this_copy() {
        let progress = read_report(Some(0), success("").as_bytes(), &choice()).unwrap();
        assert!(progress.complete);
        assert_eq!((progress.head, progress.source_head), (7, 7));
        let trailing = format!("{}\r\n", success(""));
        assert!(read_report(Some(0), trailing.as_bytes(), &choice()).is_ok());
        for other in [
            Choice {
                server_id: COPY.into(),
                ..choice()
            },
            Choice {
                user_id: "bob".into(),
                ..choice()
            },
            Choice {
                replica_id: SERVER.into(),
                ..choice()
            },
        ] {
            assert_eq!(
                read_report(Some(0), success("").as_bytes(), &other),
                Err(Fault::Binding)
            );
        }
    }

    #[test]
    fn a_report_that_breaks_the_contract_is_never_presented() {
        let line = success("");
        assert_eq!(
            read_report(Some(0), format!("{line}\n{line}\n").as_bytes(), &choice()),
            Err(Fault::Protocol)
        );
        assert_eq!(read_report(Some(0), b"", &choice()), Err(Fault::Protocol));
        assert_eq!(
            read_report(Some(0), b"not json", &choice()),
            Err(Fault::Protocol)
        );
        assert_eq!(
            read_report(Some(0), &vec![b'x'; OUTPUT_LIMIT as usize + 1], &choice()),
            Err(Fault::Protocol)
        );
        assert_eq!(
            read_report(
                Some(0),
                line.replace("\"schema\":1", "\"schema\":2").as_bytes(),
                &choice()
            ),
            Err(Fault::Protocol)
        );
        assert_eq!(
            read_report(
                Some(0),
                line.replace("\"current\"", "\"ready\"").as_bytes(),
                &choice()
            ),
            Err(Fault::Protocol)
        );
        assert_eq!(
            read_report(
                Some(0),
                line.replace("\"lineage\":\"L1\"", "\"lineage\":\"\"")
                    .as_bytes(),
                &choice()
            ),
            Err(Fault::Protocol)
        );
        assert_eq!(
            read_report(
                Some(0),
                line.replace("\"source_confirmed\":true", "\"source_confirmed\":false")
                    .as_bytes(),
                &choice()
            ),
            Err(Fault::Integrity)
        );
        assert_eq!(
            read_report(
                Some(0),
                line.replace("\"head\":7", "\"head\":9").as_bytes(),
                &choice()
            ),
            Err(Fault::Integrity)
        );
        // A non-zero exit never carries a success, and an unknown exit is not trusted.
        assert_eq!(
            read_report(Some(5), line.as_bytes(), &choice()),
            Err(Fault::Protocol)
        );
        assert_eq!(
            read_report(Some(1), line.as_bytes(), &choice()),
            Err(Fault::Protocol)
        );
        assert_eq!(
            read_report(None, line.as_bytes(), &choice()),
            Err(Fault::Protocol)
        );
    }

    #[test]
    fn promotion_is_never_read_as_readiness() {
        // The field is ignored, whatever it says: this app makes no takeover claim.
        let line = success("").replace(
            "\"promotion_eligible\":false",
            "\"promotion_eligible\":true",
        );
        let progress = read_report(Some(0), line.as_bytes(), &choice()).unwrap();
        assert!(!format!("{progress:?}").to_lowercase().contains("promot"));
    }

    #[test]
    fn pending_files_or_a_lagging_head_are_not_reported_as_complete() {
        let pending = success("")
            .replace("\"state\":\"current\"", "\"state\":\"files_pending\"")
            .replace("\"files_pending\":0", "\"files_pending\":2");
        let progress = read_report(Some(0), pending.as_bytes(), &choice()).unwrap();
        assert!(!progress.complete);
        assert_eq!(progress.files_pending, 2);
        let lagging = success("").replace("\"source_head\":7", "\"source_head\":9");
        assert!(
            !read_report(Some(0), lagging.as_bytes(), &choice())
                .unwrap()
                .complete
        );
        let inconsistent = success("").replace("\"files_pending\":0", "\"files_pending\":1");
        assert!(
            !read_report(Some(0), inconsistent.as_bytes(), &choice())
                .unwrap()
                .complete
        );
    }

    #[test]
    fn error_lines_map_to_faults_only_with_their_own_exit_code() {
        let error = |code: &str| {
            format!(r#"{{"schema":1,"state":"error","code":"{code}","message":"safe text"}}"#)
        };
        for (code, exit, fault, permanent) in [
            ("access_denied", 3, Fault::AccessDenied, true),
            ("binding", 4, Fault::Binding, true),
            ("storage", 4, Fault::Storage, true),
            ("network", 5, Fault::Network, false),
            ("busy", 5, Fault::Busy, false),
            ("integrity", 5, Fault::Integrity, false),
        ] {
            assert_eq!(
                read_report(Some(exit), error(code).as_bytes(), &choice()),
                Err(fault)
            );
            assert_eq!(fault.permanent(), permanent);
            assert_eq!(
                read_report(
                    Some(if exit == 3 { 5 } else { 3 }),
                    error(code).as_bytes(),
                    &choice()
                ),
                Err(Fault::Protocol)
            );
        }
        assert_eq!(
            read_report(Some(5), error("surprise").as_bytes(), &choice()),
            Err(Fault::Protocol)
        );
        assert_eq!(read_report(Some(5), b"", &choice()), Err(Fault::Protocol));
    }

    #[test]
    fn backoff_is_bounded_and_resets_through_the_schedule() {
        let seconds: Vec<_> = (0..8).map(|n| backoff(n).as_secs()).collect();
        assert_eq!(seconds, [30, 30, 60, 120, 240, 300, 300, 300]);
        assert_eq!(backoff(u32::MAX), BACKOFF_CAP);
        assert_eq!(INTERVAL, Duration::from_secs(30));
    }

    #[test]
    fn the_token_goes_only_to_the_pinned_origin_or_its_loopback_helper() {
        let https = "https://one.example";
        assert_eq!(
            transport_origin(https, "https://one.example/", false),
            Ok("https://one.example".into())
        );
        assert_eq!(
            transport_origin("http://127.0.0.1:5717", "http://127.0.0.1:5717/", false),
            Ok("http://127.0.0.1:5717".into())
        );
        assert_eq!(
            transport_origin("http://[::1]:5717", "http://[::1]:5717/", false),
            Ok("http://[::1]:5717".into())
        );
        let iroh = format!("http://{}.iroh:4580", "a".repeat(64));
        assert_eq!(
            transport_origin(&iroh, "http://127.0.0.1:50321/", true),
            Ok("http://127.0.0.1:50321".into())
        );
        // A helper endpoint is accepted only for a route the app really holds.
        assert_eq!(
            transport_origin(&iroh, "http://127.0.0.1:50321/", false),
            Err(Fault::Route)
        );
        assert_eq!(
            transport_origin(&iroh, "http://127.0.0.1:0/", true),
            Err(Fault::Route)
        );
        for refused in [
            "http://other.example/",
            "https://other.example/",
            "http://192.168.1.5:4580/",
            "http://localhost:50321/",
            "http://user:pass@127.0.0.1:50321/",
            "http://127.0.0.1:50321/api",
            "http://127.0.0.1:50321/?x=1",
            "ftp://127.0.0.1:50321/",
            "garbage",
        ] {
            assert!(transport_origin(&iroh, refused, true).is_err(), "{refused}");
        }
        // A plain-HTTP address that is neither local nor routed is not a safe place for a token.
        assert_eq!(
            transport_origin("http://100.64.1.2:4580", "http://100.64.1.2:4580/", false),
            Err(Fault::Unsupported)
        );
        assert_eq!(
            transport_origin("http://192.168.1.5:4580", "http://192.168.1.5:4580/", false),
            Err(Fault::Unsupported)
        );
        // A community on this PC named `localhost` is reached by its numeric address.
        assert_eq!(
            transport_origin(
                "http://localhost:5717",
                &numeric_loopback("http://localhost:5717/"),
                false
            ),
            Ok("http://127.0.0.1:5717".into())
        );
        assert_eq!(
            transport_origin("http://localhost:5717", "http://127.0.0.1:5718/", false),
            Err(Fault::Route)
        );
        // A source that resolves to another HTTPS host is never followed.
        assert_eq!(
            transport_origin(https, "https://other.example/", true),
            Err(Fault::Route)
        );
    }

    #[test]
    fn choices_are_bounded_unique_and_never_follow_another_account() {
        let origin = |url: &str, server: &str, user: &str| CommandOrigin {
            url: url.into(),
            server_id: server.into(),
            user_id: user.into(),
        };
        let mut list = vec![
            choice(),
            choice(),
            Choice::default(),
            Choice {
                replica_id: "x".into(),
                ..choice()
            },
        ];
        sanitize(&mut list);
        assert_eq!(list, vec![choice()]);
        assert!(find(&list, &origin("HTTPS://ONE.EXAMPLE/", SERVER, "alice")).is_some());
        for other in [
            origin("https://two.example", SERVER, "alice"),
            origin("https://one.example", COPY, "alice"),
            origin("https://one.example", SERVER, "bob"),
        ] {
            assert!(find(&list, &other).is_none());
        }
        // A fresh consent replaces the copy's identity instead of adding another.
        let fresh = Choice {
            replica_id: SERVER.into(),
            ..choice()
        };
        remember(&mut list, fresh.clone()).unwrap();
        assert_eq!(list, vec![fresh]);
        for n in 0..MAX_COPIES {
            let _ = remember(
                &mut list,
                Choice {
                    user_id: format!("user-{n}"),
                    ..choice()
                },
            );
        }
        assert_eq!(list.len(), MAX_COPIES);
        assert!(remember(
            &mut list,
            Choice {
                user_id: "one-too-many".into(),
                ..choice()
            }
        )
        .is_err());
        forget(&mut list, &origin("https://one.example", SERVER, "user-1"));
        assert_eq!(list.len(), MAX_COPIES - 1);
        assert!(remember(&mut Vec::new(), Choice::default()).is_err());
    }

    #[test]
    fn each_copy_has_its_own_folder_that_reveals_nothing() {
        let a = choice();
        let b = Choice {
            replica_id: SERVER.into(),
            ..choice()
        };
        let c = Choice {
            user_id: "bob".into(),
            ..choice()
        };
        let (da, db, dc) = (a.digest(), b.digest(), c.digest());
        assert_eq!(da.len(), 32);
        assert!(da.bytes().all(|b| b.is_ascii_hexdigit()));
        assert!(da != db && da != dc && db != dc);
        let folder = copy_directory(&a).unwrap();
        assert!(folder.ends_with(&da));
        assert!(!folder.to_string_lossy().contains("alice"));
        assert!(!folder.to_string_lossy().contains("one.example"));
        assert!(copy_directory(&Choice::default()).is_err());
    }

    #[test]
    fn late_answers_of_an_older_schedule_never_change_the_view() {
        let now = Instant::now();
        let mut view = View::new(choice(), 3);
        assert!(!view.apply(Event::Pulling { generation: 2 }, now));
        assert_eq!(view.phase, Phase::Starting);
        let late = Event::Finished {
            generation: 2,
            result: Err(Fault::AccessDenied),
            retry_in: None,
        };
        assert!(!view.apply(late, now));
        assert_eq!(view.phase, Phase::Starting);
        assert!(view.apply(Event::Pulling { generation: 3 }, now));
        assert_eq!(view.phase, Phase::Pulling);
    }

    #[test]
    fn a_failure_keeps_the_last_good_numbers_and_a_permanent_one_stops() {
        let now = Instant::now();
        let mut view = View::new(choice(), 1);
        let progress = read_report(Some(0), success("").as_bytes(), &choice()).unwrap();
        view.apply(
            Event::Finished {
                generation: 1,
                result: Ok(progress.clone()),
                retry_in: Some(INTERVAL),
            },
            now,
        );
        assert!(matches!(view.phase, Phase::Waiting { failed: None, .. }));
        view.apply(
            Event::Finished {
                generation: 1,
                result: Err(Fault::Network),
                retry_in: Some(backoff(1)),
            },
            now,
        );
        assert!(matches!(
            view.phase,
            Phase::Waiting {
                failed: Some((Fault::Network, 1)),
                ..
            }
        ));
        assert_eq!(view.last.as_ref().map(|(p, _)| p.clone()), Some(progress));
        view.apply(
            Event::Finished {
                generation: 1,
                result: Err(Fault::AccessDenied),
                retry_in: None,
            },
            now,
        );
        assert_eq!(view.phase, Phase::Stopped(Fault::AccessDenied));
        assert!(view.last.is_some());
    }

    // A real child process stands in for the companion, so the pipes, the exit
    // codes and the cancellation are exercised for real. It holds only synthetic
    // values and touches nothing of the user's.
    #[cfg(windows)]
    fn shell(script: &str) -> Command {
        let mut command = Command::new("powershell.exe");
        command.args(["-NoProfile", "-NonInteractive", "-Command", script]);
        command
    }

    #[cfg(windows)]
    const ECHO: &str = r#"$i = [Console]::In.ReadToEnd() | ConvertFrom-Json; '{"schema":1,"state":"current","server_id":"' + $i.server_id + '","recipient_user_id":"' + $i.recipient_user_id + '","replica_id":"' + $i.replica_id + '","lineage":"T' + $i.token.Length + '","head":1,"source_head":1,"files_pending":0,"bytes_received":0,"rows_applied":0,"source_confirmed":true,"promotion_eligible":false}'"#;

    #[cfg(windows)]
    #[test]
    fn the_child_gets_one_object_on_a_pipe_that_is_then_closed() {
        let input = input_for(&choice(), "https://one.example", "synthetic-token").unwrap();
        let cancel = AtomicBool::new(false);
        // ReadToEnd returns only if the pipe was closed after the single write.
        let progress = supervise(
            shell(ECHO),
            &input,
            &choice(),
            "https://one.example",
            &cancel,
            &Slot::default(),
            Duration::from_secs(60),
        )
        .ok()
        .expect("a report for this copy");
        assert!(progress.complete);
        assert_eq!(progress.lineage, "T15");
    }

    #[cfg(windows)]
    #[test]
    fn a_report_for_another_account_or_a_failed_exit_is_refused() {
        let input = input_for(&choice(), "https://one.example", "synthetic-token").unwrap();
        let cancel = AtomicBool::new(false);
        let other = Choice {
            user_id: "bob".into(),
            ..choice()
        };
        let run = |script: &str, choice: &Choice| {
            supervise(
                shell(script),
                &input,
                choice,
                "https://one.example",
                &cancel,
                &Slot::default(),
                Duration::from_secs(60),
            )
        };
        assert!(matches!(
            run(ECHO, &other),
            Err(Stop::Fault(Fault::Binding))
        ));
        let denied = r#"[Console]::In.ReadToEnd() | Out-Null; '{"schema":1,"state":"error","code":"access_denied","message":"x"}'; exit 3"#;
        assert!(matches!(
            run(denied, &choice()),
            Err(Stop::Fault(Fault::AccessDenied))
        ));
        assert!(matches!(
            run("exit 7", &choice()),
            Err(Stop::Fault(Fault::Protocol))
        ));
    }

    #[cfg(windows)]
    #[test]
    fn cancelling_ends_the_child_at_once_and_reports_nothing() {
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(800));
            flag.store(true, Ordering::Release);
        });
        let started = Instant::now();
        let result = supervise(
            shell("Start-Sleep -Seconds 60"),
            b"{}",
            &choice(),
            "https://one.example",
            &cancel,
            &Slot::default(),
            Duration::from_secs(120),
        );
        assert!(matches!(result, Err(Stop::Cancelled)));
        assert!(started.elapsed() < Duration::from_secs(20));
    }

    #[cfg(windows)]
    #[test]
    fn a_child_that_outlives_its_deadline_is_ended_as_a_network_fault() {
        let cancel = AtomicBool::new(false);
        let started = Instant::now();
        let result = supervise(
            shell("Start-Sleep -Seconds 60"),
            b"{}",
            &choice(),
            "https://one.example",
            &cancel,
            &Slot::default(),
            Duration::from_secs(2),
        );
        assert!(matches!(result, Err(Stop::Fault(Fault::Network))));
        assert!(started.elapsed() < Duration::from_secs(30));
    }

    #[test]
    fn a_stopped_schedule_delivers_nothing_and_a_new_one_starts_fresh() {
        let mut controller = Controller::new(eframe::egui::Context::default());
        let (sender, events) = (controller.sender.clone(), &mut controller);
        let first = events.generation;
        events.stop();
        assert!(events.generation > first);
        // An answer of the stopped schedule is dropped, a current one is kept.
        sender.send(Event::Pulling { generation: first }).unwrap();
        sender
            .send(Event::Pulling {
                generation: events.generation,
            })
            .unwrap();
        let found = events.poll();
        assert_eq!(found.len(), 1);
        assert!(
            matches!(found[0], Event::Pulling { generation } if generation == events.generation)
        );
        assert!(events.running().is_none());
    }
}
