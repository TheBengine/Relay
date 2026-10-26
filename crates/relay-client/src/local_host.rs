//! Shared per-user hosting paths and bounded runtime identity.
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub fn server_binary() -> Option<PathBuf> {
    [
        std::env::var_os("RELAY_SERVER_PATH").map(PathBuf::from),
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("relay-server.exe"))),
    ]
    .into_iter()
    .flatten()
    .find(|p| p.is_file())
}

/// The user's hosting choice survives a normal stop; process records do not.
/// No executable path, password or invitation capability is stored here.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppHost {
    pub server_id: String,
    pub mode: String,
    pub address: Option<String>,
    /// The host's own opt-in to answer "who hosts a community here?" on the
    /// local network. Off unless chosen; a private iroh start can apply it too.
    #[serde(default)]
    pub lan_discovery: bool,
}

impl AppHost {
    pub fn load(data: &Path) -> Option<Self> {
        serde_json::from_value(read_record(&data.join("app-host.json")).ok()?).ok()
    }

    pub fn validate(&self) -> Result<(), String> {
        uuid::Uuid::parse_str(&self.server_id).map_err(|_| "Invalid saved server identity.")?;
        match (self.mode.as_str(), self.address.as_deref()) {
            ("local" | "iroh", None) => Ok(()),
            ("tailscale" | "embedded", Some(address)) if crate::tailscale_connect::valid_address(address) => Ok(()),
            _ => Err("Your server's saved sharing choice is invalid. Open Invite friends to choose it again.".into()),
        }
    }

    pub fn save(&self, data: &Path) -> Result<(), String> {
        self.validate()?;
        let bytes = serde_json::to_vec(self).map_err(|_| "Cannot prepare hosting choice.")?;
        // Replaced in one step, so a start that reads it never sees half a file.
        let temporary = data.join("app-host.json.tmp");
        std::fs::write(&temporary, bytes)
            .and_then(|()| std::fs::rename(&temporary, data.join("app-host.json")))
            .map_err(|_| "Cannot remember your server's hosting choice.".into())
    }
}

/// Whether this host has chosen to be found on the local network.
pub fn lan_choice(data: &Path) -> bool {
    AppHost::load(data).is_some_and(|plan| plan.lan_discovery)
}

/// Saves the choice without touching anything else about the saved hosting
/// plan. It changes nothing for a server that is already running.
pub fn set_lan_choice(data: &Path, enabled: bool) -> Result<(), String> {
    let mut plan = match AppHost::load(data) {
        Some(plan) => plan,
        None if data.join("app-host.json").exists() => {
            return Err(
                "Your saved hosting choice could not be read. Start your server again from Relay."
                    .into(),
            )
        }
        None => {
            return Err("Start your server from Relay first; its choice is saved with it.".into())
        }
    };
    plan.lan_discovery = enabled;
    plan.save(data)
}

/// Desktop hosting keeps the ordinary API and voice on loopback. The explicit
/// LAN opt-in opens only the sealed pairing redemption on its own OS-chosen port.
pub fn local_launch(lan_discovery: bool) -> (&'static str, &'static [&'static str]) {
    if lan_discovery {
        ("127.0.0.1", &["--lan-discovery", "--lan-pairing-only"])
    } else {
        ("127.0.0.1", &[])
    }
}

/// True when the read-only status report of a stopped community says its owner
/// account exists. The server refuses a network bind before that.
#[cfg(test)]
pub fn owner_report_ready(stdout: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(stdout)
        .is_ok_and(|report| report["ready"] == true && report["owner_initialized"] == true)
}

/// Asks the companion for that report. Failing, hanging or an unreadable answer
/// all count as "not ready", so a start never silently listens more widely.
pub fn owner_ready(server: &Path, data: &Path) -> bool {
    stopped_server_report(server, data)
        .is_some_and(|report| report["ready"] == true && report["owner_initialized"] == true)
}

/// Bounded read-only companion report; never starts, migrates or creates a server.
pub fn stopped_server_report(server: &Path, data: &Path) -> Option<serde_json::Value> {
    safe_folder(server).ok()?;
    safe_folder(data).ok()?;
    let mut command = std::process::Command::new(server);
    command
        .arg("--public-hosting-status")
        .arg("--data")
        .arg(data)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let Ok(mut child) = command.spawn() else {
        return None;
    };
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    };
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        use std::io::Read;
        let mut bytes = Vec::new();
        let _ = stdout.take(16 * 1024).read_to_end(&mut bytes);
        let _ = sender.send(bytes);
    });
    let report = receiver
        .recv_timeout(std::time::Duration::from_secs(8))
        .ok().and_then(|bytes| serde_json::from_slice(&bytes).ok());
    let _ = child.kill();
    let _ = child.wait();
    report
}

/// What a running server was started with, as far as there is evidence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LanActual {
    On,
    Off,
    /// No matching record: the server was not started by this Relay, or the
    /// record could not be written. Never shown as "off".
    Unknown,
}

/// Actual startup evidence for this exact instance, written by the server after it
/// binds its listeners and starts the responder. Requested flags and an old app
/// record are not evidence that discovery succeeded.
pub fn lan_actual(data: &Path, server_id: &str, instance_id: &str) -> LanActual {
    let Ok(record) = read_record(&data.join("server-runtime.json")) else {
        return LanActual::Unknown;
    };
    if record["server_id"].as_str() != Some(server_id)
        || record["instance_id"].as_str() != Some(instance_id)
    {
        return LanActual::Unknown;
    }
    match record["lan_discovery"].as_bool() {
        Some(true) => LanActual::On,
        Some(false) => LanActual::Off,
        None => LanActual::Unknown,
    }
}

/// A saved identity never authorizes adopting a different data folder or binary.
pub fn validate_app_host(
    plan: &AppHost,
    managed: &serde_json::Value,
    data: &Path,
    exe: &Path,
) -> Result<(), String> {
    validate_app_host_data(plan, managed, data)?;
    if managed["exe"].as_str().and_then(|p| PathBuf::from(p).canonicalize().ok())
        .zip(exe.canonicalize().ok()).is_some_and(|(a, b)| a == b)
    {
        Ok(())
    } else {
        Err("This local server does not match your saved server. Its data was left untouched.".into())
    }
}

