//! M2 slice 3, HTTP side: a pull token the member mints for themselves, and the
//! routes a replica uses to pull staged, immutable export objects.
//!
//! * The pull token is a random 256-bit secret shown once and stored only as its
//!   SHA-256. It opens `/api/replica/pull/*` and nothing else; a login token does
//!   not open these routes and the pull token opens no other route.
//! * Every pull request re-checks the token, that the owner is an active member
//!   who still holds an accepted `Replica` role, and that the request names this
//!   server. Nothing is cached. Withdrawal, step-down, rotation or leaving ends
//!   access on the next request, and revocation paths delete staged exports.
//! * An export is built on its own read-only connection off the global database
//!   mutex (calls and ordinary requests never wait for it), staged as immutable
//!   parts under `<data>/replica-exports/<id>/`, and pinned to the permission
//!   generation and lineage it was built at: a change answers `409
//!   export_invalidated` and the files are deleted. Nothing stays pinned in the
//!   live database; exports do not survive a restart.
//! * Over any cap the answer is a refusal, never a smaller export.
//! * A part or file is checked before it is read and again before it is sent,
//!   with the same token, role, permission generation and lineage and without
//!   spending more rate budget: a rotation, role change or access change while
//!   the bytes were being read refuses the response instead of sending them.
//! * The rate books hold only replicas that were active in the last seconds (or
//!   today, for the byte cap), pruned at most once a second and capped in size.
use super::export::{self, Export, FileRef, Manifest, PartData, PartRef, Refusal, Request};
use super::*;
use axum::extract::State;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::path::{Path as FsPath, PathBuf};
use std::time::{Duration as StdDuration, Instant};

pub(crate) const TOKEN_LIFETIME_SECS: u64 = 90 * 24 * 3600;
pub(crate) const PART_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const READY_TTL: StdDuration = StdDuration::from_secs(30 * 60);
pub(crate) const BUILD_TTL: StdDuration = StdDuration::from_secs(90);
pub(crate) const MAX_ACTIVE_TOTAL: usize = 4;
pub(crate) const STAGING_CAP_BYTES: u64 = 4 * 1024 * 1024 * 1024;
pub(crate) const DAILY_BYTES_CAP: u64 = 20 * 1024 * 1024 * 1024;
/// Control requests (info, create, poll, acknowledge) per replica in two seconds: a
/// burst allowance around the contract's one request per second.
pub(crate) const BURST_REQUESTS: usize = 20;
/// Part and file downloads per replica in two seconds. They are bounded by bytes
/// (`DAILY_BYTES_CAP`) as well; many small parts must not look like abuse.
pub(crate) const BURST_DOWNLOADS: usize = 200;

#[derive(Clone, Copy)]
enum Traffic {
    Control,
    Download,
}
const MAX_PREVIOUS_ELIGIBLE: usize = 20_000;
/// Replicas the rate books follow at once. Only members holding an accepted
/// Replica role and a live token get this far, so a small number is ample.
const MAX_TRACKED_REPLICAS: usize = 256;
/// The books are pruned at most this often; a request never scans them more than once a second.
const BOOK_PRUNE_EVERY: StdDuration = StdDuration::from_secs(1);
const BURST_WINDOW: StdDuration = StdDuration::from_secs(2);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Building,
    Ready,
    Failed(StatusCode, &'static str),
}

struct Meta {
    owner: String,
    phase: Phase,
    lineage: String,
    permission_generation: i64,
    dir: PathBuf,
    manifest: Option<Manifest>,
    served_parts: BTreeSet<usize>,
    staged_bytes: u64,
    deadline: Instant,
}

#[derive(Default)]
struct Book {
    exports: HashMap<String, Meta>,
    creating: bool,
    recent: HashMap<String, VecDeque<Instant>>,
    daily: HashMap<String, (u64, u64)>,
    pruned: Option<Instant>,
}

/// The in-memory registry of staged exports. One per server process.
#[derive(Default)]
pub(crate) struct Service {
    book: Mutex<Book>,
    /// Test seam: runs once between a read and the final re-check of a response.
    #[cfg(test)]
    after_read: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

fn exports_root(data: &FsPath) -> PathBuf {
    data.join("replica-exports")
}

/// Exports do not survive a restart.
pub(crate) fn startup(data: &FsPath) {
    let _ = std::fs::remove_dir_all(exports_root(data));
}

impl Service {
    fn remove(book: &mut Book, id: &str) {
        if let Some(meta) = book.exports.remove(id) {
            let _ = std::fs::remove_dir_all(&meta.dir);
        }
    }

