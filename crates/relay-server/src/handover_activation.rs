//! Planned authority transfer. A receipt is only an attestation; only a grant
//! durably recorded by the irrevocably retired source permits activation.
use super::*;
use crate::handover_transfer as transfer;
use model::{ActivationGrant, Receipt, ReceiptBody};

type LocalResult<T> = Result<T, Box<dyn std::error::Error>>;

pub(super) fn lease_drain() -> Duration {
    Duration::from_millis(
        relay_protocol::direct_voice::LEASE_MS
            .max(u64::from(relay_protocol::gateway_broker::LEASE_TTL_MS)),
    )
}
fn sha(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|v| format!("{v:02x}"))
        .collect()
}
use model::{grant_message, receipt_message};
fn digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|v| v.is_ascii_digit() || (b'a'..=b'f').contains(&v))
}
fn valid_receipt(receipt: &Receipt) -> bool {
    let b = &receipt.body;
    b.term > 0
        && b.database_bytes > 0
        && b.database_bytes <= 4 * 1024 * 1024 * 1024
        && [
            &b.server_id,
            &b.source_host_id,
            &b.preparation_id,
            &b.export_id,
        ]
        .iter()
        .all(|v| uuid(v))
        && target_url(&b.target_url)
        && digest(&b.manifest_sha256)
        && digest(&b.database_sha256)
        && URL_SAFE_NO_PAD
            .decode(&b.challenge)
            .is_ok_and(|v| v.len() == 32)
        && crate::handover_target::verify(
            &model::HostProof {
                identity: b.target_identity.clone(),
                signature: receipt.signature.clone(),
            },
            &receipt_message(b),
        )
}
pub(crate) fn valid_grant(grant: &ActivationGrant) -> bool {
    valid_receipt(&grant.receipt)
        && grant.history_sha256.as_deref().is_none_or(digest)
        && grant.receipt.body.term.checked_add(1) == Some(grant.next_term)
        && crate::handover_target::verify(
            &model::HostProof {
                identity: grant.source_identity.clone(),
                signature: grant.signature.clone(),
            },
            &grant_message(grant),
        )
}
/// Each edge is independently source-authorized and target-attested. The
/// current key/term must be the exact end, never a same-server-ID shortcut.
pub(crate) fn valid_history(history:&[ActivationGrant], pin:&model::RoutePin)->bool {
    if history.len() > MAX_HISTORY { return false; }
    let Some(first)=history.first() else { return true; };
    let mut host=first.source_identity.clone();
    let mut term=first.receipt.body.term;
    for (index,grant) in history.iter().enumerate() {
        let receipt=&grant.receipt.body;
        if !valid_grant(grant) || grant.source_identity != host
            || receipt.server_id != pin.server_id || receipt.term != term
            || receipt.target_identity == host {
            return false;
        }
        if grant.history_sha256.as_ref().is_some_and(|hash|
            *hash != sha(&model::history_message(&history[..index]))) { return false; }
        host=receipt.target_identity.clone();
        term=grant.next_term;
    }
    host == pin.host && term == pin.term
}
/// A takeover term stands on its own mandate, never mixed with a planned move.
fn takeover_matches(j: &Journal) -> bool {
    j.accepted_from.is_none()
        && j.history.is_empty()
        && j.takeover.as_ref().and_then(crate::takeover_failover::mandate_route)
            == j.source_identity.as_ref().map(|host| model::RoutePin {
                server_id: j.server_id.clone(),
                host: host.clone(),
                term: j.term,
            })
}
fn receipt_scope(j: &Journal, r: &Receipt) -> bool {
    let b = &r.body;
    valid_receipt(r)
        && b.server_id == j.server_id
        && b.source_host_id == j.source_host_id
        && b.term == j.term
        && Some(&b.challenge) == j.receipt_challenge.as_ref()
        && j.preparation.as_ref().is_some_and(|p| {
            p.phase == Phase::Frozen
                && p.id == b.preparation_id
                && p.target_url == b.target_url
                && p.target_host_id == b.target_identity.host_id
        })
        && j.target_proof
            .as_ref()
            .is_some_and(|p| p.identity == b.target_identity)
}
fn epoch_prefix(value: &str) -> bool {
    let Some(rest) = value.strip_prefix("relay-") else {
        return false;
    };
    rest.len() == 74
        && uuid(&rest[..36])
        && &rest[36..37] == "-"
        && uuid(&rest[37..73])
        && &rest[73..] == "-"
}
pub(super) fn valid_journal(j: &Journal) -> bool {
    (j.term == 1 || j.accepted_from.is_some() || takeover_matches(j))
        && j.source_identity.as_ref().is_none_or(|host|valid_history(&j.route_history(),
            &model::RoutePin{server_id:j.server_id.clone(),host:host.clone(),term:j.term}))
        && (j.history.is_empty() || j.history.last() == j.accepted_from.as_ref())
        && j.destination.as_ref().is_none_or(|card|
            j.preparation.as_ref().zip(j.source_identity.as_ref()).is_some_and(|(p,host)|
                destination_matches(card,&model::RoutePin{server_id:j.server_id.clone(),host:host.clone(),term:j.term},
                    &p.target_user_id,&p.target_host_id,&p.target_url)))
        && j.media_epoch.as_ref().is_none_or(|v| uuid(v))
        && j.rtc_cleanup_prefix
            .as_ref()
            .is_none_or(|p| p.is_ascii() && epoch_prefix(p))
        && (!j.rtc_legacy_cleanup || j.rtc_cleanup_prefix.is_some())
        && (j.rtc_cleanup_prefix.is_some() == j.rtc_cleanup_binding.is_some())
        && j.rtc_cleanup_binding.as_deref().is_none_or(digest)
        && j.received.as_ref().is_none_or(|r| receipt_scope(j, r))
        && j.retired.as_ref().is_none_or(|g| {
            valid_grant(g)
                && j.route_history().len() < MAX_HISTORY
                && g.history_sha256.as_ref().is_none_or(|hash|*hash == sha(&model::history_message(&j.route_history())))
                && j.received.as_ref() == Some(&g.receipt)
                && receipt_scope(j, &g.receipt)
                && j.source_identity.as_ref() == Some(&g.source_identity)
                && !j.activation_pending
        })
        && j.accepted_from.as_ref().is_none_or(|g| {
            valid_grant(g)
                && g.receipt.body.server_id == j.server_id
                && g.next_term == j.term
                && g.receipt.body.target_identity.host_id == j.source_host_id
                && j.source_identity.as_ref() == Some(&g.receipt.body.target_identity)
        })
        && (!j.activation_pending
            || (j.accepted_from.is_some()
                && j.preparation.is_none()
                && j.activation_database_sha256.as_deref().is_some_and(digest)))
}

