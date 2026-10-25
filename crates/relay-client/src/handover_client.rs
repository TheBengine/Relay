//! Explicit planned-handover work. Ordinary copy consent never enters here.
//! Tokens stay in the background worker; the companion sees only public proofs
//! and owned paths over stdin. A received copy stays in standby until the signed
//! source grant authorizes the app-owned destination runtime.
use crate::network::CommandOrigin;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use relay_protocol::handover::{self as model, Phase, TargetCommand};
use serde::{de::DeserializeOwned, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use zeroize::Zeroizing;

const ROOT: &str = "/api/hosting/handover";
const PART: u64 = 1024 * 1024;
const MAX_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const CARD_PREFIX: &str = "Relay host setup v1: ";
#[path = "handover_preflight.rs"]
mod preflight;

#[derive(Clone)]
pub enum Action {
    SetupPrivate,
    Inspect,
    Setup { target_url: String },
    Offer { card: String },
    Accept { confirm_full_data: bool },
    Decline,
    Freeze { confirm_pause: bool },
    Cancel,
    CheckDestination,
    Complete { confirm_stop_source: bool },
    Download { retry_export: bool },
    RunDestination,
    RestoreDestination(crate::handover_runtime::Recipe),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Checking,
    Preparing,
    Waiting,
    Downloading,
    Verifying,
    Complete,
    Ready,
    CheckingMedia,
    Checked,
    StoppingSource,
    Active,
    Stopped,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    Cancelled,
    Binding,
    Denied,
    Connection,
    Storage,
    Companion,
    Integrity,
    Changed,
    Confirmation,
    Route,
    HostBusy,
    Media,
    GrantUncertain,
    DestinationPending,
    WaitingForGrant,
}
impl Fault {
    pub fn message(self) -> &'static str {
        match self {
        Self::Cancelled=>"Stopped on this PC. Downloaded data and keys are kept. Refresh the move status before resuming any host.",
        Self::Binding=>"This answer belongs to another account, host or preparation. Your existing files were kept.",
        Self::Denied=>"This account no longer has permission for this move. Refresh the host settings.",
        Self::Connection=>"The host could not be reached. Try again to resume from the parts already received.",
        Self::Storage=>"Relay cannot use its local handover storage. Existing copies and keys were kept.",
        Self::Companion=>"The Relay server companion could not finish. Check storage and use the same Relay version on both PCs. Existing keys and copies were kept.",
        Self::Integrity=>"The received data failed its integrity check. It has not become an active server.",
        Self::Changed=>"This preparation's export changed. The administrator must cancel it and start a new preparation; this PC's existing copy was kept.",
        Self::Confirmation=>"Confirm the full-data transfer or service pause before continuing.",
        Self::Route=>"The prepared destination connection could not start. Check its setup code and use the complete Relay package.",
        Self::HostBusy=>"Another local server is already running or being prepared. Stop it before starting this host. It has been left untouched.",
        Self::Media=>"The new host's generated voice/video check did not complete. Check its prepared HTTPS address and use the complete Relay package. Refresh the move status before trying again.",
        Self::GrantUncertain=>"The connection ended while permanently stopping the old host. The move may already have been granted. Refresh the move status before taking another action; never restore or restart the old database to undo this step.",
        Self::DestinationPending=>"The old host has permanently granted the move. The new host has not confirmed startup yet. Keep Relay open on that PC or resume the transferred host there; the old database must not be restarted. Refresh to retry delivery.",
        Self::WaitingForGrant=>"This PC has a prepared host awaiting the old host's signed approval. Reconnect to the old host and resume the move. No local community was started.",
    }
    }
}
type Result<T> = std::result::Result<T, Fault>;

