//! M4 slice S2b-1 (spike): a bounded SQLite VFS interposer that turns the WAL commit write into
//! an explicit, vetoable commit boundary. It is NOT wired into the server: nothing here
//! installs itself, changes an ordinary open, enables a capability or flips `Abilities`.
//!
//! What it does. An explicitly constructed [`WalVfs`] wraps the DEFAULT VFS for exactly one
//! database file (registered under a unique name for that owner's lifetime; only a connection
//! opened with [`WalVfs::open_connection`] uses it). Every byte SQLite writes to the `-wal`
//! file is held in memory (an overlay: reads of held bytes are served from it) and NOTHING of an
//! uncommitted tail reaches the real file. SQLite 3.50.2's `walFrames` writes a transaction's
//! frames (header, then page, spilled early or not), may rewrite earlier frames and their
//! checksums (`walRewriteChecksums`), and, with `synchronous=FULL`, ends the commit with ONE
//! `xSync` of the WAL BEFORE it appends the frames to the wal-index (`walIndexAppend`) and
//! before it publishes the new header. That `xSync` is the commit point used here: the held tail
//! is parsed and fully verified (salts, cumulative checksums, final non-zero db-size field),
//! turned into a [`WalTransaction`] and handed to the injected [`CommitSink`]. Approved: the
//! frames are written to the real file and synced, then SQLite goes on to publish the commit.
//! Vetoed, over quota, panicking or faulty: the held bytes are dropped and the `xSync` fails with
//! an I/O error, so SQLite rolls the transaction back, no commit marker ever reached the file and
//! nothing was published. A held header is never acknowledged as authority.
//!
//! Scope and unsupported cases (all fail closed, see `Fault`):
//! * the database must already exist in WAL mode (header format bytes 2/2) with an EMPTY or
//!   absent `-wal` (checkpoint with TRUNCATE and close normally first); a rollback journal
//!   (`MAIN_JOURNAL`/super-journal), another durable main database (`ATTACH`) or another WAL path
//!   is refused at `xOpen`; temp/transient/sub-journal files pass through untouched;
//! * exactly ONE admitted MAIN connection exists at a time. Only `open_connection` can create it
//!   (a thread-bound permit is consumed by the first MAIN `xOpen`; a direct open by VFS name, a
//!   second connection, `ATTACH` or `VACUUM INTO` is refused), so there is no independent reader
//!   or writer that could observe or change state between SQLite's commit `xSync` and its
//!   wal-index publication. Plain connections that bypass the wrapper (other processes, tools)
//!   are outside this contract and unsupported while the wrapper is armed;
//! * `synchronous` must stay FULL and the journal mode WAL: `open_connection` sets and verifies
//!   them and installs an authorizer that denies setting them (and `ATTACH`/`DETACH`). As a
//!   backstop, a completed commit frame that SQLite never syncs is detected on the next write and
//!   poisons the wrapper, but that one commit would already have been published, so the
//!   authorizer must not be replaced by the owner;
//! * the base VFS must offer shared memory (io version >= 2) and `mmap` of the WAL/database is
//!   disabled. Device flags are never altered: SQLite's own `padToSectorBoundary` decision uses
//!   the main file's `xDeviceCharacteristics` (sqlite3.c:67606), so the wrapper records exactly
//!   the value it handed SQLite. With `POWERSAFE_OVERWRITE` the commit is the plain sequence
//!   above. Without it (the bundled Windows VFS reports 0x8800) SQLite pads a commit: after the
//!   commit frame it repeats the commit's LAST FRAME (same page number, commit size, salts and
//!   page bytes, checksum chained on) up to the next sector boundary and syncs INSIDE the write
//!   that crosses that boundary (walFrames/walWriteToLog, sqlite3.c:69792 and 70051). The wrapper
//!   then accepts exactly this: the held tail must be the verified transaction, ending with its
//!   first commit-marked frame, followed by zero or more COMPLETE exact repeats and at most one
//!   exact PARTIAL repeat, and its absolute end must equal the sector boundary SQLite computes
//!   from the end of that commit frame. It is approved ONCE (as the real frames), everything held
//!   is written and synced, and the unwritten rest of the partial repeat is then accepted only
//!   byte for byte as the already-computed expected remainder, written through unsynced exactly
//!   as SQLite designs it. A repeat that differs in page number, commit size, salts, page bytes or
//!   checksum, a sync anywhere else, an unknown device flag value or any extra byte is refused;
//! * main-database writes are accepted only as ordinary checkpoint work: whole pages whose bytes
//!   equal a committed frame of the current WAL generation (SHA-256 per frame, bounded by
//!   `Config::max_tracked_frames`), header format bytes unchanged, truncation only to the last
//!   committed size; anything else is an I/O error and poisons the wrapper.
//!
//! Local commit receipt. Approval (before the commit) and the LOCAL COMMIT RECEIPT (after it)
//! are separate events and separate required methods of [`CommitSink`]; neither is inferred from
//! the other, from `xSync` or from a returned success. The receipt comes from `sqlite3_wal_hook`
//! (https://sqlite.org/c3ref/wal_hook.html), which SQLite 3.50.2 calls from `sqlite3Step` once a
//! statement is DONE on a connection in autocommit mode and the commit left frames behind
//! (`doWalCallbacks`, sqlite3.c:92182; `pWal->iCallback` is consumed by the call, so a SELECT or a
//! rollback never produces one), i.e. after the commit really happened and its write lock was
//! released. The wrapper keeps exactly one pending receipt, created when it approved and wrote the
//! commit and bound to that commit's capture sequence, WAL generation, final COMPLETE WAL position
//! (padding repeats and the finished padding remainder included) and database page count. The
//! hook accepts only the exact owned `sqlite3*`, the name `main`, a frame count equal to that
//! position and a clean wrapper (no tail, no padding remainder); then it takes the pending receipt
//! (so it can never be delivered twice), releases every wrapper lock and calls
//! `CommitSink::local_committed`. A failure or panic of the sink poisons the wrapper (all later
//! writes, snapshots and opens are refused) and the hook returns `SQLITE_ABORT` to the SQL caller
//! WHILE THE COMMIT HAS ALREADY HAPPENED: that result means "committed, receipt failed" and is
//! never a rollback; a veto before the commit is `SQLITE_IOERR_FSYNC`, and
//! `WalVfs::receipt_failed_after_commit` tells the two apart. An approved commit whose receipt is
//! missing (the hook was replaced or never ran, a padding remainder failed, the connection closed)
//! stays recoverable work: `unreceipted_commit` names it, the next commit is refused, and a
//! snapshot is refused.
//! The hook also REPLACES SQLite's default automatic checkpoint, which `sqlite3_wal_hook` shares
//! with `sqlite3_wal_autocheckpoint` / `PRAGMA wal_autocheckpoint` (sqlite3.c:185084, 185105).
//! Therefore the owned hook reproduces `sqlite3WalDefaultHook` after the receipt: when the WAL
//! holds at least `Config::autocheckpoint_frames` frames (default 1000, 0 disables) it runs
//! `sqlite3_wal_checkpoint` (PASSIVE). `BUSY`/`LOCKED` are benign as in SQLite; any other result is
//! a serious fault that poisons the wrapper and is returned, never hidden. A checkpoint is not a
//! commit and never produces a receipt. Setting `wal_autocheckpoint` is denied by the authorizer.
//! Out-of-band replacement of the hook or authorizer through native calls is NOT prevented; it is
//! unsupported and is caught only indirectly (a commit without a receipt blocks the next commit).
//!
//! Envelope. A [`WalTransaction`] keeps the page size, WAL generation (checkpoint sequence and
//! salts), the commit's first frame index, the ordered complete pages and the final database page
//! count. [`WalTransaction::chunks`] yields bounded chunks (default use: below the 1 MiB wire
//! entry) of `64` header bytes plus `(pgno, page)` records, so a transaction larger than one wire
//! entry stays representable; [`TxAssembler`] accepts the chunks of one transaction strictly in
//! order and yields an [`AssembledTx`] only when ALL are present: orphan or unapproved parts are
//! never applied. [`apply_transaction`] writes an assembled transaction into an OFFLINE target
//! (validate everything first, then page writes, set length, fsync); it is not crash-atomic and is
//! meant for a standby copy that is replaced as a whole.
//!
//! Snapshot contract (checked by the tests against real SQLite, not assumed). SQLite's online
//! backup is NOT a byte copy: `backupOnePage` copies page 1 verbatim except the in-header page
//! count, then the destination's own pager rewrites its file change counter and version-valid-for
//! and `sqlite3_backup_step` sets the destination schema cookie to its previous cookie plus one
//! (backup.c, SQLite 3.50.2), so header offsets 24, 40 and 92 differ; every other byte of a
//! checkpointed source is equal (including the WAL format bytes 2/2). The contract is therefore
//! LOGICAL: [`WalVfs::snapshot`] copies with the backup API, later transactions are applied as
//! full pages in sequence order, and equality is judged by `quick_check` and a data comparison,
//! never by file identity. Applying a later page 1 simply replaces those counters with the
//! source's.
//!
//! What makes "the copy is exactly the state after commit N" true is the OWNERSHIP invariant, not
//! the sequence readings: `snapshot` accepts only the exact connection this wrapper admitted,
//! requires it to be idle (`sqlite3_txn_state` NONE: no explicit or implicit read or write
//! transaction and no half-read statement), and runs while no other MAIN connection can exist; a
//! safe `Connection` cannot be used concurrently, so no commit (and no xSync-to-publication
//! interval) can overlap the copy. The sequence is read before and after only as a tripwire and
//! a changed value is refused, never retried. The destination is created exclusively through an
//! owned temporary file in its own directory and linked into place only on success; nothing that
//! already exists is removed, truncated or replaced.
//!
//! Memory safety. The wrapper is a plain `sqlite3_vfs`/`sqlite3_io_methods` pair from the pinned
//! bindings. Per-file storage is the `File` struct followed by the base file object at a 16-byte
//! aligned offset; alignment is checked at open. State lives in an `Arc<Inner>` that every open
//! file holds a strong count on (raw count increments/decrements, balanced in `xClose`), so the
//! registered `pAppData`, the vfs struct and the sink outlive the owner handle until the last file
//! closes. Every C entry point runs under `catch_unwind` (a panic poisons the wrapper and returns
//! an I/O error); the registration is removed in `WalVfs::drop`.
#![allow(dead_code)]

use rusqlite::{ffi, Connection, OpenFlags};
use sha2::{Digest, Sha256};
use std::{
    cell::UnsafeCell,
    collections::HashSet,
    ffi::{c_char, c_int, c_void, CStr, CString},
    io::{Read, Seek, SeekFrom, Write},
    panic::{catch_unwind, AssertUnwindSafe},
    path::{Path, PathBuf},
    ptr,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex, MutexGuard,
    },
    thread::ThreadId,
};

const WAL_HEADER: usize = 32;
const FRAME_HEADER: usize = 24;
const WAL_VERSION: u32 = 3_007_000;
const WAL_MAGIC: u32 = 0x377f_0682;
/// Largest single write forwarded to the real WAL file.
const FLUSH_SLICE: usize = 1 << 20;

/// A fixed, content-free reason; never a path, page or payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Refusal(pub(crate) &'static str);
impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "takeover WAL capture refused: {}", self.0)
    }
}
impl std::error::Error for Refusal {}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Config {
    /// Most bytes of one uncommitted WAL tail (header and frames) held in memory.
    pub(crate) max_staged_bytes: usize,
    /// Most committed frames of the current WAL generation that are remembered (32-byte digests)
    /// so checkpoint writes can be recognised as committed work. A commit that would exceed it
    /// is refused until the WAL is checkpointed and restarted.
    pub(crate) max_tracked_frames: usize,
    /// Run a PASSIVE checkpoint after a commit once the WAL holds this many frames (SQLite's
    /// default is 1000); 0 disables automatic checkpoints like `wal_autocheckpoint(0)`.
    pub(crate) autocheckpoint_frames: u32,
}
impl Config {
    pub(crate) const DEFAULT: Config = Config {
        max_staged_bytes: 256 << 20,
        max_tracked_frames: 262_144,
        autocheckpoint_frames: 1000,
    };
}