fn image_matches(receipt: &Receipt, manifest: &model::TransferManifest) -> bool {
    let b = &receipt.body;
    let c = &manifest.context;
    b.server_id == c.server_id
        && b.source_host_id == c.source_host_id
        && b.term == c.term
        && b.preparation_id == c.preparation_id
        && b.export_id == c.export_id
        && b.target_identity == c.target_identity
        && b.target_url == c.target_url
        && b.database_sha256 == c.database_sha256
        && b.database_bytes == c.database_bytes
        && serde_json::to_vec(manifest).is_ok_and(|v| sha(&v) == b.manifest_sha256)
}

pub(super) async fn receipt(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<Receipt>,
) -> ApiResult<Json<model::Status>> {
    community(&state)?;
    let auth = state.auth(&headers)?;
    let db = lock(&state.0.db);
    let mut j = lock(&state.0.handover.value);
    let p = j
        .preparation
        .as_ref()
        .ok_or_else(|| conflict("No handover is prepared"))?;
    if auth.user.id != p.target_user_id
        || !hosting::holds_replica(&db, &auth.user.id)
        || !receipt_scope(&j, &input)
    {
        return Err(forbidden());
    }
    if j.retired.is_some() {
        if j.received.as_ref() != Some(&input) {
            return Err(conflict("The source is already retired"));
        }
    } else {
        let manifest = transfer::source_manifest(&state.0.data, &p.id)
            .map_err(|_| conflict("The complete export is not ready"))?;
        if !image_matches(&input, &manifest) {
            return Err(conflict("The receipt is for another image"));
        }
        if j.received.as_ref().is_some_and(|old| old != &input) {
            return Err(conflict("A different receipt is already recorded"));
        }
        let mut next = j.clone();
        next.received = Some(input);
        state.0.handover.save(&mut j, next)?;
    }
    drop(j);
    drop(db);
    Ok(Json(state.0.handover.status()))
}