fn validate_app_host_data(plan: &AppHost, managed: &serde_json::Value, data: &Path) -> Result<(), String> {
    plan.validate()?;
    let same_path = |field: &str, expected: &Path| {
        managed[field]
            .as_str()
            .and_then(|p| PathBuf::from(p).canonicalize().ok())
            .zip(expected.canonicalize().ok())
            .is_some_and(|(a, b)| a == b)
    };
    if managed["server_id"].as_str() != Some(plan.server_id.as_str())
        || !is_local_url(managed["url"].as_str().unwrap_or_default())
        || !same_path("data", data)
    {
        return Err(
            "This local server does not match your saved server. Its data was left untouched."
                .into(),
        );
    }
    Ok(())
}

/// A stopped host may be reopened after extracting a newer portable package.
/// This exception never adopts an existing process or trusts an arbitrary exe:
/// the current bundled companion must confirm the same existing database ID.
pub fn validate_stopped_package(
    plan: &AppHost,
    managed: &serde_json::Value,
    data: &Path,
    executable: &Path,
    bundled: &Path,
    report: &serde_json::Value,
) -> Result<(), String> {
    validate_app_host_data(plan, managed, data)?;
    for path in [data, executable, bundled] { safe_folder(path)?; }
    if data.join("server-runtime.json").exists() || data.join("stack-instance.json").exists() {
        return Err("Close the earlier Relay server normally before opening it with this version. Its data is kept.".into());
    }
    if !executable.is_file()
        || !executable.canonicalize().ok().zip(bundled.canonicalize().ok()).is_some_and(|(a, b)| a == b)
        || report["ready"] != true || report["owner_initialized"] != true
        || report["server_id"].as_str() != Some(plan.server_id.as_str())
    {
        return Err("The current Relay package could not confirm this saved server. Its data was left untouched.".into());
    }
    Ok(())
}

fn storage_root() -> Result<PathBuf, String> {
    if let Some(directory) = std::env::var_os("RELAY_SERVER_DATA_DIR") {
        return Ok(PathBuf::from(directory));
    }
    #[cfg(windows)]
    let root = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    #[cfg(not(windows))]
    let root = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".local/share")));
    let destination = root
        .map(|p| p.join("Relay").join("server-data"))
        .ok_or_else(|| "Cannot find your per-user server storage folder.".to_string())?;
    Ok(destination)
}

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServerChoice {
    active: Option<String>,
    created: Vec<String>,
    #[serde(default)]
    last_creation: Option<String>,
}
fn choice(root: &Path) -> Result<ServerChoice, String> {
    let path = root.join("local-server-choice.json");
    safe_folder(&path)?;
    if !path.exists() {
        return Ok(ServerChoice::default());
    }
    let value: ServerChoice = serde_json::from_value(
        read_record(&path)
            .map_err(|_| "Cannot read your local server selection. Existing servers were kept.")?,
    )
    .map_err(|_| "Cannot read your local server selection.")?;
    if value.created.len() > 64
        || value
            .created
            .iter()
            .any(|id| !uuid::Uuid::parse_str(id).is_ok_and(|parsed| parsed.to_string() == *id))
        || value
            .active
            .as_ref()
            .is_some_and(|id| !value.created.contains(id))
    {
        return Err("Your local server selection is invalid. Existing servers were kept.".into());
    }
    Ok(value)
}
fn safe_folder(path: &Path) -> Result<(), String> {
    for parent in path.ancestors() {
        if let Ok(info) = std::fs::symlink_metadata(parent) {
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                if info.file_attributes() & 0x400 != 0 {
                    return Err("Redirected local server storage is not supported.".into());
                }
            }
            if info.file_type().is_symlink() {
                return Err("Redirected local server storage is not supported.".into());
            }
        }
    }
    Ok(())
}
/// The chosen local server has its own database, host keys and sharing plan.
/// Choosing a new server never moves or replaces an earlier community.
pub fn data_directory() -> Result<PathBuf, String> {
    let root = storage_root()?;
    safe_folder(&root)?;
    if let Some(id) = choice(&root)?.active {
        let path = root.join("communities").join(id);
        safe_folder(&path)?;
        if !path.is_dir() {
            return Err(
                "Your selected local server folder is missing. Existing servers were kept.".into(),
            );
        }
        return Ok(path);
    }
    if !root.join("relay.sqlite3").is_file() {
        if let Some(source) = legacy_data_directory() {
            return Err(format!("An earlier community exists at {}. Stop its old host, then use scripts/import-community.ps1 to copy it safely into {}. Your existing community has been left in place.",source.display(),root.display()));
        }
    }
    Ok(root)
}
/// Called only after the explicit Create server action, on the control worker.
pub fn create_directory(request: &str) -> Result<PathBuf, String> {
    create_directory_with(&storage_root()?, request)
}
#[cfg(test)]
fn create_directory_in(root: &Path) -> Result<PathBuf, String> {
    create_directory_with(root, &uuid::Uuid::new_v4().to_string())
}
fn create_directory_with(root: &Path, request: &str) -> Result<PathBuf, String> {
    if !uuid::Uuid::parse_str(request).is_ok_and(|id| id.to_string() == request) {
        return Err("Invalid server creation request.".into());
    }
    safe_folder(root)?;
    let mut selected = choice(root)?;
    if selected.last_creation.as_deref() == Some(request) {
        let data = selected
            .active
            .as_ref()
            .map(|id| root.join("communities").join(id))
            .unwrap_or_else(|| root.to_path_buf());
        safe_folder(&data)?;
        if !data.is_dir() {
            return Err("The new server folder is missing. Existing data was kept.".into());
        }
        return Ok(data);
    }
    let data = if !root.join("relay.sqlite3").exists() && selected.created.is_empty() {
        std::fs::create_dir_all(root).map_err(|_| "Cannot create your server folder.")?;
        root.to_path_buf()
    } else {
        if selected.created.len() >= 64 {
            return Err(
                "You have 64 saved local servers. Open one before creating another.".into(),
            );
        }
        let parent = root.join("communities");
        safe_folder(&parent)?;
        std::fs::create_dir_all(&parent).map_err(|_| "Cannot create your server folder.")?;
        let data = parent.join(request);
        std::fs::create_dir(&data)
            .map_err(|_| "Cannot create your new server folder. Existing data was kept.")?;
        selected.created.push(request.to_owned());
        selected.active = Some(request.to_owned());
        data
    };
    selected.last_creation = Some(request.to_owned());
    save_choice(root, &selected)?;
    Ok(data)
}

