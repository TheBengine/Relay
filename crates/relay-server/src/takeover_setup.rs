//! Setting up takeover between real machines, milestone 1: the host side.
//!
//! An administrator approves a member's data-copy role; the member accepts it.
//! Then the member's app:
//! 1. enrolls this machine's own takeover key (`enroll`), proving it holds it;
//! 2. once two co-hosts are enrolled, the host signs the authority set, and each
//!    co-host signs its consent to exactly that set (`consent`);
//! 3. asks for a full copy (`snapshot`) and fetches it in bounded parts, each with
//!    its own digest, over the member's existing connection (HTTPS, LAN or iroh);
//!    an interrupted copy resumes from the parts it already has;
//! 4. verifies and installs it as its standby image, and reports that (`verified`).
//!
//! The copy is the database's own pages: login secrets in it are sealed or keyed
//! by the host-local key (see `credential_seal`). Revoking the role removes the
//! member, the set and every consent, and deletes the member's prepared copy here.
//!
//! "Ready to take over" stays off: live quorum replication is a separate step.
use super::*;
use relay_protocol::{
    handover::HostIdentity,
    hosting::{SetupStep, REPORT_FRESH_MS},
    takeover::{self as wire, AuthoritySet, CandidateAccept, SetBody, Tip},
};
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const KEY: &str = "takeover.cohort.v1";
pub(crate) const PART: u64 = 1024 * 1024;
const MAX_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const MEMBERS: usize = 2;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Cohort {
    pub(crate) epoch: u64,
    pub(crate) members: Vec<Enrolled>,
    pub(crate) set: Option<AuthoritySet>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Enrolled {
    pub(crate) user_id: String,
    pub(crate) identity: HostIdentity,
    pub(crate) recipient_key: String,
    pub(crate) accept: Option<CandidateAccept>,
    pub(crate) snapshot: Option<Manifest>,
    pub(crate) verified_ms: Option<u64>,
}

/// One prepared full copy. `head` is the change-log position at the snapshot,
/// so the host can tell later whether changes happened since.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub snapshot_id: String,
    pub bytes: u64,
    pub sha256: String,
    pub parts: Vec<String>,
    pub base: Tip,
    pub head: i64,
    pub tracked: bool,
    pub created_ms: u64,
}

/// What the member's app sends to enroll its machine.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnrollRequest {
    pub identity: HostIdentity,
    pub recipient_key: String,
    pub proof: String,
}

pub fn enroll_message(server_id: &str, user_id: &str, recipient_key: &str) -> Vec<u8> {
    let mut out = b"relay-takeover-enroll-v1\0".to_vec();
    out.extend(serde_json::to_vec(&(server_id, user_id, recipient_key)).expect("fixed fields"));
    out
}

pub(crate) fn load(db: &Connection) -> ApiResult<Cohort> {
    let value: Option<String> = db
        .query_row("SELECT value FROM metadata WHERE key=?1", [KEY], |r| r.get(0))
        .optional()?;
    match value {
        None => Ok(Cohort::default()),
        Some(v) => serde_json::from_str(&v).map_err(|_| bad("Takeover setup is damaged")),
    }
}

fn save(db: &Connection, cohort: &Cohort) -> ApiResult<()> {
    let json = serde_json::to_string(cohort).map_err(|_| bad("Invalid takeover setup"))?;
    db.execute(
        "INSERT INTO metadata(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        params![KEY, json],
    )?;
    Ok(())
}

fn folder(state: &AppState, user: &str) -> std::path::PathBuf {
    state.0.data.join("takeover-snapshots").join(
        &format!("{:x}", Sha256::digest(user.as_bytes()))[..32],
    )
}

fn approved(db: &Connection, user: &str) -> ApiResult<()> {
    if !hosting::holds_replica(db, user) {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "Only an approved co-host with the data-copy role can keep a takeover copy",
        ));
    }
    Ok(())
}