#[derive(Clone)]
pub struct View {
    pub origin: CommandOrigin,
    pub request_id: u64,
    pub sequence: u64,
    pub stage: Stage,
    pub parts: usize,
    pub total_parts: usize,
    pub bytes: u64,
    pub status: Option<model::Status>,
    pub card: Option<String>,
    pub fault: Option<Fault>,
    pub finished: bool,
}
pub struct Job {
    cancel: Arc<AtomicBool>,
    latest: Arc<Mutex<View>>,
    runtime: Arc<Mutex<Option<crate::handover_runtime::Runtime>>>,
    restoring: bool,
    hosting: bool,
}
impl Job {
    #[cfg(test)]
    pub(crate) fn held_for_test(origin: CommandOrigin, restoring: bool) -> Self {
        let mut view = Self::failure(origin, 1, Fault::Cancelled);
        view.fault = None;
        view.finished = false;
        view.stage = Stage::Ready;
        Self {
            cancel: Arc::new(AtomicBool::new(false)),
            latest: Arc::new(Mutex::new(view)),
            runtime: Arc::new(Mutex::new(None)),
            restoring,
            hosting: true,
        }
    }
    #[cfg(test)]
    pub(crate) fn cancelled_for_test(&self) -> bool {
        self.cancel.load(Ordering::Acquire)
    }
    pub fn failure(origin: CommandOrigin, request_id: u64, fault: Fault) -> View {
        View {
            origin,
            request_id,
            sequence: 1,
            stage: Stage::Stopped,
            parts: 0,
            total_parts: 0,
            bytes: 0,
            status: None,
            card: None,
            fault: Some(fault),
            finished: true,
        }
    }
    pub fn start(
        origin: CommandOrigin,
        pin: model::RoutePin,
        token: Zeroizing<String>,
        action: Action,
        request_id: u64,
        work: Arc<AtomicUsize>,
        wake: impl Fn() + Send + Sync + 'static,
    ) -> Result<Self> {
        Self::spawn(origin, pin, token, action, request_id, work, wake, false)
    }
    pub fn restore(
        recipe: crate::handover_runtime::Recipe,
        work: Arc<AtomicUsize>,
        wake: impl Fn() + Send + Sync + 'static,
    ) -> Result<Self> {
        recipe.validate()?;
        let pin = model::RoutePin {
            server_id: recipe.status.server_id.clone(),
            host: recipe
                .status
                .source_identity
                .clone()
                .ok_or(Fault::Binding)?,
            term: recipe.status.term,
        };
        Self::spawn(
            recipe.origin.clone(),
            pin,
            Zeroizing::new(String::new()),
            Action::RestoreDestination(recipe),
            0,
            work,
            wake,
            true,
        )
    }
    fn spawn(
        origin: CommandOrigin,
        pin: model::RoutePin,
        token: Zeroizing<String>,
        action: Action,
        request_id: u64,
        work: Arc<AtomicUsize>,
        wake: impl Fn() + Send + Sync + 'static,
        restoring: bool,
    ) -> Result<Self> {
        if !crate::host_route::valid_pin(&pin)
            || pin.server_id != origin.server_id
            || crate::community::account_destination(&origin.url).is_err()
        {
            return Err(Fault::Binding);
        }
        work.fetch_update(Ordering::AcqRel, Ordering::Acquire, |v| {
            if v < 4 {
                Some(v + 1)
            } else {
                None
            }
        })
        .map_err(|_| Fault::Connection)?;
        struct Permit(Arc<AtomicUsize>);
        impl Drop for Permit {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::AcqRel);
            }
        }
        let permit = Permit(work);
        let cancel = Arc::new(AtomicBool::new(false));
        let latest = Arc::new(Mutex::new(View {
            origin: origin.clone(),
            request_id,
            sequence: 1,
            stage: Stage::Checking,
            parts: 0,
            total_parts: 0,
            bytes: 0,
            status: None,
            card: None,
            fault: None,
            finished: false,
        }));
        let stopped = cancel.clone();
        let progress = latest.clone();
        let runtime = Arc::new(Mutex::new(None));
        let owned = runtime.clone();
        let hosting = matches!(
            &action,
            Action::RunDestination | Action::RestoreDestination(_)
        );
        std::thread::Builder::new()
            .name("relay-handover".into())
            .spawn(move || {
                let _permit = permit;
                let result = (|| {
                    let data = directory(&origin)?;
                    let lock_path = data.join("operation.lock");
                    direct(&lock_path)?;
                    let lock = fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .create(true)
                        .truncate(false)
                        .open(lock_path)
                        .map_err(|_| Fault::Storage)?;
                    lock.try_lock().map_err(|_| Fault::Storage)?;
                    let mut publish = |stage, parts, total, bytes, status, card| {
                        let mut v = progress.lock().unwrap_or_else(|e| e.into_inner());
                        v.stage = stage;
                        v.parts = parts;
                        v.total_parts = total;
                        v.bytes = bytes;
                        if let Some(status) = status {
                            v.status = Some(status);
                        }
                        if let Some(card) = card {
                            v.card = Some(card);
                        }
                        v.sequence = v.sequence.wrapping_add(1);
                        drop(v);
                        wake();
                    };
                    if let Action::RestoreDestination(recipe) = &action {
                        if !restoring || recipe.origin != origin {
                            return Err(Fault::Binding);
                        }
                        let mut host = crate::handover_runtime::Runtime::start(
                            origin,
                            recipe.status.clone(),
                            data,
                            &stopped,
                        )?;
                        await_active(&mut host, &recipe.status, &mut publish)?;
                        *owned.lock().unwrap_or_else(|e| e.into_inner()) = Some(host);
                        return Ok(());
                    }
                    let mut local = LocalCompanion {
                        data: data.clone(),
                        cancel: stopped.clone(),
                    };
                    let mut source = Source::new(origin, pin, token, stopped.clone())?;
                    if matches!(&action, Action::RunDestination) {
                        let host = run_destination(&mut source, &data, &stopped, &mut publish)?;
                        *owned.lock().unwrap_or_else(|e| e.into_inner()) = Some(host);
                        return Ok(());
                    }
                    run(&mut source, &mut local, &data, action, &stopped, publish)
                })();
                let mut v = progress.lock().unwrap_or_else(|e| e.into_inner());
                if let Err(error) = result {
                    v.stage = Stage::Stopped;
                    v.fault = Some(error);
                } else if !matches!(v.stage, Stage::Active | Stage::Checked) {
                    v.stage = Stage::Complete;
                }
                v.finished = true;
                v.sequence = v.sequence.wrapping_add(1);
                drop(v);
                wake();
            })
            .map_err(|_| Fault::Companion)?;
        Ok(Self {
            cancel,
            latest,
            runtime,
            restoring,
            hosting,
        })
    }
    pub fn view(&self) -> View {
        self.latest
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Release);
    }
    pub fn is_restore(&self) -> bool {
        self.restoring
    }
    pub fn is_hosting(&self) -> bool {
        self.hosting
    }
    pub fn take_runtime(&self) -> Option<crate::handover_runtime::Runtime> {
        self.runtime
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }
}
impl Drop for Job {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
    }
}

fn stopped(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Acquire) {
        Err(Fault::Cancelled)
    } else {
        Ok(())
    }
}
fn uuid(v: &str) -> bool {
    uuid::Uuid::parse_str(v).is_ok_and(|id| id.to_string() == v)
}
fn sha(v: &[u8]) -> String {
    format!("{:x}", Sha256::digest(v))
}
pub fn decode_card(raw: &str, source: &model::RoutePin) -> Result<model::DestinationCard> {
    let encoded = raw.trim().strip_prefix(CARD_PREFIX).ok_or(Fault::Binding)?;
    if encoded.len() > 8192 {
        return Err(Fault::Binding);
    }
    let bytes = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| Fault::Binding)?;
    let card: model::DestinationCard =
        serde_json::from_slice(&bytes).map_err(|_| Fault::Binding)?;
    validate_destination_card(&card, source)?;
    Ok(card)
}
fn validate_destination_card(card: &model::DestinationCard, source: &model::RoutePin) -> Result<()> {
    if card.body.source != *source
        || !uuid(&card.body.user_id)
        || card.body.target == source.host
        || !crate::host_route::valid_pin(&model::RoutePin {
            server_id: source.server_id.clone(),
            host: card.body.target.clone(),
            term: source.term,
        })
        || crate::host_route::safe_destination(&card.body.target_url).is_err()
        || !private_destination(&card.body)
        || !crate::host_route::verify(
            &card.body.target,
            &card.signature,
            &model::destination_message(&card.body),
        )
    {
        return Err(Fault::Binding);
    }
    Ok(())
}
pub(crate) fn validate_destination_status(s: &model::Status) -> Result<()> {
    if !s.history.is_empty() {
        let pin=model::RoutePin{server_id:s.server_id.clone(),
            host:s.source_identity.clone().ok_or(Fault::Binding)?,term:s.term};
        if !crate::host_route::history_connects(&pin,&pin,&s.history) { return Err(Fault::Binding); }
    }
    let Some(card)=&s.destination else {
        return if s.preparation.as_ref().is_some_and(|p| crate::iroh_route::is_iroh(&p.target_url)) {
            Err(Fault::Binding)
        } else { Ok(()) };
    };
    let p=s.preparation.as_ref().ok_or(Fault::Binding)?;
    let source=model::RoutePin{server_id:s.server_id.clone(),
        host:s.source_identity.clone().ok_or(Fault::Binding)?,term:s.term};
    validate_destination_card(card,&source)?;
    if card.body.user_id!=p.target_user_id || card.body.target.host_id!=p.target_host_id
        || card.body.target_url!=p.target_url
        || s.target_identity.as_ref().is_some_and(|id|id!=&card.body.target)
    { return Err(Fault::Binding); }
    Ok(())
}
pub(crate) fn private_destination(body: &model::DestinationBody) -> bool {
    let Some(route) = &body.private_route else {
        return url::Url::parse(&body.target_url).is_ok_and(|url| url.scheme()=="https");
    };
    let saved = crate::iroh_route::SavedIroh {
        endpoint_id:route.endpoint_id.clone(), relay:route.relay.clone(), token:route.token.clone(),
    };
    route.scope.as_ref().is_none_or(|scope| *scope == body.target.host_id)
        && route.token.len()==43 && saved.to_invitation().is_ok() && saved.address()==body.target_url
}
fn encode_card(card: &model::DestinationCard) -> Result<String> {
    let bytes = serde_json::to_vec(card).map_err(|_| Fault::Binding)?;
    let value = format!("{CARD_PREFIX}{}", URL_SAFE_NO_PAD.encode(bytes));
    decode_card(&value, &card.body.source)?;
    Ok(value)
}

