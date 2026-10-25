//! M4 slice S2a: the real durable `LogStore` for the takeover log, in a small dedicated
//! SQLite file OUTSIDE the community database. It is private local COMPLETE-state storage for
//! the host's own voter, never an M2 selective copy, and nothing in this module is wired to the
//! running server: no hook, route, capability, `Abilities` change, network or promotion.
//!
//! Atomicity. `LogStore::commit(entry, state)` is exactly one SQLite transaction (`BEGIN
//! IMMEDIATE`): the full entry record (when given) and the single metadata row are written, or
//! neither. Journal mode is the simple rollback journal (`DELETE`; no WAL, so no unbounded side
//! file), `synchronous=FULL`, `locking_mode=EXCLUSIVE` on one owned connection, bounded busy
//! timeout. Acknowledged records are never updated or deleted: the only statements are
//! `INSERT` for a new index and `UPSERT` of the one metadata row. Compaction would be a future,
//! explicit, verified-snapshot operation, not eviction.
//!
//! Outcomes. `Full` is returned only from this module's own quota pre-checks, before any write.
//! A different record at a retained index is `Conflict` (nothing written). Every SQLite error,
//! including a disk-full error, an uncertain COMMIT or a failed rollback, is `Failed`, and the
//! store then refuses all further work until it is reopened, so the voter poisons and no
//! authority is returned. Out-of-contract inputs (index 0 or > i64::MAX, empty or oversized
//! entry, empty or oversized state) are `Failed` with nothing written and do not break the store.
//!
//! Durability assumptions (not measured here): SQLite's documented crash-safety with
//! `synchronous=FULL` on a rollback journal, an operating system and disk that honor `fsync`
//! (Windows `FlushFileBuffers`), no write-cache lies, and a directory fsync after creation on
//! Unix. No power-loss test was run.
//!
//! Ownership. SQLite's `locking_mode=EXCLUSIVE` alone only retains the lock the first access
//! happens to take (a SHARED one for a read), so it is not ownership. `open_existing` therefore
//! runs an empty `BEGIN EXCLUSIVE; COMMIT` immediately after the connection-local settings and
//! BEFORE anything is read or verified: in exclusive locking mode the EXCLUSIVE lock it takes is
//! then retained for the life of the connection. A second opener gets `Busy` (after its busy
//! timeout) and never returns a store, so no two voters can hold loaded metadata for one file.
//! `create_new` takes the same lock with its first transaction.
//!
//! No conversion. The persistent journal mode is chosen only by `create_new`. `open_existing`
//! first reads the 100-byte SQLite header with plain file I/O and refuses a foreign identity,
//! another application id, version or page size, or a non-rollback journal mode (for example a
//! cleanly closed WAL-mode file) with `Mismatch`/`Corrupt` BEFORE SQLite touches the file, so a
//! refused file is byte-for-byte unchanged and nothing is converted. Connection-local controls
//! (`synchronous=FULL`, `locking_mode=EXCLUSIVE`, `trusted_schema=OFF`, `cell_size_check`) are
//! set on every open. SQLite's own recovery of a hot rollback journal during the exclusive
//! acquisition is its documented atomic crash recovery, not an application repair.
//!
//! Safety of the files. The store lives in its own folder prepared with the existing
//! `handover_target::private_directory` (redirect refusal, private ACL on Windows, 0700 on
//! Unix; on Windows it also rejects reparse points and hard-linked files inside the folder).
//! The database file is created exclusively with owner-only permissions on Unix. `open_existing`
//! never creates, repairs or resets anything: it refuses a missing folder or file, a symlink,
//! non-regular or (Unix) hard-linked or group/world-accessible database file or rollback
//! journal, an unexpected WAL or shared-memory file, a foreign or altered schema, a failed quick check and any row outside
//! the bounds. SQLite's own hot-journal rollback after a crash is not a repair of this store.
//!
//! No payload, state byte, path or SQL parameter appears in `Debug` or in any error.
#![allow(dead_code)]

use crate::takeover_voter::{LogStore, StoreError};
use relay_protocol::takeover::MAX_ENTRY_JSON;
use rusqlite::{Connection, ErrorCode, OpenFlags, OptionalExtension, TransactionBehavior};
use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