pub(super) async fn retire(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<model::Retire>,
) -> ApiResult<Json<ActivationGrant>> {
    community(&state)?;
    let auth = state.auth(&headers)?;
    if !input.confirm_stop_source {
        return Err(bad("Confirm permanent source retirement before continuing"));
    }
    {
        let db = lock(&state.0.db);
        roles::require(&db, &auth.user.id, PERMISSION_ADMIN)?;
        let j = lock(&state.0.handover.value);
        preparation(&j, &input.change)?;
        if let Some(grant) = &j.retired {
            return Ok(Json(grant.clone()));
        }
        if j.preparation
            .as_ref()
            .is_none_or(|p| p.phase != Phase::Frozen)
            || j.received.is_none()
        {
            return Err(conflict(
                "The chosen host must confirm the full image first",
            ));
        }
    }
    if std::time::Instant::now() < *lock(&state.0.handover.drain_after) {
        return Err(conflict(
            "Wait for the old direct and gateway voice leases to expire",
        ));
    }
    state.flush_handover_media().await?;
    state.flush_rtc_removals().await?;
    let db = lock(&state.0.db);
    roles::require(&db, &auth.user.id, PERMISSION_ADMIN)?;
    let mut j = lock(&state.0.handover.value);
    let p = preparation(&j, &input.change)?;
    if state
        .0
        .handover
        .uncertain
        .load(std::sync::atomic::Ordering::Acquire)
    {
        return Err(conflict(
            "Restart the source to reconcile its uncertain handover journal",
        ));
    }
    if p.phase != Phase::Frozen
        || j.rtc_cleanup_prefix.is_some()
        || j.rtc_legacy_cleanup
        || !lock(&state.0.voice).is_empty()
        || !lock(&state.0.rtc_members).is_empty()
    {
        return Err(conflict("The old host has not finished draining its media"));
    }
    if let Some(grant) = &j.retired {
        return Ok(Json(grant.clone()));
    }
    if j.route_history().len() >= MAX_HISTORY {
        return Err(conflict("This community has reached the supported host-move history limit"));
    }
    let received = j
        .received
        .clone()
        .ok_or_else(|| conflict("No destination receipt"))?;
    let manifest = transfer::source_manifest(&state.0.data, &p.id)
        .map_err(|_| conflict("The complete export is not ready"))?;
    if !receipt_scope(&j, &received)
        || !image_matches(&received, &manifest)
        || !hosting::holds_replica(&db, &p.target_user_id)
    {
        return Err(forbidden());
    }
    check_readiness(&state, &j, &received, &manifest, input.readiness.as_ref())?;
    let mut grant = ActivationGrant {
        receipt: received,
        source_identity: j.source_identity.clone().ok_or_else(storage)?,
        next_term: j
            .term
            .checked_add(1)
            .ok_or_else(|| conflict("Authority term is exhausted"))?,
        history_sha256: Some(sha(&model::history_message(&j.route_history()))),
        signature: String::new(),
    };
    grant.signature = state.0.handover.sign(&grant_message(&grant));
    let mut next = j.clone();
    next.retired = Some(grant.clone());
    // Nothing returns the grant before the durable barrier. Cancellation,
    // restore and restart all preserve this retirement, even if the reply is lost.
    state.0.handover.save(&mut j, next)?;
    Ok(Json(grant))
}

