//! Owned destination processes. Media credentials are fresh machine-local
//! environment values, never source credentials, arguments, or a saved recipe.
use crate::{
    handover_client::{direct, Fault},
    network::CommandOrigin,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use relay_protocol::handover as model;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};
use zeroize::Zeroizing;
type Result<T> = std::result::Result<T, Fault>;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recipe {
    pub schema: u32,
    pub origin: CommandOrigin,
    pub status: model::Status,
}

fn random() -> Result<Zeroizing<String>> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).map_err(|_| Fault::Companion)?;
    Ok(Zeroizing::new(URL_SAFE_NO_PAD.encode(bytes)))
}
fn root() -> Result<PathBuf> {
    std::env::var_os("RELAY_PROFILE_DIR")
        .or_else(|| std::env::var_os("RELAY_CONFIG_DIR"))
        .map(PathBuf::from)
        .or_else(|| {
            directories::ProjectDirs::from("org", "Relay", "Relay")
                .map(|v| v.data_local_dir().to_owned())
        })
        .ok_or(Fault::Storage)
}
fn read_record(path: &Path) -> Result<Option<Recipe>> {
    direct(path)?;
    let file = match fs::File::open(path) {
        Ok(v) => v,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(Fault::Storage),
    };
    if !file.metadata().map_err(|_| Fault::Storage)?.is_file() {
        return Err(Fault::Storage);
    }
    let mut bytes = Vec::new();
    file.take(model::CONTROL_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| Fault::Storage)?;
    if bytes.len() as u64 > model::CONTROL_BYTES {
        return Err(Fault::Storage);
    }
    let value: Recipe = serde_json::from_slice(&bytes).map_err(|_| Fault::Storage)?;
    value.validate_received()?;
    if value.status.retired.is_some() {
        value.validate()?;
    }
    Ok(Some(value))
}
fn read_recipe(path: &Path) -> Result<Option<Recipe>> {
    let Some(value) = read_record(path)? else {
        return Ok(None);
    };
    let data = crate::handover_client::directory(&value.origin)?;
    resolve_record(value, &data).map(Some)
}
fn resolve_record(mut value: Recipe, data: &Path) -> Result<Recipe> {
    if value.status.retired.is_none() {
        value.status =
            delivered_status(data, &value.origin, &value.status)?.ok_or(Fault::WaitingForGrant)?;
    }
    value.validate()?;
    Ok(value)
}
impl Recipe {
    // A saved standby scope is only a recovery hint. It never authorizes a
    // writer: read_recipe must find and verify the exact source grant first.
    fn validate_received(&self) -> Result<()> {
        let s = &self.status;
        crate::handover_client::validate_destination_status(s)?;
        let p = s.preparation.as_ref().ok_or(Fault::Binding)?;
        let r = s.received.as_ref().ok_or(Fault::Binding)?;
        let b = &r.body;
        let source = s.source_identity.as_ref().ok_or(Fault::Binding)?;
        if self.schema != 1
            || self.origin.server_id != s.server_id
            || self.origin.user_id != p.target_user_id
            || uuid::Uuid::parse_str(&self.origin.user_id).is_err()
            || p.phase != model::Phase::Frozen
            || s.writes_allowed
            || (s.retired.is_none() && s.activation_available)
            || crate::community::account_destination(&self.origin.url).is_err()
            || !uuid::Uuid::parse_str(&p.id).is_ok_and(|id| id.to_string() == p.id)
            || b.server_id != s.server_id
            || b.preparation_id != p.id
            || b.target_url != p.target_url
            || crate::host_route::safe_destination(&p.target_url).is_err()
            || Some(&b.target_identity) != s.target_identity.as_ref()
            || s.receipt_challenge.as_deref() != Some(b.challenge.as_str())
            || b.source_host_id != s.source_host_id
            || b.term != s.term
            || b.target_identity.host_id != p.target_host_id
            || !crate::host_route::valid_pin(&model::RoutePin {
                server_id: s.server_id.clone(),
                host: source.clone(),
                term: s.term,
            })
            || !crate::host_route::verify(
                &b.target_identity,
                &r.signature,
                &model::receipt_message(b),
            )
        {
            return Err(Fault::Binding);
        }
        Ok(())
    }
    pub fn validate(&self) -> Result<()> {
        self.validate_received()?;
        let s = &self.status;
        let g = s.retired.as_ref().ok_or(Fault::Binding)?;
        if s.received.as_ref() != Some(&g.receipt)
            || Some(&g.source_identity) != s.source_identity.as_ref()
            || !crate::host_route::committed_history(g,&s.history)
            || !crate::host_route::valid_grant(
                &model::RoutePin {
                    server_id: s.server_id.clone(),
                    host: g.source_identity.clone(),
                    term: s.term,
                },
                g,
            )
        {
            return Err(Fault::Binding);
        }
        Ok(())
    }
    pub fn data(&self) -> Result<PathBuf> {
        self.validate()?;
        crate::handover_client::directory(&self.origin)
    }
    pub fn remember(&self) -> Result<()> {
        self.write(&root()?.join("received-host.json"))
    }
    fn write(&self, selected: &Path) -> Result<()> {
        self.validate()?;
        self.write_record(selected)
    }
    fn write_record(&self, selected: &Path) -> Result<()> {
        self.validate_received()?;
        if self.status.retired.is_some() {
            self.validate()?;
        }
        let base = selected.parent().ok_or(Fault::Storage)?;
        direct(base)?;
        fs::create_dir_all(base).map_err(|_| Fault::Storage)?;
        let _ = read_record(selected)?;
        let pending = base.join(format!(".received-host-{}.pending", uuid::Uuid::new_v4()));
        direct(&pending)?;
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&pending).map_err(|_| Fault::Storage)?;
        file.write_all(&serde_json::to_vec(self).map_err(|_| Fault::Storage)?)
            .and_then(|_| file.sync_all())
            .map_err(|_| Fault::Storage)?;
        drop(file);
        direct(&selected)?;
        fs::rename(&pending, &selected).map_err(|_| Fault::Storage)?;
        Ok(())
    }
    // Preserve the irreversible source grant before local activation. No source
    // bearer, media credentials or private key is written into either record.
    pub fn checkpoint(&self) -> Result<()> {
        self.write(&self.data()?.join("granted-host.json"))?;
        self.remember()
    }
}
pub fn remembered() -> Result<Option<Recipe>> {
    read_recipe(&root()?.join("received-host.json"))
}
pub fn recover(origin: &CommandOrigin, pin: &model::RoutePin) -> Result<Option<Recipe>> {
    let data = crate::handover_client::directory(origin)?;
    recover_at(origin, pin, &data)
}
pub(crate) fn recover_at(origin: &CommandOrigin, pin: &model::RoutePin, data: &Path) -> Result<Option<Recipe>> {
    let mut recipe = read_record(&data.join("granted-host.json"))?;
    if recipe.is_none() {
        recipe = read_record(&data.join("standby-host.json"))?;
    }
    if recipe.as_ref().is_some_and(|r| {
        r.origin != *origin
            || r.status.source_identity.as_ref() != Some(&pin.host)
            || r.status.term != pin.term
            || r.status.server_id != pin.server_id
    }) {
        return Err(Fault::Binding);
    }
    if let Some(value) = recipe.as_mut() {
        if value.status.retired.is_none() {
            let Some(status) = delivered_status(&data, origin, &value.status)? else {
                return Ok(None);
            };
            value.status = status;
        }
        value.validate()?;
    }
    Ok(recipe)
}

