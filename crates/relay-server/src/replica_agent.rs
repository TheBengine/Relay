//! One bounded pull into a separate, identity-bound local copy. The desktop owns
//! scheduling and credentials; this process never serves or promotes the copy.
use crate::replica::{
    export,
    store::{ApplyError, Store},
};
use reqwest::{Client, Method, StatusCode};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{json, Value};
use std::{fs, io::Read, path::Path, time::Duration};
use url::Url;
use zeroize::Zeroizing;

#[cfg(windows)]
#[path = "data_privacy.rs"]
mod privacy;

const APP_ID: i64 = 0x524c5943;
const MANIFEST_LIMIT: usize = 16 * 1024 * 1024;
const PART_LIMIT: usize = 16 * 1024 * 1024;
const FILE_LIMIT: usize = 10 * 1024 * 1024;
const JOB_BYTES: u64 = 512 * 1024 * 1024;
const DISK_BYTES: i64 = 8 * 1024 * 1024 * 1024;

// Deliberately no Debug/Serialize: the token only comes from the owned stdin pipe.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub schema: u32,
    pub source_url: String,
    pub transport_url: String,
    pub server_id: String,
    pub recipient_user_id: String,
    pub replica_id: String,
    pub token: Zeroizing<String>,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub schema: u32,
    pub state: &'static str,
    pub server_id: String,
    pub recipient_user_id: String,
    pub replica_id: String,
    pub lineage: String,
    pub head: i64,
    pub source_head: i64,
    pub files_pending: usize,
    pub bytes_received: u64,
    pub rows_applied: usize,
    pub source_confirmed: bool,
    pub promotion_eligible: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Failure {
    pub schema: u32,
    pub state: &'static str,
    pub code: &'static str,
    pub message: &'static str,
}

impl Failure {
    fn new(code: &'static str, message: &'static str) -> Self {
        Self {
            schema: 1,
            state: "error",
            code,
            message,
        }
    }
    pub fn exit_code(self) -> i32 {
        match self.code {
            "access_denied" => 3,
            "binding" | "storage" => 4,
            _ => 5,
        }
    }
}
fn binding() -> Failure {
    Failure::new(
        "binding",
        "This copy does not match the selected community and account.",
    )
}
fn storage() -> Failure {
    Failure::new("storage", "The local copy could not be saved. Check available space and its folder; existing data was kept.")
}
fn integrity() -> Failure {
    Failure::new(
        "integrity",
        "The copy could not be verified. The previous good copy was kept.",
    )
}
fn network() -> Failure {
    Failure::new(
        "network",
        "The community could not be reached. Relay will retry the copy.",
    )
}
fn busy() -> Failure {
    Failure::new(
        "busy",
        "Copying is temporarily busy or too large for one pass. Relay will retry.",
    )
}
fn store_error(error: ApplyError) -> Failure {
    if error == ApplyError::Database {
        storage()
    } else {
        integrity()
    }
}
fn response_error(status: StatusCode) -> Failure {
    if matches!(
        status,
        StatusCode::TOO_MANY_REQUESTS
            | StatusCode::SERVICE_UNAVAILABLE
            | StatusCode::CONFLICT
            | StatusCode::PAYLOAD_TOO_LARGE
    ) {
        busy()
    } else {
        network()
    }
}
async fn send(request: reqwest::RequestBuilder) -> Result<reqwest::Response, Failure> {
    tokio::time::timeout(Duration::from_secs(15), request.send())
        .await
        .map_err(|_| network())?
        .map_err(|_| network())
}
async fn next_chunk(
    response: &mut reqwest::Response,
) -> Result<Option<axum::body::Bytes>, Failure> {
    // An idle peer is bounded; an actively arriving body is allowed to take
    // longer than 15 seconds, still within the complete pass's 150-second bound.
    tokio::time::timeout(Duration::from_secs(15), response.chunk())
        .await
        .map_err(|_| network())?
        .map_err(|_| network())
}

