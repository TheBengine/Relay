//! M2 slice 3, source side, in process: a full snapshot or a catch-up delta for
//! one consenting member, built in one read transaction at one pinned head.
//!
//! * A delta is the state at head `H` for every key that changed in `(W, H]`
//!   plus the rows whose visibility changed. There is no limit parameter: an
//!   export that is too big is refused, never cut into a "prefix" that no state
//!   ever had.
//! * What the recipient may read is decided by `community::channel_access`, not
//!   by `channels.visibility`. Plaintext the member could not read as a member is
//!   replaced by a marker; ciphertext is exported for every target (decision D2,
//!   with the metadata and retained-copy consequences the contract states).
//! * The catch-up needs no server-side ledger: the replica returns the set of
//!   channels it was last given (`previous_eligible`). A wrong set can only make
//!   the source send more (what the member may read now) or less (it never sends
//!   what the member may not read); it cannot widen access.
//! * Blobs never enter an export. Files are listed in the manifest and fetched
//!   one by one with `read_file`, which re-checks access, version and size.
//! * One shared budget (`Limits`) is charged while rows are read: the eligible
//!   set, the collected keys (log, moved channels, dependencies), every kept row
//!   and tombstone and the file list. The first thing that does not fit ends the
//!   build with `TooLarge`; nothing is read whole first and checked afterwards.
//! * The export lives in memory only as long as it takes to stage it; staging
//!   (`service::stage`) serializes borrowed parts one at a time (`parts`).
use super::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::{Arc, Condvar};
use std::time::Instant;

pub(crate) const REPLICA_FORMAT: u32 = 1;
/// Changed keys above this answer "take a full snapshot" (`TooLarge`).
pub(crate) const MAX_DELTA_KEYS: usize = 200_000;
/// Shared work-set allowance, including row storage and serialized size. This is
/// conservative accounting, not an RSS cap for SQLite, the allocator or staging.
pub(crate) const MAX_EXPORT_BYTES: usize = 256 * 1024 * 1024;
pub(crate) const MAX_BUILD_SECS: u64 = 60;
/// Rough in-memory cost of one collected id or key beyond its text.
const ENTRY_OVERHEAD: usize = 48;

/// Caps for one build. `max_bytes` accounts for the build's retained data
/// (the eligible set, collected keys, shaped rows, tombstones, the file list);
/// row accounting includes structural storage as well as JSON size.
/// `max_keys` caps the distinct keys a delta may re-read. Tests inject small values.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Limits {
    pub max_bytes: usize,
    pub max_keys: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_bytes: MAX_EXPORT_BYTES,
            max_keys: MAX_DELTA_KEYS,
        }
    }
}

/// The build's shared allowance. It is charged while rows are read, so an export
/// that is too big is refused as soon as it is known, before it is held in memory,
/// and never answered with the part that happened to fit.
struct Budget {
    limits: Limits,
    bytes: usize,
}

impl Budget {
    fn new(limits: Limits) -> Self {
        Self { limits, bytes: 0 }
    }

    /// No cap, for the single-file read that never builds an export.
    fn unbounded() -> Self {
        Self::new(Limits {
            max_bytes: usize::MAX,
            max_keys: usize::MAX,
        })
    }

    fn charge(&mut self, bytes: usize) -> Result<(), Refusal> {
        self.bytes = self.bytes.saturating_add(bytes);
        if self.bytes > self.limits.max_bytes {
            Err(Refusal::TooLarge)
        } else {
            Ok(())
        }
    }

    fn left(&self) -> usize {
        self.limits.max_bytes.saturating_sub(self.bytes)
    }
}

/// Counts or hashes JSON as it is written, so no whole-table buffer is ever built.
struct Counter(usize);

impl std::io::Write for Counter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0 += buf.len();
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct HashWriter(Sha256);

impl std::io::Write for HashWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.update(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Serialized size of a plain JSON value, without allocating it.
fn json_len<T: Serialize + ?Sized>(value: &T) -> usize {
    let mut counter = Counter(0);
    serde_json::to_writer(&mut counter, value).expect("plain JSON values always serialize");
    counter.0
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// Friends service, or the trigger set is not intact.
    Unavailable,
    /// The change log has not been enabled.
    Disabled,
    /// The member does not hold an accepted `Replica` role (or is not active).
    NotAReplica,
    /// The cursor is older than the log floor: take a full snapshot.
    Stale,
    /// The cursor is newer than the source's head (the source was rolled back).
    Diverged,
    /// Over a cap. Never answered with a smaller export.
    TooLarge,
    /// Building took longer than allowed.
    Busy,
    Database,
}

impl From<rusqlite::Error> for Refusal {
    fn from(error: rusqlite::Error) -> Self {
        // A read cut off at the deadline is "busy", not a damaged database.
        match error {
            rusqlite::Error::SqliteFailure(failure, _)
                if failure.code == rusqlite::ErrorCode::OperationInterrupted =>
            {
                Self::Busy
            }
            _ => Self::Database,
        }
    }
}

/// Cancels a long SQL read at the deadline, on the export's OWN read-only
/// connection only. It arms only when the connection is `query_only` (the one
/// `service::open_reader` makes for an export); a read-write connection such as
/// the live database is never armed, so the live database and every other call
/// are never interrupted. Dropping it disarms and joins the watchdog, so nothing
/// can fire after the build.
pub(crate) struct ReadDeadline {
    fired: Arc<AtomicBool>,
    stop: Arc<(Mutex<bool>, Condvar)>,
    thread: Option<std::thread::JoinHandle<()>>,
}

type Job = Box<dyn FnOnce() + Send + 'static>;

fn spawn_watchdog(job: Job) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("replica-read-deadline".into())
        .spawn(job)
}