fn save_choice(root: &Path, selected: &ServerChoice) -> Result<(), String> {
    let bytes = serde_json::to_vec(&selected).map_err(|_| "Cannot save your server selection.")?;
    let temporary = root.join(format!(".server-choice-{}.tmp", uuid::Uuid::new_v4()));
    use std::io::Write;
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|_| "Cannot save your server selection.")?;
    out.write_all(&bytes)
        .and_then(|_| out.sync_all())
        .map_err(|_| "Cannot save your server selection.")?;
    drop(out);
    std::fs::rename(&temporary, root.join("local-server-choice.json"))
        .map_err(|_| "Cannot save your server selection. All server data was kept.")?;
    Ok(())
}

fn community_directories(root: &Path) -> Result<Vec<PathBuf>, String> {
    safe_folder(root)?;
    let mut paths = vec![root.to_path_buf()];
    paths.extend(choice(root)?.created.into_iter().map(|id| root.join("communities").join(id)));
    for path in &paths { safe_folder(path)?; }
    Ok(paths)
}

fn directory_identity(data: &Path) -> Option<String> {
    for name in ["relay.sqlite3", "app-host.json", "managed-instance.json", "app-community.json"] {
        safe_folder(&data.join(name)).ok()?;
    }
    if !data.join("relay.sqlite3").is_file() { return None; }
    let id = AppHost::load(data).filter(|p| p.validate().is_ok()).map(|p| p.server_id)
        .or_else(|| read_record(&data.join("managed-instance.json")).ok()?["server_id"].as_str().map(str::to_owned))?;
    uuid::Uuid::parse_str(&id).ok()?;
    Some(id)
}

fn community_directory_in(root: &Path, server_id: &str) -> Result<PathBuf, String> {
    uuid::Uuid::parse_str(server_id).map_err(|_| "Invalid local server identity.")?;
    let mut matches = community_directories(root)?.into_iter()
        .filter(|data| directory_identity(data).as_deref() == Some(server_id));
    let data = matches.next().ok_or("This local server could not be found. Its saved data was not changed.")?;
    if matches.next().is_some() { return Err("More than one local folder has this server identity. No server was started.".into()); }
    Ok(data)
}

/// Resolve by the host's saved identity, never by the shared loopback address.
pub fn community_directory(server_id: &str) -> Result<PathBuf, String> {
    community_directory_in(&storage_root()?, server_id)
}

fn saved_community(data: &Path, id: String) -> crate::community::SavedCommunity {
    read_record(&data.join("app-community.json")).ok()
        .and_then(|v| serde_json::from_value::<crate::community::SavedCommunity>(v).ok())
        .filter(|v| v.server_id == id && v.url == "http://127.0.0.1:4580")
        .unwrap_or_else(|| crate::community::SavedCommunity {
            name: format!("Local server · {}", &id[..8]), server_id: id,
            url: "http://127.0.0.1:4580".into(), ..Default::default()
        })
}

pub fn saved_communities() -> Result<Vec<crate::community::SavedCommunity>, String> {
    Ok(community_directories(&storage_root()?)?.into_iter().filter_map(|data| {
        directory_identity(&data).map(|id| saved_community(&data, id))
    }).collect())
}

/// Retain account and consent metadata with its server when another server
/// takes the same local port. Databases and keys are never moved or replaced.
pub fn remember_community(community: &crate::community::SavedCommunity) -> Result<(), String> {
    if community.url != "http://127.0.0.1:4580" { return Ok(()); }
    let data = community_directory(&community.server_id)?;
    let destination = data.join("app-community.json");
    safe_folder(&destination)?;
    let raw = serde_json::to_vec(community).map_err(|_| "Cannot remember your local server.")?;
    if raw.len() > 16_384 { return Err("Your saved local server details are too large.".into()); }
    let temp = data.join(format!(".app-community-{}.tmp", uuid::Uuid::new_v4()));
    std::fs::write(&temp, raw).and_then(|_| std::fs::rename(&temp, destination))
        .map_err(|_| "Cannot remember your local server. Existing data was kept.".into())
}

fn select_community_in(root: &Path, server_id: &str) -> Result<crate::community::SavedCommunity, String> {
    let data = community_directory_in(root, server_id)?;
    let mut selected = choice(root)?;
    selected.active = if data == root { None } else {
        Some(data.file_name().and_then(|s| s.to_str()).ok_or("Invalid local server folder.")?.into())
    };
    selected.last_creation = None;
    save_choice(root, &selected)?;
    Ok(saved_community(&data, server_id.to_owned()))
}

pub fn select_community(server_id: &str) -> Result<crate::community::SavedCommunity, String> {
    select_community_in(&storage_root()?, server_id)
}