pub(super) async fn readiness(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<model::Change>,
) -> ApiResult<Json<model::ReadinessChallenge>> {
    community(&state)?;
    let auth = state.auth(&headers)?;
    let db = lock(&state.0.db);
    roles::require(&db, &auth.user.id, PERMISSION_ADMIN)?;
    let j = lock(&state.0.handover.value);
    let p = preparation(&j, &input)?;
    let receipt = j
        .received
        .as_ref()
        .ok_or_else(|| conflict("The new host must verify the full image first"))?;
    if p.phase != Phase::Frozen || j.retired.is_some() || !receipt_scope(&j, receipt) {
        return Err(conflict(
            "No paused handover is ready for a destination check",
        ));
    }
    let mut request = model::ReadinessChallenge {
        source: model::RoutePin {
            server_id: j.server_id.clone(),
            host: j.source_identity.clone().ok_or_else(storage)?,
            term: j.term,
        },
        source_host_id: j.source_host_id.clone(),
        preparation_id: p.id.clone(),
        receipt_sha256: sha(&serde_json::to_vec(receipt).map_err(|_| storage())?),
        challenge: URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>()),
        expires_at: now().saturating_add(40),
        signature: String::new(),
    };
    request.signature = state
        .0
        .handover
        .sign(&model::readiness_challenge_message(&request));
    *lock(&state.0.handover.readiness) = Some((
        request.clone(),
        std::time::Instant::now() + Duration::from_secs(40),
    ));
    Ok(Json(request))
}

fn check_readiness(
    state: &AppState,
    j: &Journal,
    receipt: &Receipt,
    manifest: &model::TransferManifest,
    proof: Option<&model::ReadyProof>,
) -> ApiResult<()> {
    let proof=proof.ok_or_else(||conflict("Check the prepared destination's live connection before permanently stopping this host"))?;
    let held = lock(&state.0.handover.readiness);
    let (wanted, until) = held
        .as_ref()
        .ok_or_else(|| conflict("Request a fresh destination check"))?;
    let b = &proof.body;
    let r = &receipt.body;
    if std::time::Instant::now() > *until
        || wanted.expires_at <= now()
        || &b.request != wanted
        || wanted.source.server_id != j.server_id
        || wanted.source.term != j.term
        || Some(&wanted.source.host) != j.source_identity.as_ref()
        || wanted.source_host_id != j.source_host_id
        || wanted.preparation_id != r.preparation_id
        || wanted.receipt_sha256 != sha(&serde_json::to_vec(receipt).map_err(|_| storage())?)
        || b.target != r.target_identity
        || b.target_url != r.target_url
        || !uuid(&b.instance_id)
        || b.http_port == 0
        || b.voice_port == 0
        || !crate::handover_target::verify(
            &model::HostProof {
                identity: b.target.clone(),
                signature: proof.signature.clone(),
            },
            &model::ready_message(b),
        )
    {
        return Err(conflict(
            "The destination's readiness proof is stale or does not match this move",
        ));
    }
    if manifest.context.runtime.rtc_setup_required
        && (!b.rtc_ready || b.rtc_media_ready != Some(true))
    {
        return Err(conflict(
            "Complete the new host's voice/video connection check before stopping this host",
        ));
    }
    Ok(())
}

impl AppState {
    /// The pending namespace is outside SQLite and survives a frozen restart.
    /// Removing it cannot affect the new namespace used after cancellation or
    /// by the destination. Never drop this obligation when RTC is unavailable.
    pub(crate) async fn flush_handover_media(&self) -> ApiResult<()> {
        for _ in 0..2 {
            let (prefix, legacy, binding) = {
                let j = lock(&self.0.handover.value);
                (
                    j.rtc_cleanup_prefix.clone(),
                    j.rtc_legacy_cleanup,
                    j.rtc_cleanup_binding.clone(),
                )
            };
            let Some(prefix) = prefix else { return Ok(()) };
            let config = self
                .0
                .config
                .rtc
                .as_ref()
                .filter(|c| Some(c.handover_binding()) == binding)
                .ok_or_else(|| {
                    conflict("Restore the original video configuration to finish its room cleanup")
                })?;
            config
                .drain_prefix(if legacy { &prefix[..43] } else { &prefix }, legacy)
                .await?;
            let mut j = lock(&self.0.handover.value);
            if j.rtc_cleanup_prefix.as_ref() == Some(&prefix) && j.rtc_legacy_cleanup == legacy {
                let mut next = j.clone();
                if legacy {
                    next.rtc_legacy_cleanup = false;
                } else {
                    next.rtc_cleanup_prefix = None;
                    next.rtc_cleanup_binding = None;
                }
                self.0.handover.save(&mut j, next)?;
            }
        }
        Ok(())
    }
}