impl ReadDeadline {
    /// `Ok(None)` only for a read-write connection (the live database), which is
    /// never armed. On a `query_only` connection the deadline is the only thing
    /// that can stop a runaway read, so a watchdog that cannot start (or a mode
    /// that cannot be read) is a refusal, never an export built without one.
    pub(crate) fn arm(
        db: &Connection,
        after: std::time::Duration,
    ) -> Result<Option<Self>, Refusal> {
        Self::arm_with(db, after, spawn_watchdog)
    }

    pub(crate) fn arm_with(
        db: &Connection,
        after: std::time::Duration,
        spawn: impl FnOnce(Job) -> std::io::Result<std::thread::JoinHandle<()>>,
    ) -> Result<Option<Self>, Refusal> {
        let query_only = db.pragma_query_value(None, "query_only", |r| r.get::<_, i64>(0))?;
        if query_only != 1 {
            return Ok(None);
        }
        let handle = db.get_interrupt_handle();
        let fired = Arc::new(AtomicBool::new(false));
        let stop = Arc::new((Mutex::new(false), Condvar::new()));
        let (flag, signal) = (fired.clone(), stop.clone());
        let thread = spawn(Box::new(move || {
            let (done_lock, wake) = &*signal;
            let end = Instant::now() + after;
            let mut done = lock(done_lock);
            while !*done {
                let left = end.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    // Set first, so a swallowed error is still seen as a refusal.
                    flag.store(true, AtomicOrdering::Release);
                    handle.interrupt();
                    break;
                }
                done = match wake.wait_timeout(done, left) {
                    Ok((guard, _)) => guard,
                    Err(poisoned) => poisoned.into_inner().0,
                };
            }
        }))
        .map_err(|_| Refusal::Busy)?;
        Ok(Some(Self {
            fired,
            stop,
            thread: Some(thread),
        }))
    }

    pub(crate) fn fired(&self) -> bool {
        self.fired.load(AtomicOrdering::Acquire)
    }
}

impl Drop for ReadDeadline {
    fn drop(&mut self) {
        let (done_lock, wake) = &*self.stop;
        *lock(done_lock) = true;
        wake.notify_all();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The build's budget: the phase timer and, on a read-only connection, the
/// interrupt that already fired.
struct Clock<'a> {
    started: Instant,
    deadline: std::time::Duration,
    guard: Option<&'a ReadDeadline>,
}

impl Clock<'_> {
    fn check(&self) -> Result<(), Refusal> {
        if self.guard.is_some_and(|guard| guard.fired()) || self.started.elapsed() >= self.deadline
        {
            Err(Refusal::Busy)
        } else {
            Ok(())
        }
    }
}