    fn sweep(book: &mut Book) {
        let now = Instant::now();
        let stale: Vec<String> = book
            .exports
            .iter()
            .filter(|(_, meta)| now >= meta.deadline)
            .map(|(id, _)| id.clone())
            .collect();
        for id in stale {
            Self::remove(book, &id);
        }
        Self::prune_rates(book, now);
    }

    /// Forget replicas whose burst window is empty and whose byte count belongs to
    /// an earlier day, so the books follow who is active, not everyone who ever was.
    /// Cadence-bound: a request scans them at most once a second.
    fn prune_rates(book: &mut Book, current: Instant) {
        if book
            .pruned
            .is_some_and(|at| current.saturating_duration_since(at) < BOOK_PRUNE_EVERY)
        {
            return;
        }
        book.pruned = Some(current);
        let cutoff = current.checked_sub(BURST_WINDOW).unwrap_or(current);
        book.recent
            .retain(|_, window| window.back().is_some_and(|at| *at >= cutoff));
        let today = now() / 86_400;
        book.daily.retain(|_, (day, _)| *day == today);
    }

    #[cfg(test)]
    pub(crate) fn tracked(&self) -> (usize, usize) {
        let book = lock(&self.book);
        (book.recent.len(), book.daily.len())
    }

    /// `n` replicas in the rate books: long gone (`active = false`) or active right now.
    #[cfg(test)]
    pub(crate) fn seed_rates(&self, n: usize, active: bool) {
        let mut book = lock(&self.book);
        let at = if active {
            Instant::now()
        } else {
            Instant::now()
                .checked_sub(StdDuration::from_secs(30))
                .expect("the machine has been up for half a minute")
        };
        for i in 0..n {
            book.recent.insert(format!("seed{i}"), VecDeque::from([at]));
            book.daily.insert(
                format!("seed{i}"),
                (if active { now() / 86_400 } else { 0 }, 5),
            );
        }
        // The next request prunes at once rather than waiting out the cadence.
        book.pruned = None;
    }

    #[cfg(test)]
    pub(crate) fn on_next_read(&self, hook: impl FnOnce() + Send + 'static) {
        *lock(&self.after_read) = Some(Box::new(hook));
    }

    /// Between reading a part or file and re-checking the grant (tests only).
    fn after_read(&self) {
        #[cfg(test)]
        if let Some(hook) = lock(&self.after_read).take() {
            hook();
        }
    }

    /// Delete every staged export of one member (revocation, rotation, new export).
    pub(crate) fn cancel_user(&self, user: &str) {
        let mut book = lock(&self.book);
        let ids: Vec<String> = book
            .exports
            .iter()
            .filter(|(_, meta)| meta.owner == user)
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            Self::remove(&mut book, &id);
        }
    }

    pub(crate) fn cancel_all(&self) {
        let mut book = lock(&self.book);
        let ids: Vec<String> = book.exports.keys().cloned().collect();
        for id in ids {
            Self::remove(&mut book, &id);
        }
    }

    #[cfg(test)]
    pub(crate) fn count(&self) -> usize {
        lock(&self.book).exports.len()
    }

