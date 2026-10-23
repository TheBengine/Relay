use super::*;
use serde::Deserialize;
use std::io::Read;
use zeroize::Zeroizing;

const HOST_ERROR: &str =
    "Open Relay on the community host and refresh its connection to configure the bot token.";

#[derive(Deserialize)]
struct Runtime {
    server_id: String,
    instance_id: String,
    pid: u32,
    address: String,
    data: PathBuf,
    shutdown_token: Zeroizing<String>,
}

// Resolve the three accepted names to literal loopback before making requests;
// neither proxy environment variables nor DNS may receive a host credential.
fn endpoint(url: &str) -> Result<String, String> {
    if !crate::local_host::is_local_url(url) {
        return Err(HOST_ERROR.into());
    }
    let parsed = url::Url::parse(url).map_err(|_| HOST_ERROR)?;
    if !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(HOST_ERROR.into());
    }
    Ok(if parsed.host_str() == Some("[::1]") {
        "http://[::1]:4580"
    } else {
        "http://127.0.0.1:4580"
    }
    .into())
}

fn local_path(path: &std::path::Path) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err(HOST_ERROR.into());
    }
    #[cfg(windows)]
    {
        use std::path::{Component, Prefix};
        if !matches!(path.components().next(), Some(Component::Prefix(p)) if matches!(p.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_)))
        {
            return Err(HOST_ERROR.into());
        }
        use std::os::windows::fs::MetadataExt;
        for ancestor in path.ancestors() {
            let meta = std::fs::symlink_metadata(ancestor).map_err(|_| HOST_ERROR)?;
            if meta.file_attributes() & 0x400 != 0 {
                return Err(HOST_ERROR.into());
            }
        }
    }
    std::fs::canonicalize(path).map_err(|_| HOST_ERROR.into())
}

fn proof(
    data: &std::path::Path,
    base: &str,
    expected_server: &str,
    health: &crate::local_host::Identity,
) -> Result<Runtime, String> {
    let data = local_path(data)?;
    let file = local_path(&data.join("server-runtime.json"))?;
    let mut bytes = Zeroizing::new(Vec::new());
    std::fs::File::open(file)
        .map_err(|_| HOST_ERROR)?
        .take(16 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| HOST_ERROR)?;
    if bytes.len() > 16 * 1024 {
        return Err(HOST_ERROR.into());
    }
    let record: Runtime = serde_json::from_slice(&bytes).map_err(|_| HOST_ERROR)?;
    if record.server_id != expected_server
        || record.server_id != health.server_id
        || record.instance_id != health.instance_id
        || record.pid != health.pid
        || endpoint(&record.address)? != base
        || local_path(&record.data)? != data
        || !(32..=128).contains(&record.shutdown_token.len())
        || !record
            .shutdown_token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return Err(HOST_ERROR.into());
    }
    Ok(record)
}