/// The host signs a new set once two co-hosts are enrolled; any change to who is
/// in the cohort clears the set and every consent.
fn settle(state: &AppState, cohort: &mut Cohort) {
    if cohort.members.len() == MEMBERS && cohort.set.is_none() {
        let mut candidates = [cohort.members[0].identity.clone(), cohort.members[1].identity.clone()];
        candidates.sort_by(|a, b| a.host_id.cmp(&b.host_id));
        cohort.epoch += 1;
        let body = SetBody {
            schema: wire::SCHEMA,
            pin: state.0.handover.route_pin(),
            epoch: cohort.epoch,
            candidates,
        };
        cohort.set = Some(AuthoritySet {
            signature: state.0.handover.sign(&body.message()),
            body,
        });
    }
}

fn reset_set(cohort: &mut Cohort) {
    cohort.set = None;
    for m in &mut cohort.members {
        m.accept = None;
    }
}

pub(super) async fn enroll(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<EnrollRequest>,
) -> ApiResult<Json<Value>> {
    let auth = state.auth(&headers)?;
    let server = server_id(&lock(&state.0.db))?;
    if input.recipient_key.len() > 128
        || !crate::handover_target::verify(
            &relay_protocol::handover::HostProof {
                identity: input.identity.clone(),
                signature: input.proof.clone(),
            },
            &enroll_message(&server, &auth.user.id, &input.recipient_key),
        )
    {
        return Err(bad("This machine could not prove its takeover key"));
    }
    if input.identity == state.0.handover.route_pin().host {
        return Err(bad("The host itself cannot be its own co-host"));
    }
    let mut db = lock(&state.0.db);
    let tx = db.transaction()?;
    approved(&tx, &auth.user.id)?;
    let mut cohort = load(&tx)?;
    let mine = cohort.members.iter().position(|m| m.user_id == auth.user.id);
    if cohort
        .members
        .iter()
        .any(|m| m.user_id != auth.user.id && m.identity.host_id == input.identity.host_id)
    {
        return Err(bad("Another co-host already uses this machine key"));
    }
    match mine {
        Some(i) if cohort.members[i].identity == input.identity
            && cohort.members[i].recipient_key == input.recipient_key => {}
        Some(i) => {
            // A new machine for the same member starts over.
            cohort.members[i] = Enrolled {
                user_id: auth.user.id.clone(),
                identity: input.identity,
                recipient_key: input.recipient_key,
                accept: None,
                snapshot: None,
                verified_ms: None,
            };
            reset_set(&mut cohort);
        }
        None if cohort.members.len() >= MEMBERS => {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "Two co-hosts are already set up. An administrator can revoke one first.",
            ))
        }
        None => cohort.members.push(Enrolled {
            user_id: auth.user.id.clone(),
            identity: input.identity,
            recipient_key: input.recipient_key,
            accept: None,
            snapshot: None,
            verified_ms: None,
        }),
    }
    settle(&state, &mut cohort);
    save(&tx, &cohort)?;
    roles::audit(&tx, &auth.user.id, "takeover.enrolled", &auth.user.id)?;
    let out = reply(&tx, &cohort)?;
    tx.commit()?;
    Ok(out)
}