/// The store's only file name inside its dedicated folder.
pub(crate) const FILE_NAME: &str = "takeover-log.sqlite3";
/// "RLOG": identifies the file as this store.
const APPLICATION_ID: i64 = 0x524C_4F47;
const SCHEMA_VERSION: i64 = 1;
const PAGE_SIZE: u64 = 4096;
/// The voter metadata value limit (`takeover_voter::MAX_STATE_BYTES`).
const MAX_STATE: usize = 256 * 1024;
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
/// Fixed room, beyond the logical quota and a proportional page overhead, for B-tree pages,
/// the metadata row and bookkeeping. SQLite itself refuses to grow the file past this.
const ENVELOPE_BYTES: u64 = 8 * 1024 * 1024;
/// Upper bound for any configured quota, keeping page arithmetic far from overflow.
const MAX_QUOTA_BYTES: u64 = 64 << 30;

/// Logical retention limits: the number of retained entries and the sum of their exact record
/// sizes. The default is fixed; tests may inject smaller values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Quota {
    pub(crate) max_entries: u64,
    pub(crate) max_entry_bytes: u64,
}
impl Quota {
    /// 1 GiB of retained records and one million entries.
    pub(crate) const DEFAULT: Quota = Quota {
        max_entries: 1_000_000,
        max_entry_bytes: 1 << 30,
    };
    fn valid(&self) -> bool {
        self.max_entries > 0
            && self.max_entries <= i64::MAX as u64
            && self.max_entry_bytes >= 1
            && self.max_entry_bytes <= MAX_QUOTA_BYTES
    }
    /// The most pages the file may ever have: records, a one-eighth page overhead, the metadata
    /// row and the fixed envelope.
    fn page_cap(&self) -> u64 {
        (self.max_entry_bytes + self.max_entry_bytes / 8 + MAX_STATE as u64 + ENVELOPE_BYTES)
            .div_ceil(PAGE_SIZE)
    }
}

/// Why opening or creating the store was refused. It carries no path, payload or SQL text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OpenError {
    /// `create_new` found the database file already there; nothing was touched.
    AlreadyExists,
    /// `open_existing` found no folder or no database file; nothing was created.
    Missing,
    /// A redirected, non-regular, linked, wrongly permissioned or otherwise unsafe path.
    Unsafe,
    /// Not a readable database, a failed quick check or rows outside their bounds.
    Corrupt,
    /// A database that is not this store, or not this version or configuration.
    Mismatch,
    /// Another owner holds the file.
    Busy,
    /// A configured quota that is out of range.
    Invalid,
    /// Any other I/O or SQLite failure.
    Failed,
}
impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            OpenError::AlreadyExists => "takeover log already exists",
            OpenError::Missing => "takeover log is missing",
            OpenError::Unsafe => "takeover log storage is unsafe",
            OpenError::Corrupt => "takeover log is corrupt",
            OpenError::Mismatch => "takeover log does not match this version",
            OpenError::Busy => "takeover log is in use",
            OpenError::Invalid => "takeover log quota is invalid",
            OpenError::Failed => "takeover log storage failed",
        })
    }
}
impl std::error::Error for OpenError {}

fn classify(error: rusqlite::Error) -> OpenError {
    match error {
        rusqlite::Error::SqliteFailure(code, _) => match code.code {
            ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked => OpenError::Busy,
            ErrorCode::NotADatabase | ErrorCode::DatabaseCorrupt => OpenError::Corrupt,
            ErrorCode::CannotOpen => OpenError::Missing,
            _ => OpenError::Failed,
        },
        _ => OpenError::Failed,
    }
}

fn entries_sql() -> String {
    format!(
        "CREATE TABLE entries (idx INTEGER PRIMARY KEY CHECK (idx > 0), \
         body BLOB NOT NULL CHECK (length(body) BETWEEN 1 AND {MAX_ENTRY_JSON})) STRICT"
    )
}
fn meta_sql() -> String {
    format!(
        "CREATE TABLE meta (id INTEGER PRIMARY KEY CHECK (id = 1), \
         state BLOB NOT NULL CHECK (length(state) BETWEEN 1 AND {MAX_STATE})) STRICT"
    )
}