/// The injected admission boundary. `approve` runs inside SQLite's commit `xSync`, on the
/// committing thread, with no wrapper lock held; it may block (a later quorum wait). `Ok` lets
/// the commit proceed; anything else (or a panic) fails the commit with an I/O error.
pub(crate) trait CommitSink: Send + Sync {
    fn approve(&self, transaction: &WalTransaction) -> Result<(), Refusal>;
    /// REQUIRED. Called from `sqlite3_wal_hook` after SQLite has really committed the transaction
    /// that `approve` admitted (never for a veto, a failed write or a rollback, and exactly once
    /// per commit), with no wrapper lock held, so the sink may write its own recovery store.
    /// An `Err` or panic means "committed, but the receipt could not be recorded": the commit is
    /// NOT undone, the wrapper poisons and refuses further work.
    fn local_committed(&self, receipt: &LocalCommit) -> Result<(), Refusal>;
}

/// A content-free statement that one approved commit is now really committed locally.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LocalCommit {
    /// The capture sequence `approve` saw for this commit.
    pub(crate) sequence: u64,
    pub(crate) generation: WalGeneration,
    pub(crate) first_frame: u32,
    /// Frames of the transaction itself (padding repeats excluded).
    pub(crate) real_frames: u32,
    /// All complete frames in the WAL after this commit, padding repeats included.
    pub(crate) wal_frames: u32,
    /// The final complete WAL position (bytes), padding repeats and their finished remainder
    /// included.
    pub(crate) wal_end: u64,
    /// The database size in pages after the commit.
    pub(crate) db_pages: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WalGeneration {
    pub(crate) page_size: u32,
    pub(crate) checkpoint_seq: u32,
    pub(crate) salt: [u32; 2],
    big_endian: bool,
}

// ---- WAL format helpers --------------------------------------------------------------------

fn be32(bytes: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}
fn valid_page_size(size: u32) -> bool {
    (512..=65_536).contains(&size) && size.is_power_of_two()
}
/// SQLite's `walChecksumBytes`: words are read in the byte order the WAL magic announces.
fn checksum(big_endian: bool, data: &[u8], seed: (u32, u32)) -> (u32, u32) {
    let (mut s1, mut s2) = seed;
    for pair in data.chunks_exact(8) {
        let (a, b) = if big_endian {
            (
                u32::from_be_bytes([pair[0], pair[1], pair[2], pair[3]]),
                u32::from_be_bytes([pair[4], pair[5], pair[6], pair[7]]),
            )
        } else {
            (
                u32::from_le_bytes([pair[0], pair[1], pair[2], pair[3]]),
                u32::from_le_bytes([pair[4], pair[5], pair[6], pair[7]]),
            )
        };
        s1 = s1.wrapping_add(a).wrapping_add(s2);
        s2 = s2.wrapping_add(b).wrapping_add(s1);
    }
    (s1, s2)
}
fn parse_wal_header(header: &[u8]) -> Result<(WalGeneration, (u32, u32)), Refusal> {
    if header.len() < WAL_HEADER {
        return Err(Refusal("short WAL header"));
    }
    let magic = be32(header, 0);
    if magic & !1 != WAL_MAGIC || be32(header, 4) != WAL_VERSION {
        return Err(Refusal("unsupported WAL header"));
    }
    let big_endian = magic & 1 == 1;
    let page_size = be32(header, 8);
    if !valid_page_size(page_size) {
        return Err(Refusal("invalid WAL page size"));
    }
    let sum = checksum(big_endian, &header[..24], (0, 0));
    if sum != (be32(header, 24), be32(header, 28)) {
        return Err(Refusal("WAL header checksum"));
    }
    Ok((
        WalGeneration {
            page_size,
            checkpoint_seq: be32(header, 12),
            salt: [be32(header, 16), be32(header, 20)],
            big_endian,
        },
        sum,
    ))
}
fn frame_size(page_size: u32) -> usize {
    FRAME_HEADER + page_size as usize
}
fn frame_offset(index: u32, page_size: u32) -> u64 {
    WAL_HEADER as u64 + (u64::from(index) - 1) * frame_size(page_size) as u64
}
fn digest(page: &[u8]) -> [u8; 32] {
    Sha256::digest(page).into()
}

// ---- transaction envelope ---------------------------------------------------------------------

/// One approved WAL commit: the page size, the WAL generation, the first frame index, the ordered
/// complete pages and the final database page count.
pub(crate) struct WalTransaction {
    sequence: u64,
    generation: WalGeneration,
    first_frame: u32,
    db_pages: u32,
    frames: u32,
    /// The held WAL bytes (the 32-byte header first when this commit starts a generation).
    raw: Vec<u8>,
    frames_at: usize,
    base: u64,
}
impl std::fmt::Debug for WalTransaction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WalTransaction")
            .field("sequence", &self.sequence)
            .field("frames", &self.frames)
            .field("db_pages", &self.db_pages)
            .finish_non_exhaustive()
    }
}
impl WalTransaction {
    /// 1-based position among the commits approved through this registration.
    pub(crate) fn sequence(&self) -> u64 {
        self.sequence
    }
    pub(crate) fn page_size(&self) -> u32 {
        self.generation.page_size
    }
    pub(crate) fn generation(&self) -> &WalGeneration {
        &self.generation
    }
    pub(crate) fn first_frame(&self) -> u32 {
        self.first_frame
    }
    pub(crate) fn db_pages(&self) -> u32 {
        self.db_pages
    }
    pub(crate) fn frame_count(&self) -> u32 {
        self.frames
    }
    /// The ordered complete pages `(page number, page bytes)`.
    pub(crate) fn pages(&self) -> impl Iterator<Item = (u32, &[u8])> + '_ {
        (0..self.frames as usize).map(move |i| self.page_at(i))
    }
    fn page_at(&self, i: usize) -> (u32, &[u8]) {
        let size = frame_size(self.generation.page_size);
        let at = self.frames_at + i * size;
        (be32(&self.raw, at), &self.raw[at + FRAME_HEADER..at + size])
    }
    /// Bounded chunks of at most `max_bytes` each (a chunk holds whole pages). The iterator
    /// yields exactly `chunk_count` chunks; nothing is truncated.
    pub(crate) fn chunks(&self, max_bytes: usize) -> Result<Chunks<'_>, Refusal> {
        let record = 4 + self.generation.page_size as usize;
        if max_bytes < CHUNK_HEADER + record {
            return Err(Refusal("chunk limit smaller than one page"));
        }
        let per_chunk = ((max_bytes - CHUNK_HEADER) / record) as u32;
        let count = self.frames.div_ceil(per_chunk);
        Ok(Chunks {
            tx: self,
            per_chunk,
            count,
            next: 0,
        })
    }
}

const CHUNK_HEADER: usize = 64;
const CHUNK_MAGIC: &[u8; 4] = b"RWTX";

/// Fixed 64-byte chunk header, all integers big-endian.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ChunkHeader {
    sequence: u64,
    page_size: u32,
    checkpoint_seq: u32,
    salt: [u32; 2],
    first_frame: u32,
    db_pages: u32,
    total_frames: u32,
    chunk_index: u32,
    chunk_count: u32,
    frames_in_chunk: u32,
    first_in_chunk: u32,
}
impl ChunkHeader {
    fn encode(&self) -> [u8; CHUNK_HEADER] {
        let mut out = [0u8; CHUNK_HEADER];
        out[..4].copy_from_slice(CHUNK_MAGIC);
        out[4] = 1;
        out[8..16].copy_from_slice(&self.sequence.to_be_bytes());
        let words = [
            self.page_size,
            self.checkpoint_seq,
            self.salt[0],
            self.salt[1],
            self.first_frame,
            self.db_pages,
            self.total_frames,
            self.chunk_index,
            self.chunk_count,
            self.frames_in_chunk,
            self.first_in_chunk,
        ];
        for (i, word) in words.iter().enumerate() {
            out[16 + i * 4..20 + i * 4].copy_from_slice(&word.to_be_bytes());
        }
        out
    }
    fn parse(chunk: &[u8]) -> Result<(ChunkHeader, &[u8]), Refusal> {
        if chunk.len() < CHUNK_HEADER || &chunk[..4] != CHUNK_MAGIC || chunk[4] != 1 {
            return Err(Refusal("not a transaction chunk"));
        }
        if chunk[5..8] != [0, 0, 0] || chunk[60..64] != [0, 0, 0, 0] {
            return Err(Refusal("reserved chunk bytes"));
        }
        let word = |i: usize| be32(chunk, 16 + i * 4);
        let header = ChunkHeader {
            sequence: u64::from_be_bytes(chunk[8..16].try_into().expect("8 bytes")),
            page_size: word(0),
            checkpoint_seq: word(1),
            salt: [word(2), word(3)],
            first_frame: word(4),
            db_pages: word(5),
            total_frames: word(6),
            chunk_index: word(7),
            chunk_count: word(8),
            frames_in_chunk: word(9),
            first_in_chunk: word(10),
        };
        let record = 4 + header.page_size as usize;
        if !valid_page_size(header.page_size)
            || header.chunk_count == 0
            || header.chunk_index >= header.chunk_count
            || header.total_frames == 0
            || header.first_frame == 0
            || header.db_pages == 0
            || header.frames_in_chunk == 0
            || u64::from(header.first_in_chunk) + u64::from(header.frames_in_chunk)
                > u64::from(header.total_frames)
            || chunk.len() - CHUNK_HEADER != header.frames_in_chunk as usize * record
        {
            return Err(Refusal("inconsistent transaction chunk"));
        }
        Ok((header, &chunk[CHUNK_HEADER..]))
    }
    fn same_transaction(&self, other: &ChunkHeader) -> bool {
        self.sequence == other.sequence
            && self.page_size == other.page_size
            && self.checkpoint_seq == other.checkpoint_seq
            && self.salt == other.salt
            && self.first_frame == other.first_frame
            && self.db_pages == other.db_pages
            && self.total_frames == other.total_frames
            && self.chunk_count == other.chunk_count
    }
}

pub(crate) struct Chunks<'a> {
    tx: &'a WalTransaction,
    per_chunk: u32,
    count: u32,
    next: u32,
}
impl Chunks<'_> {
    pub(crate) fn chunk_count(&self) -> u32 {
        self.count
    }
}
impl Iterator for Chunks<'_> {
    type Item = Vec<u8>;
    fn next(&mut self) -> Option<Vec<u8>> {
        if self.next >= self.count {
            return None;
        }
        let tx = self.tx;
        let first = self.next * self.per_chunk;
        let frames = self.per_chunk.min(tx.frames - first);
        let header = ChunkHeader {
            sequence: tx.sequence,
            page_size: tx.generation.page_size,
            checkpoint_seq: tx.generation.checkpoint_seq,
            salt: tx.generation.salt,
            first_frame: tx.first_frame,
            db_pages: tx.db_pages,
            total_frames: tx.frames,
            chunk_index: self.next,
            chunk_count: self.count,
            frames_in_chunk: frames,
            first_in_chunk: first,
        };
        let record = 4 + tx.generation.page_size as usize;
        let mut out = Vec::with_capacity(CHUNK_HEADER + frames as usize * record);
        out.extend_from_slice(&header.encode());
        for i in first as usize..(first + frames) as usize {
            let (pgno, page) = tx.page_at(i);
            out.extend_from_slice(&pgno.to_be_bytes());
            out.extend_from_slice(page);
        }
        self.next += 1;
        Some(out)
    }
}

/// A complete transaction rebuilt from its chunks.
#[derive(Debug)]
pub(crate) struct AssembledTx {
    pub(crate) sequence: u64,
    pub(crate) page_size: u32,
    pub(crate) first_frame: u32,
    pub(crate) db_pages: u32,
    pub(crate) frames: u32,
    records: Vec<u8>,
}
impl AssembledTx {
    pub(crate) fn pages(&self) -> impl Iterator<Item = (u32, &[u8])> + '_ {
        let record = 4 + self.page_size as usize;
        self.records
            .chunks_exact(record)
            .map(|r| (be32(r, 0), &r[4..]))
    }
}