pub(super) async fn cohort(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let auth = state.auth(&headers)?;
    let db = lock(&state.0.db);
    approved(&db, &auth.user.id)?;
    reply(&db, &load(&db)?)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Consent {
    accept: CandidateAccept,
}

pub(super) async fn consent(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<Consent>,
) -> ApiResult<Json<Cohort>> {
    let auth = state.auth(&headers)?;
    let mut db = lock(&state.0.db);
    let tx = db.transaction()?;
    approved(&tx, &auth.user.id)?;
    let mut cohort = load(&tx)?;
    let set = cohort.set.clone().ok_or_else(|| bad("Wait until a second co-host has joined"))?;
    let set_sha = format!("{:x}", Sha256::digest(set.body.message()));
    let mine = cohort
        .members
        .iter_mut()
        .find(|m| m.user_id == auth.user.id)
        .ok_or_else(|| bad("Enroll this machine first"))?;
    let a = &input.accept.body;
    let valid = input.accept.validate().is_ok()
        && a.server_id == set.body.pin.server_id
        && a.set_sha256 == set_sha
        && a.epoch == set.body.epoch
        && a.candidate == mine.identity
        && a.recipient_key == mine.recipient_key
        && a.full_data
        && a.takeover
        && crate::handover_target::verify(
            &relay_protocol::handover::HostProof {
                identity: a.candidate.clone(),
                signature: input.accept.signature.clone(),
            },
            &a.message(),
        );
    if !valid {
        return Err(bad("This consent does not match the current takeover set"));
    }
    mine.accept = Some(input.accept);
    save(&tx, &cohort)?;
    roles::audit(&tx, &auth.user.id, "takeover.consented", &auth.user.id)?;
    tx.commit()?;
    Ok(Json(cohort))
}

/// Prepares a full copy for one consenting co-host, replacing its older one.
pub(super) async fn snapshot(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Manifest>> {
    let auth = state.auth(&headers)?;
    // While protection is being turned on, every co-host gets the one shared copy.
    if let Some(arming) = lock(&state.0.arming).as_ref() {
        if arming.members.iter().any(|m| m.0 == auth.user.id) {
            return Ok(Json(arming.manifest.clone()));
        }
        return Err(ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "Takeover protection is being turned on"));
    }
    if lock(&state.0.takeover).is_some() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "Protection is on: the copy on that computer follows live changes",
        ));
    }
    {
        let db = lock(&state.0.db);
        approved(&db, &auth.user.id)?;
        let cohort = load(&db)?;
        if !cohort
            .members
            .iter()
            .any(|m| m.user_id == auth.user.id && m.accept.is_some())
        {
            return Err(bad("Sign the takeover agreement first"));
        }
    }
    let dir = folder(&state, &auth.user.id);
    let _ = std::fs::remove_dir_all(&dir);
    crate::handover_target::private_directory(&dir).map_err(|_| unavailable())?;
    let snapshot_id = id();
    let file = dir.join(format!("{snapshot_id}.sqlite3"));
    let (head, tracked) = {
        let db = lock(&state.0.db);
        crate::community::backup_database(&db, &file).map_err(|_| unavailable())?;
        let tracked: bool = db
            .query_row("SELECT enabled=1 FROM replica_state WHERE id=1", [], |r| r.get(0))
            .optional()?
            .unwrap_or(false);
        (replica::head(&db)?, tracked)
    };
    let raw = std::fs::read(&file).map_err(|_| unavailable())?;
    if raw.is_empty() || raw.len() as u64 > MAX_BYTES {
        return Err(unavailable());
    }
    let sha256 = format!("{:x}", Sha256::digest(&raw));
    let parts = raw
        .chunks(PART as usize)
        .map(|c| format!("{:x}", Sha256::digest(c)))
        .collect();
    let pin = state.0.handover.route_pin();
    let manifest = Manifest {
        snapshot_id,
        bytes: raw.len() as u64,
        base: Tip {
            term: pin.term,
            index: 0,
            digest: sha256.clone(),
        },
        sha256,
        parts,
        head,
        tracked,
        created_ms: now_ms(),
    };
    let mut db = lock(&state.0.db);
    let tx = db.transaction()?;
    let mut cohort = load(&tx)?;
    let mine = cohort
        .members
        .iter_mut()
        .find(|m| m.user_id == auth.user.id && m.accept.is_some())
        .ok_or_else(|| bad("The takeover setup changed; start again"))?;
    mine.snapshot = Some(manifest.clone());
    mine.verified_ms = None;
    save(&tx, &cohort)?;
    tx.commit()?;
    Ok(Json(manifest))
}