pub(crate) fn direct(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        return Err(Fault::Storage);
    }
    for part in path.ancestors() {
        match fs::symlink_metadata(part) {
            Ok(info) => {
                let mut redirected = info.file_type().is_symlink();
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    redirected |= info.file_attributes() & 0x400 != 0;
                }
                if redirected {
                    return Err(Fault::Storage);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(Fault::Storage),
        }
    }
    Ok(())
}
pub(crate) fn directory(origin: &CommandOrigin) -> Result<PathBuf> {
    let base = std::env::var_os("RELAY_PROFILE_DIR")
        .or_else(|| std::env::var_os("RELAY_CONFIG_DIR"))
        .map(PathBuf::from)
        .or_else(|| {
            directories::ProjectDirs::from("org", "Relay", "Relay")
                .map(|v| v.data_local_dir().to_owned())
        })
        .ok_or(Fault::Storage)?;
    let name = sha(&serde_json::to_vec(&(
        "relay-local-handover-v1",
        &origin.server_id,
        &origin.user_id,
    ))
    .map_err(|_| Fault::Binding)?);
    let path = base.join("handover").join(name);
    direct(&path)?;
    fs::create_dir_all(&path).map_err(|_| Fault::Storage)?;
    Ok(path)
}
fn read(path: &Path, limit: u64) -> Result<Option<Vec<u8>>> {
    direct(path)?;
    let file = match fs::File::open(path) {
        Ok(v) => v,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(Fault::Storage),
    };
    let info = file.metadata().map_err(|_| Fault::Storage)?;
    if !info.is_file() || info.len() > limit {
        return Err(Fault::Integrity);
    }
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| Fault::Storage)?;
    if bytes.len() as u64 > limit {
        return Err(Fault::Integrity);
    }
    Ok(Some(bytes))
}
fn publish(path: &Path, bytes: &[u8]) -> Result<()> {
    direct(path)?;
    if let Some(old) = read(path, bytes.len() as u64)? {
        return if old == bytes {
            Ok(())
        } else {
            Err(Fault::Changed)
        };
    }
    let pending = path.with_extension("download-pending");
    direct(&pending)?;
    if pending.exists() {
        if !fs::symlink_metadata(&pending)
            .map_err(|_| Fault::Storage)?
            .is_file()
        {
            return Err(Fault::Storage);
        }
        fs::remove_file(&pending).map_err(|_| Fault::Storage)?;
    }
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&pending).map_err(|_| Fault::Storage)?;
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|_| Fault::Storage)?;
    drop(file);
    fs::hard_link(&pending, path).map_err(|_| Fault::Storage)?;
    fs::remove_file(&pending).map_err(|_| Fault::Storage)?;
    Ok(())
}

trait Companion {
    fn call(&mut self, command: TargetCommand) -> Result<serde_json::Value>;
}
struct LocalCompanion {
    data: PathBuf,
    cancel: Arc<AtomicBool>,
}
impl Companion for LocalCompanion {
    fn call(&mut self, command: TargetCommand) -> Result<serde_json::Value> {
        use std::process::{Command, Stdio};
        stopped(&self.cancel)?;
        let executable = crate::local_host::server_binary().ok_or(Fault::Companion)?;
        direct(&executable)?;
        let input = serde_json::to_vec(&command).map_err(|_| Fault::Companion)?;
        if input.len() as u64 > model::CONTROL_BYTES {
            return Err(Fault::Companion);
        }
        let mut process = Command::new(executable);
        process
            .arg("--handover-target")
            .arg(&self.data)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            process.creation_flags(0x0800_0000);
        }
        struct Owned(std::process::Child);
        impl Drop for Owned {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let mut child = Owned(process.spawn().map_err(|_| Fault::Companion)?);
        child
            .0
            .stdin
            .take()
            .ok_or(Fault::Companion)?
            .write_all(&input)
            .map_err(|_| Fault::Companion)?;
        let stdout = child.0.stdout.take().ok_or(Fault::Companion)?;
        let (send, receive) = std::sync::mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("relay-handover-output".into())
            .spawn(move || {
                let mut bytes = Vec::new();
                let result = stdout
                    .take(model::CONTROL_BYTES + 1)
                    .read_to_end(&mut bytes)
                    .map(|_| bytes);
                let _ = send.send(result);
            })
            .map_err(|_| Fault::Companion)?;
        let deadline = Instant::now() + Duration::from_secs(20 * 60);
        let code = loop {
            stopped(&self.cancel)?;
            if Instant::now() > deadline {
                return Err(Fault::Companion);
            }
            match child.0.try_wait().map_err(|_| Fault::Companion)? {
                Some(code) => break code,
                None => std::thread::sleep(Duration::from_millis(40)),
            }
        };
        let output = receive
            .recv_timeout(Duration::from_secs(2))
            .map_err(|_| Fault::Companion)?
            .map_err(|_| Fault::Companion)?;
        if !code.success() || output.len() as u64 > model::CONTROL_BYTES {
            return Err(Fault::Companion);
        }
        serde_json::from_slice(&output).map_err(|_| Fault::Companion)
    }
}
fn companion<T: DeserializeOwned>(local: &mut impl Companion, command: TargetCommand) -> Result<T> {
    serde_json::from_value(local.call(command)?).map_err(|_| Fault::Companion)
}