/// Runs only on the chosen destination, after verifying the on-disk image again.
pub(crate) fn attest(
    data: &FsPath,
    status: &model::Status,
    key: &crate::handover_target::TargetKey,
) -> LocalResult<Receipt> {
    let p = status.preparation.as_ref().ok_or("No frozen handover")?;
    if p.phase != Phase::Frozen || !uuid(&p.id) {
        return Err("No frozen handover".into());
    }
    let dir = data.join("handover-received").join(&p.id);
    let received = transfer::receive(data, &dir, status, key)?;
    let manifest = transfer::source_manifest_received(&dir)?;
    let body = ReceiptBody {
        server_id: status.server_id.clone(),
        source_host_id: status.source_host_id.clone(),
        term: status.term,
        preparation_id: p.id.clone(),
        export_id: received.export_id,
        target_identity: key.identity(),
        target_url: p.target_url.clone(),
        database_sha256: received.database_sha256,
        database_bytes: received.database_bytes,
        manifest_sha256: received.manifest_sha256,
        challenge: status
            .receipt_challenge
            .clone()
            .ok_or("Missing source receipt challenge")?,
    };
    let receipt = Receipt {
        signature: key.sign(&receipt_message(&body)),
        body,
    };
    if !valid_receipt(&receipt) || !image_matches(&receipt, &manifest) {
        return Err("Invalid destination receipt".into());
    }
    let path = dir.join("receipt-proof.json");
    direct(&path)?;
    if path.try_exists()? {
        let previous: Receipt = serde_json::from_slice(&transfer::read_file(&path, 4096)?)?;
        if previous != receipt {
            return Err("Earlier destination receipt retained unchanged".into());
        }
    } else {
        transfer::publish_new(&path, &serde_json::to_vec(&receipt)?)?;
    }
    Ok(receipt)
}

#[derive(Serialize)]
pub struct Activated {
    pub server_id: String,
    pub term: u64,
    pub target_host_id: String,
    pub target_url: String,
    pub data: PathBuf,
    pub runtime: model::HostRuntime,
}
fn read_journal(data: &FsPath) -> LocalResult<Option<Journal>> {
    let path = data.join(JOURNAL);
    direct(&path)?;
    if !path.try_exists()? {
        return Ok(None);
    }
    let j: Journal = serde_json::from_slice(&transfer::read_file(&path, LIMIT)?)?;
    if j.schema != model::SCHEMA
        || !uuid(&j.server_id)
        || !uuid(&j.source_host_id)
        || !valid_journal(&j)
    {
        return Err("Invalid activated authority journal".into());
    }
    Ok(Some(j))
}
fn report(dir: &FsPath, grant: &ActivationGrant, manifest: &model::TransferManifest) -> Activated {
    Activated {
        server_id: grant.receipt.body.server_id.clone(),
        term: grant.next_term,
        target_host_id: grant.receipt.body.target_identity.host_id.clone(),
        target_url: grant.receipt.body.target_url.clone(),
        data: dir.to_owned(),
        runtime: manifest.context.runtime.clone(),
    }
}
fn bound_manifest(dir: &FsPath, grant: &ActivationGrant) -> LocalResult<model::TransferManifest> {
    let m = transfer::source_manifest_received(dir)?;
    if !valid_grant(grant)
        || !image_matches(&grant.receipt, &m)
        || m.context.source_identity != grant.source_identity
    {
        return Err("Activation grant does not match the signed image".into());
    }
    Ok(m)
}
fn marker_matches(db: &Connection, j: &Journal) -> LocalResult<()> {
    let marker: String = db.query_row(
        "SELECT value FROM metadata WHERE key=?1",
        [DB_MARKER],
        |r| r.get(0),
    )?;
    let term: String = db.query_row("SELECT value FROM metadata WHERE key=?1", [DB_TERM], |r| {
        r.get(0)
    })?;
    if server_id(db)? != j.server_id || marker != j.source_host_id || term.parse::<u64>()? != j.term
    {
        return Err("Activated database and authority do not agree".into());
    }
    Ok(())
}