pub(crate) struct TakeoverStore {
    conn: Connection,
    quota: Quota,
    /// Logical usage, read from the file at open and advanced only by a successful commit.
    entries: u64,
    bytes: u64,
    /// Set by any uncertain outcome; everything then fails until the store is reopened.
    broken: bool,
    #[cfg(test)]
    abort_before_commit: bool,
}
impl std::fmt::Debug for TakeoverStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TakeoverStore")
            .field("entries", &self.entries)
            .field("bytes", &self.bytes)
            .field("broken", &self.broken)
            .finish_non_exhaustive()
    }
}

/// Unix: a single link and no group/world access. Elsewhere the private-directory validation
/// (reparse points, hard links, private ACL) is what applies.
#[cfg(unix)]
fn private_file(info: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    info.nlink() == 1 && info.mode() & 0o077 == 0
}
#[cfg(not(unix))]
fn private_file(_info: &fs::Metadata) -> bool {
    true
}
/// Makes the new file's directory entry durable (Unix); a no-op where directories cannot be
/// opened for sync.
#[cfg(unix)]
fn sync_directory(dir: &Path) -> bool {
    fs::File::open(dir).and_then(|d| d.sync_all()).is_ok()
}
#[cfg(not(unix))]
fn sync_directory(_dir: &Path) -> bool {
    true
}

