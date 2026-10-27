//! Live 2-of-3 replication over the co-hosts' existing connections (stage A1).
//!
//! The host cannot reach a co-host's computer, so the co-hosts pull: each one
//! long-polls `feed` for the next signed log entries and commit notices, applies
//! them to its own standby and posts its signed answers to `answers`. A write on
//! the host waits (bounded) for one co-host's acknowledgement before SQLite may
//! commit it; that is the 2-of-3 rule.
//!
//! Feed requests carry a per-member feed key, never a session: a write waiting
//! for its acknowledgement holds the database, so the feed must not need it.
//!
//! Arming (turning protection on) is behind a setting that is off by default:
//! writes stop, one snapshot is taken, both co-hosts install exactly that copy,
//! and only then is the replicating database attached. Nothing may change in
//! between, so nothing is lost or doubled.
use super::*;
use crate::{
    takeover_commit::{CommitNotice, Replicate},
    takeover_setup::Manifest,
    takeover_voter::AppendAnswer,
};
use relay_protocol::{
    handover::HostIdentity,
    takeover::{Ack, LogEntry, Nack},
};
use serde::Serialize;
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    sync::Condvar,
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;

/// How long one feed request waits for something new.
#[cfg(not(test))]
const POLL: Duration = Duration::from_secs(15);
#[cfg(test)]
const POLL: Duration = Duration::from_secs(1);
/// Retained for catching up; a co-host further behind needs a fresh copy.
const MAX_ENTRIES: usize = 4096;
const MAX_NOTICES: usize = 4096;
/// A co-host that polled within this time counts as present.
#[cfg(not(test))]
pub(crate) const PRESENT: Duration = Duration::from_secs(45);
#[cfg(test)]
pub(crate) const PRESENT: Duration = Duration::from_secs(3);

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "evidence", deny_unknown_fields)]
pub(crate) enum Wire {
    Ack(Ack),
    Nack(Nack),
}
impl From<AppendAnswer> for Wire {
    fn from(a: AppendAnswer) -> Self {
        match a {
            AppendAnswer::Ack(a) => Self::Ack(a),
            AppendAnswer::Nack(n) => Self::Nack(n),
        }
    }
}
impl From<Wire> for AppendAnswer {
    fn from(a: Wire) -> Self {
        match a {
            Wire::Ack(a) => Self::Ack(a),
            Wire::Nack(n) => Self::Nack(n),
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FeedRequest {
    pub(crate) key: String,
    /// Highest log index this co-host retains.
    pub(crate) after: u64,
    /// Highest index it has applied.
    pub(crate) applied: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FeedReply {
    pub(crate) entries: Vec<LogEntry>,
    pub(crate) notices: Vec<CommitNotice>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Answers {
    pub(crate) key: String,
    pub(crate) answers: Vec<(u64, Wire)>,
}

struct Member {
    // Read by the outage vote path (A4, paused); kept so restarts keep it.
    #[allow(dead_code)]
    identity: HostIdentity,
    key: zeroize::Zeroizing<String>,
    seen: Option<Instant>,
    /// Feed requests of this member that are waiting right now.
    polling: u32,
}

struct FeedState {
    members: [Member; 2],
    entries: BTreeMap<u64, LogEntry>,
    notices: VecDeque<CommitNotice>,
    answers: HashMap<u64, [Option<AppendAnswer>; 2]>,
}

/// What the host shares with its co-hosts.
pub(crate) struct Feed {
    state: Mutex<FeedState>,
    changed: Condvar,
}

impl Feed {
    pub(crate) fn new(members: [(HostIdentity, String); 2]) -> Arc<Self> {
        let [a, b] = members;
        let member = |(identity, key): (HostIdentity, String)| Member {
            identity,
            key: zeroize::Zeroizing::new(key),
            // Both just installed the copy or are reconnecting after a restart:
            // they get the usual grace period before changes pause.
            seen: Some(Instant::now()),
            polling: 0,
        };
        Arc::new(Self {
            state: Mutex::new(FeedState {
                members: [member(a), member(b)],
                entries: BTreeMap::new(),
                notices: VecDeque::new(),
                answers: HashMap::new(),
            }),
            changed: Condvar::new(),
        })
    }

    fn who(state: &FeedState, key: &str) -> Option<usize> {
        state
            .members
            .iter()
            .position(|m| m.key.len() == key.len() && bool::from(m.key.as_bytes().ct_eq(key.as_bytes())))
    }

    /// True when at least one co-host is waiting for news or asked recently.
    pub(crate) fn present(&self, now: Instant) -> bool {
        lock(&self.state).members.iter().any(|m| {
            m.polling > 0 || m.seen.is_some_and(|at| now.saturating_duration_since(at) < PRESENT)
        })
    }

    #[allow(dead_code)] // For the outage vote path (A4, paused).
    pub(crate) fn identities(&self) -> [HostIdentity; 2] {
        let s = lock(&self.state);
        [s.members[0].identity.clone(), s.members[1].identity.clone()]
    }

    /// Waits up to `limit` for entries after `after` or notices beyond `applied`.
    fn wait(&self, key: &str, after: u64, applied: u64, limit: Duration) -> Option<FeedReply> {
        let deadline = Instant::now() + limit;
        let mut state = lock(&self.state);
        let who = Self::who(&state, key)?;
        state.members[who].polling += 1;
        let reply = loop {
            state.members[who].seen = Some(Instant::now());
            let entries: Vec<LogEntry> = state
                .entries
                .range(after + 1..)
                .take(64)
                .map(|(_, e)| e.clone())
                .collect();
            let notices: Vec<CommitNotice> = state
                .notices
                .iter()
                .filter(|n| n.end.index > applied)
                .take(64)
                .cloned()
                .collect();
            let now = Instant::now();
            if !entries.is_empty() || !notices.is_empty() || now >= deadline {
                break FeedReply { entries, notices };
            }
            state = self
                .changed
                .wait_timeout(state, deadline - now)
                .map(|(g, _)| g)
                .unwrap_or_else(|e| e.into_inner().0);
        };
        state.members[who].polling -= 1;
        state.members[who].seen = Some(Instant::now());
        Some(reply)
    }

    fn answer(&self, key: &str, answers: Vec<(u64, Wire)>) -> bool {
        let mut state = lock(&self.state);
        let Some(who) = Self::who(&state, key) else {
            return false;
        };
        state.members[who].seen = Some(Instant::now());
        for (index, answer) in answers.into_iter().take(128) {
            if let Some(slot) = state.answers.get_mut(&index) {
                slot[who] = Some(answer.into());
            }
        }
        self.changed.notify_all();
        true
    }
}

/// The host's side of the 2-of-3 rule, used by the replicating database.
pub(crate) struct PullPeers(pub(crate) Arc<Feed>);

impl Replicate for PullPeers {
    fn ready(&self) -> bool {
        self.0.present(Instant::now())
    }
    fn append(&mut self, entry: &LogEntry, deadline: Instant) -> [Option<AppendAnswer>; 2] {
        let index = entry.body.index;
        let mut state = lock(&self.0.state);
        state.entries.insert(index, entry.clone());
        while state.entries.len() > MAX_ENTRIES {
            let first = *state.entries.keys().next().unwrap();
            state.entries.remove(&first);
        }
        state.answers.insert(index, [None, None]);
        self.0.changed.notify_all();
        loop {
            let slot = state.answers.get(&index).cloned().unwrap_or([None, None]);
            let acked = slot.iter().any(|a| matches!(a, Some(AppendAnswer::Ack(_))));
            let now = Instant::now();
            if acked || slot.iter().all(Option::is_some) || now >= deadline {
                state.answers.remove(&index);
                return slot;
            }
            state = self
                .0
                .changed
                .wait_timeout(state, deadline - now)
                .map(|(g, _)| g)
                .unwrap_or_else(|e| e.into_inner().0);
        }
    }

    fn committed(&mut self, notice: &CommitNotice, _deadline: Instant) {
        let mut state = lock(&self.0.state);
        state.notices.push_back(notice.clone());
        while state.notices.len() > MAX_NOTICES {
            state.notices.pop_front();
        }
        self.0.changed.notify_all();
    }
}

pub(super) async fn feed(
    State(state): State<AppState>,
    Json(input): Json<FeedRequest>,
) -> ApiResult<Json<FeedReply>> {
    let feed = lock(&state.0.feed).clone().ok_or_else(missing)?;
    let reply = tokio::task::spawn_blocking(move || {
        feed.wait(&input.key, input.after, input.applied, POLL)
    })
    .await
    .map_err(|_| missing())?
    .ok_or_else(unauthorized)?;
    Ok(Json(reply))
}

pub(super) async fn answers(
    State(state): State<AppState>,
    Json(input): Json<Answers>,
) -> ApiResult<StatusCode> {
    let feed = lock(&state.0.feed).clone().ok_or_else(missing)?;
    if feed.answer(&input.key, input.answers) {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(unauthorized())
    }
}

/// Turning protection on, between the snapshot and the attach.
pub(crate) struct Arming {
    pub(crate) manifest: Manifest,
    pub(crate) file: std::path::PathBuf,
    /// (user, identity, feed key, verified)
    pub(crate) members: Vec<(String, HostIdentity, String, bool)>,
    pub(crate) started: Instant,
}

/// How long writes may stay stopped while co-hosts fetch the copy.
pub(crate) const ARMING_LIMIT: Duration = Duration::from_secs(15 * 60);

impl AppState {
    /// Allows `arm` on this server. Off unless the operator chose it.
    pub fn enable_takeover_protection(&self) {
        self.0
            .protection_enabled
            .store(true, std::sync::atomic::Ordering::Release);
    }
    pub(crate) fn protection_enabled(&self) -> bool {
        self.0
            .protection_enabled
            .load(std::sync::atomic::Ordering::Acquire)
    }
    pub(crate) fn arming(&self) -> bool {
        lock(&self.0.arming).is_some()
    }
    /// Ends an arming that took too long; writes resume unprotected.
    pub(crate) fn expire_arming(&self) {
        let mut arming = lock(&self.0.arming);
        if arming.as_ref().is_some_and(|a| a.started.elapsed() > ARMING_LIMIT) {
            if let Some(a) = arming.take() {
                let _ = std::fs::remove_file(&a.file);
            }
        }
    }
}

fn random_key() -> String {
    let mut raw = [0u8; 32];
    rand::RngCore::fill_bytes(&mut OsRng, &mut raw);
    raw.iter().map(|b| format!("{b:02x}")).collect()
}

/// An administrator turns protection on. Writes stop until both co-hosts have
/// installed this exact copy (or the arming expires).
pub(super) async fn arm(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Manifest>> {
    let auth = state.auth(&headers)?;
    if !state.protection_enabled() {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "Takeover protection is not enabled on this server",
        ));
    }
    let mut arming = lock(&state.0.arming);
    if arming.is_some() || lock(&state.0.takeover).is_some() {
        return Err(ApiError::new(StatusCode::CONFLICT, "Protection is already on or being turned on"));
    }
    let db = lock(&state.0.db);
    roles::require(&db, &auth.user.id, PERMISSION_ADMIN)?;
    let cohort = crate::takeover_setup::load(&db)?;
    if cohort.set.is_none() || cohort.members.len() != 2 || cohort.members.iter().any(|m| m.accept.is_none()) {
        return Err(bad("Both co-hosts must join and sign the takeover agreement first"));
    }
    let dir = state.0.data.join("takeover-snapshots").join("arming");
    let _ = std::fs::remove_dir_all(&dir);
    crate::handover_target::private_directory(&dir).map_err(|_| bad("The copy could not be prepared"))?;
    let snapshot_id = id();
    let file = dir.join(format!("{snapshot_id}.sqlite3"));
    // Taken while this lock is held and before writes are stopped... and no write
    // can run until the arming state below exists, because it is set while the
    // database is still locked.
    crate::community::backup_database(&db, &file).map_err(|_| bad("The copy could not be prepared"))?;
    let raw = std::fs::read(&file).map_err(|_| bad("The copy could not be prepared"))?;
    use sha2::{Digest, Sha256};
    let sha256 = format!("{:x}", Sha256::digest(&raw));
    let pin = state.0.handover.route_pin();
    let manifest = Manifest {
        snapshot_id,
        bytes: raw.len() as u64,
        parts: raw
            .chunks(crate::takeover_setup::PART as usize)
            .map(|c| format!("{:x}", Sha256::digest(c)))
            .collect(),
        base: relay_protocol::takeover::Tip {
            term: pin.term,
            index: 0,
            digest: sha256.clone(),
        },
        sha256,
        head: replica::head(&db)?,
        tracked: false,
        created_ms: now_ms(),
    };
    *arming = Some(Arming {
        manifest: manifest.clone(),
        file,
        members: cohort
            .members
            .iter()
            .map(|m| (m.user_id.clone(), m.identity.clone(), random_key(), false))
            .collect(),
        started: Instant::now(),
    });
    Ok(Json(manifest))
}

/// After both co-hosts verified the arming copy: attach the replicating database.
pub(crate) async fn finish_arming(state: AppState) -> Result<(), String> {
    let (manifest, members, file) = {
        let arming = lock(&state.0.arming);
        let a = arming.as_ref().ok_or("No arming in progress")?;
        if a.members.len() != 2 || !a.members.iter().all(|m| m.3) {
            return Err("Waiting for both co-hosts".into());
        }
        (a.manifest.clone(), a.members.clone(), a.file.clone())
    };
    let set = crate::takeover_setup::load(&lock(&state.0.db))
        .map_err(|e| e.message.to_string())?
        .set
        .ok_or("The takeover agreement changed")?;
    let root = state.0.data.join("takeover");
    let host = state.0.handover.route_pin().host;
    let voter = crate::takeover_voter::Voter::initialize(
        crate::takeover_store::TakeoverStore::create_new(&root.join("log"), QUOTA)
            .map_err(|e| format!("{e:?}"))?,
        state.0.handover.takeover_signer(),
        set.clone(),
        None,
        manifest.base.clone(),
    )
    .map_err(|e| format!("{e:?}"))?;
    let recovery = crate::takeover_inflight::Inflight::create_new(
        &root.join("inflight"),
        set.clone(),
        host,
        manifest.base.clone(),
    )
    .map_err(|e| format!("{e:?}"))?;
    // Order the feed like the set's candidates.
    let by_identity = |identity: &HostIdentity| {
        members
            .iter()
            .find(|m| &m.1 == identity)
            .map(|m| (m.1.clone(), m.2.clone()))
    };
    let ordered = [
        by_identity(&set.body.candidates[0]).ok_or("Unknown co-host")?,
        by_identity(&set.body.candidates[1]).ok_or("Unknown co-host")?,
    ];
    // Kept privately beside the takeover log, so a restart reconnects the co-hosts.
    crate::handover_transfer::publish_new(
        &root.join(FEED_KEYS),
        &serde_json::to_vec(&ordered).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    let feed = Feed::new(ordered);
    *lock(&state.0.feed) = Some(feed.clone());
    // Writes are still stopped by the arming state: attach checks the handover
    // and takeover gates only, then swaps the connection.
    let result = state
        .attach_takeover_source(voter, recovery, Box::new(PullPeers(feed)))
        .await
        .map_err(|e| e.0.to_string());
    *lock(&state.0.arming) = None;
    let _ = std::fs::remove_file(file);
    result
}

const FEED_KEYS: &str = "feed-keys.json";

impl AppState {
    /// Startup of a protected community: reopen the host's log, its recovery
    /// checkpoint and the co-hosts' feed, then the replicating database. A
    /// transaction left half-confirmed by a crash refuses the start (recovery).
    pub(crate) fn resume_protection(&self) -> Result<(), Box<dyn std::error::Error>> {
        let root = self.0.data.join("takeover");
        let set = crate::takeover_setup::load(&lock(&self.0.db))
            .map_err(|e| e.message.to_string())?
            .set
            .ok_or("The takeover agreement is missing; protection cannot resume")?;
        let raw = crate::handover_transfer::read_file(&root.join(FEED_KEYS), 64 * 1024)?;
        let members: [(HostIdentity, String); 2] = serde_json::from_slice(&raw)?;
        if !set.body.is_candidate(&members[0].0) || !set.body.is_candidate(&members[1].0) {
            return Err("The co-hosts' feed keys do not match the agreement".into());
        }
        let voter = crate::takeover_voter::Voter::open(
            crate::takeover_store::TakeoverStore::open_existing(&root.join("log"), QUOTA)
                .map_err(|e| format!("{e:?}"))?,
            self.0.handover.takeover_signer(),
        )
        .map_err(|e| format!("{e:?}"))?;
        let recovery = crate::takeover_inflight::Inflight::open_existing(
            &root.join("inflight"),
            set,
            self.0.handover.route_pin().host,
        )
        .map_err(|e| format!("{e:?}"))?;
        let feed = Feed::new(members);
        *lock(&self.0.feed) = Some(feed.clone());
        self.resume_takeover_source(voter, recovery, Box::new(PullPeers(feed)))
            .map_err(|e| e.0.into())
    }
}

pub(crate) const QUOTA: crate::takeover_store::Quota = crate::takeover_store::Quota {
    max_entries: 1 << 20,
    max_entry_bytes: 16 * 1024 * 1024,
};

#[cfg(test)]
mod tests;