/// A later ordinary server launch uses the signed inherited public runtime,
/// never the source's machine-local media/admin credentials or environment.
pub(crate) fn active_runtime(data: &FsPath) -> LocalResult<Option<model::HostRuntime>> {
    let Some(j) = read_journal(data)? else {
        return Ok(None);
    };
    let Some(grant) = &j.accepted_from else {
        return Ok(None);
    };
    if j.activation_pending {
        return Err(
            "Finish the interrupted local handover activation before starting this host".into(),
        );
    }
    let m = bound_manifest(data, grant)?;
    crate::handover_target::source_key(data, j.source_identity.as_ref())?;
    let db = Connection::open_with_flags(
        data.join("relay.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    marker_matches(&db, &j)?;
    Ok(Some(m.context.runtime))
}

fn cold_database(dir: &FsPath) -> LocalResult<()> {
    // A private, stopped standby may have empty SQLite sidecars from a prior
    // read. Never discard a nonempty WAL/journal or replace a running database.
    for suffix in ["-wal", "-journal"] {
        let path = dir.join(format!("relay.sqlite3{suffix}"));
        direct(&path)?;
        if path.try_exists()? {
            if !fs::metadata(&path)?.is_file() || fs::metadata(&path)?.len() != 0 {
                return Err("Standby has pending SQLite changes; retained unchanged".into());
            }
            fs::remove_file(path)?;
        }
    }
    let shm = dir.join("relay.sqlite3-shm");
    direct(&shm)?;
    if shm.try_exists()? {
        if !fs::metadata(&shm)?.is_file() || fs::metadata(&shm)?.len() > 32768 {
            return Err("Unknown SQLite sidecar retained".into());
        }
        fs::remove_file(shm)?;
    }
    Ok(())
}
fn file_hash(path: &FsPath) -> LocalResult<String> {
    use std::io::Read;
    direct(path)?;
    let mut file = fs::File::open(path)?;
    if file.metadata()?.len() > 4 * 1024 * 1024 * 1024 {
        return Err("Oversized activation image".into());
    }
    let mut hash = Sha256::new();
    let mut buf = zeroize::Zeroizing::new(vec![0u8; 1024 * 1024]);
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
    }
    Ok(hash.finalize().iter().map(|v| format!("{v:02x}")).collect())
}

pub(crate) fn activate(
    data: &FsPath,
    status: &model::Status,
    grant: &ActivationGrant,
    key: &crate::handover_target::TargetKey,
) -> LocalResult<Activated> {
    let p = status.preparation.as_ref().ok_or("No accepted handover")?;
    if !uuid(&p.id)
        || grant.receipt.body.preparation_id != p.id
        || key.identity() != grant.receipt.body.target_identity
        || Some(&grant.source_identity) != status.source_identity.as_ref()
        || grant.receipt.body.server_id != status.server_id
        || grant.receipt.body.term != status.term
        || grant.receipt.body.source_host_id != status.source_host_id
        || grant.receipt.body.target_url != p.target_url
        || Some(&grant.receipt.body.challenge) != status.receipt_challenge.as_ref()
        || !valid_grant(grant)
        || status.history.len() >= MAX_HISTORY
        || !valid_history(&status.history,&model::RoutePin{server_id:status.server_id.clone(),
            host:grant.source_identity.clone(),term:status.term})
        || grant.history_sha256.as_ref().is_some_and(|hash|
            *hash != sha(&model::history_message(&status.history)))
    {
        return Err("Activation is not authorized for this destination".into());
    }
    key.check_consent(status)?;
    let dir = data.join("handover-received").join(&p.id);
    direct(&dir)?;
    let manifest = bound_manifest(&dir, grant)?;
    let existing = read_journal(&dir)?;
    if let Some(j) = &existing {
        if j.accepted_from.as_ref() != Some(grant)
            || j.source_identity.as_ref() != Some(&key.identity())
            || j.retired.is_some()
        {
            return Err("An unrelated or superseded authority is retained unchanged".into());
        }
        if !j.activation_pending {
            active_runtime(&dir)?;
            return Ok(report(&dir, grant, &manifest));
        }
    }
    let lock_path = dir.join("server.lock");
    direct(&lock_path)?;
    let lease = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)?;
    lease
        .try_lock()
        .map_err(|_| "Stop this data directory's server before activating it")?;
    let proof: Receipt =
        serde_json::from_slice(&transfer::read_file(&dir.join("receipt-proof.json"), 4096)?)?;
    if proof != grant.receipt {
        return Err("Destination's saved receipt does not match this grant".into());
    }
    let intent = dir.join("activation-intent.json");
    direct(&intent)?;
    if intent.try_exists()? {
        let previous: ActivationGrant =
            serde_json::from_slice(&transfer::read_file(&intent, LIMIT)?)?;
        if previous != *grant {
            return Err("An unrelated activation intent is retained unchanged".into());
        }
    } else {
        transfer::publish_new(&intent, &serde_json::to_vec(grant)?)?;
    }
    cold_database(&dir)?;
    let database = dir.join("relay.sqlite3");
    let staging = dir.join("activation-staging.sqlite3");
    direct(&staging)?;
    let mut j = if let Some(j) = existing {
        j
    } else {
        transfer::verify_database(&database, &manifest)?;
        // The private intent owns this one scratch filename. A partial build
        // before journal publication can be discarded without touching the image.
        if staging.try_exists()? {
            if !fs::metadata(&staging)?.is_file() {
                return Err("Unknown activation staging retained".into());
            }
            fs::remove_file(&staging)?;
        }
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        drop(options.open(&staging)?);
        fs::copy(&database, &staging)?;
        if file_hash(&staging)? != manifest.context.database_sha256 {
            return Err("Activation staging copy does not match the signed image".into());
        }
        {
            let mut db = Connection::open(&staging)?;
            db.pragma_update(None, "journal_mode", "DELETE")?;
            let tx = db.transaction()?;
            if tx.execute(
                "UPDATE metadata SET value=?2 WHERE key=?1",
                params![DB_MARKER, key.identity().host_id],
            )? != 1
            {
                return Err("Missing standby authority marker".into());
            }
            if tx.execute(
                "UPDATE metadata SET value=?2 WHERE key=?1",
                params![DB_TERM, grant.next_term.to_string()],
            )? != 1
            {
                return Err("Missing standby authority term".into());
            }
            tx.commit()?;
        }
        fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&staging)?
            .sync_all()?;
        key.copy_for_source(&dir)?;
        let j = Journal {
            schema: model::SCHEMA,
            server_id: grant.receipt.body.server_id.clone(),
            source_host_id: key.identity().host_id,
            term: grant.next_term,
            preparation: None,
            destination: None,
            restore_target: None,
            target_proof: None,
            source_identity: Some(key.identity()),
            receipt_challenge: None,
            received: None,
            retired: None,
            history: status.history.iter().cloned().chain(std::iter::once(grant.clone())).collect(),
            accepted_from: Some(grant.clone()),
            activation_database_sha256: Some(file_hash(&staging)?),
            activation_pending: true,
            media_epoch: Some(id()),
            rtc_cleanup_prefix: None,
            rtc_cleanup_binding: None,
            rtc_legacy_cleanup: false,
            takeover: None,
        };
        if !valid_journal(&j) {
            return Err("Invalid destination authority".into());
        }
        transfer::publish_new(&dir.join(JOURNAL), &serde_json::to_vec(&j)?)?;
        checkpoint("prepared")?;
        j
    };
    let expected = j
        .activation_database_sha256
        .as_ref()
        .ok_or("Missing prepared database digest")?;
    let current = file_hash(&database)?;
    if &current != expected {
        if current != manifest.context.database_sha256 || file_hash(&staging)? != *expected {
            return Err(
                "Activation checkpoint differs from its two known images; retained unchanged"
                    .into(),
            );
        }
        replace(&staging, &database)?;
        checkpoint("replaced")?;
    }
    let db = Connection::open_with_flags(&database, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    marker_matches(&db, &j)?;
    drop(db);
    j.activation_pending = false;
    persist(&dir.join(JOURNAL), &j)?;
    Ok(report(&dir, grant, &manifest))
}

#[cfg(test)]
thread_local! { static STOP_AT: std::cell::Cell<Option<&'static str>> = const { std::cell::Cell::new(None) }; }
fn checkpoint(point: &'static str) -> LocalResult<()> {
    #[cfg(test)]
    if STOP_AT.with(|v| {
        if v.get() == Some(point) {
            v.set(None);
            true
        } else {
            false
        }
    }) {
        return Err("Injected activation checkpoint stop".into());
    }
    let _ = point;
    Ok(())
}
#[cfg(test)]
pub(crate) fn stop_once_at(point: &'static str) {
    STOP_AT.with(|v| v.set(Some(point)));
}