/// Collects the chunks of ONE transaction, strictly in order. Dropping it discards any partial
/// transaction; only a complete one is ever returned.
pub(crate) struct TxAssembler {
    max_bytes: usize,
    open: Option<(ChunkHeader, u32, Vec<u8>)>,
}
impl TxAssembler {
    pub(crate) fn new(max_bytes: usize) -> Self {
        Self {
            max_bytes,
            open: None,
        }
    }
    /// `Ok(None)`: accepted, more chunks needed. `Ok(Some(tx))`: that was the last one. A bad,
    /// duplicate, out-of-order, foreign or oversized chunk is refused and discards the partial
    /// transaction.
    pub(crate) fn add(&mut self, chunk: &[u8]) -> Result<Option<AssembledTx>, Refusal> {
        let result = self.add_inner(chunk);
        if result.is_err() {
            self.open = None;
        }
        result
    }
    fn add_inner(&mut self, chunk: &[u8]) -> Result<Option<AssembledTx>, Refusal> {
        let (header, records) = ChunkHeader::parse(chunk)?;
        let (identity, mut next, mut collected) = match self.open.take() {
            None => {
                if header.chunk_index != 0 {
                    return Err(Refusal("transaction does not start at its first chunk"));
                }
                let capacity = u64::from(header.total_frames) * (4 + u64::from(header.page_size));
                if capacity > self.max_bytes as u64 {
                    return Err(Refusal("transaction exceeds the assembly limit"));
                }
                (header, 0, Vec::with_capacity(capacity as usize))
            }
            Some(open) => open,
        };
        if !identity.same_transaction(&header)
            || header.chunk_index != next
            || header.first_in_chunk as usize != collected.len() / (4 + header.page_size as usize)
        {
            return Err(Refusal("chunk does not continue the open transaction"));
        }
        collected.extend_from_slice(records);
        next += 1;
        if next < header.chunk_count {
            self.open = Some((identity, next, collected));
            return Ok(None);
        }
        let frames = (collected.len() / (4 + header.page_size as usize)) as u32;
        if frames != header.total_frames {
            return Err(Refusal("transaction is missing pages"));
        }
        Ok(Some(AssembledTx {
            sequence: header.sequence,
            page_size: header.page_size,
            first_frame: header.first_frame,
            db_pages: header.db_pages,
            frames,
            records: collected,
        }))
    }
}

/// Writes a complete transaction into an OFFLINE database copy: everything is validated first,
/// then the pages are written, the length set to the final page count and the file synced. Not
/// crash-atomic; use it on a standby copy that is swapped in as a whole.
pub(crate) fn apply_transaction(target: &Path, tx: &AssembledTx) -> Result<(), Refusal> {
    let record = 4 + tx.page_size as usize;
    if !valid_page_size(tx.page_size)
        || tx.db_pages == 0
        || tx.frames == 0
        || tx.records.len() != tx.frames as usize * record
        || tx.pages().any(|(pgno, _)| pgno == 0 || pgno > 0x7fff_ffff)
    {
        return Err(Refusal("invalid transaction"));
    }
    match std::fs::symlink_metadata(target) {
        Ok(info) if info.file_type().is_file() => {}
        _ => return Err(Refusal("apply target is not a regular file")),
    }
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(target)
        .map_err(|_| Refusal("cannot open apply target"))?;
    let mut header = [0u8; 100];
    file.read_exact(&mut header)
        .map_err(|_| Refusal("apply target is not a database"))?;
    let page_size = match u32::from(u16::from_be_bytes([header[16], header[17]])) {
        1 => 65_536,
        other => other,
    };
    if &header[..16] != b"SQLite format 3\0" || page_size != tx.page_size {
        return Err(Refusal("apply target does not match the page size"));
    }
    for (pgno, page) in tx.pages() {
        let at = (u64::from(pgno) - 1) * u64::from(tx.page_size);
        file.seek(SeekFrom::Start(at))
            .and_then(|_| file.write_all(page))
            .map_err(|_| Refusal("apply write failed"))?;
    }
    file.set_len(u64::from(tx.db_pages) * u64::from(tx.page_size))
        .and_then(|_| file.sync_all())
        .map_err(|_| Refusal("apply finish failed"))
}

// ---- shared state ---------------------------------------------------------------------------------

#[derive(Default)]
struct DbState {
    page_size: u32,
    generation: Option<WalGeneration>,
    /// WAL file offset just after the last approved commit frame (0 = nothing committed).
    committed_end: u64,
    last_cksum: (u32, u32),
    last_db_pages: u32,
    committed_frames: HashSet<(u32, [u8; 32])>,
    /// Held bytes that start at `committed_end`.
    tail: Vec<u8>,
    /// The file object that holds the tail (0 = none).
    owner: usize,
    wal_seen: bool,
    /// The `xDeviceCharacteristics` value last handed to SQLite for the MAIN file; it decides
    /// whether SQLite pads commits to a sector boundary. `None` until SQLite asked.
    main_characteristics: Option<c_int>,
    /// The first commit-marked frame completed in the held tail, if any.
    commit_mark: Option<CommitMark>,
    /// The rest of an approved, synced padding frame that SQLite has not written yet.
    pending_pad: Option<PendingPad>,
    /// The approved and written commit that still waits for SQLite's wal hook (its receipt).
    pending_receipt: Option<LocalCommit>,
}
impl DbState {
    fn reset_generation(&mut self) {
        self.generation = None;
        self.committed_end = 0;
        self.last_cksum = (0, 0);
        self.committed_frames.clear();
        self.pending_pad = None;
        self.discard_tail();
    }
    fn discard_tail(&mut self) {
        self.tail.clear();
        self.owner = 0;
        self.commit_mark = None;
    }
    /// SQLite pads commits only when the main file lacks POWERSAFE_OVERWRITE.
    fn padding_expected(&self) -> Option<bool> {
        self.main_characteristics
            .map(|c| c & ffi::SQLITE_IOCAP_POWERSAFE_OVERWRITE == 0)
    }
}

/// The first commit-marked frame of the held tail: where it ends, its 16 header bytes (page
/// number, commit size, salts) and the page length. Repeats must reproduce exactly these.
#[derive(Clone, Copy)]
struct CommitMark {
    end: usize,
    head: [u8; 16],
    page_len: usize,
}
/// The expected, already-approved remainder of a padding frame whose first bytes were synced.
struct PendingPad {
    owner: usize,
    next_at: u64,
    rest: Vec<u8>,
    cksum_after: (u32, u32),
}
impl PendingPad {
    /// Consumes `bytes` only if they are exactly the next expected bytes at exactly `at`.
    fn accept(&mut self, owner: usize, at: u64, bytes: &[u8]) -> bool {
        if owner != self.owner
            || at != self.next_at
            || bytes.is_empty()
            || bytes.len() > self.rest.len()
            || self.rest[..bytes.len()] != *bytes
        {
            return false;
        }
        self.rest.drain(..bytes.len());
        self.next_at += bytes.len() as u64;
        true
    }
    fn finished(&self) -> bool {
        self.rest.is_empty()
    }
}

enum Tail {
    Empty,
    /// Bytes are held but no complete commit frame ends them (header, spill, partial frame).
    Pending,
    Commit(CommitInfo),
}
struct CommitInfo {
    generation: WalGeneration,
    first_frame: u32,
    frames: u32,
    db_pages: u32,
    frames_at: usize,
    /// Checksum chain after the last COMPLETE frame (real frames and complete repeats).
    last_cksum: (u32, u32),
    digests: Vec<(u32, [u8; 32])>,
    /// Complete exact repeats after the commit frame, and the bytes of one more partial repeat.
    repeats: u32,
    partial: usize,
    /// The unwritten remainder of that partial repeat and the chain once it is complete.
    rest: Vec<u8>,
    cksum_after: (u32, u32),
}

/// Parses and fully verifies the held tail as recovery would. A tail is a commit only if it holds
/// the transaction up to its FIRST commit-marked frame and then exactly what SQLite writes after
/// it on this device: nothing, or (when padding is expected) exact repeats of that frame up to the
/// sector boundary at which SQLite syncs. `sector` is SQLite's own clamped sector size.
fn analyze(state: &DbState, sector: u32) -> Result<Tail, Refusal> {
    let tail = &state.tail;
    if tail.is_empty() {
        return Ok(Tail::Empty);
    }
    let (generation, frames_at, seed, first_frame) = if state.committed_end == 0 {
        if tail.len() < WAL_HEADER {
            return Ok(Tail::Pending);
        }
        let (generation, sum) = parse_wal_header(tail)?;
        if state.page_size != 0 && generation.page_size != state.page_size {
            return Err(Refusal("WAL page size differs from the database"));
        }
        (generation, WAL_HEADER, sum, 1u32)
    } else {
        let generation = state
            .generation
            .clone()
            .ok_or(Refusal("no WAL generation"))?;
        let size = frame_size(generation.page_size) as u64;
        let index = (state.committed_end - WAL_HEADER as u64) / size + 1;
        let index = u32::try_from(index).map_err(|_| Refusal("WAL frame index overflow"))?;
        (generation, 0, state.last_cksum, index)
    };
    let size = frame_size(generation.page_size);
    let body = tail.len() - frames_at;
    if body < size {
        return Ok(Tail::Pending);
    }
    let complete = body / size;
    let Some(commit_index) = (0..complete).find(|i| be32(&tail[frames_at + i * size..], 4) != 0)
    else {
        return Ok(Tail::Pending); // a spill or a partial frame: no commit frame yet
    };
    let salt = [
        generation.salt[0].to_be_bytes(),
        generation.salt[1].to_be_bytes(),
    ]
    .concat();
    let mut sum = seed;
    let mut digests = Vec::with_capacity(commit_index + 1);
    for i in 0..=commit_index {
        let frame = &tail[frames_at + i * size..frames_at + (i + 1) * size];
        let pgno = be32(frame, 0);
        let size_field = be32(frame, 4);
        if pgno == 0 || frame[8..16] != salt[..] || (i < commit_index && size_field != 0) {
            return Err(Refusal("malformed WAL frame in a commit"));
        }
        sum = checksum(generation.big_endian, &frame[..8], sum);
        sum = checksum(generation.big_endian, &frame[FRAME_HEADER..], sum);
        if sum != (be32(frame, 16), be32(frame, 20)) {
            return Err(Refusal("WAL frame checksum"));
        }
        digests.push((pgno, digest(&frame[FRAME_HEADER..])));
    }
    let commit_start = frames_at + commit_index * size;
    let commit_end = commit_start + size;
    let commit_frame = &tail[commit_start..commit_end];
    let db_pages = be32(commit_frame, 4);

    // Where SQLite syncs: at the end of the commit frame, or, when it pads, inside the write that
    // crosses the next sector boundary (iSyncPoint in walFrames).
    let Some(padding) = state.padding_expected() else {
        return Err(Refusal("device characteristics were never reported"));
    };
    let frame_end = state.committed_end + commit_end as u64;
    let sync_end = if padding {
        let sector = u64::from(sector.max(1));
        frame_end.div_ceil(sector) * sector
    } else {
        frame_end
    };
    if state.committed_end + tail.len() as u64 != sync_end {
        return Err(Refusal("unexpected WAL sync position"));
    }
    // What follows the commit frame must be exact repeats of it, with the chain continuing.
    let page = &commit_frame[FRAME_HEADER..];
    let repeat = |before: (u32, u32)| -> (Vec<u8>, (u32, u32)) {
        let mut frame = Vec::with_capacity(size);
        frame.extend_from_slice(&commit_frame[..16]);
        let mut after = checksum(generation.big_endian, &commit_frame[..8], before);
        after = checksum(generation.big_endian, page, after);
        frame.extend_from_slice(&after.0.to_be_bytes());
        frame.extend_from_slice(&after.1.to_be_bytes());
        frame.extend_from_slice(page);
        (frame, after)
    };
    let remainder = tail.len() - commit_end;
    let (repeats, partial) = (remainder / size, remainder % size);
    for j in 0..repeats {
        let (expected, after) = repeat(sum);
        if tail[commit_end + j * size..commit_end + (j + 1) * size] != expected[..] {
            return Err(Refusal("padding frame differs from the commit frame"));
        }
        sum = after;
    }
    let (rest, cksum_after) = if partial > 0 {
        let (expected, after) = repeat(sum);
        if tail[commit_end + repeats * size..] != expected[..partial] {
            return Err(Refusal(
                "partial padding frame differs from the commit frame",
            ));
        }
        (expected[partial..].to_vec(), after)
    } else {
        (Vec::new(), sum)
    };
    Ok(Tail::Commit(CommitInfo {
        generation,
        first_frame,
        frames: u32::try_from(commit_index + 1).map_err(|_| Refusal("too many frames"))?,
        db_pages,
        frames_at,
        last_cksum: sum,
        digests,
        repeats: u32::try_from(repeats).map_err(|_| Refusal("too many padding frames"))?,
        partial,
        rest,
        cksum_after,
    }))
}

/// SQLite's `sqlite3SectorSize` clamp of the file's reported sector size.
unsafe fn sector_size(real: *mut ffi::sqlite3_file) -> u32 {
    let reported = match (*(*real).pMethods).xSectorSize {
        Some(call) => call(real),
        None => 512,
    };
    if reported < 32 {
        512
    } else {
        (reported as u32).min(0x10000)
    }
}