fn legacy_data_directory() -> Option<PathBuf> {
    let config = std::env::var_os("RELAY_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            directories::ProjectDirs::from("org", "Relay", "Relay")
                .map(|directory| directory.config_dir().to_path_buf())
        });
    let legacy = config.map(|directory| directory.join("server-data"));
    let portable = std::env::current_exe().ok().and_then(|exe| {
        let dist = exe.parent()?;
        if dist
            .file_name()?
            .to_string_lossy()
            .eq_ignore_ascii_case("dist")
        {
            dist.parent().map(|package| package.join("data"))
        } else {
            None
        }
    });
    [legacy, portable]
        .into_iter()
        .flatten()
        .find(|directory| directory.join("relay.sqlite3").is_file())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Identity {
    pub server_id: String,
    pub instance_id: String,
    pub pid: u32,
}
impl Identity {
    pub fn parse(value: &serde_json::Value) -> Option<Self> {
        if value["ok"] != true || value["managed_instance"] != true {
            return None;
        }
        let server_id = value["server_id"].as_str()?.to_owned();
        let instance_id = value["instance_id"].as_str()?.to_owned();
        uuid::Uuid::parse_str(&server_id).ok()?;
        uuid::Uuid::parse_str(&instance_id).ok()?;
        let pid = u32::try_from(value["pid"].as_u64()?).ok()?;
        if pid == 0 {
            return None;
        }
        Some(Self {
            server_id,
            instance_id,
            pid,
        })
    }
    pub fn save(&self, data: &Path, executable: &Path) -> Result<(), String> {
        #[derive(Serialize)]
        struct Record<'a> {
            #[serde(flatten)]
            identity: &'a Identity,
            data: &'a Path,
            exe: PathBuf,
            url: &'static str,
        }
        let record = Record {
            identity: self,
            data,
            exe: executable.to_owned(),
            url: "http://127.0.0.1:4580",
        };
        let bytes = serde_json::to_vec_pretty(&record)
            .map_err(|_| "Cannot prepare local hosting metadata.")?;
        std::fs::write(data.join("managed-instance.json"), bytes)
            .map_err(|_| "Cannot save local hosting metadata.".into())
    }
}

