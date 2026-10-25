//! The source half of planned handover: explicit consent and a durable barrier.
//!
//! The authority journal lives outside SQLite, so a database restore cannot
//! erase a source's retirement. A complete, signed image is only standby data:
//! the destination needs the source's durable grant and a completed local
//! activation checkpoint before it may serve writes. The app/route workflow
//! still gates advertising planned_handover to ordinary clients.
use super::*;
use ed25519_dalek::{Signer, SigningKey};
use relay_protocol::handover::{self as model, Phase, Preparation};
use serde::Serialize;
use std::{fs, io::Write, path::PathBuf};

#[path = "handover_activation.rs"]
pub(crate) mod activation;
#[cfg(test)]
pub(crate) mod tests;

const JOURNAL: &str = "host-authority.json";
const DB_MARKER: &str = "host_authority.v1";
const DB_TERM: &str = "host_term.v1";
const LIMIT: u64 = model::CONTROL_BYTES;
const MAX_HISTORY: usize = model::MAX_ROUTE_MOVES;
const ROOT: &str = "/api/hosting/handover";

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    schema: u32,
    server_id: String,
    source_host_id: String,
    term: u64,
    preparation: Option<Preparation>,
    #[serde(default)]
    destination: Option<model::DestinationCard>,
    /// Recorded before a database restore; restart can reconcile either known
    /// identity if the DB commit outlives the journal's final rename.
    #[serde(default)]
    restore_target: Option<String>,
    #[serde(default)]
    target_proof: Option<model::HostProof>,
    #[serde(default)]
    source_identity: Option<model::HostIdentity>,
    #[serde(default)]
    receipt_challenge: Option<String>,
    #[serde(default)]
    received: Option<model::Receipt>,
    #[serde(default)]
    retired: Option<model::ActivationGrant>,
    #[serde(default)]
    history: Vec<model::ActivationGrant>,
    #[serde(default)]
    accepted_from: Option<model::ActivationGrant>,
    #[serde(default)]
    activation_database_sha256: Option<String>,
    #[serde(default)]
    activation_pending: bool,
    #[serde(default)]
    media_epoch: Option<String>,
    #[serde(default)]
    rtc_cleanup_prefix: Option<String>,
    #[serde(default)]
    rtc_cleanup_binding: Option<String>,
    #[serde(default)]
    rtc_legacy_cleanup: bool,
}
impl Journal {
    fn route_history(&self)->Vec<model::ActivationGrant> {
        if self.history.is_empty() {
            // Older activated journals retain at least their own signed grant.
            self.accepted_from.iter().cloned().collect()
        } else { self.history.clone() }
    }
}

pub(crate) struct Authority {
    path: PathBuf,
    value: Mutex<Journal>,
    signing: SigningKey,
    drain_after: Mutex<std::time::Instant>,
    readiness: Mutex<Option<(model::ReadinessChallenge, std::time::Instant)>>,
    // An uncertain persistence operation closes admission for this process.
    uncertain: std::sync::atomic::AtomicBool,
    blocked: std::sync::atomic::AtomicBool,
    transport_changes: tokio::sync::watch::Sender<()>,
}

fn conflict(message: &'static str) -> ApiError {
    ApiError::new(StatusCode::CONFLICT, message)
}
fn storage() -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "Cannot persist handover state; retry after checking local storage",
    )
}
fn uuid(value: &str) -> bool {
    Uuid::parse_str(value).is_ok_and(|id| id.hyphenated().to_string() == value)
}

// Journals contain no login secrets, but redirects must not place the barrier
// outside this host's storage. Both existing and newly created parents count.
pub(super) fn direct(path: &FsPath) -> std::io::Result<()> {
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
                    return Err(std::io::Error::other(
                        "Redirected authority storage refused",
                    ));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(windows)]