fn remember_standby(origin: &CommandOrigin, status: &model::Status, data: &Path) -> Result<()> {
    if status.retired.is_some() {
        return Err(Fault::Binding);
    }
    let seed = Recipe {
        schema: 1,
        origin: origin.clone(),
        status: status.clone(),
    };
    seed.validate_received()?;
    if data != crate::handover_client::directory(origin)? {
        return Err(Fault::Binding);
    }
    // Commit both before starting any process or exposing the standby route.
    // The selected record also prevents fallback to an unrelated default host.
    seed.write_record(&data.join("standby-host.json"))?;
    seed.write_record(&root()?.join("received-host.json"))
}

// Called only with a fresh authenticated response from the pinned source.
// Clearing the selected hint preserves every downloaded file and key. A local
// durable grant always wins over an alleged cancellation from the old source.
pub fn observe_source(
    origin: &CommandOrigin,
    pin: &model::RoutePin,
    status: &model::Status,
) -> Result<()> {
    if !status.writes_allowed || status.preparation.is_some() || status.retired.is_some() {
        return Ok(());
    }
    let selected = root()?.join("received-host.json");
    // A corrupt selection remains a local recovery error; it must not prevent
    // inspecting a different community or be replaced by this observation.
    let Some(seed) = read_record(&selected).ok().flatten() else {
        return Ok(());
    };
    if seed.origin != *origin || seed.status.retired.is_some() {
        return Ok(());
    }
    clear_cancelled(
        &selected,
        &crate::handover_client::directory(origin)?,
        origin,
        pin,
        status,
    )
}
fn clear_cancelled(
    selected: &Path,
    data: &Path,
    origin: &CommandOrigin,
    pin: &model::RoutePin,
    status: &model::Status,
) -> Result<()> {
    let Some(seed) = read_record(selected)? else {
        return Ok(());
    };
    if seed.status.retired.is_some()
        || seed.origin != *origin
        || seed.status.source_identity.as_ref() != Some(&pin.host)
        || seed.status.term != pin.term
        || pin.server_id != origin.server_id
        || status.source_identity.as_ref() != Some(&pin.host)
        || status.server_id != origin.server_id
        || status.term != pin.term
        || status.source_host_id != seed.status.source_host_id
        || !status.writes_allowed
        || status.preparation.is_some()
        || status.retired.is_some()
    {
        return Ok(());
    }
    if delivered_status(data, origin, &seed.status)?.is_some() {
        return Ok(());
    }
    direct(selected)?;
    fs::remove_file(selected).map_err(|_| Fault::Storage)
}

struct OwnedChild(Child);
impl Drop for OwnedChild {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}