/// What a sync of the WAL file means for the held bytes.
enum SyncPlan {
    /// Nothing is held for this file object: an ordinary sync of the real file.
    Forward,
    /// SQLite's one non-commit sync: exactly the 32-byte WAL header of a new generation.
    HeaderOnly,
    /// A complete, fully verified commit.
    Commit(CommitInfo),
    /// Any other pattern (spill, partial frame, partial or malformed header): unsupported.
    Refuse,
}
/// In SQLite 3.50.2 a WAL file object is synced (a) before the checkpoint backfill, on the
/// checkpointer's own object (sqlite3.c:68137); (b) at the padding sync point INSIDE the write
/// that crosses the sector boundary, only when the main file lacks POWERSAFE_OVERWRITE (69807,
/// 70051): that is the commit's own sync and `analyze` recognises it as a commit when the held
/// bytes are the verified transaction plus exact repeats of its commit frame ending at SQLite's
/// own sync point; (c) right after the generation's WAL header is written when `syncHeader` is
/// set, before any frame (69975); and (d) at the end of an unpadded commit (70066). Only (c) is a
/// successful sync of a non-commit tail, and only when the held bytes are exactly that valid
/// header at offset 0 of a generation. Everything else that is not a complete commit is refused
/// instead of reported as a durability boundary.
fn plan_sync(state: &DbState, owner: bool, sector: u32) -> SyncPlan {
    if !owner || state.tail.is_empty() {
        return SyncPlan::Forward;
    }
    match analyze(state, sector) {
        Ok(Tail::Commit(info)) => SyncPlan::Commit(info),
        Ok(Tail::Pending) if state.committed_end == 0 && state.tail.len() == WAL_HEADER => {
            SyncPlan::HeaderOnly
        }
        _ => SyncPlan::Refuse,
    }
}

// ---- inner object ------------------------------------------------------------------------------------

struct Inner {
    vfs: UnsafeCell<ffi::sqlite3_vfs>,
    name: CString,
    base: *mut ffi::sqlite3_vfs,
    main_path: Vec<u8>,
    wal_path: Vec<u8>,
    config: Config,
    sink: Arc<dyn CommitSink>,
    state: Mutex<DbState>,
    sequence: AtomicU64,
    /// Receipts delivered (always <= `sequence`); a receipt failure after a real commit.
    local_sequence: AtomicU64,
    receipt_failed: AtomicU64,
    checkpointed_pages: AtomicU64,
    poisoned: AtomicBool,
    registered: AtomicBool,
    /// Serialises `open_connection`.
    open_gate: Mutex<()>,
    /// The thread that may admit a MAIN open right now (armed only inside `open_connection`).
    permit: Mutex<Option<ThreadId>>,
    /// Address of the admitted MAIN file object (0 = none) and its `sqlite3*` (0 = none).
    main_open: AtomicUsize,
    owned_db: AtomicUsize,
    snapshotting: AtomicBool,
}
// SAFETY: the raw pointers are the process-wide default VFS (never freed) and this object's own
// vfs struct (written only before registration); all other state is behind a Mutex or atomics.
unsafe impl Send for Inner {}
unsafe impl Sync for Inner {}
impl Inner {
    fn lock(&self) -> MutexGuard<'_, DbState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
    fn poison(&self) {
        self.poisoned.store(true, Ordering::SeqCst);
        self.lock().discard_tail();
    }
    fn is_poisoned(&self) -> bool {
        self.poisoned.load(Ordering::SeqCst)
    }
    fn matches_main(&self, name: &[u8]) -> bool {
        same_path(name, &self.main_path)
    }
    fn matches_wal(&self, name: &[u8]) -> bool {
        same_path(name, &self.wal_path)
    }
}
#[cfg(windows)]
fn same_path(a: &[u8], b: &[u8]) -> bool {
    a.eq_ignore_ascii_case(b)
}
#[cfg(not(windows))]
fn same_path(a: &[u8], b: &[u8]) -> bool {
    a == b
}

/// Owner handle of one registration. Dropping it removes the registration; connections that are
/// still open keep working until they close because their files hold the shared state alive.
pub(crate) struct WalVfs {
    inner: Arc<Inner>,
    path: PathBuf,
}
impl std::fmt::Debug for WalVfs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WalVfs")
            .field("commits", &self.commit_sequence())
            .finish_non_exhaustive()
    }
}
static COUNTER: AtomicU64 = AtomicU64::new(1);

impl WalVfs {
    /// Registers a wrapper around the default VFS for exactly this existing, absolute database
    /// path. Nothing is activated for any other path or connection.
    pub(crate) fn register(
        path: &Path,
        config: Config,
        sink: Arc<dyn CommitSink>,
    ) -> Result<WalVfs, Refusal> {
        if !path.is_absolute()
            || config.max_staged_bytes == 0
            || config.max_tracked_frames == 0
            || !std::fs::symlink_metadata(path).is_ok_and(|i| i.file_type().is_file())
        {
            return Err(Refusal("an existing absolute database file is required"));
        }
        let c_path =
            CString::new(path.to_string_lossy().as_bytes()).map_err(|_| Refusal("bad path"))?;
        // SAFETY: sqlite3_vfs_find(NULL) returns the default VFS or NULL.
        let base = unsafe { ffi::sqlite3_vfs_find(ptr::null()) };
        if base.is_null() {
            return Err(Refusal("no default VFS"));
        }
        // SAFETY: base is non-null and stays registered for the life of the process.
        let base_ref = unsafe { &*base };
        if base_ref.iVersion < 1 || base_ref.szOsFile <= 0 || base_ref.mxPathname <= 0 {
            return Err(Refusal("unsupported base VFS"));
        }
        let full = unsafe {
            let Some(full_pathname) = base_ref.xFullPathname else {
                return Err(Refusal("base VFS cannot resolve paths"));
            };
            let mut out = vec![0u8; base_ref.mxPathname as usize + 1];
            let rc = full_pathname(
                base,
                c_path.as_ptr(),
                base_ref.mxPathname,
                out.as_mut_ptr() as *mut c_char,
            );
            if rc != ffi::SQLITE_OK {
                return Err(Refusal("base VFS could not resolve the path"));
            }
            let end = out.iter().position(|b| *b == 0).unwrap_or(out.len());
            out.truncate(end);
            out
        };
        let mut wal_path = full.clone();
        wal_path.extend_from_slice(b"-wal");
        let number = COUNTER.fetch_add(1, Ordering::SeqCst);
        let name = CString::new(format!(
            "relay-takeover-wal-{}-{number}",
            std::process::id()
        ))
        .map_err(|_| Refusal("bad name"))?;
        let file_offset = file_storage_offset();
        let sz_os_file = file_offset
            .checked_add(base_ref.szOsFile as usize)
            .and_then(|n| c_int::try_from(n).ok())
            .ok_or(Refusal("file object too large"))?;
        // SAFETY: an all-zero sqlite3_vfs (null pointers, no function pointers) is valid.
        let mut vfs: ffi::sqlite3_vfs = unsafe { std::mem::zeroed() };
        vfs.iVersion = base_ref.iVersion.min(3);
        vfs.szOsFile = sz_os_file;
        vfs.mxPathname = base_ref.mxPathname;
        vfs.xOpen = Some(x_open);
        vfs.xDelete = Some(x_delete);
        vfs.xAccess = Some(x_access);
        vfs.xFullPathname = Some(x_full_pathname);
        vfs.xDlOpen = Some(x_dl_open);
        vfs.xDlError = Some(x_dl_error);
        vfs.xDlSym = Some(x_dl_sym);
        vfs.xDlClose = Some(x_dl_close);
        vfs.xRandomness = Some(x_randomness);
        vfs.xSleep = Some(x_sleep);
        vfs.xCurrentTime = Some(x_current_time);
        vfs.xGetLastError = Some(x_get_last_error);
        if vfs.iVersion >= 2 {
            vfs.xCurrentTimeInt64 = Some(x_current_time_int64);
        }
        if vfs.iVersion >= 3 {
            vfs.xSetSystemCall = Some(x_set_system_call);
            vfs.xGetSystemCall = Some(x_get_system_call);
            vfs.xNextSystemCall = Some(x_next_system_call);
        }
        let inner = Arc::new(Inner {
            vfs: UnsafeCell::new(vfs),
            name,
            base,
            main_path: full,
            wal_path,
            config,
            sink,
            state: Mutex::new(DbState::default()),
            sequence: AtomicU64::new(0),
            local_sequence: AtomicU64::new(0),
            receipt_failed: AtomicU64::new(0),
            checkpointed_pages: AtomicU64::new(0),
            poisoned: AtomicBool::new(false),
            registered: AtomicBool::new(false),
            open_gate: Mutex::new(()),
            permit: Mutex::new(None),
            main_open: AtomicUsize::new(0),
            owned_db: AtomicUsize::new(0),
            snapshotting: AtomicBool::new(false),
        });
        // SAFETY: nothing else can see the vfs struct yet; its address is stable inside the Arc
        // and its name buffer is owned by the same Arc.
        let vfs_ptr = inner.vfs.get();
        unsafe {
            (*vfs_ptr).zName = inner.name.as_ptr();
            (*vfs_ptr).pAppData = Arc::as_ptr(&inner) as *mut c_void;
            if ffi::sqlite3_vfs_register(vfs_ptr, 0) != ffi::SQLITE_OK {
                return Err(Refusal("VFS registration failed"));
            }
        }
        inner.registered.store(true, Ordering::SeqCst);
        Ok(WalVfs {
            inner,
            path: path.to_path_buf(),
        })
    }

    /// The unique VFS name of this registration (only for opening connections through it).
    pub(crate) fn vfs_name(&self) -> &str {
        self.inner.name.to_str().unwrap_or("")
    }
    /// Whether SQLite pads commits on this device (`None` until it has asked the main file).
    pub(crate) fn padding_observed(&self) -> Option<bool> {
        self.inner.lock().padding_expected()
    }
    /// The WAL offset just after the last COMPLETE approved frame, repeats included.
    pub(crate) fn committed_wal_end(&self) -> u64 {
        self.inner.lock().committed_end
    }
    /// Local commit receipts delivered so far (the sequence of the last one).
    pub(crate) fn local_commit_sequence(&self) -> u64 {
        self.inner.local_sequence.load(Ordering::SeqCst)
    }
    /// The capture sequence of an approved commit that has no receipt (yet): recovery work.
    pub(crate) fn unreceipted_commit(&self) -> Option<u64> {
        self.inner
            .lock()
            .pending_receipt
            .as_ref()
            .map(|p| p.sequence)
    }
    /// `Some(sequence)` when that commit REALLY HAPPENED but its receipt failed or panicked;
    /// distinct from a veto, which fails before the commit.
    pub(crate) fn receipt_failed_after_commit(&self) -> Option<u64> {
        match self.inner.receipt_failed.load(Ordering::SeqCst) {
            0 => None,
            sequence => Some(sequence),
        }
    }
    /// Whole committed pages written to the main database by checkpoints so far.
    pub(crate) fn checkpointed_pages(&self) -> u64 {
        self.inner.checkpointed_pages.load(Ordering::SeqCst)
    }
    /// Commits approved so far.
    pub(crate) fn commit_sequence(&self) -> u64 {
        self.inner.sequence.load(Ordering::SeqCst)
    }
    pub(crate) fn is_poisoned(&self) -> bool {
        self.inner.is_poisoned()
    }

    /// THE connection to the armed database: at most one exists at a time (a second call fails
    /// until the first is dropped). It has `synchronous=FULL` and WAL verified and an authorizer
    /// that denies changing them, `ATTACH` and `DETACH`.
    pub(crate) fn open_connection(&self, flags: OpenFlags) -> Result<Connection, Refusal> {
        let inner = &*self.inner;
        let _serial = inner.open_gate.lock().unwrap_or_else(|e| e.into_inner());
        if self.is_poisoned() {
            return Err(Refusal("the wrapper is poisoned"));
        }
        if inner.main_open.load(Ordering::SeqCst) != 0
            || inner.owned_db.load(Ordering::SeqCst) != 0
            || inner.snapshotting.load(Ordering::SeqCst)
        {
            return Err(Refusal("a connection is already admitted"));
        }
        // Only this thread, only inside this call, may pass the MAIN open.
        *inner.permit.lock().unwrap_or_else(|e| e.into_inner()) = Some(std::thread::current().id());
        let opened = self.admit(flags);
        *inner.permit.lock().unwrap_or_else(|e| e.into_inner()) = None;
        let conn = opened?;
        if inner.main_open.load(Ordering::SeqCst) == 0 {
            return Err(Refusal("the database was never admitted"));
        }
        // SAFETY: only the pointer value is read.
        inner
            .owned_db
            .store(unsafe { conn.handle() } as usize, Ordering::SeqCst);
        Ok(conn)
    }