pub(crate) struct Request<'a> {
    pub recipient: &'a str,
    pub replica_id: &'a str,
    /// `None` asks for a full snapshot; `Some(w)` for the changes after `w`.
    pub after: Option<i64>,
    /// The channels the replica was given last time (empty for a first pull).
    pub previous_eligible: &'a BTreeSet<String>,
    pub now_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TableData {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
    /// Primary keys the replica must delete.
    pub tombstones: Vec<Vec<Value>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TableDigest {
    pub rows: usize,
    pub tombstones: usize,
    pub sha256: String,
}

/// SHA-256 of the table's JSON, hashed while it is written: a table that is
/// hundreds of megabytes is never serialized into a second buffer.
pub(crate) fn digest(data: &TableData) -> TableDigest {
    let mut hasher = HashWriter(Sha256::new());
    serde_json::to_writer(&mut hasher, data).expect("plain JSON values always serialize");
    TableDigest {
        rows: data.rows.len(),
        tombstones: data.tombstones.len(),
        sha256: format!("{:x}", hasher.0.finalize()),
    }
}

pub(crate) fn eligible_digest(eligible: &[String]) -> String {
    format!("{:x}", Sha256::digest(eligible.join("\n").as_bytes()))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FileRef {
    pub table: String,
    pub id: String,
    pub version: i64,
    pub size: i64,
}

/// One staged part of an export: a chunk of one table's rows and tombstones.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PartRef {
    pub index: usize,
    pub bytes: usize,
    pub sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PartData {
    pub table: String,
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
    pub tombstones: Vec<Vec<Value>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Manifest {
    pub replica_format: u32,
    pub source_server_id: String,
    pub lineage: String,
    pub term: i64,
    pub schema_version: u32,
    pub export_id: String,
    pub after: Option<i64>,
    pub head: i64,
    pub permission_generation: i64,
    /// Channels the recipient may read at `head`, sorted.
    pub eligible: Vec<String>,
    pub eligible_set_sha256: String,
    pub created_ms: u64,
    pub recipient_user_id: String,
    pub replica_id: String,
    pub tables: BTreeMap<String, TableDigest>,
    pub files: Vec<FileRef>,
    /// Rows left out or redacted, per table.
    pub withheld: BTreeMap<String, usize>,
    /// No copy made by this version may be promoted (contract 11).
    pub promotion_eligible: bool,
    /// Staged parts, in order. Empty until the export is staged.
    #[serde(default)]
    pub parts: Vec<PartRef>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Export {
    pub manifest: Manifest,
    pub tables: BTreeMap<String, TableData>,
}

// ---------------------------------------------------------------------------
// Visibility

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shown {
    Full,
    /// The row is exported with its conditional columns emptied.
    Redacted,
    Omit,
}

struct Ctx<'a> {
    db: &'a Connection,
    recipient: &'a str,
    eligible: &'a BTreeSet<String>,
}

/// The message-body prefix of end-to-end encrypted content (`e2ee::valid_message_content`).
const CIPHERTEXT_PREFIX: &str = "relay-e2ee:v1:";

pub(crate) fn eligible_channels(
    db: &Connection,
    recipient: &str,
) -> Result<BTreeSet<String>, Refusal> {
    eligible_within(db, recipient, &mut Budget::unbounded())
}

/// The channels the recipient may read, read one id at a time and charged to the
/// build's budget: neither the id list nor the set is built before it is known to fit.
fn eligible_within(
    db: &Connection,
    recipient: &str,
    budget: &mut Budget,
) -> Result<BTreeSet<String>, Refusal> {
    let mut statement = db.prepare("SELECT id FROM channels ORDER BY id")?;
    let mut rows = statement.query([])?;
    let mut eligible = BTreeSet::new();
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        if crate::community::channel_access(db, &id, recipient, crate::community::READ).is_ok() {
            budget.charge(id.len() + ENTRY_OVERHEAD)?;
            eligible.insert(id);
        }
    }
    Ok(eligible)
}

fn cells_of(table: &Table) -> Vec<&'static str> {
    table
        .columns
        .iter()
        .chain(table.conditional)
        .copied()
        .collect()
}

fn position(table: &Table, column: &str) -> usize {
    cells_of(table)
        .iter()
        .position(|name| *name == column)
        .unwrap_or_else(|| panic!("{} has no exported column {column}", table.name))
}

fn text(cells: &[Value], at: usize) -> Option<&str> {
    cells.get(at).and_then(Value::as_str)
}

impl Ctx<'_> {
    fn channel_ok(&self, id: &str) -> bool {
        self.eligible.contains(id)
    }

    fn direct_ok(&self, author: &str, recipient: Option<&str>) -> bool {
        self.recipient == author || recipient == Some(self.recipient)
    }

    /// Whether the message's plaintext may leave. Ciphertext always may.
    fn message_readable(
        &self,
        channel: Option<&str>,
        recipient: Option<&str>,
        author: &str,
        content: &str,
    ) -> bool {
        content.starts_with(CIPHERTEXT_PREFIX)
            || match channel {
                Some(channel) => self.channel_ok(channel),
                None => self.direct_ok(author, recipient),
            }
    }

    fn message_state(&self, id: &str) -> Result<Option<Shown>, Refusal> {
        let row: Option<(Option<String>, Option<String>, String, String)> = self
            .db
            .query_row(
                "SELECT channel_id,recipient_id,author_id,content FROM messages WHERE id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        Ok(row.map(|(channel, recipient, author, content)| {
            if self.message_readable(channel.as_deref(), recipient.as_deref(), &author, &content) {
                Shown::Full
            } else {
                Shown::Redacted
            }
        }))
    }

    /// Looked up per row by the unique `group_id`, so no table of all groups is held.
    fn group_body_ok(&self, group: &str) -> Result<bool, Refusal> {
        let target: Option<(String, String)> = self
            .db
            .prepare_cached("SELECT target_kind,target_id FROM crypto_groups WHERE group_id=?1")?
            .query_row([group], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        Ok(match target {
            Some((kind, id)) if kind == "channel" => self.channel_ok(&id),
            Some((kind, id)) if kind == "direct" => {
                id.split(':').any(|part| part == self.recipient)
            }
            _ => false,
        })
    }

    fn shape(&self, table: &Table, cells: &mut [Value]) -> Result<Shown, Refusal> {
        let at = |name: &str| position(table, name);
        let channel_scoped = |name: &str, cells: &[Value]| {
            if text(cells, at(name)).is_some_and(|id| self.channel_ok(id)) {
                Shown::Full
            } else {
                Shown::Omit
            }
        };
        Ok(match table.name {
            "channels" => {
                if text(cells, at("id")).is_some_and(|id| self.channel_ok(id)) {
                    Shown::Full
                } else {
                    cells[at("name")] = Value::Null;
                    cells[at("topic")] = Value::Null;
                    Shown::Redacted
                }
            }
            "channel_permissions"
            | "conversations"
            | "conversation_members"
            | "channel_pins"
            | "scheduled_events" => channel_scoped("channel_id", cells),
            "event_interest" => {
                let channel: Option<String> = self
                    .db
                    .query_row(
                        "SELECT channel_id FROM scheduled_events WHERE id=?1",
                        [text(cells, at("event_id")).unwrap_or_default()],
                        |r| r.get(0),
                    )
                    .optional()?;
                if channel.is_some_and(|id| self.channel_ok(&id)) {
                    Shown::Full
                } else {
                    Shown::Omit
                }
            }
            // Other people's read positions are not readable by a member today.
            "read_states" => {
                if text(cells, at("user_id")) == Some(self.recipient) {
                    Shown::Full
                } else {
                    Shown::Omit
                }
            }
            "messages" => {
                let readable = self.message_readable(
                    text(cells, at("channel_id")),
                    text(cells, at("recipient_id")),
                    text(cells, at("author_id")).unwrap_or_default(),
                    text(cells, at("content")).unwrap_or_default(),
                );
                if readable {
                    Shown::Full
                } else {
                    for name in table.conditional {
                        cells[at(name)] = Value::Null;
                    }
                    Shown::Redacted
                }
            }
            // The order slot of a marker message is still needed to place it.
            "message_order" => match text(cells, at("message_id")) {
                Some(id) if self.message_state(id)?.is_some() => Shown::Full,
                _ => Shown::Omit,
            },
            // The change feed is target-scoped: it carries no content to be redacted.
            "message_changes" => {
                let channel = text(cells, at("channel_id"));
                let ok = match channel {
                    Some(channel) => self.channel_ok(channel),
                    None => self.direct_ok(
                        text(cells, at("author_id")).unwrap_or_default(),
                        text(cells, at("recipient_id")),
                    ),
                };
                if ok {
                    Shown::Full
                } else {
                    Shown::Omit
                }
            }
            "send_receipts" | "reactions" | "mentions" | "uploads" => {
                match text(cells, at("message_id")) {
                    Some(id) if self.message_state(id)? == Some(Shown::Full) => Shown::Full,
                    _ => Shown::Omit,
                }
            }
            "crypto_events" => {
                if self.group_body_ok(text(cells, at("group_id")).unwrap_or_default())? {
                    Shown::Full
                } else {
                    cells[at("body")] = Value::Null;
                    Shown::Redacted
                }
            }
            _ => Shown::Full,
        })
    }
}