#[cfg(windows)]
pub(crate) struct ProcessGroup(windows::Win32::Foundation::HANDLE);
#[cfg(windows)]
unsafe impl Send for ProcessGroup {}
#[cfg(windows)]
impl ProcessGroup {
    pub(crate) fn new() -> Result<Self> {
        use windows::Win32::System::JobObjects::*;
        let handle = unsafe { CreateJobObjectW(None, None) }.map_err(|_| Fault::Companion)?;
        let group = Self(handle);
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        unsafe {
            SetInformationJobObject(
                group.0,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const _,
                std::mem::size_of_val(&info) as u32,
            )
        }
        .map_err(|_| Fault::Companion)?;
        Ok(group)
    }
    pub(crate) fn add(&self, child: &Child) -> Result<()> {
        use std::os::windows::io::AsRawHandle;
        unsafe {
            windows::Win32::System::JobObjects::AssignProcessToJobObject(
                self.0,
                windows::Win32::Foundation::HANDLE(child.as_raw_handle()),
            )
        }
        .map_err(|_| Fault::Companion)
    }
}
#[cfg(windows)]
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        let _ = unsafe { windows::Win32::Foundation::CloseHandle(self.0) };
    }
}
#[cfg(not(windows))]
pub(crate) struct ProcessGroup;
#[cfg(not(windows))]
impl ProcessGroup {
    pub(crate) fn new() -> Result<Self> {
        Ok(Self)
    }
    pub(crate) fn add(&self, _: &Child) -> Result<()> {
        Ok(())
    }
}

#[derive(Deserialize)]
struct Startup {
    mode: String,
    server_id: String,
    target_host_id: String,
    http_port: u16,
    voice_port: u16,
    #[serde(default)]
    network_peer_port: Option<u16>,
}
pub struct Runtime {
    server: OwnedChild,
    video: OwnedChild,
    _group: ProcessGroup,
    control: Zeroizing<String>,
    http: reqwest::blocking::Client,
    origin: CommandOrigin,
    status: model::Status,
    data: PathBuf,
    active: bool,
    route: Option<crate::iroh_route::HostLease>,
}
fn hidden(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
}
// Keep the virtual candidate on the loopback UDP mux used by the bridge.
// Rewriting multiple interfaces to one candidate can select the wrong mux.
fn media_config(key: &str, secret: &str) -> Zeroizing<String> {
    Zeroizing::new(format!("port: 7880\nbind_addresses: [\"127.0.0.1\"]\nrtc:\n  tcp_port: 7881\n  udp_port: 7882\n  use_external_ip: false\n  node_ip: 100.127.255.254\n  ips:\n    includes: [\"127.0.0.1/32\"]\n  enable_loopback_candidate: true\n  use_ice_lite: true\nkeys:\n  {key}: {secret}\nlogging:\n  level: error\n"))
}
fn local_client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_millis(500))
        .timeout(Duration::from_secs(2))
        .build()
        .map_err(|_| Fault::Companion)
}
fn delivered_status(
    data: &Path,
    origin: &CommandOrigin,
    status: &model::Status,
) -> Result<Option<model::Status>> {
    let p = status.preparation.as_ref().ok_or(Fault::Binding)?;
    if !uuid::Uuid::parse_str(&p.id).is_ok_and(|id| id.to_string() == p.id) {
        return Err(Fault::Binding);
    }
    let path = data
        .join("handover-received")
        .join(&p.id)
        .join("activation-intent.json");
    direct(&path)?;
    let file = match fs::File::open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(Fault::Storage),
    };
    let mut bytes = Vec::new();
    file.take(16 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| Fault::Storage)?;
    if bytes.len() > 16 * 1024 {
        return Err(Fault::Integrity);
    }
    let grant: model::ActivationGrant =
        serde_json::from_slice(&bytes).map_err(|_| Fault::Integrity)?;
    if status.received.as_ref() != Some(&grant.receipt) {
        return Err(Fault::Binding);
    }
    let mut next = status.clone();
    next.retired = Some(grant);
    next.activation_available = true;
    next.writes_allowed = false;
    Recipe {
        schema: 1,
        origin: origin.clone(),
        status: next.clone(),
    }
    .validate()?;
    Ok(Some(next))
}

fn json_reply<T: serde::de::DeserializeOwned>(response: reqwest::blocking::Response) -> Result<T> {
    if !response.status().is_success() {
        return Err(Fault::Companion);
    }
    let mut bytes = Vec::new();
    response
        .take(16 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| Fault::Companion)?;
    if bytes.len() > 16 * 1024 {
        return Err(Fault::Integrity);
    }
    serde_json::from_slice(&bytes).map_err(|_| Fault::Integrity)
}
fn alive(child: &mut OwnedChild) -> Result<()> {
    if child.0.try_wait().map_err(|_| Fault::Companion)?.is_some() {
        Err(Fault::Companion)
    } else {
        Ok(())
    }
}
fn cancelled(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Acquire) {
        Err(Fault::Cancelled)
    } else {
        Ok(())
    }
}