    #[cfg(test)]
    pub(crate) fn phase_of(&self, id: &str) -> Option<&'static str> {
        lock(&self.book)
            .exports
            .get(id)
            .map(|meta| match meta.phase {
                Phase::Building => "building",
                Phase::Ready => "ready",
                Phase::Failed(..) => "failed",
            })
    }

    #[cfg(test)]
    pub(crate) fn expire_all(&self) {
        for meta in lock(&self.book).exports.values_mut() {
            meta.deadline = Instant::now();
        }
    }

    #[cfg(test)]
    pub(crate) fn set_creating(&self, value: bool) {
        lock(&self.book).creating = value;
    }

    #[cfg(test)]
    pub(crate) fn dirs(&self) -> Vec<PathBuf> {
        lock(&self.book)
            .exports
            .values()
            .map(|m| m.dir.clone())
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Authentication of a pull request

fn deny() -> ApiError {
    forbidden()
}

fn plain_reason(status: StatusCode, message: &'static str) -> ApiError {
    ApiError::new(status, message)
}

/// Token, server and role: who this request is, or a refusal. It reads the
/// database only and spends no rate budget, so it can be repeated after the work
/// to see that nothing changed. `touch` records the use (first check only).
fn authenticate(state: &AppState, headers: &HeaderMap, touch: bool) -> ApiResult<String> {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|token| !token.is_empty() && token.len() <= 128)
        .ok_or_else(deny)?;
    let named = headers
        .get("x-relay-server-id")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(deny)?;
    let hash = token_hash(token);
    let db = lock(&state.0.db);
    if community::server_id(&db)? != named {
        return Err(deny());
    }
    let user: String = db
        .query_row(
            "SELECT user_id FROM replica_tokens WHERE token_hash=?1 AND expires>?2",
            params![hash, now() as i64],
            |r| r.get(0),
        )
        .optional()?
        .ok_or_else(deny)?;
    if community::active_user(&db, &user).is_err() || !hosting::holds_replica(&db, &user) {
        // Lazy cleanup: a member who lost the role keeps no token, and when no
        // accepted Replica is left the log stops. Both are retried by the periodic
        // pass if this write fails.
        let _ = db.execute("DELETE FROM replica_tokens WHERE user_id=?1", [&user]);
        let stopped = super::retire_if_unused(&db).unwrap_or(false);
        drop(db);
        state.0.replica.cancel_user(&user);
        if stopped {
            state.0.replica.cancel_all();
        }
        return Err(deny());
    }
    if touch {
        db.execute(
            "UPDATE replica_tokens SET last_used=?2 WHERE user_id=?1",
            params![user, now() as i64],
        )?;
    }
    Ok(user)
}

/// Pull request admission: who it is, then the burst allowance.
fn pull_auth(state: &AppState, headers: &HeaderMap, traffic: Traffic) -> ApiResult<String> {
    let user = authenticate(state, headers, true)?;
    let mut book = lock(&state.0.replica.book);
    Service::sweep(&mut book);
    let (key, burst) = match traffic {
        Traffic::Control => (user.clone(), BURST_REQUESTS),
        Traffic::Download => (format!("{user}:download"), BURST_DOWNLOADS),
    };
    // A new key is admitted only while the book has room; replicas already in it are never turned away for this.
    if !book.recent.contains_key(&key) && book.recent.len() >= 2 * MAX_TRACKED_REPLICAS {
        return Err(plain_reason(StatusCode::TOO_MANY_REQUESTS, "rate_limited"));
    }
    let window = book.recent.entry(key).or_default();
    let cutoff = Instant::now()
        .checked_sub(BURST_WINDOW)
        .unwrap_or_else(Instant::now);
    while window.front().is_some_and(|at| *at < cutoff) {
        window.pop_front();
    }
    if window.len() >= burst {
        return Err(plain_reason(StatusCode::TOO_MANY_REQUESTS, "rate_limited"));
    }
    window.push_back(Instant::now());
    Ok(user)
}

/// The same token, the same member and the same staged export, checked again
/// after the work and before anything is sent. It spends no rate budget. A token
/// rotated, a role withdrawn or an access change that moved the permission
/// generation or lineage while the bytes were being read refuses the response:
/// an older grant is never honoured.
fn still_granted(state: &AppState, headers: &HeaderMap, user: &str, id: &str) -> ApiResult<()> {
    if authenticate(state, headers, false)? != user {
        return Err(deny());
    }
    ready_export(state, user, id, |_| ())
}

fn refusal(refusal: Refusal) -> (StatusCode, &'static str) {
    match refusal {
        Refusal::NotAReplica => (StatusCode::FORBIDDEN, "Access denied"),
        Refusal::Stale => (StatusCode::CONFLICT, "stale"),
        Refusal::Diverged => (StatusCode::CONFLICT, "diverged"),
        Refusal::TooLarge => (StatusCode::PAYLOAD_TOO_LARGE, "export_too_large"),
        Refusal::Busy => (StatusCode::SERVICE_UNAVAILABLE, "export_busy"),
        Refusal::Unavailable | Refusal::Disabled | Refusal::Database => {
            (StatusCode::SERVICE_UNAVAILABLE, "replica_unavailable")
        }
    }
}