fn sidecar(file: &Path, suffix: &str) -> PathBuf {
    let mut name = file.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

impl TakeoverStore {
    /// The database file inside `dir`.
    pub(crate) fn file(dir: &Path) -> PathBuf {
        dir.join(FILE_NAME)
    }
    /// Logical usage: (retained entries, retained record bytes).
    pub(crate) fn usage(&self) -> (u64, u64) {
        (self.entries, self.bytes)
    }
    pub(crate) fn quota(&self) -> Quota {
        self.quota
    }

    /// Creates a brand-new store in the dedicated folder `dir` (created privately if needed).
    /// An existing database file is never touched: `AlreadyExists`.
    pub(crate) fn create_new(dir: &Path, quota: Quota) -> Result<Self, OpenError> {
        if !quota.valid() {
            return Err(OpenError::Invalid);
        }
        crate::handover_target::private_directory(dir).map_err(|_| OpenError::Unsafe)?;
        let file = Self::file(dir);
        for extra in ["-journal", "-wal", "-shm"] {
            if fs::symlink_metadata(sidecar(&file, extra)).is_ok() {
                return Err(OpenError::Unsafe);
            }
        }
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&file) {
            Ok(created) => {
                if created.sync_all().is_err() {
                    drop(created);
                    let _ = fs::remove_file(&file);
                    return Err(OpenError::Failed);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(OpenError::AlreadyExists)
            }
            Err(_) => return Err(OpenError::Failed),
        }
        if !sync_directory(dir) {
            let _ = fs::remove_file(&file);
            return Err(OpenError::Failed);
        }
        // Only this call's own, still-empty file is removed on failure; nothing was acknowledged.
        let built = Self::initialize(&file, quota);
        if built.is_err() {
            let _ = fs::remove_file(sidecar(&file, "-journal"));
            let _ = fs::remove_file(&file);
        }
        built
    }

    fn initialize(file: &Path, quota: Quota) -> Result<Self, OpenError> {
        let conn = Connection::open_with_flags(
            file,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(classify)?;
        // The page size and the persistent journal mode can only be chosen while the database
        // has no content, i.e. here and nowhere else.
        conn.pragma_update(None, "page_size", PAGE_SIZE as i64)
            .map_err(classify)?;
        connection_controls(&conn, BUSY_TIMEOUT)?;
        conn.pragma_update(None, "journal_mode", "DELETE")
            .map_err(classify)?;
        // The first transaction takes the EXCLUSIVE lock, which exclusive mode then retains.
        conn.execute_batch(&format!(
            "BEGIN EXCLUSIVE; \
             PRAGMA application_id = {APPLICATION_ID}; \
             PRAGMA user_version = {SCHEMA_VERSION}; \
             {}; {}; \
             COMMIT;",
            meta_sql(),
            entries_sql()
        ))
        .map_err(classify)?;
        limits_and_checks(&conn, &quota)?;
        let (entries, bytes) = verify(&conn)?;
        Ok(Self {
            conn,
            quota,
            entries,
            bytes,
            broken: false,
            #[cfg(test)]
            abort_before_commit: false,
        })
    }

    /// Opens a store that already exists. Never creates, repairs or resets: a missing folder or
    /// file is `Missing`, an unsafe path `Unsafe`, a damaged or foreign file `Corrupt` or
    /// `Mismatch`, with the files left exactly as found.
    pub(crate) fn open_existing(dir: &Path, quota: Quota) -> Result<Self, OpenError> {
        Self::open_with_timeout(dir, quota, BUSY_TIMEOUT)
    }

    /// `open_existing` with an explicit busy timeout (a shorter one lets tests prove `Busy`).
    fn open_with_timeout(dir: &Path, quota: Quota, busy: Duration) -> Result<Self, OpenError> {
        if !quota.valid() {
            return Err(OpenError::Invalid);
        }
        match fs::symlink_metadata(dir) {
            Ok(info) if info.file_type().is_dir() => {}
            Ok(_) => return Err(OpenError::Unsafe),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(OpenError::Missing)
            }
            Err(_) => return Err(OpenError::Unsafe),
        }
        let file = Self::file(dir);
        match fs::symlink_metadata(&file) {
            Ok(info) if info.file_type().is_file() => {
                if !private_file(&info) {
                    return Err(OpenError::Unsafe);
                }
            }
            Ok(_) => return Err(OpenError::Unsafe),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(OpenError::Missing)
            }
            Err(_) => return Err(OpenError::Unsafe),
        }
        // A rollback journal may be a left-over hot journal (SQLite recovers it); anything else
        // beside the database, and any WAL/shared-memory file, is refused.
        match fs::symlink_metadata(sidecar(&file, "-journal")) {
            Ok(info) if info.file_type().is_file() && private_file(&info) => {}
            Ok(_) => return Err(OpenError::Unsafe),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(OpenError::Unsafe),
        }
        for extra in ["-wal", "-shm"] {
            if fs::symlink_metadata(sidecar(&file, extra)).is_ok() {
                return Err(OpenError::Unsafe);
            }
        }
        // The folder already exists here, so this only validates it (and, as for the existing
        // host-key folders, re-applies its private permissions); it cannot create anything.
        crate::handover_target::private_directory(dir).map_err(|_| OpenError::Unsafe)?;
        // Identity and journal mode come from the file header with plain reads: a foreign or
        // WAL-mode database is refused before SQLite opens it, so it cannot be converted.
        check_header(&file)?;
        // No CREATE flag and no URI: a vanished file is an error, never a new database.
        let conn = Connection::open_with_flags(
            &file,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(classify)?;
        connection_controls(&conn, busy)?;
        // Real ownership first: nothing is read, verified or returned before this succeeds.
        take_ownership(&conn)?;
        limits_and_checks(&conn, &quota)?;
        let (entries, bytes) = verify(&conn)?;
        Ok(Self {
            conn,
            quota,
            entries,
            bytes,
            broken: false,
            #[cfg(test)]
            abort_before_commit: false,
        })
    }

    fn read_state(&self) -> Result<Option<Vec<u8>>, StoreError> {
        let length: Option<i64> = self
            .conn
            .query_row("SELECT length(state) FROM meta WHERE id = 1", [], |r| {
                r.get(0)
            })
            .optional()
            .map_err(|_| StoreError::Failed)?;
        let Some(length) = length else {
            return Ok(None);
        };
        // The bound is checked on the length alone, before any body is fetched or allocated.
        if length < 1 || length > MAX_STATE as i64 {
            return Err(StoreError::Failed);
        }
        let state = self
            .conn
            .query_row("SELECT state FROM meta WHERE id = 1", [], |r| {
                match r.get_ref(0)?.as_blob() {
                    Ok(blob) => Ok(blob.to_vec()),
                    Err(_) => Err(rusqlite::Error::InvalidQuery),
                }
            })
            .map_err(|_| StoreError::Failed)?;
        if state.len() as i64 != length {
            return Err(StoreError::Failed);
        }
        Ok(Some(state))
    }

    fn read_entry(&self, index: u64) -> Result<Option<Vec<u8>>, StoreError> {
        // An index SQLite cannot hold has no record.
        let Some(row) = i64::try_from(index).ok().filter(|row| *row > 0) else {
            return Ok(None);
        };
        let length: Option<i64> = self
            .conn
            .query_row(
                "SELECT length(body) FROM entries WHERE idx = ?1",
                [row],
                |r| r.get(0),
            )
            .optional()
            .map_err(|_| StoreError::Failed)?;
        let Some(length) = length else {
            return Ok(None);
        };
        if length < 1 || length > MAX_ENTRY_JSON as i64 {
            return Err(StoreError::Failed);
        }
        let body = self
            .conn
            .query_row(
                "SELECT body FROM entries WHERE idx = ?1",
                [row],
                |r| match r.get_ref(0)?.as_blob() {
                    Ok(blob) => Ok(blob.to_vec()),
                    Err(_) => Err(rusqlite::Error::InvalidQuery),
                },
            )
            .map_err(|_| StoreError::Failed)?;
        if body.len() as i64 != length {
            return Err(StoreError::Failed);
        }
        Ok(Some(body))
    }
}

/// Connection-local controls only; none of them reads or changes the database file.
fn connection_controls(conn: &Connection, busy: Duration) -> Result<(), OpenError> {
    conn.busy_timeout(busy).map_err(classify)?;
    conn.pragma_update(None, "locking_mode", "EXCLUSIVE")
        .map_err(classify)?;
    conn.pragma_update(None, "synchronous", "FULL")
        .map_err(classify)?;
    conn.pragma_update(None, "trusted_schema", "OFF")
        .map_err(classify)?;
    conn.pragma_update(None, "cell_size_check", "ON")
        .map_err(classify)?;
    Ok(())
}

/// An actual exclusive reservation, not a mode name: an empty exclusive transaction takes the
/// EXCLUSIVE lock, and because the connection is in exclusive locking mode the lock is kept
/// after the empty COMMIT for as long as the connection lives. `Busy` (after the busy timeout)
/// when anyone else holds the file; no content is changed.
fn take_ownership(conn: &Connection) -> Result<(), OpenError> {
    conn.execute_batch("BEGIN EXCLUSIVE; COMMIT;")
        .map_err(classify)
}

/// The growth limit, then a check of the values that were set.
fn limits_and_checks(conn: &Connection, quota: &Quota) -> Result<(), OpenError> {
    conn.pragma_update(None, "max_page_count", quota.page_cap() as i64)
        .map_err(classify)?;
    let synchronous: i64 = conn
        .query_row("PRAGMA synchronous", [], |r| r.get(0))
        .map_err(classify)?;
    let locking: String = conn
        .query_row("PRAGMA locking_mode", [], |r| r.get(0))
        .map_err(classify)?;
    if synchronous != 2 || !locking.eq_ignore_ascii_case("exclusive") {
        return Err(OpenError::Mismatch);
    }
    Ok(())
}

/// The first 100 bytes of the file, checked with plain reads and no SQLite involvement:
/// the SQLite magic, the rollback-journal format bytes (1/1; 2 means WAL), the page size, and
/// this store's application id and schema version. Those bytes never change after creation,
/// so this is also valid while a hot journal is waiting to be recovered.
fn check_header(file: &Path) -> Result<(), OpenError> {
    use std::io::Read;
    let mut handle = fs::File::open(file).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            OpenError::Missing
        } else {
            OpenError::Unsafe
        }
    })?;
    let info = handle.metadata().map_err(|_| OpenError::Unsafe)?;
    if !info.is_file() || !private_file(&info) {
        return Err(OpenError::Unsafe);
    }
    let mut header = [0u8; 100];
    handle
        .read_exact(&mut header)
        .map_err(|_| OpenError::Corrupt)?;
    header_identity(&header)
}
fn header_identity(header: &[u8; 100]) -> Result<(), OpenError> {
    if &header[..16] != b"SQLite format 3\0" {
        return Err(OpenError::Corrupt);
    }
    let page = u32::from(u16::from_be_bytes([header[16], header[17]]));
    let user_version = u32::from_be_bytes([header[60], header[61], header[62], header[63]]);
    let application = u32::from_be_bytes([header[68], header[69], header[70], header[71]]);
    if header[18] != 1
        || header[19] != 1
        || page != PAGE_SIZE as u32
        || u64::from(user_version) != SCHEMA_VERSION as u64
        || u64::from(application) != APPLICATION_ID as u64
    {
        return Err(OpenError::Mismatch);
    }
    Ok(())
}

