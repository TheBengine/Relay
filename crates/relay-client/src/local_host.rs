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
        std::fs::write(
            data.join("app-host.json"),
            serde_json::to_vec(self).map_err(|_| "Cannot prepare hosting choice.")?,
        )
        .map_err(|_| "Cannot remember your server's hosting choice.".into())
    }
}

/// A saved identity never authorizes adopting a different data folder or binary.
pub fn validate_app_host(
    plan: &AppHost,
    managed: &serde_json::Value,
    data: &Path,
    exe: &Path,
) -> Result<(), String> {
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
        || !same_path("exe", exe)
    {
        return Err(
            "This local server does not match your saved server. Its data was left untouched."
                .into(),
        );
    }
    Ok(())
}

pub fn data_directory() -> Result<PathBuf, String> {
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
    if !destination.join("relay.sqlite3").is_file() {
        if let Some(source) = legacy_data_directory() {
            return Err(format!("An earlier community exists at {}. Stop its old host, then use scripts/import-community.ps1 to copy it safely into {}. Your existing community has been left in place.",source.display(),destination.display()));
        }
    }
    Ok(destination)
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
        crate::embedded_network::shared_address(expected_server_id).filter(|u| u == &address)
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
}