// ---------------------------------------------------------------------------
// The member's own token

#[derive(Serialize)]
pub(crate) struct TokenIssued {
    token: String,
    expires: u64,
    server_id: String,
    note: &'static str,
}

/// The member mints (or rotates) their own pull token after accepting the role.
pub(crate) async fn mint_token(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<TokenIssued>> {
    let auth = state.auth(&headers)?;
    let token = random_token();
    let expires = now() + TOKEN_LIFETIME_SECS;
    let server_id =
        {
            let mut db = lock(&state.0.db);
            let tx = db.transaction()?;
            if community::active_user(&tx, &auth.user.id).is_err()
                || !hosting::holds_replica(&tx, &auth.user.id)
            {
                return Err(forbidden());
            }
            tx.execute(
                "DELETE FROM replica_tokens WHERE user_id=?1",
                [&auth.user.id],
            )?;
            tx.execute(
            "INSERT INTO replica_tokens(user_id,token_hash,created,expires) VALUES(?1,?2,?3,?4)",
            params![auth.user.id, token_hash(&token), now() as i64, expires as i64],
        )?;
            roles::audit(&tx, &auth.user.id, "replica.token.created", &auth.user.id)?;
            // Minting a token is the member's own consent step, and the first one
            // starts the change log: it is never on before it, and it records from
            // this commit on. Not while the Friends service makes exports unavailable.
            if !crate::people::enabled(&tx) && !super::state(&tx)?.enabled {
                super::enable(&tx)?;
                roles::audit(&tx, &auth.user.id, "replica.log.enabled", &auth.user.id)?;
            }
            let server_id = community::server_id(&tx)?;
            tx.commit()?;
            server_id
        };
    state.0.replica.cancel_user(&auth.user.id);
    Ok(Json(TokenIssued {
        token,
        expires,
        server_id,
        note: "Shown once. It opens only the data-copy routes of this community.",
    }))
}

pub(crate) async fn revoke_token(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<serde_json::Value>> {
    let auth = state.auth(&headers)?;
    {
        let mut db = lock(&state.0.db);
        let tx = db.transaction()?;
        tx.execute(
            "DELETE FROM replica_tokens WHERE user_id=?1",
            [&auth.user.id],
        )?;
        roles::audit(&tx, &auth.user.id, "replica.token.revoked", &auth.user.id)?;
        tx.commit()?;
    }
    state.0.replica.cancel_user(&auth.user.id);
    Ok(Json(serde_json::json!({"ok":true})))
}

/// Called inside the transaction that changes the roster: whoever no longer
/// holds the role loses the token in the same commit, and when that was the last
/// accepted Replica the log stops and is cleared in the same commit.
pub(crate) fn drop_token_if_role_lost(db: &Connection, user: &str) -> rusqlite::Result<bool> {
    if hosting::holds_replica(db, user) {
        return Ok(false);
    }
    let dropped = db.execute("DELETE FROM replica_tokens WHERE user_id=?1", [user])? > 0;
    super::retire_if_unused(db)?;
    Ok(dropped)
}

/// After a roster change has committed: the member's staged exports end, and
/// when no accepted Replica is left no staged export can be valid.
pub(crate) fn after_roster_change(state: &AppState, user: &str) {
    state.0.replica.cancel_user(user);
    if !hosting::any_replica(&lock(&state.0.db)) {
        state.0.replica.cancel_all();
    }
}

/// The server's periodic pass (cleanup loop): retention, and stopping the log when
/// no accepted Replica remains for any reason (a member left, a role was
/// withdrawn elsewhere, a backup was restored).
pub(crate) fn maintain(state: &AppState) {
    let outcome = super::maintain(&lock(&state.0.db), now() as i64);
    match outcome {
        Ok(super::Maintained::Retired) => state.0.replica.cancel_all(),
        Ok(_) => {}
        Err(error) => eprintln!("replica maintenance failed: {error}"),
    }
}

// ---------------------------------------------------------------------------
// Routes under /api/replica/pull

#[derive(Serialize)]
pub(crate) struct Info {
    replica_format: u32,
    server_id: String,
    lineage: String,
    term: i64,
    schema_version: u32,
    head: i64,
    floor: i64,
}

pub(crate) async fn info(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Info>> {
    pull_auth(&state, &headers, Traffic::Control)?;
    let db = lock(&state.0.db);
    let current = super::state(&db)?;
    if !current.enabled {
        return Err(plain_reason(
            StatusCode::SERVICE_UNAVAILABLE,
            "replica_unavailable",
        ));
    }
    Ok(Json(Info {
        replica_format: export::REPLICA_FORMAT,
        server_id: community::server_id(&db)?,
        lineage: current.lineage,
        term: current.term,
        schema_version: community::SCHEMA_VERSION,
        head: super::head(&db)?,
        floor: current.floor,
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreateExport {
    replica_id: String,
    #[serde(default)]
    after: Option<i64>,
    #[serde(default)]
    lineage: Option<String>,
    #[serde(default)]
    previous_eligible: Vec<String>,
}

fn clean_id(value: &str) -> bool {
    (1..=64).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
}

fn open_reader(data: &FsPath) -> rusqlite::Result<Connection> {
    let db = Connection::open_with_flags(
        data.join("relay.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    db.busy_timeout(StdDuration::from_secs(5))?;
    db.pragma_update(None, "query_only", true)?;
    Ok(db)
}

/// Cut an export into immutable parts of at most `PART_BYTES` and write them
/// with the manifest. The manifest is written last, so a half-staged export has none.
fn stage(export: &Export, dir: &FsPath, export_id: &str) -> std::io::Result<(Manifest, u64)> {
    let parts_dir = dir.join("parts");
    std::fs::create_dir_all(&parts_dir)?;
    let mut refs = Vec::new();
    let mut total = 0u64;
    for (index, part) in export::parts(export, PART_BYTES).iter().enumerate() {
        let bytes = serde_json::to_vec(part).map_err(std::io::Error::other)?;
        std::fs::write(parts_dir.join(format!("{index:05}")), &bytes)?;
        total += bytes.len() as u64;
        refs.push(PartRef {
            index,
            bytes: bytes.len(),
            sha256: format!("{:x}", Sha256::digest(&bytes)),
        });
    }
    let mut manifest = export.manifest.clone();
    // The HTTP resource and its immutable manifest name the same export.
    manifest.export_id = export_id.to_owned();
    manifest.parts = refs;
    let text = serde_json::to_vec(&manifest).map_err(std::io::Error::other)?;
    total += text.len() as u64;
    let temp = dir.join("manifest.json.tmp");
    std::fs::write(&temp, &text)?;
    std::fs::rename(&temp, dir.join("manifest.json"))?;
    Ok((manifest, total))
}

pub(crate) async fn create_export(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<CreateExport>,
) -> ApiResult<Response> {
    let user = pull_auth(&state, &headers, Traffic::Control)?;
    if !clean_id(&input.replica_id)
        || input.previous_eligible.len() > MAX_PREVIOUS_ELIGIBLE
        || input.previous_eligible.iter().any(|id| id.len() > 64)
        || input.after.is_some_and(|after| after < 0)
        || (input.after.is_some() != input.lineage.is_some())
    {
        return Err(bad("Invalid export request"));
    }
    let (lineage, generation) = {
        let db = lock(&state.0.db);
        let current = super::state(&db)?;
        if !current.enabled {
            return Err(plain_reason(
                StatusCode::SERVICE_UNAVAILABLE,
                "replica_unavailable",
            ));
        }
        if input
            .lineage
            .as_ref()
            .is_some_and(|l| *l != current.lineage)
        {
            return Err(plain_reason(StatusCode::CONFLICT, "stale"));
        }
        (current.lineage, current.permission_generation)
    };
    let id = id();
    let dir = exports_root(&state.0.data).join(&id);
    {
        let mut book = lock(&state.0.replica.book);
        Service::sweep(&mut book);
        // A refused request must not cancel the export the replica already has.
        let others = book
            .exports
            .values()
            .filter(|meta| meta.owner != user)
            .count();
        if book.creating || others >= MAX_ACTIVE_TOTAL {
            return Err(plain_reason(StatusCode::TOO_MANY_REQUESTS, "busy"));
        }
        let staged: u64 = book.exports.values().map(|meta| meta.staged_bytes).sum();
        if staged >= STAGING_CAP_BYTES {
            return Err(plain_reason(
                StatusCode::SERVICE_UNAVAILABLE,
                "staging_full",
            ));
        }
        // One active export per replica: an accepted new one cancels the old.
        let old: Vec<String> = book
            .exports
            .iter()
            .filter(|(_, meta)| meta.owner == user)
            .map(|(id, _)| id.clone())
            .collect();
        for old in old {
            Service::remove(&mut book, &old);
        }
        book.creating = true;
        book.exports.insert(
            id.clone(),
            Meta {
                owner: user.clone(),
                phase: Phase::Building,
                lineage,
                permission_generation: generation,
                dir: dir.clone(),
                manifest: None,
                served_parts: BTreeSet::new(),
                staged_bytes: 0,
                deadline: Instant::now() + BUILD_TTL,
            },
        );
    }
    let job_state = state.clone();
    let job_id = id.clone();
    tokio::task::spawn_blocking(move || {
        let previous: BTreeSet<String> = input.previous_eligible.into_iter().collect();
        let built = open_reader(&job_state.0.data)
            .map_err(|_| Refusal::Database)
            .and_then(|reader| {
                export::create(
                    &reader,
                    &Request {
                        recipient: &user,
                        replica_id: &input.replica_id,
                        after: input.after,
                        previous_eligible: &previous,
                        now_ms: now_ms(),
                    },
                )
            });
        let staged = built.and_then(|export| {
            stage(&export, &dir, &job_id)
                .map(|staged| (staged, export.manifest.permission_generation))
                .map_err(|_| Refusal::Database)
        });
        let mut book = lock(&job_state.0.replica.book);
        book.creating = false;
        let Some(meta) = book.exports.get_mut(&job_id) else {
            // Cancelled while building: nothing may stay behind.
            let _ = std::fs::remove_dir_all(&dir);
            return;
        };
        match staged {
            Ok(((manifest, bytes), generation)) => {
                meta.phase = Phase::Ready;
                meta.permission_generation = generation;
                meta.staged_bytes = bytes;
                meta.manifest = Some(manifest);
                meta.deadline = Instant::now() + READY_TTL;
            }
            Err(error) => {
                let (status, code) = refusal(error);
                meta.phase = Phase::Failed(status, code);
                meta.deadline = Instant::now() + StdDuration::from_secs(60);
                let _ = std::fs::remove_dir_all(&dir);
            }
        }
    });
    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({"export_id": id, "state": "building"})),
    )
        .into_response())
}

/// The export a request names, if it is this member's, still ready and still valid.
fn ready_export<T>(
    state: &AppState,
    user: &str,
    id: &str,
    use_meta: impl FnOnce(&mut Meta) -> T,
) -> ApiResult<T> {
    let (generation, lineage) = {
        let db = lock(&state.0.db);
        let current = super::state(&db)?;
        (current.permission_generation, current.lineage)
    };
    let mut book = lock(&state.0.replica.book);
    Service::sweep(&mut book);
    let Some(meta) = book.exports.get_mut(id).filter(|meta| meta.owner == user) else {
        return Err(missing());
    };
    match meta.phase {
        Phase::Building => return Err(plain_reason(StatusCode::CONFLICT, "building")),
        Phase::Failed(status, code) => {
            Service::remove(&mut book, id);
            return Err(plain_reason(status, code));
        }
        Phase::Ready => {}
    }
    if meta.permission_generation != generation || meta.lineage != lineage {
        Service::remove(&mut book, id);
        return Err(plain_reason(StatusCode::CONFLICT, "export_invalidated"));
    }
    Ok(use_meta(meta))
}

pub(crate) async fn get_export(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let user = pull_auth(&state, &headers, Traffic::Control)?;
    let building = {
        let book = lock(&state.0.replica.book);
        book.exports
            .get(&id)
            .is_some_and(|meta| meta.owner == user && meta.phase == Phase::Building)
    };
    if building {
        return Ok(Json(serde_json::json!({"state":"building"})).into_response());
    }
    let manifest =
        ready_export(&state, &user, &id, |meta| meta.manifest.clone())?.ok_or_else(missing)?;
    Ok(Json(manifest).into_response())
}

fn guard_bytes(state: &AppState, user: &str, add: u64) -> ApiResult<()> {
    let mut book = lock(&state.0.replica.book);
    Service::prune_rates(&mut book, Instant::now());
    let day = now() / 86_400;
    if !book.daily.contains_key(user) && book.daily.len() >= MAX_TRACKED_REPLICAS {
        return Err(plain_reason(StatusCode::TOO_MANY_REQUESTS, "daily_limit"));
    }
    let entry = book.daily.entry(user.to_string()).or_insert((day, 0));
    if entry.0 != day {
        *entry = (day, 0);
    }
    if entry.1.saturating_add(add) > DAILY_BYTES_CAP {
        return Err(plain_reason(StatusCode::TOO_MANY_REQUESTS, "daily_limit"));
    }
    entry.1 += add;
    Ok(())
}

fn binary(hash: &str, bytes: Vec<u8>) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        "application/octet-stream".parse().expect("static header"),
    );
    if let Ok(value) = hash.parse() {
        headers.insert("x-relay-sha256", value);
    }
    (StatusCode::OK, headers, bytes).into_response()
}

fn record_served(db: &Connection, user: &str, head: i64, bytes: u64) -> rusqlite::Result<()> {
    db.execute(
        "UPDATE replica_tokens SET served_head=MAX(served_head,?2),served_bytes=served_bytes+?3 WHERE user_id=?1",
        params![user, head, bytes as i64],
    )?;
    Ok(())
}

pub(crate) async fn get_part(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, index)): Path<(String, usize)>,
) -> ApiResult<Response> {
    let user = pull_auth(&state, &headers, Traffic::Download)?;
    let (dir, wanted, head) = ready_export(&state, &user, &id, |meta| {
        let manifest = meta.manifest.as_ref();
        (
            meta.dir.clone(),
            manifest.and_then(|m| m.parts.get(index).cloned()),
            manifest.map_or(0, |m| m.head),
        )
    })?;
    let part = wanted.ok_or_else(missing)?;
    guard_bytes(&state, &user, part.bytes as u64)?;
    let bytes =
        std::fs::read(dir.join("parts").join(format!("{index:05}"))).map_err(|_| missing())?;
    if bytes.len() != part.bytes || format!("{:x}", Sha256::digest(&bytes)) != part.sha256 {
        // Staged files are immutable; a change on disk is not served.
        state.0.replica.cancel_user(&user);
        return Err(plain_reason(
            StatusCode::SERVICE_UNAVAILABLE,
            "replica_unavailable",
        ));
    }
    state.0.replica.after_read();
    // The grant was checked before the read; it must still hold now.
    still_granted(&state, &headers, &user, &id)?;
    {
        let mut book = lock(&state.0.replica.book);
        if let Some(meta) = book.exports.get_mut(&id) {
            meta.served_parts.insert(index);
        }
    }
    record_served(&lock(&state.0.db), &user, head, bytes.len() as u64)?;
    Ok(binary(&part.sha256, bytes))
}

#[derive(Deserialize)]
pub(crate) struct FileQuery {
    version: i64,
    size: i64,
}

pub(crate) async fn get_file(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, table, file_id)): Path<(String, String, String)>,
    Query(query): Query<FileQuery>,
) -> ApiResult<Response> {
    let user = pull_auth(&state, &headers, Traffic::Download)?;
    let wanted = FileRef {
        table,
        id: file_id,
        version: query.version,
        size: query.size,
    };
    // Only files the manifest lists can be fetched, and only at the listed version.
    let listed = ready_export(&state, &user, &id, |meta| {
        meta.manifest
            .as_ref()
            .is_some_and(|m| m.files.contains(&wanted))
    })?;
    if !listed {
        return Err(missing());
    }
    guard_bytes(&state, &user, wanted.size.max(0) as u64)?;
    let job_state = state.clone();
    let job_user = user.clone();
    let job_file = wanted.clone();
    let read = tokio::task::spawn_blocking(move || {
        let reader = open_reader(&job_state.0.data).map_err(|_| export::FileRefusal::Database)?;
        export::read_file(&reader, &job_user, &job_file)
    })
    .await
    .map_err(|_| plain_reason(StatusCode::SERVICE_UNAVAILABLE, "replica_unavailable"))?;
    match read {
        Ok(file) => {
            state.0.replica.after_read();
            still_granted(&state, &headers, &user, &id)?;
            record_served(&lock(&state.0.db), &user, 0, file.bytes.len() as u64)?;
            Ok(binary(&file.sha256, file.bytes))
        }
        Err(export::FileRefusal::Forbidden) => Err(deny()),
        Err(export::FileRefusal::NotFound) => Err(missing()),
        Err(export::FileRefusal::Changed) => Err(plain_reason(StatusCode::GONE, "file_changed")),
        Err(export::FileRefusal::Database) => Err(plain_reason(
            StatusCode::SERVICE_UNAVAILABLE,
            "replica_unavailable",
        )),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Claim {
    applied: i64,
    #[serde(default)]
    files_pending: u32,
}

/// The replica reports what it applied. The source records it with its own clock
/// and refuses a claim above what it served.
pub(crate) async fn finish_export(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(claim): Json<Claim>,
) -> ApiResult<Json<serde_json::Value>> {
    let user = pull_auth(&state, &headers, Traffic::Control)?;
    let (complete, head) = ready_export(&state, &user, &id, |meta| {
        let manifest = meta.manifest.as_ref();
        (
            manifest.is_some_and(|m| meta.served_parts.len() == m.parts.len()),
            manifest.map_or(0, |m| m.head),
        )
    })?;
    if !complete {
        return Err(plain_reason(StatusCode::CONFLICT, "incomplete"));
    }
    if claim.applied != head {
        return Err(plain_reason(StatusCode::CONFLICT, "head_mismatch"));
    }
    {
        let db = lock(&state.0.db);
        db.execute(
            "UPDATE replica_tokens SET ack_head=?2,ack_ms=?3,claim_files_pending=?4,claim_ms=?3 WHERE user_id=?1",
            params![user, claim.applied, now_ms() as i64, claim.files_pending],
        )?;
    }
    let mut book = lock(&state.0.replica.book);
    Service::remove(&mut book, &id);
    Ok(Json(serde_json::json!({"ok":true})))
}

pub(crate) async fn cancel_export(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let user = pull_auth(&state, &headers, Traffic::Control)?;
    let mut book = lock(&state.0.replica.book);
    if book.exports.get(&id).is_some_and(|meta| meta.owner == user) {
        Service::remove(&mut book, &id);
    }
    Ok(Json(serde_json::json!({"ok":true})))
}

/// Later file progress: a claim about the copy, stored with the source's time.
pub(crate) async fn post_status(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(claim): Json<Claim>,
) -> ApiResult<Json<serde_json::Value>> {
    let user = pull_auth(&state, &headers, Traffic::Control)?;
    let db = lock(&state.0.db);
    let served: i64 = db.query_row(
        "SELECT served_head FROM replica_tokens WHERE user_id=?1",
        [&user],
        |r| r.get(0),
    )?;
    if claim.applied < 0 || claim.applied > served {
        return Err(plain_reason(StatusCode::CONFLICT, "head_mismatch"));
    }
    db.execute(
        "UPDATE replica_tokens SET ack_head=?2,claim_files_pending=?3,claim_ms=?4 WHERE user_id=?1",
        params![user, claim.applied, claim.files_pending, now_ms() as i64],
    )?;
    Ok(Json(serde_json::json!({"ok":true})))
}

/// Measured by the source: what it served and what the replica claimed, with
/// the source's own times. For the Hosts page; claims are labelled as claims.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Measured {
    pub last_used_s: Option<i64>,
    pub served_head: i64,
    pub served_bytes: i64,
    pub claimed_head: Option<i64>,
    pub claimed_ms: Option<i64>,
    pub claimed_files_pending: Option<i64>,
}

pub(crate) fn measured(db: &Connection, user: &str) -> rusqlite::Result<Option<Measured>> {
    db.query_row(
        "SELECT last_used,served_head,served_bytes,ack_head,claim_ms,claim_files_pending FROM replica_tokens WHERE user_id=?1",
        [user],
        |r| {
            Ok(Measured {
                last_used_s: r.get(0)?,
                served_head: r.get(1)?,
                served_bytes: r.get(2)?,
                claimed_head: r.get(3)?,
                claimed_ms: r.get(4)?,
                claimed_files_pending: r.get(5)?,
            })
        },
    )
    .optional()
}

// Reassembling the parts is the replica's job; this keeps the type in one place.
pub(crate) fn assemble(manifest: Manifest, parts: Vec<PartData>) -> Result<Export, ()> {
    export::assemble(manifest, parts)
}