trait Remote {
    fn origin(&self) -> &CommandOrigin;
    fn pin(&self) -> &model::RoutePin;
    fn json<T: DeserializeOwned>(
        &mut self,
        post: bool,
        path: &str,
        body: &impl Serialize,
        limit: u64,
    ) -> Result<T>;
    fn part(&mut self, input: &model::PartRequest) -> Result<Vec<u8>>;
}
struct Source {
    origin: CommandOrigin,
    pin: model::RoutePin,
    token: Zeroizing<String>,
    http: reqwest::blocking::Client,
    cancel: Arc<AtomicBool>,
    resolved: String,
}
impl Source {
    fn new(
        origin: CommandOrigin,
        pin: model::RoutePin,
        token: Zeroizing<String>,
        cancel: Arc<AtomicBool>,
    ) -> Result<Self> {
        crate::embedded_network::ensure_route(&origin.url).map_err(|_| Fault::Connection)?;
        let resolved = crate::embedded_network::endpoint(&origin.url);
        crate::replica_copy::transport_origin(
            &origin.url,
            &resolved,
            crate::embedded_network::routed(&origin.url),
        )
        .map_err(|_| Fault::Binding)?;
        let http = reqwest::blocking::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|_| Fault::Connection)?;
        Ok(Self {
            origin,
            pin,
            token,
            http,
            cancel,
            resolved,
        })
    }
    fn bytes(&self, post: bool, path: &str, body: &impl Serialize, limit: u64) -> Result<Vec<u8>> {
        stopped(&self.cancel)?;
        if crate::embedded_network::endpoint(&self.origin.url) != self.resolved {
            return Err(Fault::Connection);
        }
        let mut request = self
            .http
            .request(
                if post {
                    reqwest::Method::POST
                } else {
                    reqwest::Method::GET
                },
                format!("{}{path}", self.resolved),
            )
            .bearer_auth(self.token.as_str());
        if post {
            request = request.json(body);
        }
        let mut reply = request.send().map_err(|_| Fault::Connection)?;
        match reply.status().as_u16() {
            200..=299 => {}
            401 | 403 => return Err(Fault::Denied),
            409 => return Err(Fault::Changed),
            _ => return Err(Fault::Connection),
        }
        let mut bytes = Vec::new();
        let mut chunk = [0u8; 32 * 1024];
        loop {
            stopped(&self.cancel)?;
            let n = reply.read(&mut chunk).map_err(|_| Fault::Connection)?;
            if n == 0 {
                break;
            }
            if bytes.len() as u64 + n as u64 > limit {
                return Err(Fault::Integrity);
            }
            bytes.extend_from_slice(&chunk[..n]);
        }
        stopped(&self.cancel)?;
        Ok(bytes)
    }
}
impl Remote for Source {
    fn origin(&self) -> &CommandOrigin {
        &self.origin
    }
    fn pin(&self) -> &model::RoutePin {
        &self.pin
    }
    fn json<T: DeserializeOwned>(
        &mut self,
        post: bool,
        path: &str,
        body: &impl Serialize,
        limit: u64,
    ) -> Result<T> {
        serde_json::from_slice(&self.bytes(post, path, body, limit)?).map_err(|_| Fault::Integrity)
    }
    fn part(&mut self, input: &model::PartRequest) -> Result<Vec<u8>> {
        self.bytes(true, &format!("{ROOT}/part"), input, PART + 48)
    }
}
fn status(remote: &mut impl Remote) -> Result<model::Status> {
    let s: model::Status = remote.json(false, ROOT, &(), model::CONTROL_BYTES)?;
    if s.server_id != remote.origin().server_id
        || s.source_identity.as_ref() != Some(&remote.pin().host)
        || !uuid(&s.source_host_id)
        || s.term != remote.pin().term
    {
        return Err(Fault::Binding);
    }
    if s.preparation.as_ref().is_some_and(|p| {
        !uuid(&p.id)
            || p.target_host_id == s.source_host_id
            || p.target_host_id == remote.pin().host.host_id
    }) {
        return Err(Fault::Binding);
    }
    validate_destination_status(&s)?;
    crate::handover_runtime::observe_source(remote.origin(), remote.pin(), &s)?;
    Ok(s)
}
fn change(s: &model::Status) -> Result<model::Change> {
    Ok(model::Change {
        server_id: s.server_id.clone(),
        preparation_id: s.preparation.as_ref().ok_or(Fault::Changed)?.id.clone(),
    })
}
fn current(remote: &mut impl Remote, wanted: &model::Status) -> Result<model::Status> {
    let now = status(remote)?;
    let same = now
        .preparation
        .as_ref()
        .zip(wanted.preparation.as_ref())
        .is_some_and(|(a, b)| {
            a.id == b.id
                && a.target_user_id == b.target_user_id
                && a.target_url == b.target_url
                && a.target_host_id == b.target_host_id
                && a.phase == b.phase
        });
    if !same
        || now.source_host_id != wanted.source_host_id
        || now.target_identity != wanted.target_identity
    {
        return Err(Fault::Changed);
    }
    Ok(now)
}
fn valid_manifest(
    m: &model::TransferManifest,
    s: &model::Status,
    recipient: &model::Recipient,
) -> bool {
    let c = &m.context;
    let Some(p) = &s.preparation else {
        return false;
    };
    c.server_id == s.server_id
        && c.source_host_id == s.source_host_id
        && Some(&c.source_identity) == s.source_identity.as_ref()
        && c.term == s.term
        && c.preparation_id == p.id
        && c.target_url == p.target_url
        && Some(&c.target_identity) == s.target_identity.as_ref()
        && c.recipient_key == recipient.public_key
        && uuid(&c.export_id)
        && c.database_bytes > 0
        && c.database_bytes <= MAX_BYTES
        && m.parts.len() == c.database_bytes.div_ceil(PART) as usize
        && m.parts.len() <= 4096
        && m.parts.iter().enumerate().all(|(i, v)| {
            v.bytes == (c.database_bytes - i as u64 * PART).min(PART) + 48 && v.sha256.len() == 64
        })
        && crate::host_route::verify(
            &c.source_identity,
            &m.signature,
            &model::manifest_message(c, &m.parts),
        )
}
fn run(
    remote: &mut impl Remote,
    local: &mut impl Companion,
    data: &Path,
    action: Action,
    cancel: &AtomicBool,
    mut progress: impl FnMut(Stage, usize, usize, u64, Option<model::Status>, Option<String>),
) -> Result<()> {
    stopped(cancel)?;
    if matches!(&action, Action::Setup { .. } | Action::SetupPrivate) {
        let (target_url,private_route,target_identity) = match &action {
            Action::Setup{target_url} => (target_url.clone(),None,None),
            Action::SetupPrivate => {
                let identity:model::TargetNetworkIdentity=companion(local,TargetCommand::NetworkIdentity)?;
                let saved=crate::iroh_route::prepare_host_identity(&identity.target.host_id,identity.endpoint_id.as_deref()).map_err(|_|Fault::Route)?;
                (saved.address(),Some(model::IrohDestination {
                    endpoint_id:saved.endpoint_id,relay:saved.relay,token:saved.token,
                    scope:Some(identity.target.host_id.clone()),
                }),Some(identity.target))
            },
            _=>unreachable!(),
        };
        let target_url=crate::host_route::safe_destination(&target_url).map_err(|_| Fault::Binding)?;
        let card: model::DestinationCard = companion(
            local,
            TargetCommand::Plan {
                source: remote.pin().clone(),
                user_id: remote.origin().user_id.clone(),
                target_url: target_url.clone(),
                private_route: private_route.clone(),
            },
        )?;
        if card.body.source != *remote.pin() || card.body.user_id != remote.origin().user_id
            || card.body.target_url != target_url || card.body.private_route != private_route
            || target_identity.as_ref().is_some_and(|id| *id != card.body.target) {
            return Err(Fault::Binding);
        }
        progress(Stage::Preparing, 0, 0, 0, None, Some(encode_card(&card)?));
        return Ok(());
    }
    let mut s = status(remote)?;
    progress(Stage::Checking, 0, 0, 0, Some(s.clone()), None);
    if matches!(&action, Action::CheckDestination | Action::Complete { .. }) {
        let finish=matches!(&action,Action::Complete{..});
        if matches!(&action,Action::Complete{confirm_stop_source:false}){return Err(Fault::Confirmation);}
        if s.retired.is_some(){progress(Stage::Complete,0,0,0,Some(s.clone()),None);if finish{preflight::deliver(&s)?;}return Ok(());}
        if s.preparation.as_ref().is_none_or(|p|p.phase!=Phase::Frozen)||s.received.is_none(){return Err(Fault::Changed);}
        progress(Stage::CheckingMedia,0,0,0,Some(s.clone()),None);
        let proof=preflight::check(remote,&s,cancel)?;
        s=current(remote,&s)?;stopped(cancel)?;
        if finish && s.retired.is_none(){
            progress(Stage::StoppingSource,0,0,0,Some(s.clone()),None);
            s=preflight::finish(remote,&s,proof)?;
        }
        if finish {
            progress(Stage::Complete,0,0,0,Some(s.clone()),None);
            preflight::deliver(&s)?;
        }
        progress(if finish{Stage::Complete}else{Stage::Checked},0,0,0,Some(s),None);
        return Ok(());
    }
    let (path, body) = match action {
        Action::Inspect => return Ok(()),
        Action::Offer { card } => {
            let card = decode_card(&card, remote.pin())?;
            if s.preparation.is_some() {
                return Err(Fault::Changed);
            }
            (
                format!("{ROOT}/offer"),
                serde_json::to_value(model::Offer {
                    server_id: s.server_id.clone(),
                    target_user_id: card.body.user_id.clone(),
                    target_host_id: card.body.target.host_id.clone(),
                    target_url: card.body.target_url.clone(),
                    // Older HTTPS sources deny unknown fields. Their existing
                    // acceptance signature still binds the destination key.
                    destination: card.body.private_route.is_some().then_some(card),
                })
                .map_err(|_| Fault::Binding)?,
            )
        }
        Action::Accept { confirm_full_data } => {
            if !confirm_full_data {
                return Err(Fault::Confirmation);
            }
            let p = s.preparation.clone().ok_or(Fault::Changed)?;
            if p.target_user_id != remote.origin().user_id {
                return Err(Fault::Denied);
            }
            if p.phase != Phase::Offered {
                let _: model::Recipient = companion(local, TargetCommand::Recipient { status: s })?;
                return Ok(());
            }
            let proof: model::HostProof =
                companion(local, TargetCommand::Accept { status: s.clone() })?;
            if proof.identity.host_id != p.target_host_id
                || !crate::host_route::verify(
                    &proof.identity,
                    &proof.signature,
                    &model::acceptance_message(
                        &s.server_id,
                        &s.source_host_id,
                        s.source_identity.as_ref().ok_or(Fault::Binding)?,
                        s.term,
                        &p,
                    ),
                )
            {
                return Err(Fault::Binding);
            }
            (
                format!("{ROOT}/answer"),
                serde_json::to_value(model::Answer {
                    server_id: s.server_id.clone(),
                    preparation_id: p.id.clone(),
                    accept_full_server_data: true,
                    target_proof: Some(proof),
                })
                .map_err(|_| Fault::Binding)?,
            )
        }
        Action::Decline => (
            format!("{ROOT}/answer"),
            serde_json::to_value(model::Answer {
                server_id: s.server_id.clone(),
                preparation_id: change(&s)?.preparation_id,
                accept_full_server_data: false,
                target_proof: None,
            })
            .map_err(|_| Fault::Binding)?,
        ),
        Action::Freeze { confirm_pause } => {
            if !confirm_pause {
                return Err(Fault::Confirmation);
            }
            (
                format!("{ROOT}/freeze"),
                serde_json::to_value(change(&s)?).map_err(|_| Fault::Binding)?,
            )
        }
        Action::Cancel => (
            format!("{ROOT}/cancel"),
            serde_json::to_value(change(&s)?).map_err(|_| Fault::Binding)?,
        ),
        Action::Download { retry_export } => {
            let p = s.preparation.clone().ok_or(Fault::Changed)?;
            if p.phase != Phase::Frozen
                || p.target_user_id != remote.origin().user_id
                || s.retired.is_some()
            {
                return Err(Fault::Denied);
            }
            let recipient: model::Recipient =
                companion(local, TargetCommand::Recipient { status: s.clone() })?;
            let request = model::ExportRequest {
                change: change(&s)?,
                recipient: recipient.clone(),
                retry: retry_export,
            };
            let deadline = Instant::now() + Duration::from_secs(20 * 60);
            let manifest = loop {
                stopped(cancel)?;
                if Instant::now() > deadline {
                    return Err(Fault::Connection);
                }
                match remote.json::<model::ExportStatus>(
                    true,
                    &format!("{ROOT}/export"),
                    &request,
                    512 * 1024,
                )? {
                    model::ExportStatus::Ready { manifest } => break manifest,
                    model::ExportStatus::Failed => return Err(Fault::Changed),
                    model::ExportStatus::Building => {
                        progress(Stage::Waiting, 0, 0, 0, None, None);
                        // Keep long snapshot builds below the ordinary mutation
                        // quota; cancellation still wakes within a short step.
                        for _ in 0..16 {
                            stopped(cancel)?;
                            std::thread::sleep(Duration::from_millis(125));
                        }
                    }
                }
            };
            if !valid_manifest(&manifest, &s, &recipient) {
                return Err(Fault::Integrity);
            }
            let cache = data.join("downloads").join(&p.id);
            direct(&cache)?;
            fs::create_dir_all(&cache).map_err(|_| Fault::Storage)?;
            publish(
                &cache.join("manifest.json"),
                &serde_json::to_vec(&manifest).map_err(|_| Fault::Integrity)?,
            )?;
            let mut total = 0;
            for (index, part) in manifest.parts.iter().enumerate() {
                stopped(cancel)?;
                let path = cache.join(format!("{index}.hpke"));
                let good = read(&path, PART + 48)?.is_some_and(|bytes| {
                    bytes.len() as u64 == part.bytes && sha(&bytes) == part.sha256
                });
                if !good {
                    current(remote, &s)?;
                    let bytes = remote.part(&model::PartRequest {
                        request: request.clone(),
                        export_id: manifest.context.export_id.clone(),
                        index,
                    })?;
                    if bytes.len() as u64 != part.bytes || sha(&bytes) != part.sha256 {
                        return Err(Fault::Integrity);
                    }
                    // Only encrypted cache data under this exact signed manifest
                    // is replaceable; the standby database is never touched here.
                    if path.exists() {
                        direct(&path)?;
                        fs::remove_file(&path).map_err(|_| Fault::Storage)?;
                    }
                    publish(&path, &bytes)?;
                }
                total += part.bytes;
                progress(
                    Stage::Downloading,
                    index + 1,
                    manifest.parts.len(),
                    total,
                    None,
                    None,
                );
            }
            s = current(remote, &s)?;
            progress(
                Stage::Verifying,
                manifest.parts.len(),
                manifest.parts.len(),
                total,
                None,
                None,
            );
            let received: serde_json::Value = local.call(TargetCommand::Receive {
                status: s.clone(),
                cache,
            })?;
            if received["server_id"].as_str() != Some(s.server_id.as_str())
                || received["preparation_id"].as_str() != Some(p.id.as_str())
                || received["database_sha256"].as_str()
                    != Some(manifest.context.database_sha256.as_str())
                || received["activation_allowed"] != false
            {
                return Err(Fault::Integrity);
            }
            let receipt: model::Receipt =
                companion(local, TargetCommand::Receipt { status: s.clone() })?;
            let b = &receipt.body;
            if b.server_id != s.server_id
                || b.preparation_id != p.id
                || b.export_id != manifest.context.export_id
                || b.source_host_id != s.source_host_id
                || b.term != s.term
                || b.target_url != p.target_url
                || b.database_bytes != manifest.context.database_bytes
                || Some(&b.target_identity) != s.target_identity.as_ref()
                || b.database_sha256 != manifest.context.database_sha256
                || b.manifest_sha256
                    != sha(&serde_json::to_vec(&manifest).map_err(|_| Fault::Integrity)?)
                || Some(&b.challenge) != s.receipt_challenge.as_ref()
                || !crate::host_route::verify(
                    &b.target_identity,
                    &receipt.signature,
                    &model::receipt_message(b),
                )
            {
                return Err(Fault::Integrity);
            }
            current(remote, &s)?;
            let done: model::Status =
                remote.json(true, &format!("{ROOT}/receipt"), &receipt, model::CONTROL_BYTES)?;
            if done.received.as_ref() != Some(&receipt) {
                return Err(Fault::Binding);
            }
            progress(
                Stage::Complete,
                manifest.parts.len(),
                manifest.parts.len(),
                total,
                Some(done),
                None,
            );
            return Ok(());
        }
        Action::Setup { .. } | Action::SetupPrivate | Action::RunDestination | Action::RestoreDestination(_)
        | Action::CheckDestination | Action::Complete { .. } => {
            unreachable!()
        }
    };
    stopped(cancel)?;
    let _: model::Status = remote.json(true, &path, &body, model::CONTROL_BYTES)?;
    s = status(remote)?;
    progress(Stage::Complete, 0, 0, 0, Some(s), None);
    Ok(())
}