/// Hosting records may come from Rust or Windows PowerShell 5.1 (UTF-8 BOM).
/// Read one complete, bounded document so malformed/oversized metadata cannot
/// silently supply the identity used to restart or advertise a running server.
pub fn read_record(path: &Path) -> std::io::Result<serde_json::Value> {
    use std::io::{Error, ErrorKind, Read};
    const LIMIT: usize = 16_384;
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take((LIMIT + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > LIMIT {
        return Err(Error::new(
            ErrorKind::InvalidData,
            "Hosting record is too large",
        ));
    }
    let json = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&bytes);
    serde_json::from_slice(json)
        .map_err(|_| Error::new(ErrorKind::InvalidData, "Invalid hosting record"))
}

pub fn is_local_url(url: &str) -> bool {
    url::Url::parse(url).ok().is_some_and(|url| {
        url.scheme() == "http"
            && url.port_or_known_default() == Some(4580)
            && url.path() == "/"
            && matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"))
    })
}

/// Match both saved records to the live local instance before an automatic restart.
pub fn validate_sharing_switch(
    expected_server: &str,
    live: &Identity,
    managed: &serde_json::Value,
    stack: &serde_json::Value,
) -> Result<(), String> {
    if live.server_id != expected_server
        || !is_local_url(managed["url"].as_str().unwrap_or_default())
        || managed["server_id"].as_str() != Some(expected_server)
        || stack["server_id"].as_str() != Some(expected_server)
        || managed["instance_id"].as_str() != Some(live.instance_id.as_str())
        || stack["instance_id"].as_str() != Some(live.instance_id.as_str())
        || managed["pid"].as_u64() != Some(live.pid.into())
        || stack["relay_pid"].as_u64() != Some(live.pid.into())
        || !matches!(
            stack["hosting_mode"].as_str(),
            Some("local" | "tailscale" | "embedded")
        )
    {
        return Err("Relay couldn't confirm this server's running instance. Your server was left running. Restart it normally, then try sharing again.".into());
    }
    Ok(())
}

/// Restore only the already configured built-in route of this exact live host.
/// A normal local server must never become publicly shared just by signing in.
pub fn sharing_restore_address(
    expected: &str,
    live: &Identity,
    managed: &serde_json::Value,
    stack: &serde_json::Value,
) -> Result<Option<String>, String> {
    validate_sharing_switch(expected, live, managed, stack)?;
    if stack["hosting_mode"] != "embedded" {
        return Ok(None);
    }
    let address = stack["embedded_ipv4"]
        .as_str()
        .filter(|address| crate::tailscale_connect::valid_address(address))
        .ok_or(
            "Your server's saved sharing address is invalid. Open Invite friends to reconnect it.",
        )?;
    if stack["advertised_url"].as_str() != Some(format!("http://{address}:4580").as_str()) {
        return Err(
            "Your server's sharing settings do not match. Open Invite friends to reconnect it."
                .into(),
        );
    }
    Ok(Some(address.to_owned()))
}

pub fn shared_address(expected_server_id: &str) -> Option<String> {
    let data = data_directory().ok()?;
    let read = |name: &str| read_record(&data.join(name)).ok();
    let managed = read("managed-instance.json")?;
    let stack = read("stack-instance.json")?;
    let address = shared_address_records(expected_server_id, &managed, &stack)?;
    if stack["hosting_mode"] == "embedded" {
        crate::embedded_network::hosted_status(expected_server_id).filter(|u| u == &address)
    } else {
        Some(address)
    }
}
pub fn embedded_address(expected_server_id: &str) -> Option<String> {
    let data = data_directory().ok()?;
    let read = |name: &str| read_record(&data.join(name)).ok();
    let stack = read("stack-instance.json")?;
    if stack["hosting_mode"] != "embedded" {
        return None;
    }
    shared_address_records(expected_server_id, &read("managed-instance.json")?, &stack)
}

fn shared_address_records(
    expected: &str,
    managed: &serde_json::Value,
    stack: &serde_json::Value,
) -> Option<String> {
    if uuid::Uuid::parse_str(expected).is_err()
        || !matches!(
            stack["hosting_mode"].as_str(),
            Some("tailscale" | "embedded")
        )
        || managed["server_id"].as_str() != Some(expected)
        || stack["server_id"].as_str() != Some(expected)
        || managed["instance_id"].as_str()?.is_empty()
        || stack["instance_id"] != managed["instance_id"]
        || stack["relay_pid"].as_u64()? == 0
        || stack["relay_pid"] != managed["pid"]
    {
        return None;
    }
    let address = if stack["hosting_mode"] == "embedded" {
        stack["embedded_ipv4"].as_str()?
    } else {
        stack["tailscale_ipv4"].as_str()?
    };
    if !crate::tailscale_connect::valid_address(address) {
        return None;
    }
    let url = format!("http://{address}:4580");
    if stack["advertised_url"].as_str() != Some(url.as_str()) {
        return None;
    }
    Some(url)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn app_host_choice_survives_stop_without_storing_runtime_secrets() {
        let directory = tempfile::tempdir().unwrap();
        for (mode, address) in [
            ("local", None),
            ("iroh", None),
            ("tailscale", Some("100.70.1.2")),
            ("embedded", Some("100.70.1.3")),
        ] {
            let plan = AppHost {
                server_id: uuid::Uuid::new_v4().to_string(),
                mode: mode.into(),
                address: address.map(str::to_owned),
                lan_discovery: false,
            };
            plan.save(directory.path()).unwrap();
            assert_eq!(AppHost::load(directory.path()), Some(plan));
            let saved = std::fs::read_to_string(directory.path().join("app-host.json")).unwrap();
            assert!(!saved.contains("token") && !saved.contains("exe") && !saved.contains("pid"));
        }
        let mut plan = AppHost {
            server_id: uuid::Uuid::new_v4().to_string(),
            mode: "iroh".into(),
            address: Some("8.8.8.8".into()),
            lan_discovery: false,
        };
        assert!(plan.validate().is_err());
        plan.mode = "tailscale".into();
        assert!(plan.validate().is_err());
        plan.address = Some("100.70.1.2".into());
        plan.server_id = "wrong".into();
        assert!(plan.validate().is_err());
    }

    #[test]
    fn app_host_restore_refuses_other_community_data_binary_or_remote_url() {
        let directory = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let exe = directory.path().join("server.exe");
        std::fs::write(&exe, b"fixture").unwrap();
        let plan = AppHost {
            server_id: uuid::Uuid::new_v4().to_string(),
            mode: "local".into(),
            address: None,
            lan_discovery: false,
        };
        let managed = serde_json::json!({"server_id":plan.server_id,"url":"http://127.0.0.1:4580","exe":exe,"data":directory.path()});
        validate_app_host(&plan, &managed, directory.path(), &exe).unwrap();
        for (field, replacement) in [
            (
                "server_id",
                serde_json::json!(uuid::Uuid::new_v4().to_string()),
            ),
            ("data", serde_json::json!(other.path())),
            ("exe", serde_json::json!(directory.path())),
            ("url", serde_json::json!("http://192.168.111.6:4580")),
        ] {
            let mut changed = managed.clone();
            changed[field] = replacement;
            assert!(
                validate_app_host(&plan, &changed, directory.path(), &exe).is_err(),
                "{field}"
            );
        }
    }
    #[test]
    fn portable_package_move_reopens_only_the_same_stopped_community() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("community");
        std::fs::create_dir(&data).unwrap();
        let old = temp.path().join("old-relay-server.exe");
        let current = temp.path().join("current-relay-server.exe");
        let other = temp.path().join("unrelated.exe");
        for executable in [&old, &current, &other] {
            std::fs::write(executable, b"fixture only; not executed").unwrap();
        }
        std::fs::write(data.join("relay.sqlite3"), b"existing community").unwrap();
        std::fs::write(data.join("host-key.v1"), b"existing private key").unwrap();
        let plan = AppHost {
            server_id: uuid::Uuid::new_v4().to_string(), mode: "local".into(),
            address: None, lan_discovery: false,
        };
        let managed = serde_json::json!({"server_id":plan.server_id,"url":"http://127.0.0.1:4580","exe":old,"data":data});
        let report = serde_json::json!({"ready":true,"owner_initialized":true,"server_id":plan.server_id});
        // Running-host adoption still refuses another package. Only a stopped
        // host with matching read-only database evidence has the new route.
        assert!(validate_app_host(&plan, &managed, &data, &current).is_err());
        validate_stopped_package(&plan, &managed, &data, &current, &current, &report).unwrap();
        std::fs::remove_file(&old).unwrap();
        validate_stopped_package(&plan, &managed, &data, &current, &current, &report).unwrap();
        assert!(validate_stopped_package(&plan, &managed, &data, &other, &current, &report).is_err());
        let mut wrong = report.clone();
        wrong["server_id"] = serde_json::json!(uuid::Uuid::new_v4().to_string());
        assert!(validate_stopped_package(&plan, &managed, &data, &current, &current, &wrong).is_err());
        wrong = report.clone();
        wrong["owner_initialized"] = serde_json::json!(false);
        assert!(validate_stopped_package(&plan, &managed, &data, &current, &current, &wrong).is_err());
        let mut moved = managed.clone();
        moved["data"] = serde_json::json!(temp.path());
        assert!(validate_stopped_package(&plan, &moved, &data, &current, &current, &report).is_err());
        std::fs::write(data.join("server-runtime.json"), b"still running or not confirmed stopped").unwrap();
        assert!(validate_stopped_package(&plan, &managed, &data, &current, &current, &report).is_err());
        assert_eq!(std::fs::read(data.join("relay.sqlite3")).unwrap(), b"existing community");
        assert_eq!(std::fs::read(data.join("host-key.v1")).unwrap(), b"existing private key");
    }

    #[test]
    fn hosting_record_requires_complete_bounded_utf8_json_with_optional_bom() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("record.json");
        for prefix in [b"".as_slice(), b"\xef\xbb\xbf".as_slice()] {
            let mut bytes = prefix.to_vec();
            bytes.extend_from_slice(br#"{"hosting_mode":"local"}"#);
            std::fs::write(&path, bytes).unwrap();
            assert_eq!(read_record(&path).unwrap()["hosting_mode"], "local");
        }
        for bytes in [
            b"".as_slice(),
            b"\xef\xbb\xbf".as_slice(),
            br#"{"hosting_mode":"local""#.as_slice(),
            br#"{} {}"#.as_slice(),
            b"\xff\xfe{\x00}\x00".as_slice(),
            b"\xef\xbb\xbf\xef\xbb\xbf{}".as_slice(),
        ] {
            std::fs::write(&path, bytes).unwrap();
            assert_eq!(
                read_record(&path).unwrap_err().kind(),
                std::io::ErrorKind::InvalidData
            );
        }
        let mut exact_limit = b"{}".to_vec();
        exact_limit.resize(16_384, b' ');
        std::fs::write(&path, &exact_limit).unwrap();
        assert!(read_record(&path).is_ok());
        exact_limit.push(b' ');
        std::fs::write(&path, exact_limit).unwrap();
        assert_eq!(
            read_record(&path).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
        assert_eq!(
            read_record(&directory.path().join("missing.json"))
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::NotFound
        );
    }

    #[cfg(windows)]
    #[test]
    fn powershell_saved_stack_can_switch_and_advertise_only_matching_identity() {
        let directory = tempfile::tempdir().unwrap();
        let live = Identity {
            server_id: uuid::Uuid::new_v4().to_string(),
            instance_id: uuid::Uuid::new_v4().to_string(),
            pid: 42,
        };
        live.save(directory.path(), &directory.path().join("relay-server.exe"))
            .unwrap();
        let stack = serde_json::json!({"hosting_mode":"local","server_id":live.server_id,"instance_id":live.instance_id,"relay_pid":42,"embedded_ipv4":"100.70.1.2","advertised_url":"http://100.70.1.2:4580"});
        let path = directory.path().join("stack-instance.json");
        let output = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", "ConvertFrom-Json -InputObject $env:RELAY_TEST_STACK | ConvertTo-Json | Set-Content -LiteralPath $env:RELAY_TEST_STACK_PATH -Encoding UTF8"])
            .env("RELAY_TEST_STACK", stack.to_string())
            .env("RELAY_TEST_STACK_PATH", &path)
            .output().unwrap();
        assert!(output.status.success(), "Windows PowerShell fixture failed");
        assert!(std::fs::read(&path).unwrap().starts_with(b"\xef\xbb\xbf"));
        let managed = read_record(&directory.path().join("managed-instance.json")).unwrap();
        let mut saved = read_record(&path).unwrap();
        assert!(validate_sharing_switch(&live.server_id, &live, &managed, &saved).is_ok());
        saved["hosting_mode"] = "embedded".into();
        assert_eq!(
            shared_address_records(&live.server_id, &managed, &saved).as_deref(),
            Some("http://100.70.1.2:4580")
        );
        saved["relay_pid"] = 99.into();
        assert!(validate_sharing_switch(&live.server_id, &live, &managed, &saved).is_err());
        assert!(shared_address_records(&live.server_id, &managed, &saved).is_none());
    }

    #[test]
    fn sharing_recovery_restores_only_an_existing_matching_private_host() {
        let live = Identity {
            server_id: uuid::Uuid::new_v4().to_string(),
            instance_id: uuid::Uuid::new_v4().to_string(),
            pid: 42,
        };
        let managed = serde_json::json!({"server_id":live.server_id,"instance_id":live.instance_id,"pid":42,"url":"http://127.0.0.1:4580"});
        let mut stack = serde_json::json!({"server_id":live.server_id,"instance_id":live.instance_id,"relay_pid":42,"hosting_mode":"local"});
        assert_eq!(
            sharing_restore_address(&live.server_id, &live, &managed, &stack),
            Ok(None)
        );
        stack["hosting_mode"] = "embedded".into();
        for address in [crate::iroh_route::ONLY_ADDRESS, "100.70.1.2"] {
            stack["embedded_ipv4"] = address.into();
            stack["advertised_url"] = format!("http://{address}:4580").into();
            assert_eq!(
                sharing_restore_address(&live.server_id, &live, &managed, &stack),
                Ok(Some(address.into()))
            );
        }
        for (key, value) in [
            (
                "server_id",
                serde_json::json!(uuid::Uuid::new_v4().to_string()),
            ),
            (
                "instance_id",
                serde_json::json!(uuid::Uuid::new_v4().to_string()),
            ),
            ("relay_pid", serde_json::json!(99)),
            ("embedded_ipv4", serde_json::json!("203.0.113.9")),
            (
                "advertised_url",
                serde_json::json!("http://100.70.1.9:4580"),
            ),
        ] {
            let mut changed = stack.clone();
            changed[key] = value;
            assert!(
                sharing_restore_address(&live.server_id, &live, &managed, &changed).is_err(),
                "{key}"
            );
        }
        assert!(sharing_restore_address(
            &uuid::Uuid::new_v4().to_string(),
            &live,
            &managed,
            &stack
        )
        .is_err());
    }

    #[test]
    fn automatic_sharing_rejects_stale_instances_wrong_communities_and_unknown_modes() {
        let live = Identity {
            server_id: uuid::Uuid::new_v4().to_string(),
            instance_id: uuid::Uuid::new_v4().to_string(),
            pid: 42,
        };
        let managed = serde_json::json!({"server_id":live.server_id,"instance_id":live.instance_id,"pid":42,"url":"http://127.0.0.1:4580"});
        let stack = serde_json::json!({"server_id":live.server_id,"instance_id":live.instance_id,"relay_pid":42,"hosting_mode":"local"});
        assert!(validate_sharing_switch(&live.server_id, &live, &managed, &stack).is_ok());
        for (key, value) in [
            (
                "server_id",
                serde_json::json!(uuid::Uuid::new_v4().to_string()),
            ),
            (
                "instance_id",
                serde_json::json!(uuid::Uuid::new_v4().to_string()),
            ),
            ("relay_pid", serde_json::json!(7)),
            ("hosting_mode", serde_json::json!("public")),
        ] {
            let mut changed = stack.clone();
            changed[key] = value;
            assert!(validate_sharing_switch(&live.server_id, &live, &managed, &changed).is_err());
        }
        for (key, value) in [
            ("pid", serde_json::json!(7)),
            (
                "instance_id",
                serde_json::json!(uuid::Uuid::new_v4().to_string()),
            ),
            ("url", serde_json::json!("http://example.com:4580")),
        ] {
            let mut changed = managed.clone();
            changed[key] = value;
            assert!(validate_sharing_switch(&live.server_id, &live, &changed, &stack).is_err());
        }
    }
    #[test]
    fn shared_address_requires_current_mode_server_instance_and_pid() {
        let id = uuid::Uuid::new_v4().to_string();
        let instance = uuid::Uuid::new_v4().to_string();
        let managed = serde_json::json!({"server_id":id,"instance_id":instance,"pid":42});
        let mut stack = serde_json::json!({"hosting_mode":"tailscale","server_id":id,"instance_id":instance,"relay_pid":42,"tailscale_ipv4":"100.70.1.2","advertised_url":"http://100.70.1.2:4580"});
        assert_eq!(
            shared_address_records(&id, &managed, &stack).as_deref(),
            Some("http://100.70.1.2:4580")
        );
        stack["hosting_mode"] = "local".into();
        assert!(shared_address_records(&id, &managed, &stack).is_none());
        stack["hosting_mode"] = "tailscale".into();
        stack["relay_pid"] = 99.into();
        assert!(shared_address_records(&id, &managed, &stack).is_none());
        stack["relay_pid"] = 42.into();
        stack["instance_id"] = "another".into();
        assert!(shared_address_records(&id, &managed, &stack).is_none());
    }
    #[test]
    fn runtime_identity_requires_concrete_unique_ids_and_local_origin() {
        let valid = serde_json::json!({"ok":true,"managed_instance":true,"server_id":uuid::Uuid::new_v4().to_string(),"instance_id":uuid::Uuid::new_v4().to_string(),"pid":1});
        assert!(Identity::parse(&valid).is_some());
        assert!(Identity::parse(&serde_json::json!({"ok":true,"pid":1})).is_none());
        assert!(is_local_url("http://127.0.0.1:4580"));
        for url in [
            "http://127.0.0.1.evil.example:4580",
            "https://127.0.0.1:4580",
            "http://127.0.0.1:4581",
            "http://localhost:4580/other",
        ] {
            assert!(!is_local_url(url));
        }
    }

    fn plan(mode: &str, address: Option<&str>) -> AppHost {
        AppHost {
            server_id: uuid::Uuid::new_v4().to_string(),
            mode: mode.into(),
            address: address.map(str::to_owned),
            lan_discovery: false,
        }
    }

    #[test]
    fn network_discovery_is_off_unless_chosen_and_an_older_choice_file_keeps_it_off() {
        let directory = tempfile::tempdir().unwrap();
        assert!(!lan_choice(directory.path()));
        let id = uuid::Uuid::new_v4().to_string();
        std::fs::write(
            directory.path().join("app-host.json"),
            format!(r#"{{"server_id":"{id}","mode":"local","address":null}}"#),
        )
        .unwrap();
        assert!(!lan_choice(directory.path()));
        assert_eq!(AppHost::load(directory.path()).unwrap().server_id, id);
    }

    #[test]
    fn saving_the_discovery_choice_changes_nothing_else_about_the_saved_host() {
        let directory = tempfile::tempdir().unwrap();
        // Nothing is saved before a server has started, so there is nothing to change.
        assert!(set_lan_choice(directory.path(), true).is_err());
        let saved = plan("tailscale", Some("100.70.1.2"));
        saved.save(directory.path()).unwrap();
        set_lan_choice(directory.path(), true).unwrap();
        let on = AppHost::load(directory.path()).unwrap();
        assert!(on.lan_discovery);
        assert_eq!(
            (
                on.server_id.as_str(),
                on.mode.as_str(),
                on.address.as_deref()
            ),
            (saved.server_id.as_str(), "tailscale", Some("100.70.1.2"))
        );
        set_lan_choice(directory.path(), false).unwrap();
        assert_eq!(AppHost::load(directory.path()), Some(saved));
        assert!(!directory.path().join("app-host.json.tmp").exists());
        // A damaged file is reported, never replaced by a fresh plan.
        std::fs::write(directory.path().join("app-host.json"), b"{").unwrap();
        assert!(set_lan_choice(directory.path(), true).is_err());
        assert_eq!(
            std::fs::read(directory.path().join("app-host.json")).unwrap(),
            b"{"
        );
    }

    #[test]
    fn desktop_lan_keeps_normal_ports_on_loopback() {
        assert_eq!(local_launch(false), ("127.0.0.1", &[][..]));
        assert_eq!(
            local_launch(true),
            ("127.0.0.1", &["--lan-discovery", "--lan-pairing-only"][..])
        );
    }

    #[test]
    fn a_network_bind_waits_for_the_owner_account_the_companion_reports() {
        assert!(owner_report_ready(
            br#"{"ready":true,"owner_initialized":true,"server_id":"x"}"#
        ));
        let refused: [&[u8]; 6] = [
            br#"{"ready":true,"owner_initialized":false}"#,
            br#"{"ready":false,"owner_initialized":true}"#,
            br#"{"ready":false,"owner_initialized":false,"error_code":"community_missing"}"#,
            br#"{"ready":"true","owner_initialized":true}"#,
            b"not json",
            b"",
        ];
        for report in refused {
            assert!(!owner_report_ready(report));
        }
    }

    #[test]
    fn runtime_visibility_is_bound_to_the_running_instance_and_not_requested_flags() {
        let directory = tempfile::tempdir().unwrap();
        let server = uuid::Uuid::new_v4().to_string();
        let instance = uuid::Uuid::new_v4().to_string();
        let actual = || lan_actual(directory.path(), &server, &instance);
        let write = |value: serde_json::Value| {
            std::fs::write(
                directory.path().join("server-runtime.json"),
                serde_json::to_vec(&value).unwrap(),
            )
            .unwrap();
        };
        assert_eq!(actual(), LanActual::Unknown);
        // Requested settings cannot claim the responder actually started.
        for name in ["lan-discovery.json", "stack-instance.json"] {
            std::fs::write(directory.path().join(name), serde_json::to_vec(
                &serde_json::json!({"server_id":server,"instance_id":instance,"enabled":true,"lan_discovery":true})
            ).unwrap()).unwrap();
        }
        assert_eq!(actual(), LanActual::Unknown);
        write(serde_json::json!({"server_id":server,"instance_id":instance,"lan_discovery":false}));
        assert_eq!(actual(), LanActual::Off);
        write(
            serde_json::json!({"server_id":server,"instance_id":instance,"lan_discovery":true,"lan_pairing_port":49152}),
        );
        assert_eq!(actual(), LanActual::On);
        for value in [
            serde_json::json!({"server_id":server,"instance_id":"other","lan_discovery":true}),
            serde_json::json!({"server_id":"other","instance_id":instance,"lan_discovery":false}),
            serde_json::json!({"server_id":server,"instance_id":instance,"lan_discovery":"yes"}),
            serde_json::json!({"server_id":server,"instance_id":instance}),
        ] {
            write(value);
            assert_eq!(actual(), LanActual::Unknown);
        }
        std::fs::write(directory.path().join("server-runtime.json"), b"{").unwrap();
        assert_eq!(actual(), LanActual::Unknown);
    }
    #[test]
    fn creating_a_server_keeps_the_existing_database_keys_and_selection() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("server-data");
        assert_eq!(create_directory_in(&root).unwrap(), root);
        std::fs::write(root.join("relay.sqlite3"), b"existing-community-database").unwrap();
        std::fs::write(root.join("host-key.v1"), b"existing-private-key").unwrap();
        let request = uuid::Uuid::new_v4().to_string();
        let first = create_directory_with(&root, &request).unwrap();
        assert_ne!(first, root);
        assert!(first.is_dir() && !first.join("relay.sqlite3").exists());
        std::fs::write(first.join("relay.sqlite3"), b"second-community").unwrap();
        assert_eq!(
            create_directory_with(&root, &request).unwrap(),
            first,
            "Retry must reopen the same newly-created server"
        );
        let second = create_directory_in(&root).unwrap();
        assert_ne!(first, second);
        assert_eq!(
            std::fs::read(root.join("relay.sqlite3")).unwrap(),
            b"existing-community-database"
        );
        assert_eq!(
            std::fs::read(root.join("host-key.v1")).unwrap(),
            b"existing-private-key"
        );
        assert_eq!(
            std::fs::read(first.join("relay.sqlite3")).unwrap(),
            b"second-community"
        );
        let selected = choice(&root).unwrap();
        assert_eq!(selected.created.len(), 2);
        assert_eq!(
            root.join("communities").join(selected.active.unwrap()),
            second
        );
        // A malformed pointer must not select or create an arbitrary path.
        std::fs::write(
            root.join("local-server-choice.json"),
            br#"{"active":"../other","created":["../other"]}"#,
        )
        .unwrap();
        assert!(create_directory_in(&root).is_err());
        assert!(!fixture.path().join("other").exists());
    }

    #[test]
    fn selecting_an_older_local_server_keeps_each_identity_account_and_database() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("server-data");
        create_directory_in(&root).unwrap();
        let old_id = uuid::Uuid::new_v4().to_string();
        let new_id = uuid::Uuid::new_v4().to_string();
        let seed = |data: &Path, id: &str, name: &str, consent: u64| {
            std::fs::write(data.join("relay.sqlite3"), name).unwrap();
            std::fs::write(data.join("host-key.v1"), id).unwrap();
            AppHost { server_id: id.into(), mode: "local".into(), address: None, lan_discovery: false }.save(data).unwrap();
            let saved = crate::community::SavedCommunity {
                url: "http://127.0.0.1:4580".into(), server_id: id.into(), name: name.into(),
                username: format!("owner-{name}"), open_protection_accepted: Some(consent), ..Default::default()
            };
            std::fs::write(data.join("app-community.json"), serde_json::to_vec(&saved).unwrap()).unwrap();
        };
        seed(&root, &old_id, "First", 7);
        let nested = create_directory_in(&root).unwrap();
        seed(&nested, &new_id, "Second", 2);
        let first = select_community_in(&root, &old_id).unwrap();
        assert_eq!((&first.name, &first.username, first.open_protection_accepted), (&"First".to_owned(), &"owner-First".to_owned(), Some(7)));
        assert!(choice(&root).unwrap().active.is_none());
        assert!(choice(&root).unwrap().last_creation.is_none());
        assert_eq!(community_directory_in(&root, &old_id).unwrap(), root);
        let second = select_community_in(&root, &new_id).unwrap();
        assert_eq!(second.server_id, new_id);
        assert_eq!(second.open_protection_accepted, Some(2));
        assert_eq!(community_directory_in(&root, &new_id).unwrap(), nested);
        assert_eq!(root.join("communities").join(choice(&root).unwrap().active.unwrap()), nested);
        // Even at the same local address, wrong identity metadata cannot supply
        // another community's account or previously accepted protection choice.
        std::fs::write(root.join("app-community.json"), serde_json::to_vec(&second).unwrap()).unwrap();
        let first = select_community_in(&root, &old_id).unwrap();
        assert_eq!(first.server_id, old_id);
        assert!(first.username.is_empty() && first.open_protection_accepted.is_none());
        assert!(select_community_in(&root, &uuid::Uuid::new_v4().to_string()).is_err());
        assert_eq!(std::fs::read(root.join("relay.sqlite3")).unwrap(), b"First");
        assert_eq!(std::fs::read(nested.join("relay.sqlite3")).unwrap(), b"Second");
        assert_eq!(std::fs::read(root.join("host-key.v1")).unwrap(), old_id.as_bytes());
        assert_eq!(std::fs::read(nested.join("host-key.v1")).unwrap(), new_id.as_bytes());
    }
}