/// The cohort plus the host's current change-log position, so a co-host can
/// tell whether its verified copy is still up to date.
fn reply(db: &Connection, cohort: &Cohort) -> ApiResult<Json<Value>> {
    let mut value = serde_json::to_value(cohort).map_err(|_| bad("Invalid takeover setup"))?;
    value["head"] = replica::head(db)?.into();
    Ok(Json(value))
}

fn unavailable() -> ApiError {
    ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "The copy could not be prepared; try again")
}

/// One part of the member's own prepared copy, never anyone else's.
pub(super) async fn part(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((snapshot_id, index)): Path<(String, usize)>,
) -> ApiResult<Vec<u8>> {
    let auth = state.auth(&headers)?;
    let arming = lock(&state.0.arming).as_ref().and_then(|a| {
        (a.manifest.snapshot_id == snapshot_id && a.members.iter().any(|m| m.0 == auth.user.id))
            .then(|| (a.manifest.clone(), a.file.clone()))
    });
    let manifest = if let Some((manifest, _)) = &arming {
        manifest.clone()
    } else {
        let db = lock(&state.0.db);
        approved(&db, &auth.user.id)?;
        load(&db)?
            .members
            .into_iter()
            .find(|m| m.user_id == auth.user.id)
            .and_then(|m| m.snapshot)
            .filter(|s| s.snapshot_id == snapshot_id)
            .ok_or_else(missing)?
    };
    if index >= manifest.parts.len() {
        return Err(missing());
    }
    let file = match arming {
        Some((_, file)) => file,
        None => folder(&state, &auth.user.id).join(format!("{snapshot_id}.sqlite3")),
    };
    let mut handle = std::fs::File::open(file).map_err(|_| missing())?;
    use std::io::{Read, Seek, SeekFrom};
    handle
        .seek(SeekFrom::Start(index as u64 * PART))
        .map_err(|_| unavailable())?;
    let mut out = Vec::new();
    handle
        .take(PART)
        .read_to_end(&mut out)
        .map_err(|_| unavailable())?;
    if format!("{:x}", Sha256::digest(&out)) != manifest.parts[index] {
        return Err(unavailable());
    }
    Ok(out)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Verified {
    snapshot_id: String,
    sha256: String,
}

/// The co-host installed and checked the copy. The host keeps only the record.
pub(super) async fn verified(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<Verified>,
) -> ApiResult<Json<Value>> {
    let auth = state.auth(&headers)?;
    // The shared arming copy: recorded in memory only, since writes are stopped.
    let armed = {
        let mut arming = lock(&state.0.arming);
        match arming.as_mut() {
            Some(a) if a.manifest.snapshot_id == input.snapshot_id => {
                if a.manifest.sha256 != input.sha256 {
                    return Err(bad("This is not the copy the host prepared"));
                }
                let mine = a
                    .members
                    .iter_mut()
                    .find(|m| m.0 == auth.user.id)
                    .ok_or_else(missing)?;
                mine.3 = true;
                Some((mine.2.clone(), a.members.iter().all(|m| m.3)))
            }
            _ => None,
        }
    };
    if let Some((feed_key, both)) = armed {
        if both {
            let finishing = state.clone();
            tokio::spawn(async move {
                let _ = crate::takeover_live::finish_arming(finishing).await;
            });
        }
        return Ok(Json(json!({ "feed_key": feed_key, "arming": !both })));
    }
    let cohort = {
        let mut db = lock(&state.0.db);
        let tx = db.transaction()?;
        approved(&tx, &auth.user.id)?;
        let mut cohort = load(&tx)?;
        let mine = cohort
            .members
            .iter_mut()
            .find(|m| m.user_id == auth.user.id)
            .ok_or_else(missing)?;
        let ok = mine
            .snapshot
            .as_ref()
            .is_some_and(|s| s.snapshot_id == input.snapshot_id && s.sha256 == input.sha256);
        if !ok {
            return Err(bad("This is not the copy the host prepared"));
        }
        mine.verified_ms = Some(now_ms());
        save(&tx, &cohort)?;
        roles::audit(&tx, &auth.user.id, "takeover.copy_verified", &auth.user.id)?;
        tx.commit()?;
        cohort
    };
    // The prepared file has done its job.
    let _ = std::fs::remove_dir_all(folder(&state, &auth.user.id));
    Ok(Json(serde_json::to_value(cohort).map_err(|_| bad("Invalid takeover setup"))?))
}

/// Called whenever roles change: a member without the data-copy role leaves the
/// cohort, which also voids the set and every consent.
pub(crate) fn drop_if_role_lost(db: &Connection, user: &str) -> ApiResult<bool> {
    if hosting::holds_replica(db, user) {
        return Ok(false);
    }
    let mut cohort = load(db)?;
    let before = cohort.members.len();
    cohort.members.retain(|m| m.user_id != user);
    if cohort.members.len() == before {
        return Ok(false);
    }
    reset_set(&mut cohort);
    save(db, &cohort)?;
    Ok(true)
}

pub(crate) fn forget_files(state: &AppState, user: &str) {
    let _ = std::fs::remove_dir_all(folder(state, user));
}

/// The checklist steps this module can measure.
pub(crate) fn steps(db: &Connection, name: impl Fn(&str) -> String) -> ApiResult<Vec<SetupStep>> {
    let cohort = load(db)?;
    let now = now_ms();
    let head = replica::head(db)?;
    let step = |what: &str, done: bool, text: String| SetupStep {
        what: what.into(),
        done,
        text,
    };
    let enrolled = cohort.members.len();
    let consented = cohort.members.iter().filter(|m| m.accept.is_some()).count();
    let mut copies = Vec::new();
    let mut ready = 0;
    for m in &cohort.members {
        let who = name(&m.user_id);
        let line = match (&m.snapshot, m.verified_ms) {
            (Some(s), Some(_)) if s.tracked && s.head == head => {
                ready += 1;
                format!("{who}: Data copy up to date (verified copy, no changes since)")
            }
            (Some(s), Some(_)) if s.tracked => {
                format!("{who}: Can't take over: copy is out of date ({} changes since)", head.saturating_sub(s.head))
            }
            (Some(_), Some(at)) => format!(
                "{who}: verified copy from {} min ago; changes since are not tracked, so it counts as out of date",
                now.saturating_sub(at) / 60_000
            ),
            (Some(s), None) if now.saturating_sub(s.created_ms) > REPORT_FRESH_MS * 15 => {
                format!("{who}: the copy was interrupted; open Hosts on that computer to resume it")
            }
            (Some(_), None) => format!("{who}: copying"),
            (None, _) if m.accept.is_some() => format!("{who}: ready to copy"),
            (None, _) => format!("{who}: waiting for the takeover agreement"),
        };
        copies.push(line);
    }
    Ok(vec![
        step(
            "Co-hosts joined with this computer's own key",
            enrolled >= MEMBERS,
            format!("{enrolled} of {MEMBERS}. Each co-host chooses Keep a takeover copy on this computer."),
        ),
        step(
            "Both co-hosts signed the takeover agreement",
            cohort.set.is_some() && consented >= MEMBERS,
            if cohort.set.is_none() {
                "The host signs the agreement once two co-hosts have joined.".into()
            } else {
                format!("{consented} of {MEMBERS} signed the agreement for exactly these three machines.")
            },
        ),
        step(
            "Each co-host holds a verified full copy",
            ready >= MEMBERS,
            if copies.is_empty() {
                "No co-host has a copy yet.".into()
            } else {
                copies.join(" · ")
            },
        ),
    ])
}

#[cfg(test)]
mod tests;