/// Extra row filter for tables where only part of the rows may ever leave.
fn row_filter(table: &str) -> String {
    match table {
        "crypto_devices" => {
            "active=1 AND user_id IN (SELECT user_id FROM membership WHERE status='active')".into()
        }
        "device_attestations" => "device_id IN (SELECT device_id FROM crypto_devices WHERE active=1) AND user_id IN (SELECT user_id FROM membership WHERE status='active')".into(),
        "metadata" => {
            let keys = METADATA_KEYS
                .iter()
                .map(|key| format!("'{key}'"))
                .collect::<Vec<_>>()
                .join(",");
            format!("key IN({keys})")
        }
        _ => String::new(),
    }
}

/// Rows of this table that belong to the channels in `?1` (a JSON array of ids).
fn channel_scope(table: &str) -> Option<&'static str> {
    Some(match table {
        "channels" => "id IN (SELECT value FROM json_each(?1))",
        "channel_permissions" | "conversations" | "conversation_members" | "channel_pins"
        | "scheduled_events" | "messages" | "message_changes" => {
            "channel_id IN (SELECT value FROM json_each(?1))"
        }
        "event_interest" => {
            "event_id IN (SELECT id FROM scheduled_events WHERE channel_id IN (SELECT value FROM json_each(?1)))"
        }
        "message_order" | "send_receipts" | "reactions" | "mentions" | "uploads" => {
            "message_id IN (SELECT id FROM messages WHERE channel_id IN (SELECT value FROM json_each(?1)))"
        }
        "crypto_events" => {
            "group_id IN (SELECT group_id FROM crypto_groups WHERE target_kind='channel' AND target_id IN (SELECT value FROM json_each(?1)))"
        }
        _ => return None,
    })
}

fn value_of(
    value: rusqlite::types::ValueRef<'_>,
    taken: &mut usize,
    left: usize,
) -> Result<Value, Refusal> {
    use rusqlite::types::ValueRef;
    Ok(match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => Value::from(i),
        ValueRef::Real(f) => serde_json::Number::from_f64(f).map_or(Value::Null, Value::Number),
        ValueRef::Text(bytes) => {
            // Checked before the text is copied: one oversized cell is refused, not allocated.
            *taken = taken.saturating_add(bytes.len().saturating_mul(2));
            if *taken > left {
                return Err(Refusal::TooLarge);
            }
            Value::String(String::from_utf8(bytes.to_vec()).map_err(|_| Refusal::Database)?)
        }
        // Blob columns are never selected; a blob here is a bug, not data.
        ValueRef::Blob(_) => return Err(Refusal::Database),
    })
}

pub(crate) fn sql_value(value: &Value) -> Option<rusqlite::types::Value> {
    use rusqlite::types::Value as Sql;
    match value {
        Value::Null => Some(Sql::Null),
        Value::Number(n) => n.as_i64().map(Sql::Integer),
        Value::String(s) => Some(Sql::Text(s.clone())),
        _ => None,
    }
}

/// One row as JSON values. `left` is what the build may still hold: a row whose
/// text alone is bigger is refused before any of it is copied.
fn read_row(row: &rusqlite::Row<'_>, width: usize, left: usize) -> Result<Vec<Value>, Refusal> {
    let mut taken = row_storage(width, 0);
    if taken > left {
        return Err(Refusal::TooLarge);
    }
    let mut cells = Vec::with_capacity(width);
    for i in 0..width {
        cells.push(value_of(row.get_ref(i)?, &mut taken, left)?);
    }
    Ok(cells)
}

fn row_storage(capacity: usize, text_capacity: usize) -> usize {
    // The values occupy space even when their wire form is just `0` or `null`.
    // The factor also allows for the outer row vector's spare capacity and
    // allocation overhead, rather than pretending JSON bytes measure RAM.
    std::mem::size_of::<Vec<Value>>()
        .saturating_add(capacity.saturating_mul(std::mem::size_of::<Value>()))
        .saturating_add(text_capacity)
        .saturating_mul(2)
}

pub(crate) fn row_charge(cells: &Vec<Value>) -> usize {
    let text_capacity = cells.iter().fold(0usize, |sum, value| {
        sum.saturating_add(match value {
            Value::String(text) => text.capacity(),
            _ => 0,
        })
    });
    row_storage(cells.capacity(), text_capacity).max(json_len(cells).saturating_add(1))
}

fn select_list(table: &Table) -> String {
    cells_of(table).join(",")
}

fn key_positions(table: &Table) -> Vec<usize> {
    table.key.iter().map(|k| position(table, k)).collect()
}

fn key_of(table: &Table, cells: &[Value]) -> Vec<Value> {
    key_positions(table)
        .into_iter()
        .map(|i| cells[i].clone())
        .collect()
}

fn key_text(key: &[Value]) -> String {
    serde_json::to_string(key).unwrap_or_default()
}

fn empty_table(table: &Table) -> TableData {
    TableData {
        columns: cells_of(table).iter().map(|c| c.to_string()).collect(),
        rows: Vec::new(),
        tombstones: Vec::new(),
    }
}

fn push_row(data: &mut TableData, cells: Vec<Value>, budget: &mut Budget) -> Result<(), Refusal> {
    budget.charge(row_charge(&cells))?;
    data.rows.push(cells);
    Ok(())
}

fn push_tombstone(
    data: &mut TableData,
    key: Vec<Value>,
    budget: &mut Budget,
) -> Result<(), Refusal> {
    budget.charge(row_charge(&key))?;
    data.tombstones.push(key);
    Ok(())
}