fn origin(value: &str) -> Result<Url, Failure> {
    let mut url = Url::parse(value).map_err(|_| binding())?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !matches!(url.path(), "" | "/")
        || !matches!(url.scheme(), "https" | "http")
        || url.host_str().is_none()
    {
        return Err(binding());
    }
    url.set_path("/");
    Ok(url)
}
fn loopback(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        _ => false,
    }
}
fn helper_origin(url: &Url) -> bool {
    let host = url.host_str().unwrap_or_default();
    if let Some(key) = host.strip_suffix(".iroh") {
        return key.len() == 64 && key.bytes().all(|b| b.is_ascii_hexdigit());
    }
    host.parse::<std::net::Ipv4Addr>().is_ok_and(|ip| {
        let b = ip.octets();
        b[0] == 100 && (64..=127).contains(&b[1])
    })
}

impl Config {
    fn validate(&self) -> Result<(Url, Url), Failure> {
        let source = origin(&self.source_url)?;
        let transport = origin(&self.transport_url)?;
        if self.schema != 1
            || uuid::Uuid::parse_str(&self.server_id).is_err()
            || uuid::Uuid::parse_str(&self.replica_id).is_err()
            || self.recipient_user_id.is_empty()
            || self.recipient_user_id.len() > 128
            || !(32..=128).contains(&self.token.len())
            || !self
                .token
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
        {
            return Err(binding());
        }
        let direct = source == transport && (source.scheme() == "https" || loopback(&source));
        let local_name = source.scheme() == "http"
            && source.host_str() == Some("localhost")
            && transport.scheme() == "http"
            && loopback(&transport)
            && source.port_or_known_default() == transport.port_or_known_default();
        // Only the app supplies a freshly resolved and identity-bound helper.
        // Never resolve a .iroh name or silently fall back to an ordinary route.
        let helper = source.scheme() == "http"
            && helper_origin(&source)
            && transport.scheme() == "http"
            && loopback(&transport);
        if !direct && !local_name && !helper {
            return Err(binding());
        }
        Ok((source, transport))
    }
}

fn direct(path: &Path) -> Result<(), Failure> {
    if !path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(storage());
    }
    #[cfg(windows)]
    {
        use std::path::{Component, Prefix};
        if !matches!(path.components().next(), Some(Component::Prefix(p)) if matches!(p.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_)))
        {
            return Err(storage());
        }
    }
    for ancestor in path.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(info) => {
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    if info.file_attributes() & 0x400 != 0 {
                        return Err(storage());
                    }
                }
                if info.file_type().is_symlink() {
                    return Err(storage());
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(storage()),
        }
    }
    Ok(())
}

