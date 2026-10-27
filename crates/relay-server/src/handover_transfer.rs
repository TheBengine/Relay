//! Complete SQLite handover image, separate from the filtered M2 member copy.
//! Each bounded part is independently HPKE-encrypted to a recipient key signed
//! by the accepted destination. A received image is standby data, never a writer.
use super::*;
use crate::handover_target::TargetKey;
use hpke_rs::hpke_types::{AeadAlgorithm, KdfAlgorithm, KemAlgorithm};
use hpke_rs::{Hpke, HpkePublicKey, Mode};
use relay_protocol::handover::{self as model, ExportStatus, TransferManifest};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{Read, Write},
    path::{Path as DiskPath, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::Instant,
};
use zeroize::Zeroizing;

const PART: usize = 1024 * 1024;
const MAX_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const MAX_PARTS: usize = MAX_BYTES as usize / PART;
const MAX_MANIFEST: u64 = 512 * 1024;
const INFO: &[u8] = b"relay-full-handover-part-hpke-v1";

#[cfg(test)]
mod tests;

pub(crate) fn hpke() -> Hpke<hpke_rs::rustcrypto::HpkeRustCrypto> {
    Hpke::new(
        Mode::Base,
        KemAlgorithm::DhKem25519,
        KdfAlgorithm::HkdfSha256,
        AeadAlgorithm::ChaCha20Poly1305,
    )
}
fn unavailable() -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "Handover export could not finish; retry or cancel the preparation",
    )
}
fn stale() -> ApiError {
    ApiError::new(
        StatusCode::CONFLICT,
        "This handover export is no longer current",
    )
}
fn root(data: &DiskPath) -> PathBuf {
    data.join("handover-exports")
}
fn folder(data: &DiskPath, preparation: &str) -> PathBuf {
    root(data).join(preparation)
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn digest(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}
pub use model::manifest_message;

fn aad(context: &model::TransferContext, index: usize) -> Vec<u8> {
    serde_json::to_vec(&("relay-full-handover-part-v1", context, index))
        .expect("bounded transfer context")
}
pub(crate) fn read_file(path: &DiskPath, limit: u64) -> std::io::Result<Vec<u8>> {
    crate::handover::direct(path)?;
    let file = fs::File::open(path)?;
    if !file.metadata()?.is_file() || file.metadata()?.len() > limit {
        return Err(std::io::Error::other("Invalid handover file"));
    }
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(std::io::Error::other("Oversized handover file"));
    }
    Ok(bytes)
}
fn write_new(path: &DiskPath, bytes: &[u8]) -> std::io::Result<()> {
    crate::handover::direct(path)?;
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

pub(crate) fn publish_new(path: &DiskPath, bytes: &[u8]) -> std::io::Result<()> {
    let pending = path.with_file_name(format!(
        "{}.pending",
        path.file_name()
            .and_then(|v| v.to_str())
            .ok_or_else(|| std::io::Error::other("Invalid handover metadata path"))?
    ));
    crate::handover::direct(&pending)?;
    if pending.try_exists()? {
        let previous = read_file(&pending, MAX_MANIFEST)?;
        if !bytes.starts_with(&previous) {
            return Err(std::io::Error::other("Unrelated pending metadata retained"));
        }
        fs::remove_file(&pending)?;
    }
    write_new(&pending, bytes)?;
    fs::hard_link(&pending, path)?;
    fs::remove_file(&pending)?;
    #[cfg(unix)]
    fs::File::open(
        path.parent()
            .ok_or_else(|| std::io::Error::other("Missing handover metadata parent"))?,
    )?
    .sync_all()?;
    Ok(())
}
// No recursive removal or arbitrary filenames: preflight the complete bounded
// owned directory before unlinking any file. Unexpected content is retained.
fn clear_owned(dir: &DiskPath) -> std::io::Result<()> {
    crate::handover::direct(dir)?;
    if !dir.try_exists()? {
        return Ok(());
    }
    let mut paths = Vec::new();
    for entry in fs::read_dir(dir)?.take(MAX_PARTS + 8) {
        let path = entry?.path();
        crate::handover::direct(&path)?;
        let name = path.file_name().and_then(|x| x.to_str()).unwrap_or("");
        let part = name
            .strip_suffix(".hpke")
            .and_then(|v| v.parse::<usize>().ok())
            .is_some_and(|v| v < MAX_PARTS && name == format!("{v}.hpke"));
        if !part
            && !matches!(
                name,
                "binding.json"
                    | "manifest.json"
                    | "manifest.pending"
                    | "snapshot.sqlite3"
                    | "snapshot.sqlite3-wal"
                    | "snapshot.sqlite3-shm"
                    | "snapshot.sqlite3-journal"
            )
        {
            return Err(std::io::Error::other("Foreign export content retained"));
        }
        if !fs::symlink_metadata(&path)?.is_file() {
            return Err(std::io::Error::other("Foreign export content retained"));
        }
        paths.push(path);
    }
    if paths.len() > MAX_PARTS + 7 {
        return Err(std::io::Error::other("Oversized export directory retained"));
    }
    for path in paths {
        fs::remove_file(path)?;
    }
    fs::remove_dir(dir)
}

struct Job {
    preparation: String,
    recipient: String,
    cancelled: Arc<AtomicBool>,
    result: ExportStatus,
}
#[derive(Default)]
pub(crate) struct Service {
    job: Mutex<Option<Job>>,
    #[cfg(test)]
    before_publish: Mutex<Option<Box<dyn FnOnce(&AppState) + Send>>>,
}
impl Service {
    pub(crate) fn discard(&self, data: &DiskPath, preparation: &str) {
        let mut job = lock(&self.job);
        if let Some(j) = job.as_ref().filter(|j| j.preparation == preparation) {
            j.cancelled.store(true, Ordering::Release);
            if matches!(j.result, ExportStatus::Building) {
                return;
            }
        }
        *job = None;
        let _ = clear_owned(&folder(data, preparation));
    }

    /// Reconcile only our bound export folders. A completed current export is
    /// resumable; plaintext and partial work left by a crashed process are not.
    /// Unknown files are retained, never recursively removed.
    pub(crate) fn recover(&self, data: &DiskPath, status: &model::Status) {
        let book = lock(&self.job);
        let staging = root(data);
        if crate::handover::direct(&staging).is_err() {
            return;
        }
        let Ok(entries) = fs::read_dir(&staging) else {
            return;
        };
        for entry in entries.take(64).flatten() {
            let dir = entry.path();
            let Some(name) = dir.file_name().and_then(|v| v.to_str()) else {
                continue;
            };
            if !Uuid::parse_str(name).is_ok_and(|id| id.to_string() == name) {
                continue;
            }
            let Some((preparation, recipient)) = read_file(&dir.join("binding.json"), 4096)
                .ok()
                .and_then(|raw| serde_json::from_slice::<(String, String)>(&raw).ok())
            else {
                continue;
            };
            if preparation != name {
                continue;
            }
            // A cancelled worker owns its files until it has stopped writing.
            // Its completion always removes them, including a late cancellation.
            if let Some(job) = book.as_ref().filter(|j| j.preparation == preparation) {
                if matches!(job.result, ExportStatus::Building) {
                    if !status
                        .preparation
                        .as_ref()
                        .is_some_and(|p| p.id == preparation && p.phase == model::Phase::Frozen)
                    {
                        job.cancelled.store(true, Ordering::Release);
                    }
                    continue;
                }
            }
            let keep = status
                .preparation
                .as_ref()
                .is_some_and(|p| p.id == preparation && p.phase == model::Phase::Frozen)
                && read_file(&dir.join("manifest.json"), MAX_MANIFEST)
                    .ok()
                    .and_then(|raw| serde_json::from_slice::<TransferManifest>(&raw).ok())
                    .is_some_and(|m| matches_status(&m, status, &recipient));
            if keep {
                for name in [
                    "snapshot.sqlite3",
                    "snapshot.sqlite3-wal",
                    "snapshot.sqlite3-shm",
                    "snapshot.sqlite3-journal",
                    "manifest.pending",
                ] {
                    let path = dir.join(name);
                    if crate::handover::direct(&path).is_ok()
                        && fs::symlink_metadata(&path).is_ok_and(|info| info.is_file())
                    {
                        let _ = fs::remove_file(path);
                    }
                }
            } else {
                let _ = clear_owned(&dir);
            }
        }
    }
}

fn authorize(
    state: &AppState,
    headers: &HeaderMap,
    input: &model::ExportRequest,
) -> ApiResult<model::Status> {
    if state.0.config.people {
        return Err(forbidden());
    }
    let auth = state.auth(headers)?;
    let db = lock(&state.0.db);
    let status = state.0.handover.status();
    let p = status.preparation.as_ref().ok_or_else(stale)?;
    if p.phase != model::Phase::Frozen
        || p.id != input.change.preparation_id
        || status.server_id != input.change.server_id
        || auth.user.id != p.target_user_id
        || !hosting::holds_replica(&db, &auth.user.id)
    {
        return Err(forbidden());
    }
    let identity = status.target_identity.as_ref().ok_or_else(stale)?;
    let message =
        model::recipient_message(&status, &input.recipient.public_key).ok_or_else(stale)?;
    let key = URL_SAFE_NO_PAD
        .decode(&input.recipient.public_key)
        .map_err(|_| forbidden())?;
    if key.len() != 32
        || !crate::handover_target::verify(
            &model::HostProof {
                identity: identity.clone(),
                signature: input.recipient.signature.clone(),
            },
            &message,
        )
    {
        return Err(forbidden());
    }
    Ok(status)
}
fn current(state: &AppState, p: &str, cancelled: &AtomicBool, deadline: Instant) -> ApiResult<()> {
    if cancelled.load(Ordering::Acquire)
        || Instant::now() >= deadline
        || state
            .0
            .handover
            .status()
            .preparation
            .is_none_or(|v| v.id != p || v.phase != model::Phase::Frozen)
    {
        return Err(stale());
    }
    Ok(())
}
fn matches_status(m: &TransferManifest, status: &model::Status, recipient: &str) -> bool {
    status
        .preparation
        .as_ref()
        .is_some_and(|p| m.context.preparation_id == p.id && m.context.target_url == p.target_url)
        && m.context.server_id == status.server_id
        && m.context.source_host_id == status.source_host_id
        && Some(&m.context.source_identity) == status.source_identity.as_ref()
        && m.context.term == status.term
        && Some(&m.context.target_identity) == status.target_identity.as_ref()
        && m.context.recipient_key == recipient
        && valid_manifest(m)
}
fn valid_manifest(m: &TransferManifest) -> bool {
    let c = &m.context;
    let count = c.database_bytes.div_ceil(PART as u64) as usize;
    let sha = |s: &str| {
        s.len() == 64
            && s.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    };
    let uuid = |s: &str| Uuid::parse_str(s).is_ok_and(|id| id.to_string() == s);
    c.schema_version == SCHEMA_VERSION
        && c.term > 0
        && (1..=MAX_BYTES).contains(&c.database_bytes)
        && [
            &c.export_id,
            &c.server_id,
            &c.source_host_id,
            &c.preparation_id,
            &c.target_identity.host_id,
        ]
        .iter()
        .all(|v| uuid(v))
        && crate::handover::target_url(&c.target_url)
        && sha(&c.database_sha256)
        && URL_SAFE_NO_PAD
            .decode(&c.recipient_key)
            .is_ok_and(|v| v.len() == 32)
        && count == m.parts.len()
        && count <= MAX_PARTS
        && m.parts.iter().enumerate().all(|(i, p)| {
            let clear = (c.database_bytes - i as u64 * PART as u64).min(PART as u64);
            p.bytes == clear + 48 && sha(&p.sha256)
        })
        && crate::handover_target::verify(
            &model::HostProof {
                identity: c.source_identity.clone(),
                signature: m.signature.clone(),
            },
            &manifest_message(c, &m.parts),
        )
}

pub(super) async fn export(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<model::ExportRequest>,
) -> ApiResult<Json<ExportStatus>> {
    let status = authorize(&state, &headers, &input)?;
    let preparation = input.change.preparation_id.clone();
    let dir = folder(&state.0.data, &preparation);
    let mut job = lock(&state.0.handover_exports.job);
    if let Some(j) = job.as_ref() {
        if j.preparation != preparation || j.recipient != input.recipient.public_key {
            return Err(stale());
        }
        if !matches!(j.result, ExportStatus::Failed) || !input.retry {
            return Ok(Json(j.result.clone()));
        }
    }
    let manifest_path = dir.join("manifest.json");
    crate::handover::direct(&manifest_path).map_err(|_| unavailable())?;
    if manifest_path.try_exists().map_err(|_| unavailable())? {
        let m: TransferManifest = serde_json::from_slice(
            &read_file(&manifest_path, MAX_MANIFEST).map_err(|_| unavailable())?,
        )
        .map_err(|_| unavailable())?;
        if !matches_status(&m, &status, &input.recipient.public_key) {
            return Err(stale());
        }
        return Ok(Json(ExportStatus::Ready { manifest: m }));
    }
    let cancelled = Arc::new(AtomicBool::new(false));
    *job = Some(Job {
        preparation: preparation.clone(),
        recipient: input.recipient.public_key.clone(),
        cancelled: cancelled.clone(),
        result: ExportStatus::Building,
    });
    let worker = state.clone();
    tokio::task::spawn_blocking(move || {
        let result = build(&worker, &status, &input.recipient.public_key, &cancelled);
        // Serialize the final cancellation check with discard(). Otherwise a
        // cancel between the last build check and publication can orphan a
        // successfully built image after the in-memory job is forgotten.
        let mut book = lock(&worker.0.handover_exports.job);
        if (result.is_err() || cancelled.load(Ordering::Acquire))
            && read_file(&dir.join("binding.json"), 4096)
                .ok()
                .and_then(|raw| serde_json::from_slice::<(String, String)>(&raw).ok())
                .is_some_and(|binding| {
                    binding == (preparation.clone(), input.recipient.public_key.clone())
                })
        {
            let _ = clear_owned(&dir);
        }
        if let Some(j) = book
            .as_mut()
            .filter(|j| j.preparation == preparation && Arc::ptr_eq(&j.cancelled, &cancelled))
        {
            if cancelled.load(Ordering::Acquire) {
                *book = None;
            } else {
                j.result = match result {
                    Ok(manifest) => ExportStatus::Ready { manifest },
                    Err(_) => ExportStatus::Failed,
                };
            }
        }
    });
    Ok(Json(ExportStatus::Building))
}

fn build(
    state: &AppState,
    status: &model::Status,
    recipient: &str,
    cancelled: &AtomicBool,
) -> ApiResult<TransferManifest> {
    let p = status.preparation.as_ref().ok_or_else(stale)?;
    let dir = folder(&state.0.data, &p.id);
    crate::handover::direct(&dir).map_err(|_| unavailable())?;
    // One current handover; a few retained failed/cancelled folders may not
    // grow into an unbounded staging store across restarts.
    let staging = root(&state.0.data);
    crate::handover_target::private_directory(&staging).map_err(|_| unavailable())?;
    if fs::read_dir(&staging)
        .map_err(|_| unavailable())?
        .take(4)
        .count()
        >= 4
    {
        return Err(unavailable());
    }
    if dir.try_exists().map_err(|_| unavailable())? {
        if fs::read_dir(&dir)
            .map_err(|_| unavailable())?
            .next()
            .is_none()
        {
            fs::remove_dir(&dir).map_err(|_| unavailable())?;
        } else {
            let binding: (String, String) = serde_json::from_slice(
                &read_file(&dir.join("binding.json"), 4096).map_err(|_| unavailable())?,
            )
            .map_err(|_| unavailable())?;
            if binding != (p.id.clone(), recipient.into()) {
                return Err(stale());
            }
            clear_owned(&dir).map_err(|_| unavailable())?;
        }
    }
    crate::handover_target::private_directory(&dir).map_err(|_| unavailable())?;
    write_new(
        &dir.join("binding.json"),
        &serde_json::to_vec(&(p.id.clone(), recipient)).map_err(|_| unavailable())?,
    )
    .map_err(|_| unavailable())?;
    let snapshot_path = dir.join("snapshot.sqlite3");
    let source = Connection::open_with_flags(
        state.0.data.join("relay.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .map_err(|_| unavailable())?;
    source
        .pragma_update(None, "query_only", true)
        .map_err(|_| unavailable())?;
    let pages: u64 = source
        .pragma_query_value(None, "page_count", |r| r.get(0))
        .map_err(|_| unavailable())?;
    let page_size: u64 = source
        .pragma_query_value(None, "page_size", |r| r.get(0))
        .map_err(|_| unavailable())?;
    if pages
        .checked_mul(page_size)
        .is_none_or(|v| v == 0 || v > MAX_BYTES)
    {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Complete handover image exceeds the 4 GiB limit",
        ));
    }
    // A bounded background job, not an HTTP timeout. Large databases need time
    // for backup, hashing and encryption; every stage remains cancellable.
    let deadline = Instant::now() + export_duration(pages * page_size);
    write_new(&snapshot_path, &[]).map_err(|_| unavailable())?;
    let mut snapshot = Connection::open(&snapshot_path).map_err(|_| unavailable())?;
    {
        let backup =
            rusqlite::backup::Backup::new(&source, &mut snapshot).map_err(|_| unavailable())?;
        loop {
            current(state, &p.id, cancelled, deadline)?;
            match backup.step(128).map_err(|_| unavailable())? {
                rusqlite::backup::StepResult::Done => break,
                rusqlite::backup::StepResult::More => {}
                _ => std::thread::sleep(Duration::from_millis(5)),
            }
        }
    }
    // The destination is trusted with the community, not with login secrets.
    crate::host_copy_secrets::strip(&snapshot).map_err(|_| unavailable())?;
    drop(snapshot);
    drop(source);
    let mut file = fs::File::open(&snapshot_path).map_err(|_| unavailable())?;
    let bytes = file.metadata().map_err(|_| unavailable())?.len();
    if bytes == 0 || bytes > MAX_BYTES {
        return Err(unavailable());
    }
    let mut hash = Sha256::new();
    let mut buffer = Zeroizing::new(vec![0u8; PART]);
    loop {
        current(state, &p.id, cancelled, deadline)?;
        let n = file.read(&mut buffer).map_err(|_| unavailable())?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    let runtime = model::HostRuntime {
        name: state.0.name.clone(),
        storage_limit_bytes: state.0.config.storage_limit_bytes,
        user_storage_limit_bytes: state.0.config.user_storage_limit_bytes,
        account_provider: state.0.config.account_provider.as_ref().map(|v| {
            let mut p = v.public();
            p.community_url = status
                .preparation
                .as_ref()
                .unwrap()
                .target_url
                .trim_end_matches('/')
                .into();
            p
        }),
        rtc_setup_required: state.0.config.rtc.is_some(),
    };
    let context = model::TransferContext {
        export_id: id(),
        server_id: status.server_id.clone(),
        source_host_id: status.source_host_id.clone(),
        source_identity: status.source_identity.clone().ok_or_else(stale)?,
        term: status.term,
        preparation_id: p.id.clone(),
        target_identity: status.target_identity.clone().ok_or_else(stale)?,
        target_url: p.target_url.clone(),
        recipient_key: recipient.into(),
        schema_version: SCHEMA_VERSION,
        database_bytes: bytes,
        database_sha256: hex(&hash.finalize()),
        runtime,
    };
    let public = HpkePublicKey::new(URL_SAFE_NO_PAD.decode(recipient).map_err(|_| forbidden())?);
    drop(file);
    let mut file = fs::File::open(&snapshot_path).map_err(|_| unavailable())?;
    let mut parts = Vec::new();
    while parts.len() < bytes.div_ceil(PART as u64) as usize {
        current(state, &p.id, cancelled, deadline)?;
        let n = (bytes - parts.len() as u64 * PART as u64).min(PART as u64) as usize;
        file.read_exact(&mut buffer[..n])
            .map_err(|_| unavailable())?;
        let (mut sealed, cipher) = hpke()
            .seal(
                &public,
                INFO,
                &aad(&context, parts.len()),
                &buffer[..n],
                None,
                None,
                None,
            )
            .map_err(|_| unavailable())?;
        sealed.extend(cipher);
        write_new(&dir.join(format!("{}.hpke", parts.len())), &sealed)
            .map_err(|_| unavailable())?;
        parts.push(model::TransferPart {
            bytes: sealed.len() as u64,
            sha256: digest(&sealed),
        });
    }
    drop(file);
    fs::remove_file(&snapshot_path).map_err(|_| unavailable())?;
    current(state, &p.id, cancelled, deadline)?;
    let signature = state
        .0
        .handover
        .sign(&manifest_message(&context, &parts));
    let manifest = TransferManifest {
        context,
        parts,
        signature,
    };
    #[cfg(test)]
    if let Some(hook) = lock(&state.0.handover_exports.before_publish).take() {
        hook(state);
    }
    let raw = serde_json::to_vec(&manifest).map_err(|_| unavailable())?;
    if raw.len() as u64 > MAX_MANIFEST {
        return Err(unavailable());
    }
    write_new(&dir.join("manifest.pending"), &raw).map_err(|_| unavailable())?;
    fs::rename(dir.join("manifest.pending"), dir.join("manifest.json"))
        .map_err(|_| unavailable())?;
    #[cfg(unix)]
    fs::File::open(&dir)
        .and_then(|f| f.sync_all())
        .map_err(|_| unavailable())?;
    Ok(manifest)
}

fn export_duration(bytes: u64) -> Duration {
    Duration::from_secs(30 + bytes.min(MAX_BYTES).div_ceil(32 * 1024 * 1024) * 8)
}

pub(super) async fn part(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<model::PartRequest>,
) -> ApiResult<Response> {
    let status = authorize(&state, &headers, &input.request)?;
    let dir = folder(&state.0.data, &input.request.change.preparation_id);
    let m: TransferManifest = serde_json::from_slice(
        &read_file(&dir.join("manifest.json"), MAX_MANIFEST).map_err(|_| stale())?,
    )
    .map_err(|_| stale())?;
    if !matches_status(&m, &status, &input.request.recipient.public_key)
        || input.export_id != m.context.export_id
    {
        return Err(stale());
    }
    let p = m.parts.get(input.index).ok_or_else(missing)?;
    let bytes = read_file(&dir.join(format!("{}.hpke", input.index)), PART as u64 + 48)
        .map_err(|_| unavailable())?;
    if bytes.len() as u64 != p.bytes || digest(&bytes) != p.sha256 {
        return Err(unavailable());
    }
    let again = authorize(&state, &headers, &input.request)?;
    if !matches_status(&m, &again, &input.request.recipient.public_key) {
        return Err(stale());
    }
    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        bytes,
    )
        .into_response())
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Received {
    pub export_id: String,
    pub preparation_id: String,
    pub server_id: String,
    pub target_host_id: String,
    pub database_sha256: String,
    pub manifest_sha256: String,
    pub database_bytes: u64,
    pub activation_allowed: bool,
}

/// The source key must have been pinned from its HTTPS/iroh route when accepting
/// the offer. The receiver checks that saved consent, the signed manifest,
/// frozen preparation and all encrypted parts and writes only to
/// its private standby folder; no source authority journal is copied.
pub fn receive(
    data: &DiskPath,
    cache: &DiskPath,
    status: &model::Status,
    key: &TargetKey,
) -> Result<Received, Box<dyn std::error::Error>> {
    let manifest: TransferManifest =
        serde_json::from_slice(&read_file(&cache.join("manifest.json"), MAX_MANIFEST)?)?;
    let recipient = key.recipient(status)?;
    if !matches_status(&manifest, status, &recipient.public_key)
        || manifest.context.target_identity != key.identity()
        || status
            .preparation
            .as_ref()
            .is_none_or(|p| p.phase != model::Phase::Frozen)
    {
        return Err("Standby image does not match this frozen handover".into());
    }
    let dir = data
        .join("handover-received")
        .join(&manifest.context.preparation_id);
    crate::handover_target::private_directory(&dir)?;
    let lock_path = dir.join("server.lock");
    crate::handover::direct(&lock_path)?;
    let lease = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)?;
    lease
        .try_lock()
        .map_err(|_| "The standby image is in use; retry after its current operation finishes")?;
    let receipt = dir.join("received.json");
    let database = dir.join("relay.sqlite3");
    let pending = dir.join("receiving.sqlite3");
    crate::handover::direct(&receipt)?;
    crate::handover::direct(&database)?;
    crate::handover::direct(&pending)?;
    let received = Received {
        export_id: manifest.context.export_id.clone(),
        preparation_id: manifest.context.preparation_id.clone(),
        server_id: manifest.context.server_id.clone(),
        target_host_id: key.identity().host_id,
        database_sha256: manifest.context.database_sha256.clone(),
        manifest_sha256: digest(&serde_json::to_vec(&manifest)?),
        database_bytes: manifest.context.database_bytes,
        activation_allowed: false,
    };
    if receipt.try_exists()? {
        let previous: Received = serde_json::from_slice(&read_file(&receipt, 4096)?)?;
        if previous != received {
            return Err("Different standby image retained".into());
        }
        if digest(&read_file(&dir.join("manifest.json"), MAX_MANIFEST)?) != received.manifest_sha256
        {
            return Err("Standby runtime manifest changed".into());
        }
        verify_database(&database, &manifest)?;
        return Ok(previous);
    }
    let marker = dir.join("transfer.json");
    crate::handover::direct(&marker)?;
    if marker.try_exists()? {
        let previous: Received = serde_json::from_slice(&read_file(&marker, 4096)?)?;
        if previous != received {
            return Err("Unrelated incomplete standby image retained".into());
        }
    } else {
        if database.try_exists()? || pending.try_exists()? {
            return Err("Unknown standby output retained".into());
        }
        publish_new(&marker, &serde_json::to_vec(&received)?)?;
    }
    let saved_manifest = dir.join("manifest.json");
    crate::handover::direct(&saved_manifest)?;
    let save_manifest = || -> Result<(), Box<dyn std::error::Error>> {
        if saved_manifest.try_exists()? {
            if digest(&read_file(&saved_manifest, MAX_MANIFEST)?) != received.manifest_sha256 {
                return Err("Standby runtime manifest changed".into());
            }
        } else {
            publish_new(&saved_manifest, &serde_json::to_vec(&manifest)?)?;
        }
        Ok(())
    };
    if database.try_exists()? {
        verify_database(&database, &manifest)?;
        save_manifest()?;
        publish_new(&receipt, &serde_json::to_vec(&received)?)?;
        return Ok(received);
    }
    // Only our exact incomplete output may be replaced, never an existing DB.
    if pending.try_exists()? {
        if !fs::symlink_metadata(&pending)?.is_file() {
            return Err("Unknown standby output retained".into());
        }
        fs::remove_file(&pending)?;
    }
    write_new(&pending, &[])?;
    let mut output = fs::OpenOptions::new().write(true).open(&pending)?;
    let keys = key.encryption_keys()?;
    let mut hash = Sha256::new();
    for (index, p) in manifest.parts.iter().enumerate() {
        let sealed = read_file(&cache.join(format!("{index}.hpke")), PART as u64 + 48)?;
        if sealed.len() as u64 != p.bytes || digest(&sealed) != p.sha256 {
            return Err("Handover part is incomplete or changed".into());
        }
        let plain = Zeroizing::new(hpke().open(
            &sealed[..32],
            keys.private_key(),
            INFO,
            &aad(&manifest.context, index),
            &sealed[32..],
            None,
            None,
            None,
        )?);
        if plain.len() + 48 != sealed.len() {
            return Err("Handover part length mismatch".into());
        }
        hash.update(&*plain);
        output.write_all(&plain)?;
    }
    output.sync_all()?;
    drop(output);
    if hex(&hash.finalize()) != manifest.context.database_sha256 {
        return Err("Handover database digest mismatch".into());
    }
    verify_database(&pending, &manifest)?;
    save_manifest()?;
    // Hard-link publication is create-only on both supported OSes. It cannot
    // overwrite a previous image or a symlink planted at the destination.
    crate::handover::direct(&database)?;
    fs::hard_link(&pending, &database)?;
    fs::remove_file(&pending)?;
    publish_new(&receipt, &serde_json::to_vec(&received)?)?;
    Ok(received)
}
pub(crate) fn verify_database(
    path: &DiskPath,
    manifest: &TransferManifest,
) -> Result<(), Box<dyn std::error::Error>> {
    crate::handover::direct(path)?;
    let db = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    db.pragma_update(None, "query_only", true)?;
    let _deadline = crate::replica::export::ReadDeadline::arm(&db, export_duration(manifest.context.database_bytes))
        .map_err(|_| "Standby verification deadline unavailable")?;
    let version: u32 = db.pragma_query_value(None, "user_version", |r| r.get(0))?;
    let integrity: String = db.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    if version != SCHEMA_VERSION
        || integrity != "ok"
        || server_id(&db)? != manifest.context.server_id
        || db.prepare("PRAGMA foreign_key_check")?.exists([])?
        || !crate::host_copy_secrets::is_stripped(&db)?
    {
        return Err("Standby database validation failed".into());
    }
    let mut file = fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = Zeroizing::new(vec![0u8; PART]);
    let mut bytes = 0u64;
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        bytes += n as u64;
        if bytes > MAX_BYTES {
            return Err("Standby database is too large".into());
        }
        hash.update(&buffer[..n]);
    }
    if bytes != manifest.context.database_bytes
        || hex(&hash.finalize()) != manifest.context.database_sha256
    {
        return Err("Standby database changed".into());
    }
    Ok(())
}

pub(crate) fn source_manifest_received(
    dir: &DiskPath,
) -> Result<TransferManifest, Box<dyn std::error::Error>> {
    let m: TransferManifest =
        serde_json::from_slice(&read_file(&dir.join("manifest.json"), MAX_MANIFEST)?)?;
    if !valid_manifest(&m) {
        return Err("Invalid signed handover manifest".into());
    }
    Ok(m)
}
pub(crate) fn source_manifest(
    data: &DiskPath,
    preparation: &str,
) -> Result<TransferManifest, Box<dyn std::error::Error>> {
    if !Uuid::parse_str(preparation).is_ok_and(|id| id.to_string() == preparation) {
        return Err("Invalid handover preparation".into());
    }
    source_manifest_received(&folder(data, preparation))
}