/// A phase timer check every so many rows: cheap, and a long read is also cut by the deadline.
const CLOCK_EVERY: usize = 256;

/// Every row of one table as the recipient may see it, read one row at a time and
/// charged to the shared budget as it is kept. The first row that does not fit
/// ends the build with `TooLarge`; nothing already read is handed out.
fn snapshot_table(
    ctx: &Ctx,
    table: &Table,
    clock: &Clock,
    budget: &mut Budget,
    withheld: &mut BTreeMap<String, usize>,
) -> Result<TableData, Refusal> {
    let filter = row_filter(table.name);
    let wher = if filter.is_empty() {
        String::new()
    } else {
        format!(" WHERE {filter}")
    };
    let sql = format!(
        "SELECT {} FROM {}{wher} ORDER BY {}",
        select_list(table),
        table.name,
        table.key.join(",")
    );
    let width = cells_of(table).len();
    let mut data = empty_table(table);
    let mut statement = ctx.db.prepare(&sql)?;
    let mut rows = statement.query([])?;
    let mut seen = 0usize;
    while let Some(row) = rows.next()? {
        let mut cells = read_row(row, width, budget.left())?;
        match ctx.shape(table, &mut cells)? {
            Shown::Full => push_row(&mut data, cells, budget)?,
            Shown::Redacted => {
                *withheld.entry(table.name.into()).or_default() += 1;
                push_row(&mut data, cells, budget)?;
            }
            Shown::Omit => *withheld.entry(table.name.into()).or_default() += 1,
        }
        seen += 1;
        if seen % CLOCK_EVERY == 0 {
            clock.check()?;
        }
    }
    Ok(data)
}

fn fetch(
    ctx: &Ctx,
    table: &Table,
    key: &[Value],
    left: usize,
) -> Result<Option<Vec<Value>>, Refusal> {
    let mut conditions: Vec<String> = table
        .key
        .iter()
        .enumerate()
        .map(|(i, column)| format!("{column}=?{}", i + 1))
        .collect();
    let filter = row_filter(table.name);
    if !filter.is_empty() {
        conditions.push(filter);
    }
    let sql = format!(
        "SELECT {} FROM {} WHERE {}",
        select_list(table),
        table.name,
        conditions.join(" AND ")
    );
    let params: Vec<rusqlite::types::Value> = key
        .iter()
        .map(|value| sql_value(value).ok_or(Refusal::Database))
        .collect::<Result<_, _>>()?;
    let width = cells_of(table).len();
    let mut statement = ctx.db.prepare_cached(&sql)?;
    let mut rows = statement.query(rusqlite::params_from_iter(params))?;
    match rows.next()? {
        Some(row) => Ok(Some(read_row(row, width, left)?)),
        None => Ok(None),
    }
}

/// The distinct keys a delta has to re-read. Every key is charged to the budget
/// and counted against `max_keys` before it is stored, so a delta that touches
/// too much is refused while it is being collected, not after it is held.
#[derive(Default)]
struct Affected {
    keys: BTreeMap<&'static str, BTreeMap<String, Vec<Value>>>,
    count: usize,
}

impl Affected {
    fn insert(
        &mut self,
        table: &'static str,
        key: Vec<Value>,
        budget: &mut Budget,
    ) -> Result<(), Refusal> {
        let text = key_text(&key);
        if self
            .keys
            .get(table)
            .is_some_and(|keys| keys.contains_key(&text))
        {
            return Ok(());
        }
        if self.count >= budget.limits.max_keys {
            return Err(Refusal::TooLarge);
        }
        budget.charge(
            text.capacity()
                .saturating_add(row_charge(&key))
                .saturating_add(ENTRY_OVERHEAD),
        )?;
        self.keys.entry(table).or_default().insert(text, key);
        self.count += 1;
        Ok(())
    }

    fn of(&self, table: &str) -> impl Iterator<Item = &Vec<Value>> {
        self.keys
            .get(table)
            .into_iter()
            .flat_map(|keys| keys.values())
    }
}

/// Stream the primary keys of the rows of `table` matching `condition` into
/// `affected`, one row at a time. Only key columns are read, never row content.
fn stream_keys(
    db: &Connection,
    table: &Table,
    condition: &str,
    params: impl rusqlite::Params,
    affected: &mut Affected,
    budget: &mut Budget,
    clock: &Clock,
) -> Result<(), Refusal> {
    let sql = format!(
        "SELECT {} FROM {} WHERE {condition}",
        table.key.join(","),
        table.name
    );
    let mut statement = db.prepare_cached(&sql)?;
    let mut rows = statement.query(params)?;
    let mut seen = 0usize;
    while let Some(row) = rows.next()? {
        let key = read_row(row, table.key.len(), budget.left())?;
        affected.insert(table.name, key, budget)?;
        seen += 1;
        if seen % CLOCK_EVERY == 0 {
            clock.check()?;
        }
    }
    Ok(())
}

fn keys_in_channels(
    db: &Connection,
    table: &Table,
    channels: &BTreeSet<String>,
    affected: &mut Affected,
    budget: &mut Budget,
    clock: &Clock,
) -> Result<(), Refusal> {
    let Some(scope) = channel_scope(table.name) else {
        return Ok(());
    };
    let list = serde_json::to_string(&channels.iter().collect::<Vec<_>>())
        .map_err(|_| Refusal::Database)?;
    stream_keys(db, table, scope, [list], affected, budget, clock)
}