    fn admit(&self, flags: OpenFlags) -> Result<Connection, Refusal> {
        let conn = Connection::open_with_flags_and_vfs(
            &self.path,
            flags | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            self.inner.name.to_str().map_err(|_| Refusal("bad name"))?,
        )
        .map_err(|_| Refusal("open through the wrapper failed"))?;
        conn.pragma_update(None, "synchronous", "FULL")
            .map_err(|_| Refusal("cannot set synchronous"))?;
        let sync: i64 = conn
            .query_row("PRAGMA synchronous", [], |r| r.get(0))
            .map_err(|_| Refusal("cannot read synchronous"))?;
        // The open above already passed the MAIN admission; these queries prove the connection works.
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .map_err(|_| Refusal("cannot read journal mode"))?;
        if sync != 2 || !mode.eq_ignore_ascii_case("wal") {
            return Err(Refusal("synchronous=FULL and WAL are required"));
        }
        // SAFETY: the handle is valid for the life of `conn`; the callback is a plain function
        // with no user data. rusqlite installs no authorizer without its `hooks` feature.
        let rc =
            unsafe { ffi::sqlite3_set_authorizer(conn.handle(), Some(authorize), ptr::null_mut()) };
        if rc != ffi::SQLITE_OK {
            return Err(Refusal("cannot install the authorizer"));
        }
        // SAFETY: the handle is valid for the life of `conn`. The argument is the shared state:
        // it stays alive for as long as this connection can call the hook, because the
        // connection's admitted MAIN file object holds a strong count on it until it closes.
        // The hook replaces SQLite's default automatic checkpoint, which it reproduces itself.
        unsafe {
            ffi::sqlite3_wal_hook(
                conn.handle(),
                Some(wal_hook),
                Arc::as_ptr(&self.inner) as *mut c_void,
            );
        }
        Ok(conn)
    }

    /// A logical snapshot of the admitted connection into the NEW file `dest`; returns N such
    /// that the copy is the state after commit N. Refused unless `source` is exactly the
    /// connection this wrapper admitted and it is idle (no transaction of any kind, no statement
    /// half-read), nothing is held in the wrapper, and `dest` and its side files do not exist.
    /// Nothing existing is removed, truncated or replaced: the copy is written to an owned
    /// temporary file beside `dest` and linked into place (never over something) only on success.
    /// The copy is NOT byte-identical to the source (see the module docs).
    pub(crate) fn snapshot(&self, source: &Connection, dest: &Path) -> Result<u64, Refusal> {
        let inner = &*self.inner;
        if inner.is_poisoned() {
            return Err(Refusal("the wrapper is poisoned"));
        }
        // SAFETY: only the pointer value is read.
        let handle = unsafe { source.handle() } as usize;
        if handle == 0
            || handle != inner.owned_db.load(Ordering::SeqCst)
            || inner.main_open.load(Ordering::SeqCst) == 0
        {
            return Err(Refusal("not the connection admitted by this wrapper"));
        }
        if inner.snapshotting.swap(true, Ordering::SeqCst) {
            return Err(Refusal("a snapshot is already running"));
        }
        struct Running<'a>(&'a AtomicBool);
        impl Drop for Running<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::SeqCst);
            }
        }
        let _running = Running(&inner.snapshotting);
        // SAFETY: the handle belongs to `source`, which is borrowed for this call.
        let txn =
            unsafe { ffi::sqlite3_txn_state(source.handle(), b"main\0".as_ptr() as *const c_char) };
        if txn != ffi::SQLITE_TXN_NONE || !source.is_autocommit() {
            return Err(Refusal("the source connection is inside a transaction"));
        }
        {
            let state = inner.lock();
            if !state.tail.is_empty()
                || state.owner != 0
                || state.pending_pad.is_some()
                || state.pending_receipt.is_some()
            {
                return Err(Refusal("a commit is in progress or has no receipt yet"));
            }
        }
        let temporary = Self::reserve_destination(dest, inner)?;
        let before = self.commit_sequence();
        let copied = Self::copy_into(source, &temporary.path);
        let outcome = match copied {
            Ok(()) if self.commit_sequence() == before && !inner.is_poisoned() => {
                std::fs::hard_link(&temporary.path, dest)
                    .map_err(|_| Refusal("the destination appeared or cannot be linked"))
            }
            Ok(()) => Err(Refusal("a commit overlapped the snapshot")),
            Err(refusal) => Err(refusal),
        };
        temporary.discard();
        outcome.map(|_| before)
    }

    fn copy_into(source: &Connection, temporary: &Path) -> Result<(), Refusal> {
        let mut target = Connection::open(temporary).map_err(|_| Refusal("snapshot open"))?;
        {
            let backup = rusqlite::backup::Backup::new(source, &mut target)
                .map_err(|_| Refusal("snapshot start"))?;
            match backup.step(-1) {
                Ok(rusqlite::backup::StepResult::Done) => {}
                _ => return Err(Refusal("snapshot copy did not complete")),
            }
        }
        target.close().map_err(|_| Refusal("snapshot close"))?;
        if std::fs::symlink_metadata(sidecar_path(temporary, "-journal")).is_ok() {
            return Err(Refusal("snapshot left a journal behind"));
        }
        Ok(())
    }

    /// Validates the destination and creates the owned temporary file next to it.
    fn reserve_destination(dest: &Path, inner: &Inner) -> Result<Reserved, Refusal> {
        let (Some(parent), Some(name)) = (dest.parent(), dest.file_name()) else {
            return Err(Refusal("the destination needs a directory and a name"));
        };
        let in_parent = std::fs::symlink_metadata(parent).is_ok_and(|i| i.file_type().is_dir());
        if !dest.is_absolute() || !in_parent || !direct_path(parent) {
            return Err(Refusal(
                "the destination directory is not a direct directory",
            ));
        }
        let wanted = dest.to_string_lossy();
        if inner.matches_main(wanted.as_bytes()) || inner.matches_wal(wanted.as_bytes()) {
            return Err(Refusal("the destination is the source itself"));
        }
        for suffix in ["", "-journal", "-wal", "-shm"] {
            if std::fs::symlink_metadata(sidecar_path(dest, suffix)).is_ok() {
                return Err(Refusal("the destination or a side file already exists"));
            }
        }
        let number = COUNTER.fetch_add(1, Ordering::SeqCst);
        let path = parent.join(format!(
            ".{}.snapshot-{}-{number}.tmp",
            name.to_string_lossy(),
            std::process::id()
        ));
        if std::fs::symlink_metadata(&path).is_ok()
            || std::fs::symlink_metadata(sidecar_path(&path, "-journal")).is_ok()
        {
            return Err(Refusal("the temporary snapshot name is taken"));
        }
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options
            .open(&path)
            .map_err(|_| Refusal("cannot create the temporary snapshot"))?;
        Ok(Reserved { path })
    }
}

/// The temporary snapshot file this call itself created; only it (and its journal) is ever removed.
struct Reserved {
    path: PathBuf,
}
impl Reserved {
    fn discard(self) {
        let _ = std::fs::remove_file(sidecar_path(&self.path, "-journal"));
        let _ = std::fs::remove_file(&self.path);
    }
}
fn sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}
/// No symlink or reparse point anywhere on the path.
fn direct_path(path: &Path) -> bool {
    path.ancestors()
        .all(|part| match std::fs::symlink_metadata(part) {
            Ok(info) => !info.file_type().is_symlink() && !is_reparse(&info),
            Err(error) => error.kind() == std::io::ErrorKind::NotFound,
        })
}
#[cfg(windows)]
fn is_reparse(info: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    info.file_attributes() & 0x400 != 0
}
#[cfg(not(windows))]
fn is_reparse(_info: &std::fs::Metadata) -> bool {
    false
}
impl Drop for WalVfs {
    fn drop(&mut self) {
        if self.inner.registered.swap(false, Ordering::SeqCst) {
            // SAFETY: the struct was registered by this owner and is still alive.
            unsafe {
                ffi::sqlite3_vfs_unregister(self.inner.vfs.get());
            }
        }
    }
}
impl Drop for Inner {
    fn drop(&mut self) {
        if self.registered.swap(false, Ordering::SeqCst) {
            // SAFETY: as above; defensive only (the owner handle normally unregisters first).
            unsafe {
                ffi::sqlite3_vfs_unregister(self.vfs.get());
            }
        }
    }
}

unsafe extern "C" fn authorize(
    _user: *mut c_void,
    action: c_int,
    first: *const c_char,
    second: *const c_char,
    _third: *const c_char,
    _fourth: *const c_char,
) -> c_int {
    let deny = catch_unwind(|| match action {
        ffi::SQLITE_ATTACH | ffi::SQLITE_DETACH => true,
        // A pragma WITH a value is a change; querying stays allowed.
        ffi::SQLITE_PRAGMA if !first.is_null() && !second.is_null() => {
            // SAFETY: SQLite passes NUL-terminated strings for these arguments.
            let name = unsafe { CStr::from_ptr(first) }.to_bytes();
            const DENIED: [&[u8]; 6] = [
                b"synchronous",
                b"journal_mode",
                b"locking_mode",
                b"writable_schema",
                b"mmap_size",
                // It would replace the owned wal hook (and with it the receipts).
                b"wal_autocheckpoint",
            ];
            DENIED
                .iter()
                .any(|denied| name.eq_ignore_ascii_case(denied))
        }
        _ => false,
    })
    .unwrap_or(true);
    if deny {
        ffi::SQLITE_DENY
    } else {
        ffi::SQLITE_OK
    }
}

/// What the SQL caller sees when the commit HAPPENED but its receipt (or the checkpoint that
/// followed) failed. Not a rollback: SQLITE_ABORT is used only on this path (a veto is
/// SQLITE_IOERR_FSYNC).
const AFTER_COMMIT_FAILED: c_int = ffi::SQLITE_ABORT;

/// SQLite's wal hook: runs after a real commit, outside the write lock.
unsafe extern "C" fn wal_hook(
    arg: *mut c_void,
    db: *mut ffi::sqlite3,
    name: *const c_char,
    pages: c_int,
) -> c_int {
    if arg.is_null() {
        return AFTER_COMMIT_FAILED;
    }
    let inner = &*(arg as *const Inner);
    guarded(inner, AFTER_COMMIT_FAILED, || {
        local_commit(inner, db, name, pages)
    })
}

unsafe fn local_commit(
    inner: &Inner,
    db: *mut ffi::sqlite3,
    name: *const c_char,
    pages: c_int,
) -> c_int {
    let is_main = !name.is_null() && CStr::from_ptr(name).to_bytes() == b"main";
    let owned = db as usize != 0
        && inner.owned_db.load(Ordering::SeqCst) == db as usize
        && inner.main_open.load(Ordering::SeqCst) != 0;
    if !is_main || !owned || pages <= 0 || inner.is_poisoned() {
        inner.poison();
        return AFTER_COMMIT_FAILED;
    }
    // Bind to the pending capture and take it, so it can be delivered exactly once.
    let receipt = {
        let mut state = inner.lock();
        let bound = state.tail.is_empty()
            && state.pending_pad.is_none()
            && state.pending_receipt.as_ref().is_some_and(|p| {
                p.wal_frames as i64 == i64::from(pages) && p.wal_end == state.committed_end
            });
        if !bound {
            drop(state);
            inner.poison();
            return AFTER_COMMIT_FAILED;
        }
        state.pending_receipt.take()
    };
    let Some(receipt) = receipt else {
        inner.poison();
        return AFTER_COMMIT_FAILED;
    };
    // No wrapper lock is held while the sink writes its own recovery store.
    match catch_unwind(AssertUnwindSafe(|| inner.sink.local_committed(&receipt))) {
        Ok(Ok(())) => {}
        _ => {
            inner
                .receipt_failed
                .store(receipt.sequence, Ordering::SeqCst);
            inner.poison();
            return AFTER_COMMIT_FAILED;
        }
    }
    inner
        .local_sequence
        .store(receipt.sequence, Ordering::SeqCst);
    // SQLite's default hook (sqlite3WalDefaultHook, sqlite3.c:~185060): once the WAL holds the
    // threshold, run a PASSIVE checkpoint. BUSY/LOCKED just mean "not now"; anything else is
    // serious and is neither ignored nor mistaken for a commit receipt.
    let threshold = inner.config.autocheckpoint_frames;
    if threshold > 0 && pages as u32 >= threshold {
        let rc = ffi::sqlite3_wal_checkpoint(db, name);
        if !matches!(
            rc & 0xff,
            ffi::SQLITE_OK | ffi::SQLITE_BUSY | ffi::SQLITE_LOCKED
        ) {
            inner.poison();
            return if rc == ffi::SQLITE_OK {
                AFTER_COMMIT_FAILED
            } else {
                rc
            };
        }
    }
    ffi::SQLITE_OK
}