/// Identity, schema, integrity and bounds; returns the logical usage (entries, bytes).
fn verify(conn: &Connection) -> Result<(u64, u64), OpenError> {
    let number = |sql: &str| -> Result<i64, OpenError> {
        conn.query_row(sql, [], |r| r.get(0)).map_err(classify)
    };
    let mode: String = conn
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .map_err(classify)?;
    if !mode.eq_ignore_ascii_case("delete") {
        return Err(OpenError::Mismatch);
    }
    if number("PRAGMA application_id")? != APPLICATION_ID
        || number("PRAGMA user_version")? != SCHEMA_VERSION
        || number("PRAGMA page_size")? != PAGE_SIZE as i64
    {
        return Err(OpenError::Mismatch);
    }
    // Exactly the two expected tables and nothing else (no view, trigger or index).
    let mut objects = Vec::new();
    {
        let mut statement = conn
            .prepare("SELECT type, name FROM sqlite_master ORDER BY name")
            .map_err(classify)?;
        let rows = statement
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .map_err(classify)?;
        for row in rows {
            objects.push(row.map_err(classify)?);
            if objects.len() > 4 {
                return Err(OpenError::Mismatch);
            }
        }
    }
    if objects
        != [
            ("table".to_owned(), "entries".to_owned()),
            ("table".to_owned(), "meta".to_owned()),
        ]
    {
        return Err(OpenError::Mismatch);
    }
    for (table, expected) in [
        (
            "entries",
            [("idx", "INTEGER", 0i64, 1i64), ("body", "BLOB", 1, 0)],
        ),
        (
            "meta",
            [("id", "INTEGER", 0i64, 1i64), ("state", "BLOB", 1, 0)],
        ),
    ] {
        let mut statement = conn
            .prepare(&format!("PRAGMA table_info({table})"))
            .map_err(classify)?;
        let columns = statement
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, i64>(5)?,
                ))
            })
            .map_err(classify)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(classify)?;
        let wanted: Vec<_> = expected
            .iter()
            .map(|(n, t, nn, pk)| ((*n).to_owned(), (*t).to_owned(), *nn, *pk))
            .collect();
        if columns != wanted {
            return Err(OpenError::Mismatch);
        }
    }
    let check: String = conn
        .query_row("PRAGMA quick_check(1)", [], |r| r.get(0))
        .map_err(classify)?;
    if check != "ok" {
        return Err(OpenError::Corrupt);
    }
    // Bounds on every row, from lengths alone (no body is read or allocated here).
    let (count, total, smallest, largest, wrong_type): (i64, i64, i64, i64, i64) = conn
        .query_row(
            "SELECT count(*), COALESCE(sum(length(body)), 0), COALESCE(min(length(body)), 1), \
             COALESCE(max(length(body)), 1), COALESCE(sum(typeof(body) != 'blob' OR idx < 1), 0) \
             FROM entries",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .map_err(classify)?;
    if count < 0 || total < 0 || smallest < 1 || largest > MAX_ENTRY_JSON as i64 || wrong_type != 0
    {
        return Err(OpenError::Corrupt);
    }
    let (rows, widest, thinnest, bad_state): (i64, i64, i64, i64) = conn
        .query_row(
            "SELECT count(*), COALESCE(max(length(state)), 1), COALESCE(min(length(state)), 1), \
             COALESCE(sum(typeof(state) != 'blob' OR id != 1), 0) FROM meta",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .map_err(classify)?;
    if rows > 1 || widest > MAX_STATE as i64 || thinnest < 1 || bad_state != 0 {
        return Err(OpenError::Corrupt);
    }
    Ok((count as u64, total as u64))
}

enum Outcome {
    Done { inserted: Option<u64> },
    Full,
    Conflict,
    Failed,
}

/// One transaction. Dropping an unfinished `Transaction` rolls it back.
fn transact(
    conn: &mut Connection,
    quota: Quota,
    used: (u64, u64),
    record: Option<(i64, &[u8])>,
    state: &[u8],
    abort: bool,
) -> Outcome {
    let Ok(tx) = conn.transaction_with_behavior(TransactionBehavior::Immediate) else {
        return Outcome::Failed;
    };
    // A definitive refusal before any write: the rollback must also succeed to stay definitive.
    let refuse = |tx: rusqlite::Transaction<'_>, outcome: Outcome| match tx.rollback() {
        Ok(()) => outcome,
        Err(_) => Outcome::Failed,
    };
    let mut inserted = None;
    if let Some((row, bytes)) = record {
        let length: Result<Option<i64>, _> = tx
            .query_row(
                "SELECT length(body) FROM entries WHERE idx = ?1",
                [row],
                |r| r.get(0),
            )
            .optional();
        match length {
            Err(_) => return Outcome::Failed,
            Ok(Some(length)) => {
                // Same index: only the exact bytes are idempotent; anything else is a conflict
                // that writes nothing (the metadata is not touched either). A read that fails
                // is a failure, never a conflict.
                let same: Result<bool, rusqlite::Error> = if length == bytes.len() as i64 {
                    tx.query_row(
                        "SELECT body FROM entries WHERE idx = ?1",
                        [row],
                        |r| match r.get_ref(0)?.as_blob() {
                            Ok(blob) => Ok(blob == bytes),
                            Err(_) => Err(rusqlite::Error::InvalidQuery),
                        },
                    )
                } else {
                    Ok(false)
                };
                match same {
                    Ok(true) => {}
                    Ok(false) => return refuse(tx, Outcome::Conflict),
                    Err(_) => return Outcome::Failed,
                }
            }
            Ok(None) => {
                let size = bytes.len() as u64;
                if used.0 >= quota.max_entries
                    || used
                        .1
                        .checked_add(size)
                        .is_none_or(|sum| sum > quota.max_entry_bytes)
                {
                    return refuse(tx, Outcome::Full);
                }
                if tx
                    .execute(
                        "INSERT INTO entries (idx, body) VALUES (?1, ?2)",
                        rusqlite::params![row, bytes],
                    )
                    .is_err()
                {
                    return Outcome::Failed;
                }
                inserted = Some(size);
            }
        }
    }
    if tx
        .execute(
            "INSERT INTO meta (id, state) VALUES (1, ?1) \
             ON CONFLICT (id) DO UPDATE SET state = excluded.state",
            rusqlite::params![state],
        )
        .is_err()
    {
        return Outcome::Failed;
    }
    if abort {
        return Outcome::Failed;
    }
    match tx.commit() {
        Ok(()) => Outcome::Done { inserted },
        Err(_) => Outcome::Failed,
    }
}