/// Rows whose visibility depends on another row that changed, without being
/// written themselves: a device follows its owner's membership, an attestation
/// follows its device, and the attachments of a message follow the message.
/// The children are streamed into the same bounded set, never gathered first.
fn expand_dependencies(
    db: &Connection,
    affected: &mut Affected,
    budget: &mut Budget,
    clock: &Clock,
) -> Result<(), Refusal> {
    // The parents are read before anything is added, as before: an attestation
    // found through a membership is not expanded a second time through its device.
    let first = |affected: &Affected, name: &str| -> Vec<Value> {
        affected.of(name).map(|key| key[0].clone()).collect()
    };
    let members = first(affected, "membership");
    let devices = first(affected, "crypto_devices");
    let messages = first(affected, "messages");

    let children = |table: &str,
                    column: &str,
                    value: &Value,
                    affected: &mut Affected,
                    budget: &mut Budget|
     -> Result<(), Refusal> {
        let info = table_for(table).ok_or(Refusal::Database)?;
        let bound = sql_value(value).ok_or(Refusal::Database)?;
        stream_keys(
            db,
            info,
            &format!("{column}=?1"),
            [bound],
            affected,
            budget,
            clock,
        )
    };
    for member in &members {
        children("crypto_devices", "user_id", member, affected, budget)?;
        children("device_attestations", "user_id", member, affected, budget)?;
    }
    for device in &devices {
        children("device_attestations", "device_id", device, affected, budget)?;
    }
    for message in &messages {
        for child in [
            "message_order",
            "reactions",
            "mentions",
            "uploads",
            "send_receipts",
        ] {
            children(child, "message_id", message, affected, budget)?;
        }
    }
    Ok(())
}

fn parse_key(table: &Table, text: &str) -> Result<Vec<Value>, Refusal> {
    let key: Vec<Value> = serde_json::from_str(text).map_err(|_| Refusal::Unavailable)?;
    if key.len() == table.key.len() {
        Ok(key)
    } else {
        Err(Refusal::Unavailable)
    }
}

// ---------------------------------------------------------------------------
// Building an export

fn files_of(
    db: &Connection,
    tables: &BTreeMap<String, TableData>,
    budget: &mut Budget,
) -> Result<Vec<FileRef>, Refusal> {
    let mut files = Vec::new();
    let mut add = |file: FileRef, budget: &mut Budget| -> Result<(), Refusal> {
        budget.charge(json_len(&file) + 1)?;
        files.push(file);
        Ok(())
    };
    if let Some(data) = tables.get("uploads") {
        let table = table_for("uploads").ok_or(Refusal::Database)?;
        let (id, size) = (position(table, "id"), position(table, "size"));
        for row in &data.rows {
            if let (Some(id), Some(size)) = (text(row, id), row[size].as_i64()) {
                add(
                    FileRef {
                        table: "uploads".into(),
                        id: id.into(),
                        version: 0,
                        size,
                    },
                    budget,
                )?;
            }
        }
    }
    for (name, key, blob) in [
        ("user_profiles", "user_id", "avatar"),
        ("presentation_assets", "id", "data"),
    ] {
        let Some(data) = tables.get(name) else {
            continue;
        };
        let table = table_for(name).ok_or(Refusal::Database)?;
        let (id, version) = (position(table, key), position(table, "version"));
        for row in &data.rows {
            let (Some(id), Some(version)) = (text(row, id), row[version].as_i64()) else {
                continue;
            };
            let size: Option<i64> = db
                .query_row(
                    &format!("SELECT length({blob}) FROM {name} WHERE {key}=?1"),
                    [id],
                    |r| r.get(0),
                )
                .optional()?
                .flatten();
            if let Some(size) = size.filter(|size| *size > 0) {
                add(
                    FileRef {
                        table: name.into(),
                        id: id.into(),
                        version,
                        size,
                    },
                    budget,
                )?;
            }
        }
    }
    files.sort_by(|a, b| (&a.table, &a.id).cmp(&(&b.table, &b.id)));
    Ok(files)
}

/// Build an export within the standard time budget and size caps.
pub(crate) fn create(db: &Connection, request: &Request) -> Result<Export, Refusal> {
    create_with_deadline(db, request, std::time::Duration::from_secs(MAX_BUILD_SECS))
}

pub(crate) fn create_with_deadline(
    db: &Connection,
    request: &Request,
    deadline: std::time::Duration,
) -> Result<Export, Refusal> {
    create_with_limits(db, request, deadline, Limits::default())
}

/// Build an export; on a read-only connection a read still running at
/// `deadline` is interrupted, and any build that saw the interrupt is `Busy`,
/// never a smaller export. A build that would pass `limits` is `TooLarge`.
pub(crate) fn create_with_limits(
    db: &Connection,
    request: &Request,
    deadline: std::time::Duration,
    limits: Limits,
) -> Result<Export, Refusal> {
    // Fails closed: a read-only connection with no working watchdog never builds.
    let guard = ReadDeadline::arm(db, deadline)?;
    let clock = Clock {
        started: Instant::now(),
        deadline,
        guard: guard.as_ref(),
    };
    let built = build(db, request, &clock, limits);
    // An interrupt can hide inside a swallowed error (an access check that failed
    // because its read was cut off). The flag is set before the interrupt, so
    // whatever was built from such a read is refused.
    if guard.as_ref().is_some_and(ReadDeadline::fired) {
        return Err(Refusal::Busy);
    }
    built
}