fn open_store(directory: &Path, c: &Config, source: &Url) -> Result<(Store, fs::File), Failure> {
    direct(directory)?;
    // Refuse an ordinary server directory before changing its ACL or files.
    if directory.exists() {
        for entry in fs::read_dir(directory).map_err(|_| storage())? {
            let entry = entry.map_err(|_| storage())?;
            direct(&entry.path())?;
            if !matches!(
                entry.file_name().to_str(),
                Some(
                    "copy.sqlite3"
                        | "copy.sqlite3-wal"
                        | "copy.sqlite3-shm"
                        | "copy.sqlite3-journal"
                        | "copy.lock"
                )
            ) || !entry.file_type().map_err(|_| storage())?.is_file()
            {
                return Err(storage());
            }
        }
    }
    #[cfg(windows)]
    privacy::protect_data_directory(directory).map_err(|_| storage())?;
    #[cfg(not(windows))]
    {
        fs::create_dir_all(directory).map_err(|_| storage())?;
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).map_err(|_| storage())?;
    }
    let lease = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join("copy.lock"))
        .map_err(|_| storage())?;
    lease.try_lock().map_err(|_| busy())?;
    let path = directory.join("copy.sqlite3");
    let existing = path.exists();
    let mut db = Connection::open(&path).map_err(|_| storage())?;
    db.busy_timeout(Duration::from_secs(2))
        .map_err(|_| storage())?;
    let mut initialize = !existing;
    if existing {
        let app: i64 = db
            .pragma_query_value(None, "application_id", |r| r.get(0))
            .map_err(|_| binding())?;
        let tables: i64 = db
            .query_row("SELECT COUNT(*) FROM sqlite_master", [], |r| r.get(0))
            .map_err(|_| binding())?;
        // Cancellation before the very first binding commit can leave only an
        // empty SQLite file. Resume that case; never adopt any populated DB.
        initialize = app == 0 && tables == 0;
        if app != APP_ID && !initialize {
            return Err(binding());
        }
    }
    if !initialize {
        let saved: Option<(String, String, String, String)> = db
            .query_row(
                "SELECT origin,server,user,replica FROM copy_binding WHERE id=1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()
            .map_err(|_| binding())?;
        if saved
            != Some((
                source.as_str().into(),
                c.server_id.clone(),
                c.recipient_user_id.clone(),
                c.replica_id.clone(),
            ))
        {
            return Err(binding());
        }
    } else {
        let tx = db.transaction().map_err(|_| storage())?;
        tx.execute_batch("CREATE TABLE copy_binding(id INTEGER PRIMARY KEY CHECK(id=1),origin TEXT NOT NULL,server TEXT NOT NULL,user TEXT NOT NULL,replica TEXT NOT NULL)").map_err(|_| storage())?;
        tx.execute(
            "INSERT INTO copy_binding VALUES(1,?1,?2,?3,?4)",
            params![
                source.as_str(),
                c.server_id,
                c.recipient_user_id,
                c.replica_id
            ],
        )
        .map_err(|_| storage())?;
        tx.pragma_update(None, "application_id", APP_ID)
            .map_err(|_| storage())?;
        tx.commit().map_err(|_| storage())?;
    }
    let page: i64 = db
        .pragma_query_value(None, "page_size", |r| r.get(0))
        .map_err(|_| storage())?;
    if !(512..=65536).contains(&page) {
        return Err(storage());
    }
    db.pragma_update(None, "max_page_count", DISK_BYTES / page)
        .map_err(|_| storage())?;
    db.pragma_update(None, "journal_mode", "WAL")
        .map_err(|_| storage())?;
    db.pragma_update(None, "synchronous", "FULL")
        .map_err(|_| storage())?;
    let store =
        Store::new(db, &c.server_id, &c.recipient_user_id, &c.replica_id).map_err(|_| storage())?;
    Ok((store, lease))
}

#[derive(Deserialize)]
struct Info {
    replica_format: u32,
    server_id: String,
    lineage: String,
    term: i64,
    schema_version: u32,
    head: i64,
    #[serde(default)]
    floor: i64,
}