impl LogStore for TakeoverStore {
    fn load_state(&mut self) -> Result<Option<Vec<u8>>, StoreError> {
        if self.broken {
            return Err(StoreError::Failed);
        }
        let result = self.read_state();
        if result.is_err() {
            self.broken = true;
        }
        result
    }

    fn load_entry(&mut self, index: u64) -> Result<Option<Vec<u8>>, StoreError> {
        if self.broken {
            return Err(StoreError::Failed);
        }
        let result = self.read_entry(index);
        if result.is_err() {
            self.broken = true;
        }
        result
    }

    fn commit(&mut self, entry: Option<(u64, &[u8])>, state: &[u8]) -> Result<(), StoreError> {
        if self.broken {
            return Err(StoreError::Failed);
        }
        // Contract violations fail closed before touching the file.
        if state.is_empty() || state.len() > MAX_STATE {
            return Err(StoreError::Failed);
        }
        let record = match entry {
            Some((index, bytes)) => match i64::try_from(index).ok().filter(|row| *row > 0) {
                Some(row) if !bytes.is_empty() && bytes.len() <= MAX_ENTRY_JSON => {
                    Some((row, bytes))
                }
                _ => return Err(StoreError::Failed),
            },
            None => None,
        };
        #[cfg(test)]
        let abort = self.abort_before_commit;
        #[cfg(not(test))]
        let abort = false;
        match transact(
            &mut self.conn,
            self.quota,
            (self.entries, self.bytes),
            record,
            state,
            abort,
        ) {
            Outcome::Done { inserted } => {
                if let Some(size) = inserted {
                    self.entries += 1;
                    self.bytes += size;
                }
                Ok(())
            }
            Outcome::Full => Err(StoreError::Full),
            Outcome::Conflict => Err(StoreError::Conflict),
            Outcome::Failed => {
                self.broken = true;
                Err(StoreError::Failed)
            }
        }
    }
}

#[cfg(test)]
mod tests;