// ---- file objects ------------------------------------------------------------------------------------

#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Main,
    Wal,
    Other,
}
/// The per-file object. SQLite allocates `szOsFile` bytes: this struct first, then (at
/// `file_storage_offset()`) the base VFS's own file object.
#[repr(C)]
struct File {
    base: ffi::sqlite3_file,
    kind: Kind,
    inner: *const Inner,
    real: *mut ffi::sqlite3_file,
}
fn file_storage_offset() -> usize {
    std::mem::size_of::<File>().div_ceil(16) * 16
}

/// Runs `body`, converting a panic into a poisoned wrapper and `rc`.
fn guarded(inner: &Inner, rc: c_int, body: impl FnOnce() -> c_int) -> c_int {
    match catch_unwind(AssertUnwindSafe(body)) {
        Ok(code) => code,
        Err(_) => {
            inner.poison();
            rc
        }
    }
}
unsafe fn inner_of<'a>(vfs: *mut ffi::sqlite3_vfs) -> &'a Inner {
    &*((*vfs).pAppData as *const Inner)
}
unsafe fn file_of<'a>(file: *mut ffi::sqlite3_file) -> &'a File {
    &*(file as *const File)
}
unsafe fn base_of<'a>(inner: &Inner) -> &'a ffi::sqlite3_vfs {
    &*inner.base
}

unsafe fn r_read(real: *mut ffi::sqlite3_file, buf: *mut c_void, amt: c_int, at: i64) -> c_int {
    match (*(*real).pMethods).xRead {
        Some(f) => f(real, buf, amt, at),
        None => ffi::SQLITE_IOERR_READ,
    }
}
unsafe fn r_write(real: *mut ffi::sqlite3_file, data: &[u8], at: u64) -> c_int {
    let Ok(at) = i64::try_from(at) else {
        return ffi::SQLITE_IOERR_WRITE;
    };
    let Some(f) = (*(*real).pMethods).xWrite else {
        return ffi::SQLITE_IOERR_WRITE;
    };
    let mut done = 0usize;
    while done < data.len() {
        let slice = &data[done..(done + FLUSH_SLICE).min(data.len())];
        let rc = f(
            real,
            slice.as_ptr() as *const c_void,
            slice.len() as c_int,
            at + done as i64,
        );
        if rc != ffi::SQLITE_OK {
            return rc;
        }
        done += slice.len();
    }
    ffi::SQLITE_OK
}
unsafe fn r_sync(real: *mut ffi::sqlite3_file, flags: c_int) -> c_int {
    match (*(*real).pMethods).xSync {
        Some(f) => f(real, flags),
        None => ffi::SQLITE_IOERR_FSYNC,
    }
}
unsafe fn r_size(real: *mut ffi::sqlite3_file, out: &mut i64) -> c_int {
    match (*(*real).pMethods).xFileSize {
        Some(f) => f(real, out),
        None => ffi::SQLITE_IOERR,
    }
}
unsafe fn r_truncate(real: *mut ffi::sqlite3_file, size: i64) -> c_int {
    match (*(*real).pMethods).xTruncate {
        Some(f) => f(real, size),
        None => ffi::SQLITE_IOERR_TRUNCATE,
    }
}
unsafe fn r_close(real: *mut ffi::sqlite3_file) -> c_int {
    match (*(*real).pMethods).xClose {
        Some(f) => f(real),
        None => ffi::SQLITE_OK,
    }
}

unsafe extern "C" fn x_open(
    vfs: *mut ffi::sqlite3_vfs,
    name: ffi::sqlite3_filename,
    file: *mut ffi::sqlite3_file,
    flags: c_int,
    out_flags: *mut c_int,
) -> c_int {
    let inner = inner_of(vfs);
    guarded(inner, ffi::SQLITE_CANTOPEN, || {
        open_file(inner, name, file, flags, out_flags)
    })
}

unsafe fn open_file(
    inner: &Inner,
    name: ffi::sqlite3_filename,
    file: *mut ffi::sqlite3_file,
    flags: c_int,
    out_flags: *mut c_int,
) -> c_int {
    (*file).pMethods = ptr::null();
    if (file as usize) % std::mem::align_of::<File>() != 0 || inner.is_poisoned() {
        return ffi::SQLITE_CANTOPEN;
    }
    let path: Option<&[u8]> = if name.is_null() {
        None
    } else {
        Some(CStr::from_ptr(name).to_bytes())
    };
    let kind = if flags & ffi::SQLITE_OPEN_MAIN_DB != 0 {
        if !path.is_some_and(|p| inner.matches_main(p)) {
            return ffi::SQLITE_CANTOPEN; // another durable database (ATTACH)
        }
        // Exactly one admitted MAIN connection: only open_connection's own thread, once.
        let mut permit = inner.permit.lock().unwrap_or_else(|e| e.into_inner());
        if *permit != Some(std::thread::current().id())
            || inner.main_open.load(Ordering::SeqCst) != 0
        {
            return ffi::SQLITE_CANTOPEN;
        }
        *permit = None;
        Kind::Main
    } else if flags & ffi::SQLITE_OPEN_WAL != 0 {
        if !path.is_some_and(|p| inner.matches_wal(p)) {
            return ffi::SQLITE_CANTOPEN;
        }
        Kind::Wal
    } else if flags & (ffi::SQLITE_OPEN_MAIN_JOURNAL | ffi::SQLITE_OPEN_SUPER_JOURNAL) != 0 {
        return ffi::SQLITE_CANTOPEN; // a rollback journal would bypass the WAL capture
    } else {
        Kind::Other
    };
    let base = base_of(inner);
    let Some(base_open) = base.xOpen else {
        return ffi::SQLITE_CANTOPEN;
    };
    let real = (file as *mut u8).add(file_storage_offset()) as *mut ffi::sqlite3_file;
    ptr::write_bytes(real as *mut u8, 0, base.szOsFile as usize);
    let rc = base_open(inner.base, name, real, flags, out_flags);
    if rc != ffi::SQLITE_OK {
        return rc;
    }
    let admitted = !(*real).pMethods.is_null()
        && match kind {
            Kind::Main => admit_main(inner, real),
            Kind::Wal => admit_wal(inner, real),
            Kind::Other => true,
        };
    if !admitted {
        if !(*real).pMethods.is_null() {
            r_close(real);
        }
        return ffi::SQLITE_CANTOPEN;
    }
    // Hold one strong count on the shared state for as long as this file is open.
    Arc::increment_strong_count(inner as *const Inner);
    ptr::write(
        file as *mut File,
        File {
            base: ffi::sqlite3_file { pMethods: &METHODS },
            kind,
            inner: inner as *const Inner,
            real,
        },
    );
    if kind == Kind::Main {
        inner.main_open.store(file as usize, Ordering::SeqCst);
    }
    ffi::SQLITE_OK
}

/// The armed database must exist in WAL format and offer shared memory.
unsafe fn admit_main(inner: &Inner, real: *mut ffi::sqlite3_file) -> bool {
    let methods = &*(*real).pMethods;
    if methods.iVersion < 2
        || methods.xShmMap.is_none()
        || methods.xShmLock.is_none()
        || methods.xShmBarrier.is_none()
        || methods.xShmUnmap.is_none()
    {
        return false;
    }
    let mut size = 0i64;
    let mut header = [0u8; 100];
    if r_size(real, &mut size) != ffi::SQLITE_OK
        || size < 100
        || r_read(real, header.as_mut_ptr() as *mut c_void, 100, 0) != ffi::SQLITE_OK
    {
        return false;
    }
    let page_size = match u32::from(u16::from_be_bytes([header[16], header[17]])) {
        1 => 65_536,
        other => other,
    };
    if &header[..16] != b"SQLite format 3\0"
        || header[18] != 2
        || header[19] != 2
        || !valid_page_size(page_size)
    {
        return false;
    }
    let mut state = inner.lock();
    if state.page_size == 0 {
        state.page_size = page_size;
    }
    state.page_size == page_size
}

/// The WAL file must start empty and not be a leftover from a crash (it would hold frames this
/// wrapper never saw). Missing POWERSAFE_OVERWRITE is no longer a reason to refuse: SQLite pads
/// such commits and the wrapper verifies exactly that padding (see the module docs).
unsafe fn admit_wal(inner: &Inner, real: *mut ffi::sqlite3_file) -> bool {
    let mut size = 0i64;
    if r_size(real, &mut size) != ffi::SQLITE_OK {
        return false;
    }
    let mut state = inner.lock();
    if !state.wal_seen {
        if size != 0 {
            return false;
        }
        state.wal_seen = true;
    }
    true
}

static METHODS: ffi::sqlite3_io_methods = ffi::sqlite3_io_methods {
    iVersion: 3,
    xClose: Some(x_close),
    xRead: Some(x_read),
    xWrite: Some(x_write),
    xTruncate: Some(x_truncate),
    xSync: Some(x_sync),
    xFileSize: Some(x_file_size),
    xLock: Some(x_lock),
    xUnlock: Some(x_unlock),
    xCheckReservedLock: Some(x_check_reserved_lock),
    xFileControl: Some(x_file_control),
    xSectorSize: Some(x_sector_size),
    xDeviceCharacteristics: Some(x_device_characteristics),
    xShmMap: Some(x_shm_map),
    xShmLock: Some(x_shm_lock),
    xShmBarrier: Some(x_shm_barrier),
    xShmUnmap: Some(x_shm_unmap),
    xFetch: Some(x_fetch),
    xUnfetch: Some(x_unfetch),
};

unsafe extern "C" fn x_close(file: *mut ffi::sqlite3_file) -> c_int {
    let f = file_of(file);
    let inner_ptr = f.inner;
    let rc = {
        let inner = &*inner_ptr;
        guarded(inner, ffi::SQLITE_IOERR, || {
            if f.kind == Kind::Main {
                // An approved commit that never got its receipt must not vanish silently.
                if inner.lock().pending_receipt.is_some() { inner.poison(); }
            }
            if f.kind == Kind::Wal {
                let me = f as *const File as usize;
                let mut state = inner.lock();
                // A completed commit that was never synced, or a padding frame that SQLite never
                // finished, must not be dropped silently.
                let unfinished = (state.owner == me && state.commit_mark.is_some())
                    || state
                        .pending_pad
                        .as_ref()
                        .is_some_and(|pad| pad.owner == me);
                if unfinished {
                    drop(state);
                    inner.poison();
                } else if state.owner == me {
                    state.discard_tail();
                }
            }
            let rc = r_close(f.real);
            // Keep admission reserved until the underlying MAIN file has actually
            // closed; another open must not race the last part of its close.
            if f.kind == Kind::Main
                && inner.main_open.load(Ordering::SeqCst) == f as *const File as usize
            {
                inner.owned_db.store(0, Ordering::SeqCst);
                inner.main_open.store(0, Ordering::SeqCst);
            }
            rc
        })
    };
    // Balance the count taken at open; `inner` may be freed here and is not used again.
    Arc::decrement_strong_count(inner_ptr);
    rc
}

unsafe extern "C" fn x_read(
    file: *mut ffi::sqlite3_file,
    buf: *mut c_void,
    amt: c_int,
    at: i64,
) -> c_int {
    let f = file_of(file);
    let inner = &*f.inner;
    guarded(inner, ffi::SQLITE_IOERR_READ, || match f.kind {
        Kind::Wal => wal_read(inner, f, buf, amt, at),
        _ => r_read(f.real, buf, amt, at),
    })
}