fn build(
    db: &Connection,
    request: &Request,
    clock: &Clock,
    limits: Limits,
) -> Result<Export, Refusal> {
    if crate::people::enabled(db) {
        return Err(Refusal::Unavailable);
    }
    // One read transaction: every read below sees the same state.
    let tx = db.unchecked_transaction()?;
    let db: &Connection = &tx;
    let state = state(db)?;
    if !state.enabled {
        return Err(Refusal::Disabled);
    }
    if !trigger_set_intact(db)? {
        return Err(Refusal::Unavailable);
    }
    if crate::community::active_user(db, request.recipient).is_err()
        || !crate::hosting::holds_replica(db, request.recipient)
    {
        return Err(Refusal::NotAReplica);
    }
    let head = head(db)?;
    if let Some(after) = request.after {
        if after < state.floor {
            return Err(Refusal::Stale);
        }
        if after > head {
            return Err(Refusal::Diverged);
        }
    }
    // Everything the build keeps is charged here, from the first id on.
    let mut budget = Budget::new(limits);
    let eligible = eligible_within(db, request.recipient, &mut budget)?;
    let ctx = Ctx {
        db,
        recipient: request.recipient,
        eligible: &eligible,
    };
    clock.check()?;

    let mut tables = BTreeMap::new();
    let mut withheld: BTreeMap<String, usize> = BTreeMap::new();
    match request.after {
        None => {
            for table in logged_tables() {
                let data = snapshot_table(&ctx, table, clock, &mut budget, &mut withheld)?;
                tables.insert(table.name.to_string(), data);
                clock.check()?;
            }
        }
        Some(after) => {
            // Keys changed in (after, head], plus rows whose visibility changed.
            let mut affected = Affected::default();
            {
                let mut statement = db.prepare(
                    "SELECT tbl,pk FROM replica_log WHERE seq>?1 AND seq<=?2 GROUP BY tbl,pk",
                )?;
                let mut rows = statement.query(params![after, head])?;
                let mut seen = 0usize;
                while let Some(row) = rows.next()? {
                    let (name, key): (String, String) = (row.get(0)?, row.get(1)?);
                    let table = table_for(&name)
                        .filter(|t| t.class.logged())
                        .ok_or(Refusal::Unavailable)?;
                    affected.insert(table.name, parse_key(table, &key)?, &mut budget)?;
                    seen += 1;
                    if seen % CLOCK_EVERY == 0 {
                        clock.check()?;
                    }
                }
            }
            // Channels gained or lost since the replica's last pull: every row of
            // theirs is re-read, so the key set is bounded like any other.
            let mut moved = BTreeSet::new();
            for id in eligible.symmetric_difference(request.previous_eligible) {
                budget.charge(id.len() + ENTRY_OVERHEAD)?;
                moved.insert(id.clone());
            }
            if !moved.is_empty() {
                for table in logged_tables() {
                    keys_in_channels(db, table, &moved, &mut affected, &mut budget, clock)?;
                }
            }
            expand_dependencies(db, &mut affected, &mut budget, clock)?;
            clock.check()?;
            for table in logged_tables() {
                let mut data = empty_table(table);
                for key in affected.of(table.name) {
                    match fetch(&ctx, table, key, budget.left())? {
                        Some(mut cells) => match ctx.shape(table, &mut cells)? {
                            Shown::Full => push_row(&mut data, cells, &mut budget)?,
                            Shown::Redacted => {
                                *withheld.entry(table.name.into()).or_default() += 1;
                                push_row(&mut data, cells, &mut budget)?;
                            }
                            Shown::Omit => {
                                *withheld.entry(table.name.into()).or_default() += 1;
                                push_tombstone(&mut data, key.clone(), &mut budget)?;
                            }
                        },
                        None => push_tombstone(&mut data, key.clone(), &mut budget)?,
                    }
                }
                tables.insert(table.name.to_string(), data);
                clock.check()?;
            }
        }
    }

    let mut digests = BTreeMap::new();
    for (name, data) in &tables {
        digests.insert(name.clone(), digest(data));
        clock.check()?;
    }
    // The manifest repeats the eligible list: charge the copy before it is made.
    budget.charge(
        eligible
            .iter()
            .map(|id| id.len() + ENTRY_OVERHEAD)
            .sum::<usize>(),
    )?;
    let eligible_list: Vec<String> = eligible.iter().cloned().collect();
    let files = files_of(db, &tables, &mut budget)?;
    let manifest = Manifest {
        replica_format: REPLICA_FORMAT,
        source_server_id: crate::community::server_id(db)?,
        lineage: state.lineage,
        term: state.term,
        schema_version: crate::community::SCHEMA_VERSION,
        export_id: id(),
        after: request.after,
        head,
        permission_generation: state.permission_generation,
        eligible_set_sha256: eligible_digest(&eligible_list),
        eligible: eligible_list,
        created_ms: request.now_ms,
        recipient_user_id: request.recipient.into(),
        replica_id: request.replica_id.into(),
        tables: digests,
        files,
        withheld,
        promotion_eligible: false,
        parts: Vec::new(),
    };
    drop(tx);
    Ok(Export { manifest, tables })
}

/// One part of an export, borrowed from it. Staging serializes these one at a
/// time, so cutting an export into parts never copies its rows.
#[derive(Serialize)]
pub(crate) struct PartView<'a> {
    pub table: &'a str,
    pub columns: &'a [String],
    pub rows: &'a [Vec<Value>],
    pub tombstones: &'a [Vec<Value>],
}

/// Cut an export into parts of roughly `limit` bytes each (a row is never split).
pub(crate) fn parts(export: &Export, limit: usize) -> Vec<PartView<'_>> {
    let mut out = Vec::new();
    for (name, data) in &export.tables {
        // The part being filled is `rows[row_from..row_to]` plus `tombstones[tomb_from..tomb_to]`.
        let (mut row_from, mut row_to, mut tomb_from, mut tomb_to) = (0, 0, 0, 0);
        let mut size = 0usize;
        let mut flush = |row_from: &mut usize,
                         row_to: usize,
                         tomb_from: &mut usize,
                         tomb_to: usize,
                         size: &mut usize| {
            if row_to > *row_from || tomb_to > *tomb_from {
                out.push(PartView {
                    table: name,
                    columns: &data.columns,
                    rows: &data.rows[*row_from..row_to],
                    tombstones: &data.tombstones[*tomb_from..tomb_to],
                });
                *row_from = row_to;
                *tomb_from = tomb_to;
                *size = 0;
            }
        };
        for (i, row) in data.rows.iter().enumerate() {
            let n = json_len(row) + 1;
            if size + n > limit {
                flush(&mut row_from, row_to, &mut tomb_from, tomb_to, &mut size);
            }
            row_to = i + 1;
            size += n;
        }
        for (i, key) in data.tombstones.iter().enumerate() {
            let n = json_len(key) + 1;
            if size + n > limit {
                flush(&mut row_from, row_to, &mut tomb_from, tomb_to, &mut size);
            }
            tomb_to = i + 1;
            size += n;
        }
        flush(&mut row_from, row_to, &mut tomb_from, tomb_to, &mut size);
    }
    out
}