impl Worker {
    pub(super) fn discord_host(
        &mut self,
        origin: CommandOrigin,
        action: DiscordHostAction,
        request_id: u64,
    ) -> Result<(), String> {
        // This runs serially with account switches; reject stale queued commands
        // before touching a runtime file or sending either secret.
        let connection = self.command_connection(&origin)?;
        let base = endpoint(&connection.url)?;
        let http = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(1))
            .timeout(Duration::from_secs(4))
            .build()
            .map_err(|_| HOST_ERROR)?;
        let health = read_json::<serde_json::Value>(
            http.get(format!("{base}/health"))
                .send()
                .map_err(|_| HOST_ERROR)?,
            16 * 1024,
        )?;
        let health = crate::local_host::Identity::parse(&health).ok_or(HOST_ERROR)?;
        let data = crate::local_host::data_directory()?;
        let record = proof(&data, &base, &connection.server_id, &health)?;
        let mut control = reqwest::header::HeaderValue::from_str(&record.shutdown_token)
            .map_err(|_| HOST_ERROR)?;
        control.set_sensitive(true);
        let method = match &action {
            DiscordHostAction::Status => reqwest::Method::GET,
            DiscordHostAction::Save(_) => reqwest::Method::PUT,
            DiscordHostAction::Remove => reqwest::Method::DELETE,
            DiscordHostAction::Retry => reqwest::Method::POST,
        };
        let mut request = http
            .request(method, format!("{base}/api/local/discord-credentials"))
            .bearer_auth(&connection.session.token)
            .header("x-relay-local-control", control)
            .header("x-relay-instance-id", record.instance_id)
            .header("x-relay-server-id", record.server_id);
        if let DiscordHostAction::Save(token) = &action {
            #[derive(serde::Serialize)]
            struct Input<'a> {
                token: &'a str,
            }
            let bytes =
                Zeroizing::new(serde_json::to_vec(&Input { token }).map_err(|_| HOST_ERROR)?);
            request = request
                .header("Content-Type", "application/json")
                .body(bytes.to_vec());
        }
        let response = request.send().map_err(|_| {
            "Cannot reach the local host. The token has not been shown or shared.".to_string()
        })?;
        if !response.status().is_success() {
            // A host error body is never echoed into a UI/log: it might contain
            // secret input if an incompatible service is running at the address.
            return Err("The host rejected this setup request. Refresh the connection and check your administrator access.".into());
        }
        let data: serde_json::Value = read_json(response, 4096)?;
        // Narrow response shape, so an untrusted body cannot return secrets into
        // the ordinary admin cache, render probe or notifications.
        let source = data["source"]
            .as_str()
            .filter(|v| ["saved", "environment", "removed", "none"].contains(v))
            .ok_or(HOST_ERROR)?;
        let supported = data["supported"].as_bool().ok_or(HOST_ERROR)?;
        let failed = !data["error"].is_null();
        self.emit(Update::AdminResult {
            request_id,
            data: serde_json::json!({"source":source,"supported":supported,"failed":failed}),
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn host_endpoint_does_not_use_dns_proxies_paths_or_embedded_credentials() {
        assert_eq!(
            endpoint("http://localhost:4580").unwrap(),
            "http://127.0.0.1:4580"
        );
        assert_eq!(endpoint("http://[::1]:4580").unwrap(), "http://[::1]:4580");
        for value in [
            "http://localhost.evil:4580",
            "http://localhost:4580?token=x",
            "http://user:pass@localhost:4580",
            "http://localhost:4580/#x",
            "https://localhost:4580",
            "http://localhost:4580/secret",
        ] {
            assert!(endpoint(value).is_err(), "{value}");
        }
    }
    #[test]
    fn runtime_proof_requires_current_server_instance_pid_and_storage() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().canonicalize().unwrap();
        let health = crate::local_host::Identity {
            server_id: uuid::Uuid::new_v4().to_string(),
            instance_id: uuid::Uuid::new_v4().to_string(),
            pid: 42,
        };
        let record = serde_json::json!({"server_id":health.server_id,"instance_id":health.instance_id,"pid":42,"address":"http://localhost:4580","data":data,"shutdown_token":"fixture-control-0123456789-0123456789"});
        let file = data.join("server-runtime.json");
        std::fs::write(&file, record.to_string()).unwrap();
        assert!(proof(&data, "http://127.0.0.1:4580", &health.server_id, &health).is_ok());
        for (field, changed) in [
            (
                "server_id",
                serde_json::json!(uuid::Uuid::new_v4().to_string()),
            ),
            (
                "instance_id",
                serde_json::json!(uuid::Uuid::new_v4().to_string()),
            ),
            ("pid", serde_json::json!(43)),
            ("address", serde_json::json!("http://remote.invalid:4580")),
            ("shutdown_token", serde_json::json!("short")),
        ] {
            let mut forged = record.clone();
            forged[field] = changed;
            std::fs::write(&file, forged.to_string()).unwrap();
            assert!(
                proof(&data, "http://127.0.0.1:4580", &health.server_id, &health).is_err(),
                "{field}"
            );
        }
        std::fs::write(&file, vec![b'a'; 16 * 1024 + 1]).unwrap();
        assert!(proof(&data, "http://127.0.0.1:4580", &health.server_id, &health).is_err());
    }
}