unsafe fn wal_read(inner: &Inner, f: &File, buf: *mut c_void, amt: c_int, at: i64) -> c_int {
    if amt < 0 || at < 0 {
        return ffi::SQLITE_IOERR_READ;
    }
    let (amount, start) = (amt as usize, at as u64);
    let mut real_size = 0i64;
    let rc = r_size(f.real, &mut real_size);
    if rc != ffi::SQLITE_OK {
        return rc;
    }
    let real_size = real_size.max(0) as u64;
    let from_file = if start < real_size {
        (amount as u64).min(real_size - start) as usize
    } else {
        0
    };
    if from_file > 0 {
        let rc = r_read(f.real, buf, from_file as c_int, at);
        if rc != ffi::SQLITE_OK {
            return rc;
        }
    }
    // The raw buffer is only viewed as a slice once the real read has finished.
    let out = std::slice::from_raw_parts_mut(buf as *mut u8, amount);
    out[from_file..].fill(0);
    let me = f as *const File as usize;
    let state = inner.lock();
    let mut logical = real_size;
    if state.owner == me && !state.tail.is_empty() {
        let base = state.committed_end;
        let tail_end = base + state.tail.len() as u64;
        logical = logical.max(tail_end);
        let a = start.max(base);
        let b = (start + amount as u64).min(tail_end);
        if a < b {
            out[(a - start) as usize..(b - start) as usize]
                .copy_from_slice(&state.tail[(a - base) as usize..(b - base) as usize]);
        }
    }
    if start + amount as u64 > logical {
        ffi::SQLITE_IOERR_SHORT_READ
    } else {
        ffi::SQLITE_OK
    }
}

unsafe extern "C" fn x_write(
    file: *mut ffi::sqlite3_file,
    data: *const c_void,
    amt: c_int,
    at: i64,
) -> c_int {
    let f = file_of(file);
    let inner = &*f.inner;
    guarded(inner, ffi::SQLITE_IOERR_WRITE, || {
        if amt < 0 || at < 0 || (data.is_null() && amt > 0) {
            return ffi::SQLITE_IOERR_WRITE;
        }
        let bytes: &[u8] = if amt == 0 {
            &[]
        } else {
            std::slice::from_raw_parts(data as *const u8, amt as usize)
        };
        if inner.is_poisoned() && f.kind != Kind::Other {
            return ffi::SQLITE_IOERR_WRITE;
        }
        match f.kind {
            Kind::Wal => wal_write(inner, f, bytes, at as u64),
            Kind::Main => main_write(inner, f, bytes, at as u64),
            Kind::Other => r_write(f.real, bytes, at as u64),
        }
    })
}

/// Holds the bytes in memory. Nothing of an uncommitted tail reaches the real file.
unsafe fn wal_write(inner: &Inner, f: &File, bytes: &[u8], at: u64) -> c_int {
    let me = f as *const File as usize;
    let mut state = inner.lock();
    // The rest of an approved padding frame: only exactly the expected bytes, written through.
    if state.pending_pad.is_some() {
        let accepted = state
            .pending_pad
            .as_mut()
            .is_some_and(|pad| pad.accept(me, at, bytes));
        if !accepted {
            drop(state);
            inner.poison();
            return ffi::SQLITE_IOERR_WRITE;
        }
        if state.pending_pad.as_ref().is_some_and(PendingPad::finished) {
            let after = state.pending_pad.take().map(|pad| pad.cksum_after);
            let size = state
                .generation
                .as_ref()
                .map(|g| frame_size(g.page_size) as u64);
            let (Some(after), Some(size)) = (after, size) else {
                drop(state);
                inner.poison();
                return ffi::SQLITE_IOERR_WRITE;
            };
            state.committed_end += size;
            state.last_cksum = after;
        }
        drop(state);
        let rc = r_write(f.real, bytes, at);
        if rc != ffi::SQLITE_OK {
            inner.poison();
        }
        return rc;
    }
    // A write at offset 0 after commits is a WAL restart: only valid with a fresh header whose
    // salts differ from the current generation's (SQLite restarts only when fully checkpointed).
    if at == 0 && state.committed_end > 0 {
        let restart_ok = bytes.len() == WAL_HEADER
            && parse_wal_header(bytes).is_ok_and(|(g, _)| {
                state
                    .generation
                    .as_ref()
                    .is_some_and(|old| old.salt != g.salt)
            });
        if !restart_ok {
            drop(state);
            inner.poison();
            return ffi::SQLITE_IOERR_WRITE;
        }
        state.reset_generation();
    }
    if state.owner != 0 && state.owner != me {
        // The previous writer ended without committing (SQLite serialises writers): drop its tail.
        if at != state.committed_end {
            drop(state);
            inner.poison();
            return ffi::SQLITE_IOERR_WRITE;
        }
        state.discard_tail();
    }
    let base = state.committed_end;
    if at < base {
        drop(state);
        inner.poison();
        return ffi::SQLITE_IOERR_WRITE; // would rewrite committed WAL bytes
    }
    let relative = (at - base) as usize;
    if relative > state.tail.len() {
        drop(state);
        inner.poison();
        return ffi::SQLITE_IOERR_WRITE; // a gap SQLite never leaves
    }
    // After a completed commit frame only exact repeats of it may follow, and only on a device
    // where SQLite pads. Anything else means the commit was never synced (synchronous < FULL
    // slipped through) or the layout is not what the wrapper verifies: refuse.
    if relative == state.tail.len() && !bytes.is_empty() {
        if let Some(mark) = state.commit_mark {
            let size = FRAME_HEADER + mark.page_len;
            let allowed = state.padding_expected() == Some(true)
                && repeats_commit_frame(&state.tail, &mark, size, relative, bytes);
            if !allowed {
                drop(state);
                inner.poison();
                return ffi::SQLITE_IOERR_WRITE;
            }
        }
    }
    let end = relative + bytes.len();
    if end > inner.config.max_staged_bytes {
        state.discard_tail(); // quota: this transaction fails, the wrapper stays usable
        return ffi::SQLITE_IOERR_WRITE;
    }
    if end > state.tail.len() {
        state.tail.resize(end, 0);
    }
    state.tail[relative..end].copy_from_slice(bytes);
    state.owner = me;
    // A write that completes a commit frame ends the transaction: bytes of an earlier, rolled
    // back attempt that lie beyond it are stale and must not be mistaken for part of it.
    if let Some((from, size)) = frame_layout(&state) {
        if end >= from + size && (end - from) % size == 0 && be32(&state.tail, end - size + 4) != 0
        {
            // Repeats (padding) after the first commit frame are verified at the sync; a new or
            // smaller commit frame replaces a stale mark and drops the stale bytes behind it.
            let replace = state.commit_mark.is_none_or(|mark| end <= mark.end);
            if replace {
                let start = end - size;
                let mut head = [0u8; 16];
                head.copy_from_slice(&state.tail[start..start + 16]);
                state.commit_mark = Some(CommitMark {
                    end,
                    head,
                    page_len: size - FRAME_HEADER,
                });
                state.tail.truncate(end);
            }
        }
    }
    ffi::SQLITE_OK
}

/// Do `bytes`, appended at tail offset `at` after the first commit frame, reproduce the repeat
/// of that frame (same page number, commit size, salts and page bytes)? The checksum bytes are
/// verified at the sync, when the whole chain is known.
fn repeats_commit_frame(
    tail: &[u8],
    mark: &CommitMark,
    size: usize,
    at: usize,
    bytes: &[u8],
) -> bool {
    let page = &tail[mark.end - mark.page_len..mark.end];
    bytes
        .iter()
        .enumerate()
        .all(|(i, byte)| match (at + i - mark.end) % size {
            n @ 0..=15 => *byte == mark.head[n],
            16..=23 => true,
            n => *byte == page[n - FRAME_HEADER],
        })
}

/// Where the frames start in the held tail, and the frame size, once the page size is known.
fn frame_layout(state: &DbState) -> Option<(usize, usize)> {
    if state.committed_end > 0 {
        return state
            .generation
            .as_ref()
            .map(|g| (0, frame_size(g.page_size)));
    }
    if state.tail.len() >= WAL_HEADER {
        let page_size = be32(&state.tail, 8);
        if valid_page_size(page_size) {
            return Some((WAL_HEADER, frame_size(page_size)));
        }
    }
    None
}

/// Only ordinary checkpoint work is forwarded: whole pages that equal a committed frame.
unsafe fn main_write(inner: &Inner, f: &File, bytes: &[u8], at: u64) -> c_int {
    let state = inner.lock();
    let page = state.page_size as u64;
    let ok = page != 0
        && bytes.len() as u64 == page
        && at % page == 0
        && (at > 0 || (bytes.len() >= 20 && bytes[18] == 2 && bytes[19] == 2))
        && state
            .committed_frames
            .contains(&((at / page + 1) as u32, digest(bytes)));
    drop(state);
    if !ok {
        inner.poison();
        return ffi::SQLITE_IOERR_WRITE;
    }
    let rc = r_write(f.real, bytes, at);
    if rc == ffi::SQLITE_OK {
        inner.checkpointed_pages.fetch_add(1, Ordering::SeqCst);
    }
    rc
}

unsafe extern "C" fn x_truncate(file: *mut ffi::sqlite3_file, size: i64) -> c_int {
    let f = file_of(file);
    let inner = &*f.inner;
    guarded(inner, ffi::SQLITE_IOERR_TRUNCATE, || {
        let mut state = inner.lock();
        match f.kind {
            Kind::Wal => {
                if size < 0
                    || !state.tail.is_empty()
                    || state.pending_pad.is_some()
                    || inner.is_poisoned()
                {
                    drop(state);
                    inner.poison();
                    return ffi::SQLITE_IOERR_TRUNCATE;
                }
                if size == 0 {
                    state.reset_generation(); // a fully checkpointed WAL was emptied
                } else if (size as u64) < state.committed_end {
                    drop(state);
                    inner.poison();
                    return ffi::SQLITE_IOERR_TRUNCATE;
                }
                drop(state);
                r_truncate(f.real, size)
            }
            Kind::Main => {
                let expected = u64::from(state.last_db_pages) * u64::from(state.page_size);
                if state.last_db_pages == 0 || size < 0 || size as u64 != expected {
                    drop(state);
                    inner.poison();
                    return ffi::SQLITE_IOERR_TRUNCATE;
                }
                drop(state);
                r_truncate(f.real, size)
            }
            Kind::Other => {
                drop(state);
                r_truncate(f.real, size)
            }
        }
    })
}

unsafe extern "C" fn x_sync(file: *mut ffi::sqlite3_file, flags: c_int) -> c_int {
    let f = file_of(file);
    let inner = &*f.inner;
    guarded(inner, ffi::SQLITE_IOERR_FSYNC, || match f.kind {
        Kind::Wal => wal_sync(inner, f, flags),
        _ => r_sync(f.real, flags),
    })
}