struct Pull<'a> {
    http: Client,
    base: Url,
    c: &'a Config,
    received: u64,
    download_window: tokio::time::Instant,
    downloads: usize,
    started: tokio::time::Instant,
}
impl Pull<'_> {
    async fn download_slot(&mut self) {
        // Leave room below the source's 200-download/two-second allowance.
        if self.downloads >= 80 {
            tokio::time::sleep_until(self.download_window + Duration::from_secs(1)).await;
            self.download_window = tokio::time::Instant::now();
            self.downloads = 0;
        }
        self.downloads += 1;
    }
    async fn request(
        &mut self,
        method: Method,
        segments: &[&str],
        body: Option<&Value>,
        limit: usize,
        auth: bool,
    ) -> Result<(StatusCode, reqwest::header::HeaderMap, Vec<u8>), Failure> {
        let mut url = self.base.clone();
        url.path_segments_mut()
            .map_err(|_| binding())?
            .clear()
            .extend(segments);
        let mut request = self.http.request(method, url);
        if auth {
            request = request
                .bearer_auth(self.c.token.as_str())
                .header("x-relay-server-id", &self.c.server_id);
        }
        if let Some(body) = body {
            request = request.json(body);
        }
        let mut response = send(request).await?;
        let status = response.status();
        if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
            return Err(Failure::new(
                "access_denied",
                "Copying is no longer allowed. Sign in and check your Replica role.",
            ));
        }
        if status.is_redirection() {
            return Err(binding());
        }
        let headers = response.headers().clone();
        let cap = if status.is_success() { limit } else { 16_384 };
        if response.content_length().is_some_and(|n| n > cap as u64) {
            return Err(integrity());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = next_chunk(&mut response).await? {
            if chunk.len() > cap.saturating_sub(bytes.len()) {
                return Err(integrity());
            }
            self.received = self.received.saturating_add(chunk.len() as u64);
            if self.received > JOB_BYTES {
                return Err(busy());
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok((status, headers, bytes))
    }
    async fn json<T: DeserializeOwned>(
        &mut self,
        method: Method,
        tail: &[&str],
        body: Option<&Value>,
        limit: usize,
    ) -> Result<T, Failure> {
        let mut path = vec!["api", "replica", "pull"];
        path.extend_from_slice(tail);
        let (status, _, bytes) = self.request(method, &path, body, limit, true).await?;
        if !status.is_success() {
            return Err(response_error(status));
        }
        serde_json::from_slice(&bytes).map_err(|_| integrity())
    }
    async fn info(&mut self) -> Result<Info, Failure> {
        let info: Info = self.json(Method::GET, &["info"], None, 16_384).await?;
        if info.server_id != self.c.server_id
            || info.replica_format != export::REPLICA_FORMAT
            || info.schema_version != crate::community::SCHEMA_VERSION
            || info.head < 0
            || info.floor < 0
            || info.term < 0
            || uuid::Uuid::parse_str(&info.lineage).is_err()
        {
            return Err(binding());
        }
        Ok(info)
    }
    async fn perform(&mut self, store: &mut Store) -> Result<Report, Failure> {
        // No capability token leaves the process until the selected route answers
        // anonymously with the pinned community id (including helper loopback).
        let (status, _, health) = self
            .request(Method::GET, &["api", "health"], None, 16_384, false)
            .await?;
        let health: Value = serde_json::from_slice(&health).map_err(|_| binding())?;
        if !status.is_success() || health["server_id"] != self.c.server_id {
            return Err(binding());
        }
        let info = self.info().await?;
        let cursor = store.cursor().map_err(|_| storage())?;
        if cursor.as_ref().is_some_and(|c| {
            c.term > info.term || (c.lineage == info.lineage && c.head > info.head)
        }) {
            return Err(binding());
        }
        let pending = !store.files_pending().map_err(|_| storage())?.is_empty();
        let mut body = json!({"replica_id":self.c.replica_id,"previous_eligible":[]});
        if let Some(c) = cursor
            .as_ref()
            .filter(|c| !pending && c.lineage == info.lineage && c.head >= info.floor)
        {
            body["after"] = json!(c.head);
            body["lineage"] = json!(c.lineage);
            body["previous_eligible"] = json!(c.eligible);
        }
        let started: Value = self
            .json(Method::POST, &["exports"], Some(&body), 16_384)
            .await?;
        let id = started["export_id"]
            .as_str()
            .filter(|s| uuid::Uuid::parse_str(s).is_ok())
            .ok_or_else(integrity)?
            .to_owned();
        let result = self
            .copy_export(store, &info, &id, body["after"].as_i64())
            .await;
        if result.is_err() {
            // Best effort only: a dead source must not prevent preserving local data.
            let _ = self
                .json::<Value>(Method::DELETE, &["exports", &id], None, 16_384)
                .await;
        }
        result
    }
    async fn copy_export(
        &mut self,
        store: &mut Store,
        info: &Info,
        id: &str,
        after: Option<i64>,
    ) -> Result<Report, Failure> {
        let manifest = loop {
            let value: Value = self
                .json(Method::GET, &["exports", id], None, MANIFEST_LIMIT)
                .await?;
            if value["state"] == "building" {
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
            break serde_json::from_value::<export::Manifest>(value).map_err(|_| integrity())?;
        };
        if manifest.export_id != id
            || manifest.source_server_id != self.c.server_id
            || manifest.recipient_user_id != self.c.recipient_user_id
            || manifest.replica_id != self.c.replica_id
            || manifest.lineage != info.lineage
            || manifest.term != info.term
            || manifest.head < info.head
            || manifest.after != after
            || manifest.promotion_eligible
            || manifest.parts.len() > 4096
            || manifest.parts.iter().any(|p| p.bytes > PART_LIMIT)
            || manifest
                .parts
                .iter()
                .try_fold(0usize, |n, p| n.checked_add(p.bytes))
                .is_none_or(|n| n > export::MAX_EXPORT_BYTES)
            || manifest
                .files
                .iter()
                .any(|f| f.size < 0 || f.size as usize > FILE_LIMIT)
        {
            return Err(integrity());
        }
        let mut parts = Vec::new();
        let mut memory = 0usize;
        for (index, part) in manifest.parts.iter().enumerate() {
            self.download_slot().await;
            let (status, _, bytes) = self
                .request(
                    Method::GET,
                    &[
                        "api",
                        "replica",
                        "pull",
                        "exports",
                        id,
                        "parts",
                        &index.to_string(),
                    ],
                    None,
                    part.bytes,
                    true,
                )
                .await?;
            if !status.is_success() {
                return Err(response_error(status));
            }
            let decoded = export::parse_part(&manifest, index, &bytes).map_err(|_| integrity())?;
            for row in decoded.rows.iter().chain(&decoded.tombstones) {
                if row
                    .iter()
                    .any(|v| !matches!(v, Value::Null | Value::String(_)) && v.as_i64().is_none())
                {
                    return Err(integrity());
                }
                memory = memory.saturating_add(export::row_charge(row));
            }
            memory = memory.saturating_add(1024);
            if memory > export::MAX_EXPORT_BYTES {
                return Err(busy());
            }
            parts.push(decoded);
        }
        let export = export::assemble(manifest, parts).map_err(|_| integrity())?;
        let applied = store.apply(&export, crate::now_ms()).map_err(store_error)?;
        let available: std::collections::BTreeSet<_> = export
            .manifest
            .files
            .iter()
            .map(|f| (f.table.as_str(), f.id.as_str(), f.version, f.size))
            .collect();
        for file in store.files_pending().map_err(|_| storage())? {
            if !available.contains(&(
                file.table.as_str(),
                file.id.as_str(),
                file.version,
                file.size,
            )) {
                continue;
            }
            // A normal batch limit is progress, not a connection failure. Leave
            // time to acknowledge metadata and report the remaining file count.
            if self.received.saturating_add(file.size as u64) > JOB_BYTES
                || self.started.elapsed() > Duration::from_secs(100)
            {
                break;
            }
            self.download_slot().await;
            let mut url = self.base.clone();
            url.path_segments_mut()
                .map_err(|_| binding())?
                .clear()
                .extend([
                    "api",
                    "replica",
                    "pull",
                    "exports",
                    id,
                    "files",
                    file.table.as_str(),
                    file.id.as_str(),
                ]);
            url.query_pairs_mut()
                .append_pair("version", &file.version.to_string())
                .append_pair("size", &file.size.to_string());
            // The query is fixed metadata from the verified manifest, never an arbitrary URL.
            let response = send(
                self.http
                    .get(url)
                    .bearer_auth(self.c.token.as_str())
                    .header("x-relay-server-id", &self.c.server_id),
            )
            .await?;
            if response.status() == StatusCode::GONE {
                continue;
            }
            if response.status() == StatusCode::FORBIDDEN {
                // A file may have become unreadable while this member still
                // holds the role. Only loss of the token/role ends consent.
                self.info().await?;
                break; // retain pending progress; refresh its manifest next pass
            }
            let bytes = self.file_response(response, file.size as usize).await?;
            store
                .accept_file(&file, &bytes.0, &bytes.1)
                .map_err(store_error)?;
        }
        let files_pending = store.files_pending().map_err(|_| storage())?.len();
        let claim = json!({"applied":export.manifest.head,"files_pending":files_pending});
        let _: Value = self
            .json(Method::POST, &["exports", id, "done"], Some(&claim), 16_384)
            .await?;
        // Do not turn a failed final check into an invented up-to-date claim.
        let final_info = self.info().await?;
        if final_info.lineage != export.manifest.lineage
            || final_info.term != export.manifest.term
            || final_info.head < export.manifest.head
        {
            return Err(binding());
        }
        let source_head = final_info.head;
        Ok(Report {
            schema: 1,
            state: if files_pending > 0 {
                "files_pending"
            } else {
                "current"
            },
            server_id: self.c.server_id.clone(),
            recipient_user_id: self.c.recipient_user_id.clone(),
            replica_id: self.c.replica_id.clone(),
            lineage: export.manifest.lineage,
            head: export.manifest.head,
            source_head,
            files_pending,
            bytes_received: self.received,
            rows_applied: applied.rows,
            source_confirmed: true,
            promotion_eligible: false,
        })
    }
    async fn file_response(
        &mut self,
        mut response: reqwest::Response,
        expected: usize,
    ) -> Result<(String, Vec<u8>), Failure> {
        if matches!(
            response.status(),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
        ) {
            return Err(Failure::new(
                "access_denied",
                "Copying is no longer allowed. Check your Replica role.",
            ));
        }
        if !response.status().is_success() {
            return Err(response_error(response.status()));
        }
        let sha = response
            .headers()
            .get("x-relay-sha256")
            .and_then(|v| v.to_str().ok())
            .filter(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))
            .ok_or_else(integrity)?
            .to_owned();
        if response
            .content_length()
            .is_some_and(|n| n != expected as u64)
        {
            return Err(integrity());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = next_chunk(&mut response).await? {
            if chunk.len() > expected.saturating_sub(bytes.len()) {
                return Err(integrity());
            }
            self.received = self.received.saturating_add(chunk.len() as u64);
            if self.received > JOB_BYTES {
                return Err(busy());
            }
            bytes.extend_from_slice(&chunk);
        }
        if bytes.len() != expected {
            return Err(integrity());
        }
        Ok((sha, bytes))
    }
}

pub async fn run(config: Config, directory: &Path) -> Result<Report, Failure> {
    let (source, base) = config.validate()?;
    let (mut store, _lease) = open_store(directory, &config, &source)?;
    let http = Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(150))
        .build()
        .map_err(|_| network())?;
    let started = tokio::time::Instant::now();
    let mut pull = Pull {
        http,
        base,
        c: &config,
        received: 0,
        download_window: started,
        downloads: 0,
        started,
    };
    tokio::time::timeout(Duration::from_secs(150), pull.perform(&mut store))
        .await
        .map_err(|_| busy())?
}

pub async fn run_stdin(directory: &Path) -> Result<Report, Failure> {
    let mut bytes = Zeroizing::new(Vec::new());
    std::io::stdin()
        .lock()
        .take(8193)
        .read_to_end(&mut bytes)
        .map_err(|_| binding())?;
    if bytes.len() > 8192 {
        return Err(binding());
    }
    let config: Config = serde_json::from_slice(&bytes).map_err(|_| binding())?;
    run(config, directory).await
}

#[cfg(test)]
#[path = "replica_agent_tests.rs"]
mod tests;