fn replace(from: &FsPath, to: &FsPath) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::{
        core::PCWSTR,
        Win32::Storage::FileSystem::{
            MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
        },
    };
    let from: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe {
        MoveFileExW(
            PCWSTR(from.as_ptr()),
            PCWSTR(to.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    }
    .map_err(|_| std::io::Error::last_os_error())
}
#[cfg(not(windows))]
fn replace(from: &FsPath, to: &FsPath) -> std::io::Result<()> {
    fs::rename(from, to)?;
    fs::File::open(
        to.parent()
            .ok_or_else(|| std::io::Error::other("Missing authority parent"))?,
    )?
    .sync_all()
}

fn persist(path: &FsPath, journal: &Journal) -> std::io::Result<()> {
    direct(path)?;
    let pending = path.with_file_name(format!(".host-authority-{}.tmp", id()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| {
        let mut file = options.open(&pending)?;
        file.write_all(&serde_json::to_vec(journal)?)?;
        file.sync_all()?;
        drop(file);
        replace(&pending, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(pending);
    }
    result
}

impl Authority {
    fn open(data: &FsPath, server_id: String) -> Result<Self, Box<dyn std::error::Error>> {
        let path = data.join(JOURNAL);
        direct(&path)?;
        let mut value = match fs::symlink_metadata(&path) {
            Ok(info) => {
                if !info.is_file() || info.len() > LIMIT {
                    return Err("Invalid authority journal".into());
                }
                let value: Journal = serde_json::from_reader(fs::File::open(&path)?)?;
                if value.schema != model::SCHEMA
                    || (value.server_id != server_id
                        && value.restore_target.as_ref() != Some(&server_id))
                    || !uuid(&value.source_host_id)
                    || value.term == 0
                    || !activation::valid_journal(&value)
                    || value.restore_target.as_ref().is_some_and(|id| !uuid(id))
                    || value.restore_target.is_some() && value.preparation.is_some()
                    || value
                        .target_proof
                        .as_ref()
                        .is_some_and(|proof| !bound_target(&value, proof))
                    || value
                        .preparation
                        .as_ref()
                        .is_some_and(|p| !valid_preparation(p))
                {
                    return Err("Authority journal does not match this community".into());
                }
                value
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let value = Journal {
                    schema: model::SCHEMA,
                    server_id,
                    source_host_id: id(),
                    term: 1,
                    preparation: None,
                    destination: None,
                    restore_target: None,
                    target_proof: None,
                    source_identity: None,
                    receipt_challenge: None,
                    received: None,
                    retired: None,
                    history: Vec::new(),
                    accepted_from: None,
                    activation_database_sha256: None,
                    activation_pending: false,
                    media_epoch: Some(id()),
                    rtc_cleanup_prefix: None,
                    rtc_cleanup_binding: None,
                    rtc_legacy_cleanup: false,
                };
                persist(&path, &value)?;
                value
            }
            Err(error) => return Err(error.into()),
        };
        if value.source_identity.is_none() && value.preparation.is_some() {
            return Err("Cancel the earlier handover using its original Relay version before upgrading this host".into());
        }
        let signing = crate::handover_target::source_key(data, value.source_identity.as_ref())?;
        if value.source_identity.is_none() {
            value.source_identity =
                Some(crate::handover_target::identity(&signing.verifying_key()));
            persist(&path, &value)?;
        }
        if value.media_epoch.is_none() {
            value.media_epoch = Some(id());
            persist(&path, &value)?;
        }
        let blocked = value.activation_pending
            || value.restore_target.is_some()
            || value
                .preparation
                .as_ref()
                .is_some_and(|p| p.phase == Phase::Frozen);
        Ok(Self {
            path,
            value: Mutex::new(value),
            signing,
            drain_after: Mutex::new(std::time::Instant::now() + activation::lease_drain()),
            readiness: Mutex::new(None),
            uncertain: false.into(),
            blocked: blocked.into(),
            transport_changes: tokio::sync::watch::channel(()).0,
        })
    }

    pub(crate) fn open_database(
        data: &FsPath,
        db: &Connection,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        direct(&data.join(JOURNAL))?;
        let marker: Option<String> = db
            .query_row(
                "SELECT value FROM metadata WHERE key=?1",
                [DB_MARKER],
                |r| r.get(0),
            )
            .optional()?;
        if marker.is_some() && !data.join(JOURNAL).try_exists()? {
            return Err("This community's authority journal is missing; restore the host's journal before starting".into());
        }
        let authority = Self::open(data, server_id(db)?)?;
        let value = lock(&authority.value);
        let restoring = value.restore_target.is_some();
        let activation_source = value
            .accepted_from
            .as_ref()
            .filter(|_| value.activation_pending)
            .map(|g| &g.receipt.body);
        let term: Option<String> = db
            .query_row("SELECT value FROM metadata WHERE key=?1", [DB_TERM], |r| {
                r.get(0)
            })
            .optional()?;
        if !restoring
            && term.as_deref().and_then(|v| v.parse::<u64>().ok()) != Some(value.term)
            && !activation_source.is_some_and(|b| {
                term.as_deref().and_then(|v| v.parse::<u64>().ok()) == Some(b.term)
            })
            && !(term.is_none() && value.term == 1 && !value.activation_pending)
        {
            return Err("This database belongs to another authority term".into());
        }
        if !restoring
            && marker.as_ref().is_some_and(|id| {
                id != &value.source_host_id
                    && !activation_source.is_some_and(|b| id == &b.source_host_id)
            })
        {
            return Err("This database belongs to another host authority".into());
        }
        if !restoring
            && marker.is_none()
            && value
                .preparation
                .as_ref()
                .is_some_and(|p| p.phase == Phase::Frozen)
        {
            return Err("Cancel the earlier handover using its original Relay version before upgrading this host".into());
        }
        if !value.activation_pending
            && term.is_none()
            && value
                .preparation
                .as_ref()
                .is_some_and(|p| p.phase == Phase::Frozen)
        {
            return Err("Cancel the earlier handover using its original Relay version before upgrading this host".into());
        }
        let stamp = !value.activation_pending && (restoring || marker.is_none() || term.is_none());
        drop(value);
        if stamp {
            authority.stamp_database(db).map_err(|e| e.message)?;
        }
        if restoring {
            authority
                .finish_restore(server_id(db)?)
                .map_err(|e| e.message)?;
        }
        Ok(authority)
    }

    /// Kept inside the restore intent until BOTH the DB marker and the final
    /// journal agree. A crash between the writes remains a resumable restore.
    pub(crate) fn stamp_database(&self, db: &Connection) -> ApiResult<()> {
        let value = lock(&self.value);
        let actual = server_id(db)?;
        if value.server_id != actual && value.restore_target.as_ref() != Some(&actual) {
            return Err(conflict(
                "The database is not this host's prepared community",
            ));
        }
        let tx = db.unchecked_transaction()?;
        tx.execute("INSERT INTO metadata(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![DB_MARKER, value.source_host_id])?;
        tx.execute("INSERT INTO metadata(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![DB_TERM, value.term.to_string()])?;
        tx.commit()?;
        Ok(())
    }

    fn save(&self, value: &mut Journal, next: Journal) -> ApiResult<()> {
        if persist(&self.path, &next).is_err() {
            if value.activation_pending
                || value.retired.is_some()
                || next.retired.is_some()
                || value.restore_target.is_some()
                || value
                    .preparation
                    .as_ref()
                    .is_some_and(|p| p.phase == Phase::Frozen)
                || next
                    .preparation
                    .as_ref()
                    .is_some_and(|p| p.phase == Phase::Frozen)
            {
                self.uncertain
                    .store(true, std::sync::atomic::Ordering::Release);
                self.block_transports();
            }
            return Err(storage());
        }
        *value = next;
        self.uncertain
            .store(false, std::sync::atomic::Ordering::Release);
        if value.activation_pending
            || value.retired.is_some()
            || value.restore_target.is_some()
            || value
                .preparation
                .as_ref()
                .is_some_and(|p| p.phase == Phase::Frozen)
        {
            self.block_transports();
        } else {
            self.blocked
                .store(false, std::sync::atomic::Ordering::Release);
        }
        Ok(())
    }

    pub(crate) fn writes_allowed(&self) -> bool {
        !self.blocked.load(std::sync::atomic::Ordering::Acquire)
    }

    pub(crate) fn issue_locator(&self, input: relay_protocol::host_locator::Issue,
        invite_hash: [u8;32], expiry:u64) -> ApiResult<relay_protocol::host_locator::Record> {
        self.issue_locator_for(relay_protocol::host_locator::Purpose::Invitation,input,invite_hash,expiry)
    }
    pub(crate) fn issue_session_locator(&self,input:relay_protocol::host_locator::Issue,
        session_hash:[u8;32],expiry:u64)->ApiResult<relay_protocol::host_locator::Record> {
        self.issue_locator_for(relay_protocol::host_locator::Purpose::Session,input,session_hash,expiry)
    }
    fn issue_locator_for(&self,purpose:relay_protocol::host_locator::Purpose,input:relay_protocol::host_locator::Issue,
        verifier:[u8;32],expiry:u64)->ApiResult<relay_protocol::host_locator::Record> {
        let value=lock(&self.value);
        if !self.writes_allowed() || value.activation_pending || value.retired.is_some()
            || value.restore_target.is_some() || self.uncertain.load(std::sync::atomic::Ordering::Acquire)
            || value.preparation.as_ref().is_some_and(|p|p.phase == Phase::Frozen) {
            return Err(forbidden());
        }
        let current=model::RoutePin{server_id:value.server_id.clone(),host:value.source_identity.clone().ok_or_else(storage)?,term:value.term};
        crate::handover_locator::seal_for(purpose,input,verifier,expiry,current,value.route_history(),|message|self.sign(message))
    }

    pub(crate) fn sign(&self, message: &[u8]) -> String {
        URL_SAFE_NO_PAD.encode(self.signing.sign(message).to_bytes())
    }

    /// Adapt the already loaded immutable key. This grants no new authority;
    /// arming and real-write admission remain the caller's responsibility.
    pub(crate) fn takeover_signer(&self) -> crate::takeover_signer::HostKey {
        crate::takeover_signer::HostKey::from_loaded(self.signing.clone())
    }

    fn block_transports(&self) {
        if !self.blocked.swap(true, std::sync::atomic::Ordering::AcqRel) {
            self.transport_changes.send_replace(());
        }
    }

    /// Subscribe during HTTP admission, before on_upgrade releases its guard.
    /// A freeze followed by a quick cancellation still closes the old socket.
    pub(crate) fn watch_transports(&self) -> tokio::sync::watch::Receiver<()> {
        self.transport_changes.subscribe()
    }

    /// An explicitly restored database may have a different community identity.
    /// The local journal must follow that authorized restore, never silently do
    /// so on startup (which could otherwise erase an existing host's barrier).
    pub(crate) fn begin_restore(&self, server_id: String) -> ApiResult<()> {
        let mut value = lock(&self.value);
        if value.accepted_from.is_some() && value.server_id != server_id {
            return Err(conflict("Restore a different community into its own data directory; this host's authority is bound to its community"));
        }
        if value.retired.is_some()
            || value.activation_pending
            || value
                .preparation
                .as_ref()
                .is_some_and(|p| p.phase == Phase::Frozen)
            || self.uncertain.load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(conflict(
                "Cancel handover preparation before restoring a database",
            ));
        }
        let mut next = value.clone();
        next.restore_target = Some(server_id);
        next.preparation = None;
        next.destination = None;
        next.target_proof = None;
        self.save(&mut value, next)
    }

    pub(crate) fn finish_restore(&self, server_id: String) -> ApiResult<()> {
        let mut value = lock(&self.value);
        if value.server_id != server_id && value.restore_target.as_ref() != Some(&server_id) {
            return Err(conflict(
                "The restored database is not the prepared community",
            ));
        }
        let mut next = value.clone();
        next.server_id = server_id;
        next.restore_target = None;
        self.save(&mut value, next).map_err(|_| {
            ApiError::new(StatusCode::SERVICE_UNAVAILABLE,
            "Database restore reached its journal checkpoint. Restart this host to finish recovery")
        })
    }

    /// Called inside the existing maintenance read/write guard, never before
    /// waiting for it: an older queued request must see the new barrier too.
    pub(crate) fn admit(&self, path: &str) -> ApiResult<()> {
        if self.writes_allowed()
            || (path == "/api/bootstrap" && self.paused_view())
            || matches!(
                path,
                "/health"
                    | "/api/hosting/handover/readiness"
                    | "/api/health"
                    | ROOT
                    | "/api/hosting/handover/route"
                    | "/api/hosting/handover/cancel"
                    | "/api/hosting/handover/export"
                    | "/api/hosting/handover/part"
                    | "/api/hosting/handover/receipt"
                    | "/api/hosting/handover/retire"
                    | "/api/local/shutdown"
            )
        {
            return Ok(());
        }
        Err(conflict(
            "This host is paused for handover; check its handover status",
        ))
    }

    pub(crate) fn paused_view(&self) -> bool {
        let j = lock(&self.value);
        j.retired.is_none()
            && !j.activation_pending
            && j.restore_target.is_none()
            && !self.uncertain.load(std::sync::atomic::Ordering::Acquire)
            && j.preparation
                .as_ref()
                .is_some_and(|p| p.phase == Phase::Frozen)
    }

    pub(crate) fn route_pin(&self) -> model::RoutePin {
        let value = lock(&self.value);
        model::RoutePin {
            server_id: value.server_id.clone(),
            host: value.source_identity.clone().expect("validated source key"),
            term: value.term,
        }
    }

    pub(super) fn status(&self) -> model::Status {
        let value = lock(&self.value);
        model::Status {
            server_id: value.server_id.clone(),
            source_host_id: value.source_host_id.clone(),
            term: value.term,
            writes_allowed: !value.activation_pending
                && value.retired.is_none()
                && !self.uncertain.load(std::sync::atomic::Ordering::Acquire)
                && value.restore_target.is_none()
                && !value
                    .preparation
                    .as_ref()
                    .is_some_and(|p| p.phase == Phase::Frozen),
            preparation: value.preparation.clone(),
            activation_available: value.retired.is_some(),
            target_identity: value
                .target_proof
                .as_ref()
                .map(|proof| proof.identity.clone()),
            source_identity: value.source_identity.clone(),
            receipt_challenge: value.receipt_challenge.clone(),
            received: value.received.clone(),
            retired: value.retired.clone(),
            destination: value.destination.clone(),
            history: value.route_history(),
        }
    }
}

pub(super) fn target_url(raw: &str) -> bool {
    raw.len() <= 2048
        && url::Url::parse(raw).is_ok_and(|url| {
            url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none()
                && url.path() == "/"
                && url.host_str().is_some()
                && (url.scheme() == "https"
                    || url.scheme() == "http"
                        && url.host_str().is_some_and(|host| {
                            host.strip_suffix(".iroh").is_some_and(|key| {
                                key.len() == 64 && key.bytes().all(|b| b.is_ascii_hexdigit())
                            })
                        }))
        })
}
pub(super) fn destination_route(raw: &str, route: Option<&model::IrohDestination>) -> bool {
    let Ok(url) = url::Url::parse(raw) else { return false };
    let Some(route) = route else { return url.scheme() == "https" };
    if url.scheme() != "http" || url.port() != Some(4580)
        || url.host_str() != Some(format!("{}.iroh", route.endpoint_id).as_str())
        || route.endpoint_id.len() != 64
        || !route.endpoint_id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        || route.token.len() != 43
        || !route.token.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b,b'_'|b'-'))
        || route.scope.as_ref().is_some_and(|scope| !uuid(scope))
    { return false; }
    route.relay.as_ref().is_none_or(|relay| relay.len() <= 200 && url::Url::parse(relay).is_ok_and(|u|
        u.scheme()=="https" && u.host_str().is_some() && u.username().is_empty()
        && u.password().is_none() && u.query().is_none() && u.fragment().is_none()
        && u.path()=="/" && u.port()!=Some(0)))
}
pub(super) fn destination_matches(card: &model::DestinationCard, source: &model::RoutePin, user: &str, host: &str, url: &str) -> bool {
    let b = &card.body;
    b.source == *source && b.user_id == user && b.target.host_id == host
        && b.target != source.host && b.target_url == url
        && target_url(url) && destination_route(url,b.private_route.as_ref())
        && b.private_route.as_ref().and_then(|r| r.scope.as_ref()).is_none_or(|scope| *scope == b.target.host_id)
        && crate::handover_target::verify(&model::HostProof {
            identity:b.target.clone(),signature:card.signature.clone(),
        },&model::destination_message(b))
}
fn valid_preparation(p: &Preparation) -> bool {
    uuid(&p.id)
        && uuid(&p.target_host_id)
        && uuid(&p.target_user_id)
        && uuid(&p.offered_by)
        && target_url(&p.target_url)
}
fn bound_target(journal: &Journal, proof: &model::HostProof) -> bool {
    journal
        .source_identity
        .as_ref()
        .zip(journal.preparation.as_ref())
        .is_some_and(|(source, p)| {
            proof.identity.host_id == p.target_host_id
                && crate::handover_target::verify(
                    proof,
                    &model::acceptance_message(
                        &journal.server_id,
                        &journal.source_host_id,
                        source,
                        journal.term,
                        p,
                    ),
                )
        })
}
fn preparation<'a>(journal: &'a Journal, input: &model::Change) -> ApiResult<&'a Preparation> {
    if journal.server_id != input.server_id {
        return Err(conflict("This preparation belongs to another community"));
    }
    journal
        .preparation
        .as_ref()
        .filter(|p| p.id == input.preparation_id)
        .ok_or_else(|| conflict("This handover preparation is no longer current"))
}

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route(ROOT, get(status))
        .route("/api/hosting/handover/route", post(route))
        .route(relay_protocol::host_locator::ISSUE_PATH, post(crate::handover_locator::issue))
        .route("/api/hosting/handover/offer", post(offer))
        .route("/api/hosting/handover/answer", post(answer))
        .route("/api/hosting/handover/freeze", post(freeze))
        .route("/api/hosting/handover/cancel", post(cancel))
        .route("/api/hosting/handover/receipt", post(activation::receipt))
        .route(
            "/api/hosting/handover/readiness",
            post(activation::readiness),
        )
        .route("/api/hosting/handover/retire", post(activation::retire))
        .route(
            "/api/hosting/handover/export",
            post(crate::handover_transfer::export),
        )
        .route(
            "/api/hosting/handover/part",
            post(crate::handover_transfer::part),
        )
        .layer(DefaultBodyLimit::max(4096))
}
fn community(state: &AppState) -> ApiResult<()> {
    if state.0.config.people {
        return Err(forbidden());
    }
    Ok(())
}
async fn route(
    State(state): State<AppState>,
    Json(input): Json<model::RouteChallenge>,
) -> ApiResult<Json<model::RouteProof>> {
    community(&state)?;
    if input.challenge.len() != 43
        || !URL_SAFE_NO_PAD
            .decode(&input.challenge)
            .is_ok_and(|v| v.len() == 32)
    {
        return Err(bad("Use a fresh 32-byte route challenge"));
    }
    let value = lock(&state.0.handover.value);
    let state_value = model::RouteState {
        pin: model::RoutePin {
            server_id: value.server_id.clone(),
            host: value.source_identity.clone().ok_or_else(storage)?,
            term: value.term,
        },
        challenge: input.challenge,
        writes_allowed: state.writes_allowed(),
        moved: value.retired.clone(),
        history: value.route_history(),
    };
    let signature = state.0.handover.sign(&model::route_message(&state_value));
    Ok(Json(model::RouteProof {
        state: state_value,
        signature,
    }))
}
async fn status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<model::Status>> {
    community(&state)?;
    let auth = state.auth(&headers)?;
    let status = state.0.handover.status();
    let db = lock(&state.0.db);
    if !roles::has(&db, &auth.user.id, PERMISSION_ADMIN)
        && !hosting::holds_replica(&db, &auth.user.id)
        && !status
            .preparation
            .as_ref()
            .is_some_and(|p| p.target_user_id == auth.user.id)
    {
        return Err(forbidden());
    }
    Ok(Json(status))
}
async fn offer(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<model::Offer>,
) -> ApiResult<Json<model::Status>> {
    community(&state)?;
    let auth = state.auth(&headers)?;
    let db = lock(&state.0.db);
    roles::require(&db, &auth.user.id, PERMISSION_ADMIN)?;
    if !hosting::holds_replica(&db, &input.target_user_id) {
        return Err(forbidden());
    }
    let mut journal = lock(&state.0.handover.value);
    if journal.term == u64::MAX {
        return Err(conflict("Authority term is exhausted"));
    }
    if journal.server_id != input.server_id
        || !uuid(&input.target_host_id)
        || input.target_host_id == journal.source_host_id
        || journal.source_identity.as_ref().is_some_and(|source|input.target_host_id==source.host_id)
        || !target_url(&input.target_url)
    {
        return Err(bad(
            "Choose a different approved host and a secure destination",
        ));
    }
    if journal.retired.is_some() || journal.activation_pending || journal.preparation.is_some() {
        return Err(conflict("Cancel the existing preparation first"));
    }
    if journal.route_history().len() >= MAX_HISTORY {
        return Err(conflict("This community has reached the supported host-move history limit"));
    }
    let pin = model::RoutePin { server_id: journal.server_id.clone(),
        host: journal.source_identity.clone().ok_or_else(storage)?, term: journal.term };
    if input.destination.as_ref().is_some_and(|card| !destination_matches(card,&pin,
        &input.target_user_id,&input.target_host_id,&input.target_url))
        || input.destination.is_none() && !destination_route(&input.target_url,None)
    { return Err(bad("The destination setup code does not match this move")); }
    let mut next = journal.clone();
    next.target_proof = None;
    next.destination = input.destination;
    next.preparation = Some(Preparation {
        id: id(),
        target_user_id: input.target_user_id,
        target_host_id: input.target_host_id,
        target_url: input.target_url,
        offered_by: auth.user.id,
        phase: Phase::Offered,
        updated_ms: now_ms(),
    });
    state.0.handover.save(&mut journal, next)?;
    drop(journal);
    state
        .0
        .handover_exports
        .recover(&state.0.data, &state.0.handover.status());
    Ok(Json(state.0.handover.status()))
}
async fn answer(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<model::Answer>,
) -> ApiResult<Json<model::Status>> {
    community(&state)?;
    let auth = state.auth(&headers)?;
    let db = lock(&state.0.db);
    if !hosting::holds_replica(&db, &auth.user.id) {
        return Err(forbidden());
    }
    let mut journal = lock(&state.0.handover.value);
    let current = preparation(
        &journal,
        &model::Change {
            server_id: input.server_id,
            preparation_id: input.preparation_id,
        },
    )?;
    if current.target_user_id != auth.user.id {
        return Err(forbidden());
    }
    if current.phase != Phase::Offered {
        return Err(conflict("This preparation has already been answered"));
    }
    let mut next = journal.clone();
    if input.accept_full_server_data {
        if let Some(proof) = &input.target_proof {
            if !bound_target(&journal, proof) {
                return Err(forbidden());
            }
        }
        next.target_proof = input.target_proof;
        let p = next.preparation.as_mut().unwrap();
        p.phase = Phase::Accepted;
        p.updated_ms = now_ms();
    } else {
        next.preparation = None;
        next.destination = None;
        next.target_proof = None;
    }
    state.0.handover.save(&mut journal, next)?;
    drop(journal);
    Ok(Json(state.0.handover.status()))
}
async fn freeze(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<model::Change>,
) -> ApiResult<Json<model::Status>> {
    community(&state)?;
    let auth = state.auth(&headers)?;
    let db = lock(&state.0.db);
    roles::require(&db, &auth.user.id, PERMISSION_ADMIN)?;
    let mut journal = lock(&state.0.handover.value);
    let current = preparation(&journal, &input)?;
    if current.phase != Phase::Accepted {
        return Err(conflict(
            "The chosen host must accept full server data first",
        ));
    }
    if journal.target_proof.is_none() {
        return Err(conflict(
            "The chosen host must prove its own identity before handover",
        ));
    }
    if !hosting::holds_replica(&db, &current.target_user_id) {
        return Err(forbidden());
    }
    let mut next = journal.clone();
    let p = next.preparation.as_mut().unwrap();
    p.phase = Phase::Frozen;
    p.updated_ms = now_ms();
    if journal.rtc_cleanup_prefix.is_some() || journal.rtc_legacy_cleanup {
        return Err(conflict(
            "Wait for the previous video room cleanup before another handover",
        ));
    }
    next.receipt_challenge = Some(URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>()));
    next.received = None;
    if state.0.config.rtc.is_some() {
        next.rtc_cleanup_prefix = Some(media_prefix(&journal));
        next.rtc_cleanup_binding = state.0.config.rtc.as_ref().map(|c| c.handover_binding());
        next.rtc_legacy_cleanup = true;
    }
    next.media_epoch = Some(id());
    // Existing HTTP work is drained by dispatch's exclusive maintenance guard.
    // query_only also stops background SQLite writers during the preparation.
    db.pragma_update(None, "query_only", true)?;
    state.0.handover.save(&mut journal, next)?;
    *lock(&state.0.handover.drain_after) = std::time::Instant::now() + activation::lease_drain();
    drop(journal);
    drop(db);
    state.revoke_all_rtc();
    crate::voice::revoke_all_direct(&state);
    lock(&state.0.voice).clear();
    lock(&state.0.frames).clear();
    lock(&state.0.typing).clear();
    state.notify("refresh", None, None, None);
    Ok(Json(state.0.handover.status()))
}
async fn cancel(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<model::Change>,
) -> ApiResult<Json<model::Status>> {
    community(&state)?;
    let auth = state.auth(&headers)?;
    let db = lock(&state.0.db);
    let mut journal = lock(&state.0.handover.value);
    let current = preparation(&journal, &input)?;
    if !roles::has(&db, &auth.user.id, PERMISSION_ADMIN) && current.target_user_id != auth.user.id {
        return Err(forbidden());
    }
    if journal.retired.is_some()
        || state
            .0
            .handover
            .uncertain
            .load(std::sync::atomic::Ordering::Acquire)
    {
        return Err(conflict(
            "A retired or uncertain source cannot be resumed; inspect its durable handover status",
        ));
    }
    let mut next = journal.clone();
    next.preparation = None;
        next.destination = None;
    next.target_proof = None;
    next.received = None;
    next.receipt_challenge = None;
    state.0.handover.save(&mut journal, next)?;
    if let Err(error) = db.pragma_update(None, "query_only", false) {
        state
            .0
            .handover
            .uncertain
            .store(true, std::sync::atomic::Ordering::Release);
        state.0.handover.block_transports();
        return Err(error.into());
    }
    drop(journal);
    drop(db);
    state
        .0
        .handover_exports
        .discard(&state.0.data, &input.preparation_id);
    Ok(Json(state.0.handover.status()))
}

fn media_prefix(j: &Journal) -> String {
    format!(
        "relay-{}-{}-",
        j.server_id,
        j.media_epoch.as_deref().expect("persisted media epoch")
    )
}
impl Authority {
    #[cfg(test)]
    pub(crate) fn restart_media_wait_for_test(&self) {
        *lock(&self.drain_after) = std::time::Instant::now() + activation::lease_drain();
    }
    #[cfg(test)]
    pub(crate) fn elapse_media_leases(&self) {
        *lock(&self.drain_after) = std::time::Instant::now();
    }
    pub(crate) fn media_room(&self, channel: &str) -> String {
        format!("{}{channel}", media_prefix(&lock(&self.value)))
    }
}