/// The commit point: verify the held tail, ask the sink, then (and only then) write and sync.
unsafe fn wal_sync(inner: &Inner, f: &File, flags: c_int) -> c_int {
    let me = f as *const File as usize;
    let sector = sector_size(f.real);
    let mut state = inner.lock();
    if inner.is_poisoned() {
        return ffi::SQLITE_IOERR_FSYNC;
    }
    if state.pending_pad.is_some() {
        // A synced padding frame is still being completed: no other sync is expected.
        drop(state);
        inner.poison();
        return ffi::SQLITE_IOERR_FSYNC;
    }
    let info = match plan_sync(&state, state.owner == me, sector) {
        SyncPlan::Forward => {
            drop(state);
            return r_sync(f.real, flags);
        }
        // Nothing is durable yet: the header is written and synced together with the commit that
        // follows it, and only that commit is authority.
        SyncPlan::HeaderOnly => return ffi::SQLITE_OK,
        SyncPlan::Refuse => {
            drop(state);
            inner.poison();
            return ffi::SQLITE_IOERR_FSYNC;
        }
        SyncPlan::Commit(info) => info,
    };
    if state.pending_receipt.is_some() {
        // The previous approved commit never got its wal-hook receipt (the hook was replaced or
        // did not run): refuse this commit and everything after it.
        drop(state);
        inner.poison();
        return ffi::SQLITE_IOERR_FSYNC;
    }
    if state.committed_frames.len() + info.frames as usize > inner.config.max_tracked_frames {
        state.discard_tail(); // checkpoint and restart the WAL before more commits
        return ffi::SQLITE_IOERR_FSYNC;
    }
    let base = state.committed_end;
    let raw = std::mem::take(&mut state.tail);
    state.commit_mark = None;
    let sequence = inner.sequence.load(Ordering::SeqCst) + 1;
    let transaction = WalTransaction {
        sequence,
        generation: info.generation.clone(),
        first_frame: info.first_frame,
        db_pages: info.db_pages,
        frames: info.frames,
        raw,
        frames_at: info.frames_at,
        base,
    };
    drop(state);
    // No wrapper lock is held while the sink decides.
    let verdict = catch_unwind(AssertUnwindSafe(|| inner.sink.approve(&transaction)));
    let mut state = inner.lock();
    state.owner = 0;
    match verdict {
        Ok(Ok(())) if !inner.is_poisoned() => {}
        Ok(_) => return ffi::SQLITE_IOERR_FSYNC, // veto, over quota, or poisoned meanwhile
        Err(_) => {
            drop(state);
            inner.poison();
            return ffi::SQLITE_IOERR_FSYNC;
        }
    }
    let mut rc = r_write(f.real, &transaction.raw, base);
    if rc == ffi::SQLITE_OK {
        rc = r_sync(f.real, flags);
    }
    if rc != ffi::SQLITE_OK {
        // Written but not known durable: the outcome is uncertain, so nothing more is accepted.
        drop(state);
        inner.poison();
        return ffi::SQLITE_IOERR_FSYNC;
    }
    if base == 0 {
        state.generation = Some(info.generation.clone());
    }
    // The committed end is the end of the last COMPLETE frame (real frames and complete repeats);
    // the bytes of a partial repeat were synced too and are completed by `PendingPad`.
    let size = frame_size(info.generation.page_size) as u64;
    state.committed_end =
        base + info.frames_at as u64 + (u64::from(info.frames) + u64::from(info.repeats)) * size;
    state.last_cksum = info.last_cksum;
    state.last_db_pages = info.db_pages;
    if info.partial > 0 {
        state.pending_pad = Some(PendingPad {
            owner: me,
            next_at: base + transaction.raw.len() as u64,
            rest: info.rest.clone(),
            cksum_after: info.cksum_after,
        });
    }
    state.committed_frames.extend(info.digests);
    // The commit is approved and written; SQLite now publishes it. Its local receipt is pending
    // until the wal hook reports the real commit, bound to this exact final position.
    let final_end = state.committed_end + if info.partial > 0 { size } else { 0 };
    state.pending_receipt = Some(LocalCommit {
        sequence,
        generation: info.generation.clone(),
        first_frame: info.first_frame,
        real_frames: info.frames,
        wal_frames: u32::try_from((final_end - WAL_HEADER as u64) / size).unwrap_or(u32::MAX),
        wal_end: final_end,
        db_pages: info.db_pages,
    });
    inner.sequence.store(sequence, Ordering::SeqCst);
    ffi::SQLITE_OK
}

unsafe extern "C" fn x_file_size(file: *mut ffi::sqlite3_file, out: *mut i64) -> c_int {
    let f = file_of(file);
    let inner = &*f.inner;
    guarded(inner, ffi::SQLITE_IOERR_READ, || {
        let mut size = 0i64;
        let rc = r_size(f.real, &mut size);
        if rc != ffi::SQLITE_OK {
            return rc;
        }
        if f.kind == Kind::Wal {
            let state = inner.lock();
            if state.owner == f as *const File as usize && !state.tail.is_empty() {
                size = size.max((state.committed_end + state.tail.len() as u64) as i64);
            }
        }
        *out = size;
        ffi::SQLITE_OK
    })
}

macro_rules! forward_file {
    ($name:ident, $field:ident, $fallback:expr, ($($arg:ident : $ty:ty),*)) => {
        unsafe extern "C" fn $name(file: *mut ffi::sqlite3_file $(, $arg: $ty)*) -> c_int {
            let f = file_of(file);
            let inner = &*f.inner;
            guarded(inner, $fallback, || match (*(*f.real).pMethods).$field {
                Some(call) => call(f.real $(, $arg)*),
                None => $fallback,
            })
        }
    };
}
forward_file!(x_lock, xLock, ffi::SQLITE_IOERR_LOCK, (level: c_int));
forward_file!(x_unlock, xUnlock, ffi::SQLITE_IOERR_UNLOCK, (level: c_int));
forward_file!(
    x_check_reserved_lock,
    xCheckReservedLock,
    ffi::SQLITE_IOERR_CHECKRESERVEDLOCK,
    (out: *mut c_int)
);
forward_file!(
    x_file_control,
    xFileControl,
    ffi::SQLITE_NOTFOUND,
    (op: c_int, arg: *mut c_void)
);
forward_file!(
    x_shm_map,
    xShmMap,
    ffi::SQLITE_IOERR_SHMMAP,
    (page: c_int, size: c_int, extend: c_int, out: *mut *mut c_void)
);
forward_file!(
    x_shm_lock,
    xShmLock,
    ffi::SQLITE_IOERR_SHMLOCK,
    (offset: c_int, n: c_int, flags: c_int)
);
forward_file!(x_shm_unmap, xShmUnmap, ffi::SQLITE_OK, (delete: c_int));

unsafe extern "C" fn x_shm_barrier(file: *mut ffi::sqlite3_file) {
    let f = file_of(file);
    let inner = &*f.inner;
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if let Some(call) = (*(*f.real).pMethods).xShmBarrier {
            call(f.real);
        }
    }))
    .map_err(|_| inner.poison());
}
unsafe extern "C" fn x_sector_size(file: *mut ffi::sqlite3_file) -> c_int {
    let f = file_of(file);
    let inner = &*f.inner;
    guarded(inner, 4096, || match (*(*f.real).pMethods).xSectorSize {
        Some(call) => call(f.real),
        None => 4096,
    })
}
unsafe extern "C" fn x_device_characteristics(file: *mut ffi::sqlite3_file) -> c_int {
    let f = file_of(file);
    let inner = &*f.inner;
    guarded(inner, 0, || {
        let value = match (*(*f.real).pMethods).xDeviceCharacteristics {
            Some(call) => call(f.real),
            None => 0,
        };
        if f.kind == Kind::Main {
            // Exactly what SQLite is told about the main file, which decides its padding.
            inner.lock().main_characteristics = Some(value);
        }
        value
    })
}
/// Memory mapping is never offered: every read and write goes through the wrapper.
unsafe extern "C" fn x_fetch(
    _file: *mut ffi::sqlite3_file,
    _at: i64,
    _amt: c_int,
    out: *mut *mut c_void,
) -> c_int {
    if !out.is_null() {
        *out = ptr::null_mut();
    }
    ffi::SQLITE_OK
}
unsafe extern "C" fn x_unfetch(
    _file: *mut ffi::sqlite3_file,
    _at: i64,
    _page: *mut c_void,
) -> c_int {
    ffi::SQLITE_OK
}

// ---- VFS-level forwarding ----------------------------------------------------------------------------

unsafe extern "C" fn x_delete(
    vfs: *mut ffi::sqlite3_vfs,
    name: *const c_char,
    sync_dir: c_int,
) -> c_int {
    let inner = inner_of(vfs);
    guarded(inner, ffi::SQLITE_IOERR_DELETE, || {
        if !name.is_null() && inner.matches_wal(CStr::from_ptr(name).to_bytes()) {
            // SQLite deletes the WAL only after the last connection checkpointed and closed it.
            let mut state = inner.lock();
            state.reset_generation();
            state.wal_seen = false;
        }
        match base_of(inner).xDelete {
            Some(call) => call(inner.base, name, sync_dir),
            None => ffi::SQLITE_IOERR_DELETE,
        }
    })
}
unsafe extern "C" fn x_access(
    vfs: *mut ffi::sqlite3_vfs,
    name: *const c_char,
    flags: c_int,
    out: *mut c_int,
) -> c_int {
    let inner = inner_of(vfs);
    guarded(inner, ffi::SQLITE_IOERR_ACCESS, || {
        match base_of(inner).xAccess {
            Some(call) => call(inner.base, name, flags, out),
            None => ffi::SQLITE_IOERR_ACCESS,
        }
    })
}
unsafe extern "C" fn x_full_pathname(
    vfs: *mut ffi::sqlite3_vfs,
    name: *const c_char,
    n: c_int,
    out: *mut c_char,
) -> c_int {
    let inner = inner_of(vfs);
    guarded(inner, ffi::SQLITE_CANTOPEN, || {
        match base_of(inner).xFullPathname {
            Some(call) => call(inner.base, name, n, out),
            None => ffi::SQLITE_CANTOPEN,
        }
    })
}
unsafe extern "C" fn x_dl_open(vfs: *mut ffi::sqlite3_vfs, name: *const c_char) -> *mut c_void {
    let inner = inner_of(vfs);
    catch_unwind(AssertUnwindSafe(|| match base_of(inner).xDlOpen {
        Some(call) => call(inner.base, name),
        None => ptr::null_mut(),
    }))
    .unwrap_or(ptr::null_mut())
}
unsafe extern "C" fn x_dl_error(vfs: *mut ffi::sqlite3_vfs, n: c_int, out: *mut c_char) {
    let inner = inner_of(vfs);
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if let Some(call) = base_of(inner).xDlError {
            call(inner.base, n, out);
        }
    }));
}
unsafe extern "C" fn x_dl_sym(
    vfs: *mut ffi::sqlite3_vfs,
    handle: *mut c_void,
    symbol: *const c_char,
) -> Option<unsafe extern "C" fn(*mut ffi::sqlite3_vfs, *mut c_void, *const c_char)> {
    let inner = inner_of(vfs);
    catch_unwind(AssertUnwindSafe(|| match base_of(inner).xDlSym {
        Some(call) => call(inner.base, handle, symbol),
        None => None,
    }))
    .unwrap_or(None)
}
unsafe extern "C" fn x_dl_close(vfs: *mut ffi::sqlite3_vfs, handle: *mut c_void) {
    let inner = inner_of(vfs);
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if let Some(call) = base_of(inner).xDlClose {
            call(inner.base, handle);
        }
    }));
}
unsafe extern "C" fn x_randomness(vfs: *mut ffi::sqlite3_vfs, n: c_int, out: *mut c_char) -> c_int {
    let inner = inner_of(vfs);
    guarded(inner, 0, || match base_of(inner).xRandomness {
        Some(call) => call(inner.base, n, out),
        None => 0,
    })
}
unsafe extern "C" fn x_sleep(vfs: *mut ffi::sqlite3_vfs, micros: c_int) -> c_int {
    let inner = inner_of(vfs);
    guarded(inner, 0, || match base_of(inner).xSleep {
        Some(call) => call(inner.base, micros),
        None => 0,
    })
}
unsafe extern "C" fn x_current_time(vfs: *mut ffi::sqlite3_vfs, out: *mut f64) -> c_int {
    let inner = inner_of(vfs);
    guarded(inner, 1, || match base_of(inner).xCurrentTime {
        Some(call) => call(inner.base, out),
        None => 1,
    })
}
unsafe extern "C" fn x_get_last_error(
    vfs: *mut ffi::sqlite3_vfs,
    n: c_int,
    out: *mut c_char,
) -> c_int {
    let inner = inner_of(vfs);
    guarded(inner, 0, || match base_of(inner).xGetLastError {
        Some(call) => call(inner.base, n, out),
        None => 0,
    })
}
unsafe extern "C" fn x_current_time_int64(vfs: *mut ffi::sqlite3_vfs, out: *mut i64) -> c_int {
    let inner = inner_of(vfs);
    guarded(inner, 1, || match base_of(inner).xCurrentTimeInt64 {
        Some(call) => call(inner.base, out),
        None => 1,
    })
}
unsafe extern "C" fn x_set_system_call(
    vfs: *mut ffi::sqlite3_vfs,
    name: *const c_char,
    call: ffi::sqlite3_syscall_ptr,
) -> c_int {
    let inner = inner_of(vfs);
    guarded(inner, ffi::SQLITE_NOTFOUND, || {
        match base_of(inner).xSetSystemCall {
            Some(forward) => forward(inner.base, name, call),
            None => ffi::SQLITE_NOTFOUND,
        }
    })
}
unsafe extern "C" fn x_get_system_call(
    vfs: *mut ffi::sqlite3_vfs,
    name: *const c_char,
) -> ffi::sqlite3_syscall_ptr {
    let inner = inner_of(vfs);
    catch_unwind(AssertUnwindSafe(|| match base_of(inner).xGetSystemCall {
        Some(call) => call(inner.base, name),
        None => None,
    }))
    .unwrap_or(None)
}
unsafe extern "C" fn x_next_system_call(
    vfs: *mut ffi::sqlite3_vfs,
    name: *const c_char,
) -> *const c_char {
    let inner = inner_of(vfs);
    catch_unwind(AssertUnwindSafe(|| match base_of(inner).xNextSystemCall {
        Some(call) => call(inner.base, name),
        None => ptr::null(),
    }))
    .unwrap_or(ptr::null())
}

#[cfg(test)]
mod tests;