fn await_active(
    host: &mut crate::handover_runtime::Runtime,
    status: &model::Status,
    progress: &mut impl FnMut(Stage, usize, usize, u64, Option<model::Status>, Option<String>),
) -> Result<()> {
    // After the source grant exists this is owned local lifecycle work. Finish
    // its bounded proof even if the client account changed; never send another
    // community bearer from that replaced account.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if host.check_active(status)? {
            progress(Stage::Active, 0, 0, 0, Some(status.clone()), None);
            return Ok(());
        }
        if Instant::now() > deadline {
            return Err(Fault::Companion);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
fn run_destination(
    remote: &mut impl Remote,
    data: &Path,
    cancel: &AtomicBool,
    progress: &mut impl FnMut(Stage, usize, usize, u64, Option<model::Status>, Option<String>),
) -> Result<crate::handover_runtime::Runtime> {
    let mut s = match crate::handover_runtime::recover(remote.origin(), remote.pin())? {
        Some(recipe) => recipe.status,
        None => status(remote)?,
    };
    let p = s.preparation.as_ref().ok_or(Fault::Changed)?;
    if p.phase != Phase::Frozen
        || p.target_user_id != remote.origin().user_id
        || s.received.is_none()
    {
        return Err(Fault::Denied);
    }
    if s.retired.is_some() {
        crate::handover_runtime::Recipe {
            schema: 1,
            origin: remote.origin().clone(),
            status: s.clone(),
        }
        .checkpoint()?;
    }
    progress(Stage::Preparing, 0, 0, 0, Some(s.clone()), None);
    let mut host = crate::handover_runtime::Runtime::start(
        remote.origin().clone(),
        s.clone(),
        data.to_owned(),
        cancel,
    )?;
    if s.retired.is_none() {
        progress(Stage::Ready, 0, 0, 0, Some(s.clone()), None);
        let deadline = Instant::now() + Duration::from_secs(2 * 60 * 60);
        loop {
            if let Some(delivered)=host.delivered_grant()? {
                crate::handover_runtime::Recipe{schema:1,origin:remote.origin().clone(),status:delivered.clone()}.checkpoint()?;
                progress(Stage::Ready,0,0,0,Some(delivered.clone()),None);
                stopped(cancel)?;s=delivered;break;
            }
            stopped(cancel)?;
            if !host.is_alive() {
                return Err(Fault::Companion);
            }
            if Instant::now() > deadline {
                return Err(Fault::Connection);
            }
            match current(remote,&s){
                Ok(value)=>s=value,
                Err(Fault::Connection)=>{for _ in 0..16{stopped(cancel)?;std::thread::sleep(Duration::from_millis(125));}continue;}
                Err(error)=>return Err(error),
            }
            if s.retired.is_some() {
                progress(Stage::Ready,0,0,0,Some(s.clone()),None);
                host.activate(&s)?;
                break;
            }
            for _ in 0..16 {
                stopped(cancel)?;
                std::thread::sleep(Duration::from_millis(125));
            }
        }
    }
    await_active(&mut host, &s, progress)?;
    Ok(host)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use serde_json::json;
    fn signed(key: &SigningKey, bytes: Vec<u8>) -> String {
        URL_SAFE_NO_PAD.encode(key.sign(&bytes).to_bytes())
    }
    struct Mock {
        origin: CommandOrigin,
        pin: model::RoutePin,
        status: model::Status,
        manifest: model::TransferManifest,
        recipient: model::Recipient,
        parts: Vec<Vec<u8>>,
        requested: Vec<usize>,
        fail: bool,
        receipts: usize,
    }
    fn fixture() -> (Mock, Helper) {
        let (source, target, pin, grant) = crate::host_route::tests::fixture();
        let r = &grant.receipt.body;
        let origin = CommandOrigin {
            url: "https://source.example".into(),
            server_id: pin.server_id.clone(),
            user_id: uuid::Uuid::new_v4().to_string(),
        };
        let status = model::Status {
            server_id: pin.server_id.clone(),
            source_host_id: r.source_host_id.clone(),
            term: pin.term,
            writes_allowed: false,
            preparation: Some(model::Preparation {
                id: r.preparation_id.clone(),
                target_user_id: origin.user_id.clone(),
                target_host_id: r.target_identity.host_id.clone(),
                target_url: r.target_url.clone(),
                offered_by: uuid::Uuid::new_v4().to_string(),
                phase: Phase::Frozen,
                updated_ms: 1,
            }),
            activation_available: false,
            history: Vec::new(),
            destination: None,
            target_identity: Some(r.target_identity.clone()),
            source_identity: Some(pin.host.clone()),
            receipt_challenge: Some(r.challenge.clone()),
            received: None,
            retired: None,
        };
        let public_key = URL_SAFE_NO_PAD.encode([17; 32]);
        let recipient = model::Recipient {
            signature: signed(
                &target,
                model::recipient_message(&status, &public_key).unwrap(),
            ),
            public_key: public_key.clone(),
        };
        let parts = vec![vec![5; PART as usize + 48], vec![6; 53]];
        let context = model::TransferContext {
            export_id: r.export_id.clone(),
            server_id: pin.server_id.clone(),
            source_host_id: r.source_host_id.clone(),
            source_identity: pin.host.clone(),
            term: pin.term,
            preparation_id: r.preparation_id.clone(),
            target_identity: r.target_identity.clone(),
            target_url: r.target_url.clone(),
            recipient_key: public_key,
            schema_version: 25,
            database_bytes: PART + 5,
            database_sha256: "d".repeat(64),
            runtime: model::HostRuntime {
                name: "Synthetic".into(),
                storage_limit_bytes: 0,
                user_storage_limit_bytes: 0,
                account_provider: None,
                rtc_setup_required: false,
            },
        };
        let mut manifest = model::TransferManifest {
            context,
            parts: parts
                .iter()
                .map(|p| model::TransferPart {
                    bytes: p.len() as u64,
                    sha256: sha(p),
                })
                .collect(),
            signature: String::new(),
        };
        manifest.signature = signed(
            &source,
            model::manifest_message(&manifest.context, &manifest.parts),
        );
        let helper = Helper {
            key: target,
            recipient: recipient.clone(),
            manifest: manifest.clone(),
            received: 0,
        };
        (
            Mock {
                origin,
                pin,
                status,
                manifest,
                recipient,
                parts,
                requested: vec![],
                fail: true,
                receipts: 0,
            },
            helper,
        )
    }
    impl Remote for Mock {
        fn origin(&self) -> &CommandOrigin {
            &self.origin
        }
        fn pin(&self) -> &model::RoutePin {
            &self.pin
        }
        fn json<T: DeserializeOwned>(
            &mut self,
            post: bool,
            path: &str,
            body: &impl Serialize,
            _: u64,
        ) -> Result<T> {
            let value = if !post && path == ROOT {
                json!(self.status)
            } else if path.ends_with("/export") {
                json!(model::ExportStatus::Ready {
                    manifest: self.manifest.clone()
                })
            } else if path.ends_with("/receipt") {
                self.receipts += 1;
                self.status.received = Some(serde_json::from_value(json!(body)).unwrap());
                json!(self.status)
            } else {
                panic!("Unexpected handover mutation {path}")
            };
            serde_json::from_value(value).map_err(|_| Fault::Integrity)
        }
        fn part(&mut self, p: &model::PartRequest) -> Result<Vec<u8>> {
            assert_eq!(p.request.recipient.public_key, self.recipient.public_key);
            self.requested.push(p.index);
            if p.index == 1 && self.fail {
                self.fail = false;
                return Err(Fault::Connection);
            }
            Ok(self.parts[p.index].clone())
        }
    }
    struct Helper {
        key: SigningKey,
        recipient: model::Recipient,
        manifest: model::TransferManifest,
        received: usize,
    }
    impl Companion for Helper {
        fn call(&mut self, command: TargetCommand) -> Result<serde_json::Value> {
            match command {
                TargetCommand::Recipient { .. } => Ok(json!(self.recipient)),
                TargetCommand::Receive { status, cache } => {
                    self.received += 1;
                    assert_eq!(
                        fs::read(cache.join("manifest.json")).unwrap(),
                        serde_json::to_vec(&self.manifest).unwrap()
                    );
                    for (i, part) in self.manifest.parts.iter().enumerate() {
                        assert_eq!(
                            sha(&fs::read(cache.join(format!("{i}.hpke"))).unwrap()),
                            part.sha256
                        );
                    }
                    Ok(
                        json!({"server_id":status.server_id,"preparation_id":self.manifest.context.preparation_id,"database_sha256":self.manifest.context.database_sha256,"activation_allowed":false}),
                    )
                }
                TargetCommand::Receipt { status } => {
                    let c = &self.manifest.context;
                    let body = model::ReceiptBody {
                        server_id: c.server_id.clone(),
                        source_host_id: c.source_host_id.clone(),
                        term: c.term,
                        preparation_id: c.preparation_id.clone(),
                        export_id: c.export_id.clone(),
                        target_identity: c.target_identity.clone(),
                        target_url: c.target_url.clone(),
                        database_sha256: c.database_sha256.clone(),
                        database_bytes: c.database_bytes,
                        manifest_sha256: sha(&serde_json::to_vec(&self.manifest).unwrap()),
                        challenge: status.receipt_challenge.unwrap(),
                    };
                    Ok(json!(model::Receipt {
                        signature: signed(&self.key, model::receipt_message(&body)),
                        body
                    }))
                }
                _ => panic!("No activation or device operations in a download"),
            }
        }
    }
    #[test]
    fn encrypted_download_resumes_missing_parts_and_never_activates() {
        let (mut remote, mut local) = fixture();
        let temp = tempfile::tempdir().unwrap();
        let stop = AtomicBool::new(false);
        assert_eq!(
            run(
                &mut remote,
                &mut local,
                temp.path(),
                Action::Download {
                    retry_export: false
                },
                &stop,
                |_, _, _, _, _, _| {}
            ),
            Err(Fault::Connection)
        );
        assert_eq!(local.received, 0);
        assert_eq!(remote.receipts, 0);
        run(
            &mut remote,
            &mut local,
            temp.path(),
            Action::Download {
                retry_export: false,
            },
            &stop,
            |_, _, _, _, _, _| {},
        )
        .unwrap();
        assert_eq!(remote.requested, [0, 1, 1]);
        assert_eq!(local.received, 1);
        assert_eq!(remote.receipts, 1);
        assert!(remote.status.received.is_some());
        assert!(!remote.status.activation_available);
    }
    #[test]
    fn altered_manifest_wrong_account_and_local_stop_do_not_reach_companion() {
        let (mut remote, mut local) = fixture();
        let temp = tempfile::tempdir().unwrap();
        remote.manifest.context.target_url = "https://other.example".into();
        assert_eq!(
            run(
                &mut remote,
                &mut local,
                temp.path(),
                Action::Download {
                    retry_export: false
                },
                &AtomicBool::new(false),
                |_, _, _, _, _, _| {}
            ),
            Err(Fault::Integrity)
        );
        remote.origin.user_id = uuid::Uuid::new_v4().to_string();
        assert_eq!(
            run(
                &mut remote,
                &mut local,
                temp.path(),
                Action::Download {
                    retry_export: false
                },
                &AtomicBool::new(false),
                |_, _, _, _, _, _| {}
            ),
            Err(Fault::Denied)
        );
        assert_eq!(
            run(
                &mut remote,
                &mut local,
                temp.path(),
                Action::Inspect,
                &AtomicBool::new(true),
                |_, _, _, _, _, _| {}
            ),
            Err(Fault::Cancelled)
        );
        assert!(remote.requested.is_empty());
        assert_eq!(local.received, 0);
    }
    #[test]
    fn setup_code_binds_both_hosts_account_address_and_term() {
        let (remote, local) = fixture();
        let body = model::DestinationBody {
            source: remote.pin.clone(),
            user_id: remote.origin.user_id.clone(),
            target: remote.manifest.context.target_identity.clone(),
            target_url: remote.manifest.context.target_url.clone(),
            private_route: None,
        };
        let mut card = model::DestinationCard {
            signature: signed(&local.key, model::destination_message(&body)),
            body,
        };
        let code = encode_card(&card).unwrap();
        assert!(decode_card(&code, &remote.pin).is_ok());
        let wire=serde_json::to_value(TargetCommand::Plan{source:remote.pin.clone(),
            user_id:remote.origin.user_id.clone(),target_url:card.body.target_url.clone(),private_route:None}).unwrap();
        assert!(wire.get("private_route").is_none(),"legacy HTTPS companions deny unknown fields");
        let wire=serde_json::to_value(model::Offer{server_id:remote.pin.server_id.clone(),
            target_user_id:remote.origin.user_id.clone(),target_host_id:card.body.target.host_id.clone(),
            target_url:card.body.target_url.clone(),destination:None}).unwrap();
        assert!(wire.get("destination").is_none(),"legacy HTTPS sources keep their existing contract");
        let mut pin = remote.pin.clone();
        pin.term += 1;
        assert!(decode_card(&code, &pin).is_err());
        card.body.target_url = "https://other.example".into();
        assert!(encode_card(&card).is_err());
    }
    #[test]
    fn private_setup_codes_bind_the_new_endpoint_and_redact_its_capability() {
        let (remote, local)=fixture();
        let endpoint="a".repeat(64);
        let capability="N".repeat(43);
        assert_eq!(capability.len(),43);
        let body=model::DestinationBody {
            source:remote.pin.clone(),user_id:remote.origin.user_id.clone(),
            target:remote.manifest.context.target_identity.clone(),
            target_url:format!("http://{endpoint}.iroh:4580"),
            private_route:Some(model::IrohDestination{endpoint_id:endpoint,relay:None,token:capability.clone(),scope:None}),
        };
        let sign=|body:model::DestinationBody|model::DestinationCard {
            signature:signed(&local.key,model::destination_message(&body)),body,
        };
        let card=sign(body.clone());
        assert!(encode_card(&card).is_ok());
        assert!(serde_json::to_value(&card).unwrap()["body"]["private_route"].get("scope").is_none(),"legacy cards retain their exact signed serialization");
        let mut scoped=body.clone();
        scoped.private_route.as_mut().unwrap().scope=Some(scoped.target.host_id.clone());
        assert!(encode_card(&sign(scoped.clone())).is_ok());
        scoped.private_route.as_mut().unwrap().scope=Some(uuid::Uuid::new_v4().to_string());
        assert!(encode_card(&sign(scoped)).is_err(),"even a signed card cannot borrow a different target's transport namespace");
        assert!(!format!("{card:?}").contains(&capability));
        for change in 0..6 {
            let mut bad=body.clone();
            match change {
                0=>bad.target_url="http://unrelated.example:4580".into(),
                1=>bad.private_route.as_mut().unwrap().endpoint_id="b".repeat(64),
                2=>bad.private_route.as_mut().unwrap().relay=Some("http://relay.example".into()),
                3=>bad.private_route.as_mut().unwrap().token="short".into(),
                4=>bad.private_route=None,
                _=>bad.target_url.push_str("/api/local"),
            }
            assert!(encode_card(&sign(bad)).is_err(),"mutation {change}");
        }
        let mut altered=card;altered.body.private_route.as_mut().unwrap().token="B".repeat(43);
        assert!(encode_card(&altered).is_err(),"capability changes require the target signature");
    }
    #[test]
    fn setup_never_substitutes_a_different_signed_destination() {
        struct Reply(model::DestinationCard);
        impl Companion for Reply {
            fn call(&mut self,_:TargetCommand)->Result<serde_json::Value>{Ok(serde_json::to_value(&self.0).unwrap())}
        }
        let (mut remote,local)=fixture();let folder=tempfile::tempdir().unwrap();
        for address in ["https://requested.example","https://other.example"] {
            let body=model::DestinationBody{source:remote.pin.clone(),user_id:remote.origin.user_id.clone(),
                target:remote.manifest.context.target_identity.clone(),target_url:address.into(),private_route:None};
            let mut reply=Reply(model::DestinationCard{signature:signed(&local.key,model::destination_message(&body)),body});
            let result=run(&mut remote,&mut reply,folder.path(),Action::Setup{target_url:"https://requested.example/".into()},
                &AtomicBool::new(false),|_,_,_,_,_,_|{});
            assert_eq!(result.is_ok(),address=="https://requested.example");
        }
        assert!(!folder.path().join("relay.sqlite3").exists());
    }
}