impl Runtime {
    pub fn start(
        origin: CommandOrigin,
        status: model::Status,
        data: PathBuf,
        cancel: &AtomicBool,
    ) -> Result<Self> {
        cancelled(cancel)?;
        let p = status.preparation.as_ref().ok_or(Fault::Binding)?;
        if p.target_user_id != origin.user_id
            || status.server_id != origin.server_id
            || status.received.is_none()
        {
            return Err(Fault::Binding);
        }
        crate::handover_client::validate_destination_status(&status)?;
        let destination =
            crate::host_route::safe_destination(&p.target_url).map_err(|_| Fault::Route)?;
        let target = url::Url::parse(&destination).map_err(|_| Fault::Binding)?;
        let private=crate::iroh_route::is_iroh(&destination);
        if target.scheme() != "https" && !private {
            return Err(Fault::Route);
        }
        direct(&data)?;
        let server = crate::local_host::server_binary().ok_or(Fault::Companion)?;
        direct(&server)?;
        let sfu = std::env::var_os("RELAY_SFU_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|| server.parent().unwrap().join("livekit-server.exe"));
        direct(&sfu)?;
        if !sfu.is_file() {
            return Err(Fault::Companion);
        }
        if status.retired.is_none() {
            remember_standby(&origin, &status, &data)?;
        }
        let group = ProcessGroup::new()?;
        let key = random()?;
        let secret = random()?;
        let control = random()?;
        let ingress=if private {Some(random()?)}else{None};
        let mut video_command = Command::new(sfu);
        hidden(&mut video_command);
        video_command
            .env("LIVEKIT_CONFIG", media_config(&key, &secret).as_str())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let video = OwnedChild(video_command.spawn().map_err(|_| Fault::Companion)?);
        group.add(&video.0)?;
        let http = local_client()?;
        let mut result = Self {
            server: Self::start_server(
                &server, &data, &status, &key, &secret, &control, ingress.as_ref().map(|v|v.as_str()), cancel, &group, &http,
            )?,
            video,
            _group: group,
            control,
            http,
            origin,
            status,
            data,
            active: false,
            route: None,
        };
        alive(&mut result.video)?;
        if let Some(ingress)=ingress {
            let route=result.status.destination.as_ref().and_then(|c|c.body.private_route.as_ref()).ok_or(Fault::Binding)?;
            let expected=crate::iroh_route::SavedIroh{endpoint_id:route.endpoint_id.clone(),relay:route.relay.clone(),token:route.token.clone()};
            result.route=Some(crate::iroh_route::HostLease::open(4582,&ingress,&result.status.server_id,&expected,
                route.scope.as_deref(),result.status.retired.is_some()).map_err(|_|Fault::Route)?);
        }
        Ok(result)
    }
    fn start_server(
        exe: &Path,
        data: &Path,
        status: &model::Status,
        key: &str,
        secret: &str,
        control: &str,
        ingress: Option<&str>,
        cancel: &AtomicBool,
        group: &ProcessGroup,
        http: &reqwest::blocking::Client,
    ) -> Result<OwnedChild> {
        let until = Instant::now() + Duration::from_secs(10);
        // Native standby subsequently verifies these exact credentials; this
        // bounded health wait alone never constitutes a positive handover proof.
        while !http
            .get("http://127.0.0.1:7880/")
            .send()
            .is_ok_and(|r| r.status().is_success())
        {
            cancelled(cancel)?;
            if Instant::now() > until {
                return Err(Fault::Companion);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let p = status.preparation.as_ref().ok_or(Fault::Binding)?;
        let mut public = url::Url::parse(&p.target_url).map_err(|_| Fault::Binding)?;
        let scheme=if crate::iroh_route::is_iroh(&p.target_url){"ws"}else{"wss"};
        public.set_scheme(scheme).map_err(|_| Fault::Binding)?;
        let mut command = Command::new(exe);
        hidden(&mut command);
        command
            .arg("--handover-ready")
            .arg(data)
            .args([
                "--bind",
                "127.0.0.1",
                "--port",
                "4580",
                "--udp-port",
                "4581",
            ])
            .env("RELAY_HANDOVER_CONTROL_TOKEN", control)
            .env("RELAY_RTC_URL", public.as_str())
            .env("RELAY_RTC_API_URL", "http://127.0.0.1:7880")
            .env("RELAY_RTC_API_KEY", key)
            .env("RELAY_RTC_API_SECRET", secret)
            .env("RELAY_RTC_MEDIA_UDP_PORT", "7882")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if let Some(ingress)=ingress{
            command.args(["--network-peer-port","4582"]).env("RELAY_HANDOVER_NETWORK_TOKEN",ingress);
        }
        let mut child = OwnedChild(command.spawn().map_err(|_| Fault::Companion)?);
        group.add(&child.0)?;
        child
            .0
            .stdin
            .take()
            .ok_or(Fault::Companion)?
            .write_all(&serde_json::to_vec(status).map_err(|_| Fault::Binding)?)
            .map_err(|_| Fault::Companion)?;
        let output = child.0.stdout.take().ok_or(Fault::Companion)?;
        let (send, receive) = std::sync::mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("relay-host-output".into())
            .spawn(move || {
                let mut reader = BufReader::new(output);
                let mut first = String::new();
                let read = (&mut reader).take(16 * 1024 + 1).read_line(&mut first);
                let _ = send.send(read.map(|_| first));
                let _ = std::io::copy(&mut reader, &mut std::io::sink());
            })
            .map_err(|_| Fault::Companion)?;
        let until = Instant::now() + Duration::from_secs(120);
        let first = loop {
            cancelled(cancel)?;
            alive(&mut child)?;
            if Instant::now() > until {
                return Err(Fault::Companion);
            }
            match receive.recv_timeout(Duration::from_millis(50)) {
                Ok(v) => break v.map_err(|_| Fault::Companion)?,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(_) => return Err(Fault::Companion),
            }
        };
        if first.len() > 16 * 1024 {
            return Err(Fault::Integrity);
        }
        let report: Startup = serde_json::from_str(&first).map_err(|_| Fault::Companion)?;
        if !matches!(
            report.mode.as_str(),
            "handover-standby" | "handover-activating"
        ) || report.server_id != status.server_id
            || Some(report.target_host_id.as_str())
                != status.target_identity.as_ref().map(|i| i.host_id.as_str())
            || report.http_port != 4580
            || report.voice_port != 4581
            || report.network_peer_port != ingress.map(|_|4582)
        {
            return Err(Fault::Binding);
        }
        Ok(child)
    }
    pub fn activate(&mut self, status: &model::Status) -> Result<()> {
        alive(&mut self.server)?;
        alive(&mut self.video)?;
        let recipe = Recipe {
            schema: 1,
            origin: self.origin.clone(),
            status: status.clone(),
        };
        recipe.validate()?;
        if self.status.preparation.as_ref().map(|p| &p.id)
            != status.preparation.as_ref().map(|p| &p.id)
        {
            return Err(Fault::Binding);
        }
        recipe.checkpoint()?;
        self.status = status.clone();
        let value: serde_json::Value = json_reply(
            self.http
                .post("http://127.0.0.1:4580/api/local/handover/activate")
                .bearer_auth(self.control.as_str())
                .json(status.retired.as_ref().ok_or(Fault::Binding)?)
                .send()
                .map_err(|_| Fault::Companion)?,
        )?;
        if value["activating"] != true {
            return Err(Fault::Binding);
        }
        Ok(())
    }
    pub fn check_active(&mut self, status: &model::Status) -> Result<bool> {
        alive(&mut self.server)?;
        alive(&mut self.video)?;
        let Some(grant) = &status.retired else {
            return Ok(false);
        };
        let challenge = random()?;
        let reply = self
            .http
            .post("http://127.0.0.1:4580/api/hosting/handover/route")
            .json(&model::RouteChallenge {
                challenge: challenge.to_string(),
            })
            .send();
        let Ok(reply) = reply else {
            return Ok(false);
        };
        if reply.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(false);
        }
        let proof: model::RouteProof = json_reply(reply)?;
        let pin = model::RoutePin {
            server_id: status.server_id.clone(),
            host: grant.receipt.body.target_identity.clone(),
            term: grant.next_term,
        };
        if proof.state.pin != pin
            || proof.state.challenge != challenge.as_str()
            || !proof.state.writes_allowed
            || proof.state.moved.is_some()
            || !crate::host_route::verify(
                &pin.host,
                &proof.signature,
                &model::route_message(&proof.state),
            )
        {
            return Err(Fault::Binding);
        }
        self.status = status.clone();
        self.active = true;
        Ok(true)
    }
    pub fn recipe(&self) -> Result<Recipe> {
        if !self.active {
            return Err(Fault::Binding);
        }
        let r = Recipe {
            schema: 1,
            origin: self.origin.clone(),
            status: self.status.clone(),
        };
        r.validate()?;
        Ok(r)
    }
    pub(crate) fn is_active(&self)->bool {self.active}

    pub(crate) fn locator_task(&self, service:&str) -> Result<LocatorTask> {
        if !self.active {return Err(Fault::Binding);}
        let grant=self.status.retired.as_ref().ok_or(Fault::Binding)?;
        let current=model::RoutePin{server_id:self.status.server_id.clone(),
            host:grant.receipt.body.target_identity.clone(),term:grant.next_term};
        let mut history=self.status.history.clone();
        history.push(grant.clone());
        let url=crate::host_route::safe_destination(&grant.receipt.body.target_url).map_err(|_|Fault::Route)?;
        let iroh=if crate::iroh_route::is_iroh(&url) {
            let owned=self.route.as_ref().and_then(|lease|lease.invitation()).ok_or(Fault::Route)?;
            let card=self.status.destination.as_ref().and_then(|c|c.body.private_route.as_ref()).ok_or(Fault::Binding)?;
            if owned.endpoint_id!=card.endpoint_id || owned.address()!=url {return Err(Fault::Binding);}
            Some(model::IrohDestination{scope:Some(current.host.host_id.clone()),endpoint_id:owned.endpoint_id,
                relay:owned.relay,token:owned.token})
        } else {None};
        let destination=relay_protocol::host_locator::Destination{url,iroh};
        let renewal=crate::host_locator_renewal::Renewal::new(self.data.clone(),current.clone(),history.clone(),
            service,&destination).map_err(|_|Fault::Storage)?;
        let sessions=crate::host_locator_renewal::Renewal::new_session(self.data.clone(),current.clone(),history,
            service,&destination).map_err(|_|Fault::Storage)?;
        Ok(LocatorTask{renewal,sessions,http:self.http.clone(),control:self.control.clone(),current,destination})
    }

    pub fn server_id(&self) -> &str {
        &self.status.server_id
    }
    pub fn matches_connection(&self,server_id:&str,url:&str)->bool {
        self.active && self.status.server_id == server_id
            && (crate::local_host::is_local_url(url) || self.status.preparation.as_ref()
                .is_some_and(|p| p.target_url == url.trim_end_matches('/')))
    }
    pub fn delivered_grant(&self) -> Result<Option<model::Status>> {
        delivered_status(&self.data, &self.origin, &self.status)
    }
    pub fn is_alive(&mut self) -> bool {
        alive(&mut self.server).is_ok() && alive(&mut self.video).is_ok()
            && self.route.as_ref().is_none_or(|r|r.alive()!=Some(false))
    }
    pub fn shutdown(&mut self) {
        // Detach only our own ingress lease; an old cleanup cannot stop a
        // replacement host or the helper's unrelated outgoing calls.
        self.route.take();
        // A public signed grant may have arrived just before a user stops this
        // job or changes accounts. Preserve its restart recipe before dropping
        // our owned children, without fetching any account credential.
        if let Ok(Some(status)) = self.delivered_grant() {
            let _ = Recipe {
                schema: 1,
                origin: self.origin.clone(),
                status,
            }
            .checkpoint();
        }
        if !std::mem::replace(&mut self.active, false) || alive(&mut self.server).is_err() {
            return;
        }
        let Some(p) = &self.status.preparation else {
            return;
        };
        let data = self.data.join("handover-received").join(&p.id);
        let path = data.join("server-runtime.json");
        if direct(&path).is_err() {
            return;
        }
        let Ok(record) = crate::local_host::read_record(&path) else {
            return;
        };
        if record["server_id"].as_str() != Some(self.status.server_id.as_str())
            || record["pid"].as_u64() != Some(self.server.0.id() as u64)
        {
            return;
        }
        let Some(token) = record["shutdown_token"].as_str() else {
            return;
        };
        let _=self.http.post("http://127.0.0.1:4580/api/local/shutdown").bearer_auth(token).json(&serde_json::json!({"instance_id":record["instance_id"],"confirm_server_id":self.status.server_id})).send();
        let until = Instant::now() + Duration::from_secs(2);
        while Instant::now() < until {
            if alive(&mut self.server).is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(40));
        }
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn recipe() -> Recipe {
        let (_, _, pin, grant) = crate::host_route::tests::fixture();
        let origin = CommandOrigin {
            url: "https://source.example".into(),
            server_id: pin.server_id.clone(),
            user_id: uuid::Uuid::new_v4().to_string(),
        };
        let b = &grant.receipt.body;
        Recipe {
            schema: 1,
            origin: origin.clone(),
            status: model::Status {
                server_id: pin.server_id,
                source_host_id: b.source_host_id.clone(),
                term: pin.term,
                writes_allowed: false,
                preparation: Some(model::Preparation {
                    id: b.preparation_id.clone(),
                    target_user_id: origin.user_id,
                    target_host_id: b.target_identity.host_id.clone(),
                    target_url: b.target_url.clone(),
                    offered_by: uuid::Uuid::new_v4().to_string(),
                    phase: model::Phase::Frozen,
                    updated_ms: 0,
                }),
                activation_available: false,
                history: Vec::new(),
                destination: None,
                target_identity: Some(b.target_identity.clone()),
                source_identity: Some(pin.host),
                receipt_challenge: Some(b.challenge.clone()),
                received: Some(grant.receipt.clone()),
                retired: Some(grant),
            },
        }
    }

    #[test]
    fn restart_requires_the_signed_grant_and_exact_received_image() {
        let good = recipe();
        good.validate().unwrap();
        // The database marker is deliberately distinct from the signing ID.
        assert_ne!(
            good.status.source_host_id,
            good.status.source_identity.as_ref().unwrap().host_id
        );
        for change in 0..7 {
            let mut bad = good.clone();
            match change {
                0 => bad.status.retired = None,
                1 => bad.status.received.as_mut().unwrap().body.database_sha256 = "c".repeat(64),
                2 => bad.status.retired.as_mut().unwrap().next_term += 1,
                3 => {
                    bad.status.preparation.as_mut().unwrap().target_url =
                        "https://unrelated.example".into()
                }
                4 => bad.origin.user_id = uuid::Uuid::new_v4().to_string(),
                5 => bad.status.writes_allowed = true,
                _ => bad.status.receipt_challenge = Some("different".into()),
            }
            assert!(bad.validate().is_err(), "mutation {change}");
        }
    }

    #[test]
    fn delivered_source_grants_can_be_recovered_without_the_old_host() {
        let good = recipe();
        let mut waiting = good.status.clone();
        let grant = waiting.retired.take().unwrap();
        let folder = tempfile::tempdir().unwrap();
        assert!(delivered_status(folder.path(), &good.origin, &waiting)
            .unwrap()
            .is_none());
        let target = folder
            .path()
            .join("handover-received")
            .join(&waiting.preparation.as_ref().unwrap().id);
        fs::create_dir_all(&target).unwrap();
        let path = target.join("activation-intent.json");
        fs::write(&path, serde_json::to_vec(&grant).unwrap()).unwrap();
        let found = delivered_status(folder.path(), &good.origin, &waiting)
            .unwrap()
            .unwrap();
        assert_eq!(found.retired, Some(grant.clone()));
        assert!(found.activation_available && !found.writes_allowed);
        for raw in [b"broken".to_vec(), {
            let mut bad = grant.clone();
            bad.next_term += 1;
            serde_json::to_vec(&bad).unwrap()
        }] {
            fs::write(&path, &raw).unwrap();
            assert!(delivered_status(folder.path(), &good.origin, &waiting).is_err());
            assert_eq!(fs::read(&path).unwrap(), raw);
        }
    }

    #[test]
    fn restart_record_is_bounded_and_never_overwrites_corrupt_or_unrelated_data() {
        let folder = tempfile::tempdir().unwrap();
        let selected = folder.path().join("host.json");
        let good = recipe();
        good.write(&selected).unwrap();
        let saved = read_recipe(&selected).unwrap().unwrap();
        assert_eq!(saved.status.retired, good.status.retired);
        assert_eq!(saved.origin, good.origin);
        let serialized = serde_json::to_value(saved).unwrap();
        assert_eq!(serialized.as_object().unwrap().len(), 3);
        assert_eq!(serialized["origin"].as_object().unwrap().len(), 3);
        // Existing unreadable state is a recovery problem, never a request to
        // fall back to the old default server or replace a stranger's record.
        for original in [b"not our record".to_vec(), vec![b' '; model::CONTROL_BYTES as usize + 1]] {
            fs::write(&selected, &original).unwrap();
            assert!(good.write(&selected).is_err());
            assert_eq!(fs::read(&selected).unwrap(), original);
        }
    }

    #[test]
    fn a_cold_standby_restart_requires_the_exact_durable_grant() {
        let good = recipe();
        let mut seed = good.clone();
        seed.status.retired = None;
        let folder = tempfile::tempdir().unwrap();
        let selected = folder.path().join("received-host.json");
        seed.write_record(&selected).unwrap();
        let cold = || read_record(&selected).unwrap().unwrap();
        assert!(matches!(
            resolve_record(cold(), folder.path()),
            Err(Fault::WaitingForGrant)
        ));
        assert!(
            cold().validate().is_err(),
            "a pending scope cannot authorize restore"
        );
        let target = folder
            .path()
            .join("handover-received")
            .join(&seed.status.preparation.as_ref().unwrap().id);
        fs::create_dir_all(&target).unwrap();
        let intent = target.join("activation-intent.json");
        let before = fs::read(&selected).unwrap();
        let wrong = recipe().status.retired.unwrap();
        fs::write(&intent, serde_json::to_vec(&wrong).unwrap()).unwrap();
        assert!(resolve_record(cold(), folder.path()).is_err());
        fs::write(&intent, b"interrupted or damaged grant").unwrap();
        assert!(resolve_record(cold(), folder.path()).is_err());
        assert_eq!(fs::read(&selected).unwrap(), before);
        fs::write(&intent, serde_json::to_vec(&good.status.retired).unwrap()).unwrap();
        let restored = resolve_record(cold(), folder.path()).unwrap();
        restored.validate().unwrap();
        assert_eq!(restored.status.retired, good.status.retired);
        // Upgrade the same selected standby record after a cold restart.
        restored.write(&selected).unwrap();
        assert!(read_record(&selected)
            .unwrap()
            .unwrap()
            .status
            .retired
            .is_some());
    }

    #[test]
    fn cancelling_a_pending_selection_preserves_data_and_never_overrides_a_grant() {
        let good = recipe();
        let mut seed = good.clone();
        seed.status.retired = None;
        let folder = tempfile::tempdir().unwrap();
        let selected = folder.path().join("received-host.json");
        seed.write_record(&selected).unwrap();
        let database = folder.path().join("kept-database");
        fs::write(&database, b"existing database and keys").unwrap();
        let pin = model::RoutePin {
            server_id: good.status.server_id.clone(),
            host: good.status.source_identity.clone().unwrap(),
            term: good.status.term,
        };
        let mut resumed = seed.status.clone();
        resumed.preparation = None;
        resumed.writes_allowed = true;
        let mut wrong = pin.clone();
        wrong.term += 1;
        clear_cancelled(&selected, folder.path(), &seed.origin, &wrong, &resumed).unwrap();
        assert!(selected.exists());
        let target = folder
            .path()
            .join("handover-received")
            .join(&seed.status.preparation.as_ref().unwrap().id);
        fs::create_dir_all(&target).unwrap();
        let intent = target.join("activation-intent.json");
        fs::write(&intent, serde_json::to_vec(&good.status.retired).unwrap()).unwrap();
        clear_cancelled(&selected, folder.path(), &seed.origin, &pin, &resumed).unwrap();
        assert!(
            selected.exists(),
            "the durable grant wins over a stale source response"
        );
        fs::remove_file(&intent).unwrap();
        clear_cancelled(&selected, folder.path(), &seed.origin, &pin, &resumed).unwrap();
        assert!(!selected.exists());
        assert_eq!(fs::read(&database).unwrap(), b"existing database and keys");
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "requires the explicitly supplied bundled SFU; no backend/source/target process"]
    fn owned_sfu_accepts_fresh_credentials_and_exits_with_its_process_group() {
        use hmac::{Hmac, Mac};
        use sha2::Sha256;
        let exe = PathBuf::from(std::env::var_os("RELAY_TEST_SFU_ONLY").expect("owned SFU path"));
        direct(&exe).unwrap();
        let http_port = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = http_port.local_addr().unwrap().port();
        let rtc_port = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let tcp = rtc_port.local_addr().unwrap().port();
        let udp_port = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let udp = udp_port.local_addr().unwrap().port();
        let key = random().unwrap();
        let secret = random().unwrap();
        let config = Zeroizing::new(
            media_config(&key, &secret)
                .replace("port: 7880", &format!("port: {port}"))
                .replace("tcp_port: 7881", &format!("tcp_port: {tcp}"))
                .replace("udp_port: 7882", &format!("udp_port: {udp}")),
        );
        let group = ProcessGroup::new().unwrap();
        let mut command = Command::new(exe);
        hidden(&mut command);
        command
            .env("LIVEKIT_CONFIG", config.as_str())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        drop((http_port, rtc_port, udp_port));
        let mut child = OwnedChild(command.spawn().unwrap());
        group.add(&child.0).unwrap();
        let http = local_client().unwrap();
        let url = format!("http://127.0.0.1:{port}");
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            alive(&mut child).unwrap();
            if http.get(&url).send().is_ok_and(|r| r.status().is_success()) {
                break;
            }
            assert!(Instant::now() < until, "owned SFU did not become healthy");
            std::thread::sleep(Duration::from_millis(50));
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let payload = serde_json::json!({"iss":key.as_str(),"nbf":now-1,"exp":now+30,"video":{"roomList":true}});
        let signing = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(br#"{"alg":"HS256","typ":"JWT"}"#),
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap())
        );
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(signing.as_bytes());
        let bearer = Zeroizing::new(format!(
            "{signing}.{}",
            URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
        ));
        let endpoint = format!("{url}/twirp/livekit.RoomService/ListRooms");
        assert_eq!(
            http.post(&endpoint)
                .bearer_auth("wrong-owned-fixture")
                .json(&serde_json::json!({}))
                .send()
                .unwrap()
                .status(),
            reqwest::StatusCode::UNAUTHORIZED
        );
        let rooms: serde_json::Value = json_reply(
            http.post(endpoint)
                .bearer_auth(bearer.as_str())
                .json(&serde_json::json!({}))
                .send()
                .unwrap(),
        )
        .unwrap();
        assert!(rooms["rooms"].as_array().is_none_or(|r| r.is_empty()));
        drop(group);
        let until = Instant::now() + Duration::from_secs(3);
        while child.0.try_wait().unwrap().is_none() {
            assert!(Instant::now() < until, "owned process survived its job");
            std::thread::sleep(Duration::from_millis(40));
        }
    }
}

/// The only authority carried to the publisher is this owned process's local
/// control secret. It is used on loopback only; it never reaches the directory.
pub(crate) struct LocatorTask {
    renewal:crate::host_locator_renewal::Renewal,
    sessions:crate::host_locator_renewal::Renewal,
    http:reqwest::blocking::Client,
    control:Zeroizing<String>,
    current:model::RoutePin,
    destination:relay_protocol::host_locator::Destination,
}
impl LocatorTask {
    pub(crate) fn advance(&self,queue:&mut crate::host_locator_outbox::Outbox,now_ms:u64)
        -> std::result::Result<crate::host_locator_renewal::Advance,String> {
        use crate::host_locator_renewal::Advance;
        use relay_protocol::host_locator as locator;
        // Remembered members are renewed first; neither walk skips queued pages.
        match self.walk(&self.sessions,locator::LOCAL_SESSION_PAGE_PATH,queue,now_ms)? {
            Advance::Complete=>self.walk(&self.renewal,locator::LOCAL_PAGE_PATH,queue,now_ms),
            progress=>Ok(progress),
        }
    }
    fn walk(&self,renewal:&crate::host_locator_renewal::Renewal,path:&str,
        queue:&mut crate::host_locator_outbox::Outbox,now_ms:u64)
        -> std::result::Result<crate::host_locator_renewal::Advance,String> {
        renewal.advance(queue,now_ms,|after| {
            use relay_protocol::host_locator as locator;
            let response=self.http.post(format!("http://127.0.0.1:4580{path}"))
                .bearer_auth(self.control.as_str()).json(&locator::PageRequest{
                    current:self.current.clone(),destination:self.destination.clone(),after,
                }).send().map_err(|_|Fault::Companion.message().to_owned())?;
            if !response.status().is_success() {return Err(Fault::Route.message().into());}
            let mut bytes=Vec::new();
            response.take(locator::MAX_PAGE_BYTES as u64+1).read_to_end(&mut bytes)
                .map_err(|_|Fault::Companion.message().to_owned())?;
            if bytes.len()>locator::MAX_PAGE_BYTES {return Err(Fault::Binding.message().into());}
            serde_json::from_slice(&bytes).map_err(|_|Fault::Binding.message().into())
        })
    }
}