/// Check one fetched part against the manifest and decode it.
pub(crate) fn parse_part(manifest: &Manifest, index: usize, bytes: &[u8]) -> Result<PartData, ()> {
    let wanted = manifest.parts.get(index).ok_or(())?;
    if wanted.index != index
        || wanted.bytes != bytes.len()
        || format!("{:x}", Sha256::digest(bytes)) != wanted.sha256
    {
        return Err(());
    }
    serde_json::from_slice(bytes).map_err(|_| ())
}

/// Put the parts back together. Tables with no rows and no tombstones have no
/// part and come back empty. The replica store still checks every digest.
pub(crate) fn assemble(manifest: Manifest, parts: Vec<PartData>) -> Result<Export, ()> {
    let mut tables: BTreeMap<String, TableData> = logged_tables()
        .map(|table| {
            (
                table.name.to_string(),
                TableData {
                    columns: cells_of(table).iter().map(|c| c.to_string()).collect(),
                    rows: Vec::new(),
                    tombstones: Vec::new(),
                },
            )
        })
        .collect();
    for part in parts {
        let table = tables.get_mut(&part.table).ok_or(())?;
        if table.columns != part.columns {
            return Err(());
        }
        table.rows.extend(part.rows);
        table.tombstones.extend(part.tombstones);
    }
    Ok(Export { manifest, tables })
}

// ---------------------------------------------------------------------------
// File objects

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FileRefusal {
    Forbidden,
    NotFound,
    /// The row now has another version or size; it arrives in the next delta.
    Changed,
    Database,
}

impl From<rusqlite::Error> for FileRefusal {
    fn from(_: rusqlite::Error) -> Self {
        Self::Database
    }
}

#[derive(Debug)]
pub(crate) struct FileBytes {
    pub sha256: String,
    pub bytes: Vec<u8>,
}

/// `(version, size, bytes, owning message)` of a stored file.
type FileRow = (i64, Option<i64>, Option<Vec<u8>>, Option<String>);

/// One file object, after the same checks as an export: role, access to the
/// owning message, and that the version and size still match the manifest.
pub(crate) fn read_file(
    db: &Connection,
    recipient: &str,
    file: &FileRef,
) -> Result<FileBytes, FileRefusal> {
    // The same bound as an export: a read still running at the deadline is cut off
    // on this (read-only) connection; a read-write connection is never armed, and a
    // read-only one whose watchdog cannot start does not read at all.
    let _deadline = ReadDeadline::arm(db, std::time::Duration::from_secs(MAX_BUILD_SECS))
        .map_err(|_| FileRefusal::Database)?;
    let tx = db.unchecked_transaction()?;
    let db: &Connection = &tx;
    if crate::people::enabled(db)
        || !state(db)?.enabled
        || crate::community::active_user(db, recipient).is_err()
        || !crate::hosting::holds_replica(db, recipient)
    {
        return Err(FileRefusal::Forbidden);
    }
    let sql = match file.table.as_str() {
        // Uploads are written once, so their version is always 0.
        "uploads" => "SELECT 0,size,data,message_id FROM uploads WHERE id=?1",
        "user_profiles" => {
            "SELECT version,length(avatar),avatar,NULL FROM user_profiles WHERE user_id=?1"
        }
        "presentation_assets" => {
            "SELECT version,length(data),data,NULL FROM presentation_assets WHERE id=?1"
        }
        _ => return Err(FileRefusal::NotFound),
    };
    let row: Option<FileRow> = db
        .query_row(sql, [&file.id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })
        .optional()?;
    let Some((version, size, bytes, message)) = row else {
        return Err(FileRefusal::NotFound);
    };
    let (Some(size), Some(bytes)) = (size, bytes) else {
        return Err(FileRefusal::NotFound);
    };
    if file.table == "uploads" {
        let eligible = eligible_channels(db, recipient).map_err(|_| FileRefusal::Database)?;
        let ctx = Ctx {
            db,
            recipient,
            eligible: &eligible,
        };
        let readable = match message {
            Some(id) => {
                ctx.message_state(&id).map_err(|_| FileRefusal::Database)? == Some(Shown::Full)
            }
            None => false,
        };
        if !readable {
            return Err(FileRefusal::Forbidden);
        }
    }
    if version != file.version || size != file.size || bytes.len() as i64 != size {
        return Err(FileRefusal::Changed);
    }
    Ok(FileBytes {
        sha256: format!("{:x}", Sha256::digest(&bytes)),
        bytes,
    })
}

#[cfg(test)]
mod budget_tests {
    use super::*;

    #[test]
    fn scalar_rows_charge_their_storage_even_when_their_json_is_small() {
        let cells = vec![Value::Null; 128];
        let allowance = json_len(&cells) * 2;
        assert!(row_storage(cells.capacity(), 0) > allowance);
        let mut budget = Budget::new(Limits {
            max_bytes: allowance,
            max_keys: MAX_DELTA_KEYS,
        });
        let mut data = TableData {
            columns: Vec::new(),
            rows: Vec::new(),
            tombstones: Vec::new(),
        };
        assert_eq!(
            push_row(&mut data, cells, &mut budget),
            Err(Refusal::TooLarge)
        );
        assert!(data.rows.is_empty());
    }
}
