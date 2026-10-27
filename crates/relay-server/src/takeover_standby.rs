//! M4 slice S3: an OWNED, OFFLINE standby database image that is brought forward by complete
//! admitted WAL transactions, one durable step at a time. It is local storage only: nothing in
//! this module serves requests, opens the image for the running server, enables an `Ability`,
//! decides a quorum, publishes, promotes or touches the network. The retained signed log, the
//! quorum and the trusted tips come from the caller; this helper never infers authority from a
//! hash.
//!
//! Layout. One private root folder holds exactly the fixed image `standby-image.sqlite3` and a
//! control sub-folder `standby-control` that contains the existing [`TakeoverStore`] (exclusive
//! ownership for the life of the [`Standby`], one metadata blob committed with
//! `synchronous=FULL`). No second locking or fsync library exists here. The control record is the
//! only authority about the image: `{context, page_size, applied tip, applied page count,
//! optional pending}` plus a domain-separated check digest.
//!
//! Lifecycle (the caller's proof obligations are in capitals).
//! * [`Standby::create_new`]: only after the CALLER has verified a COMPLETE P0 and placed the
//!   backup-derived seed image at the fixed path. The image is inspected, never copied, moved,
//!   replaced or deleted; an existing control store is `AlreadyExists`. The tip it is created at
//!   (the snapshot/applied tip) is the CALLER'S verified statement about that image.
//! * [`Standby::open_existing`]: never initializes, repairs, converts or regenerates anything. A
//!   missing, foreign, linked, redirected or corrupt image or control store, an unexpected
//!   `-wal`/`-shm`/`-journal` file, and any set/member/context mismatch are refused with the
//!   files left as found. The image is NOT opened in SQLite here.
//! * [`Standby::apply`]: ONE complete transaction from the retained log, with the trusted
//!   `parent` (it must be exactly the applied tip) and `end` tips. THE CALLER MUST HAVE DECODED
//!   THE TRANSACTION FROM A QUORUM-COMMITTED ENTRY AT `end`. Everything is validated before any
//!   byte moves; then the order is: (1) the control record gets `Pending{parent, end, SHA-256 of
//!   the canonical transaction, db_pages, frames}` and is committed durably; (2) the full changed
//!   pages are written, the file is set to the final size and synced; (3) the control record
//!   advances `applied` to `end` and clears `pending` in one durable commit. The retained log
//!   entry, not the digest, is the redo source.
//! * Interruption. Anything that fails or is cut off after step 1 leaves `Pending` in place. A
//!   live instance and a re-opened one then refuse `apply` and `verify` ([`StandbyError::
//!   RecoveryNeeded`]) and expose [`Standby::recovery_needed`]; the image is never opened in
//!   SQLite while pending. [`Standby::recover`] resumes ONLY with the exact same transaction,
//!   parent and end (digest compared); it rewrites ALL the pages (idempotent: full page images
//!   plus the final size) and finishes the marker. A wrong payload or tip is
//!   [`StandbyError::WrongRecovery`] and clears nothing. There is no abort and no fallback to
//!   the stale applied marker. If step 3 is the one that failed or is unknown, the instance is
//!   poisoned; re-opening finds either `Pending` (redo) or the new applied tip (done).
//! * [`Standby::verify`]: an explicit, once-only, final check before a possible promotion. It
//!   needs no pending work and EXACTLY the requested applied tip, and then runs SQLite's
//!   `quick_check` plus page-size/page-count agreement on a private scratch copy of the image
//!   (see below). It is NOT writer/election authority and returns only the bounded
//!   [`Verified`] statement.
//!
//! Page semantics. A transaction is the ordered list of complete `(pgno, page)` images plus the
//! final database page count, as SQLite wrote them. Pages with `pgno > db_pages` are obsolete
//! spilled pages: they take part in the digest but are neither written nor allowed to grow the
//! file, exactly as a checkpoint would drop them. A repeated page number is applied in order
//! (last wins). `pgno` must be in `1..=0x7fff_ffff`, the page length must be the standby's page
//! size, `frames <= Limits::max_frames` and `db_pages * page_size <= Limits::max_image_bytes`,
//! so a malformed count can never create a sparse multi-terabyte image. The transaction digest
//! covers the page size, `db_pages`, `frames` and every `(pgno, page)` in order; the capture
//! sequence and WAL frame position are deliberately excluded because they are local to one
//! source registration.
//!
//! Cost. A step costs one small durable control commit before and one after, the changed pages
//! and one image sync. The image is never cloned, hashed or scanned per message; the only full
//! read is the explicit `verify`.
//!
//! Why `verify` copies. A snapshot taken from a WAL-mode source keeps the WAL format bytes
//! (header offsets 18 and 19 equal 2). SQLite cannot open such a file with `immutable`/
//! `nolock` (it needs a lock manager for the WAL index, `sqlite3PagerWalSupported`), and a
//! normal read-only open of it would create `-wal`/`-shm` files beside the image. So `verify`
//! streams the image into its own scratch file in the private folder, sets only those two header
//! bytes to the rollback-journal value in the COPY, opens the copy read-only and removes it. The
//! image itself is only read with plain file I/O and never gets a side file. Cost: temporary
//! space of one image and one full read, once.
//!
//! Safety of the files. The root is prepared with `handover_target::private_directory`
//! (redirect refusal on every ancestor, private ACL / 0700). Every file is checked with
//! `symlink_metadata` first: regular, not a reparse point or symlink, (Unix) one link only.
//! `Debug` and errors contain no path, key, page, digest or payload.
//!
//! Not claimed. No power-loss or crash test was run; durability is exactly that of
//! `FlushFileBuffers`/`fsync` plus SQLite's rollback journal for the control file. Ownership of
//! the image is ownership of the control store: another process that writes the image by path
//! is outside this contract. Hard links on Windows are not detected. An interrupted
//! `create_new` can leave a control store without a record; `open_existing` refuses it and
//! nothing is repaired. [`crate::takeover_wal_vfs::apply_transaction`] is unchanged and is still
//! not crash-atomic; this module does not use it.
#![allow(dead_code)]

use crate::{
    handover_target,
    takeover_store::{OpenError, Quota, TakeoverStore},
    takeover_voter::LogStore,
    takeover_wal_vfs::AssembledTx,
};
use relay_protocol::{
    handover::{HostIdentity, HostProof},
    takeover::{canonical_digest, canonical_identity, AuthoritySet, Signed, Tip},
};
use rusqlite::{Connection, ErrorCode, OpenFlags};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

/// The only image file name inside the standby root.
pub(crate) const IMAGE_NAME: &str = "standby-image.sqlite3";
/// The control sub-folder (a [`TakeoverStore`] folder) inside the standby root.
pub(crate) const CONTROL_DIR: &str = "standby-control";
/// The scratch copy `verify` makes and removes.
pub(crate) const VERIFY_COPY: &str = "standby-verify.tmp";

const STATE_VERSION: u32 = 1;
const MAX_STATE_JSON: usize = 16 * 1024;
/// The highest page number SQLite can address.
const MAX_PGNO: u32 = 0x7fff_ffff;
/// The control store keeps no log entries; one entry of one byte is the smallest valid quota.
const CONTROL_QUOTA: Quota = Quota {
    max_entries: 1,
    max_entry_bytes: 1,
};

/// Storage bounds, fixed when the standby is opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Limits {
    /// The largest image (`db_pages * page_size`) that is ever created, extended or accepted.
    pub(crate) max_image_bytes: u64,
    /// The most frames (pages, obsolete ones included) one transaction may carry.
    pub(crate) max_frames: u32,
}
impl Limits {
    /// 16 GiB of image and the capture side's own 262 144-frame tracking bound.
    pub(crate) const DEFAULT: Limits = Limits {
        max_image_bytes: 16 << 30,
        max_frames: 262_144,
    };
    fn valid(&self) -> bool {
        (512..=1 << 40).contains(&self.max_image_bytes)
            && (1..=4_194_304).contains(&self.max_frames)
    }
}

/// Why a standby operation was refused or failed. Fixed text only: never a path, page, key or
/// digest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StandbyError {
    /// `create_new` found a control store already there; nothing was touched.
    AlreadyExists,
    /// No folder, image or control store; nothing was created.
    Missing,
    /// A redirected, linked, non-regular or foreign file, or an unexpected side file.
    Unsafe,
    /// A damaged image or control record, or a failed integrity check.
    Corrupt,
    /// Another set, member, page size or context than the one this standby was created for.
    Mismatch,
    /// Another owner holds the control store.
    Busy,
    /// An input that is out of its contract (limits, tips, set, member, transaction).
    Invalid(&'static str),
    /// A storage bound (`Limits`) would be exceeded; nothing was written.
    Quota,
    /// `parent` is not the exact applied tip.
    Parent,
    /// A transaction is pending; only `recover` with the exact transaction may continue.
    RecoveryNeeded,
    /// `recover` got another transaction or tips than the pending ones; nothing changed.
    WrongRecovery,
    /// `recover` was called with nothing pending.
    NothingPending,
    /// An earlier control-store failure; re-open to learn the durable outcome.
    Poisoned,
    /// Any other I/O or storage failure. After a pending record exists it is recoverable.
    Failed,
}
impl std::fmt::Display for StandbyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            StandbyError::AlreadyExists => "standby already exists",
            StandbyError::Missing => "standby is missing",
            StandbyError::Unsafe => "standby storage is unsafe",
            StandbyError::Corrupt => "standby is corrupt",
            StandbyError::Mismatch => "standby does not match its context",
            StandbyError::Busy => "standby is in use",
            StandbyError::Invalid(why) => return write!(f, "standby input refused: {why}"),
            StandbyError::Quota => "standby storage bound exceeded",
            StandbyError::Parent => "standby parent tip does not match",
            StandbyError::RecoveryNeeded => "standby needs recovery",
            StandbyError::WrongRecovery => "standby recovery input does not match",
            StandbyError::NothingPending => "standby has nothing to recover",
            StandbyError::Poisoned => "standby is poisoned",
            StandbyError::Failed => "standby storage failed",
        })
    }
}
impl std::error::Error for StandbyError {}

fn from_open(error: OpenError) -> StandbyError {
    match error {
        OpenError::AlreadyExists => StandbyError::AlreadyExists,
        OpenError::Missing => StandbyError::Missing,
        OpenError::Unsafe => StandbyError::Unsafe,
        OpenError::Corrupt => StandbyError::Corrupt,
        OpenError::Mismatch => StandbyError::Mismatch,
        OpenError::Busy => StandbyError::Busy,
        OpenError::Invalid => StandbyError::Invalid("control quota"),
        OpenError::Failed => StandbyError::Failed,
    }
}
fn from_sqlite(error: rusqlite::Error) -> StandbyError {
    match error {
        rusqlite::Error::SqliteFailure(code, _)
            if matches!(
                code.code,
                ErrorCode::NotADatabase | ErrorCode::DatabaseCorrupt
            ) =>
        {
            StandbyError::Corrupt
        }
        _ => StandbyError::Failed,
    }
}

// ---- the durable record ---------------------------------------------------------------------

/// The one transaction between `Pending` being committed and `applied` advancing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingRec {
    parent: Tip,
    end: Tip,
    tx_sha256: String,
    db_pages: u32,
    frames: u32,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    version: u32,
    context: String,
    page_size: u32,
    applied: Tip,
    /// The image size in pages at `applied` (meaningful while nothing is pending).
    pages: u32,
    pending: Option<PendingRec>,
}
#[derive(Serialize)]
struct StoredRef<'a> {
    state: &'a State,
    check: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Stored {
    state: State,
    check: String,
}

fn valid_page_size(size: u32) -> bool {
    size.is_power_of_two() && (512..=65_536).contains(&size)
}
impl State {
    fn sane(&self) -> Result<(), StandbyError> {
        let ok = self.version == STATE_VERSION
            && canonical_digest(&self.context)
            && valid_page_size(self.page_size)
            && (1..=MAX_PGNO).contains(&self.pages)
            && self.applied.validate().is_ok();
        if !ok {
            return Err(StandbyError::Corrupt);
        }
        if let Some(p) = &self.pending {
            let ok = p.parent.validate().is_ok()
                && p.end.validate().is_ok()
                && p.parent == self.applied
                && p.end.index > p.parent.index
                && p.end.term >= p.parent.term
                && canonical_digest(&p.tx_sha256)
                && (1..=MAX_PGNO).contains(&p.db_pages)
                && p.frames >= 1;
            if !ok {
                return Err(StandbyError::Corrupt);
            }
        }
        Ok(())
    }
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(DIGITS[usize::from(b >> 4)] as char);
        out.push(DIGITS[usize::from(b & 15)] as char);
    }
    out
}
fn state_check(json: &[u8]) -> String {
    let mut hash = Sha256::new();
    hash.update(b"relay-takeover-standby-state-v1\0");
    hash.update(json);
    hex(&hash.finalize())
}
fn encode(state: &State) -> Result<Vec<u8>, StandbyError> {
    let json = serde_json::to_vec(state).map_err(|_| StandbyError::Failed)?;
    serde_json::to_vec(&StoredRef {
        state,
        check: state_check(&json),
    })
    .map_err(|_| StandbyError::Failed)
}
fn decode(bytes: &[u8]) -> Result<State, StandbyError> {
    if bytes.len() > MAX_STATE_JSON {
        return Err(StandbyError::Corrupt);
    }
    let stored: Stored = serde_json::from_slice(bytes).map_err(|_| StandbyError::Corrupt)?;
    let json = serde_json::to_vec(&stored.state).map_err(|_| StandbyError::Corrupt)?;
    if stored.check != state_check(&json) {
        return Err(StandbyError::Corrupt);
    }
    stored.state.sane()?;
    Ok(stored.state)
}

/// The exact signed set, the exact member and the fixed image name, as one digest.
fn context_digest(set: &AuthoritySet, member: &HostIdentity) -> String {
    let mut hash = Sha256::new();
    hash.update(b"relay-takeover-standby-context-v1\0");
    for field in [
        set.body.message().as_slice(),
        set.signature.as_bytes(),
        member.host_id.as_bytes(),
        member.public_key.as_bytes(),
        IMAGE_NAME.as_bytes(),
    ] {
        hash.update((field.len() as u32).to_be_bytes());
        hash.update(field);
    }
    hex(&hash.finalize())
}
/// The set must be well formed, signed by its own leader, and the member one of its three hosts.
fn check_context(set: &AuthoritySet, member: &HostIdentity) -> Result<String, StandbyError> {
    set.validate()
        .map_err(|_| StandbyError::Invalid("authority set"))?;
    let signed = handover_target::verify(
        &HostProof {
            identity: set.signer().clone(),
            signature: set.signature().to_owned(),
        },
        &set.signing_message(),
    );
    if !signed {
        return Err(StandbyError::Invalid("authority set signature"));
    }
    if !canonical_identity(member) || !set.body.is_member(member) {
        return Err(StandbyError::Invalid("member"));
    }
    Ok(context_digest(set, member))
}

/// The canonical digest of a transaction: page size, `db_pages`, `frames` and every
/// `(pgno, page)` in order. The capture sequence and WAL position are not part of it.
fn tx_digest(tx: &AssembledTx) -> String {
    let mut hash = Sha256::new();
    hash.update(b"relay-takeover-standby-tx-v1\0");
    hash.update(u64::from(tx.page_size).to_be_bytes());
    hash.update(u64::from(tx.db_pages).to_be_bytes());
    hash.update(u64::from(tx.frames).to_be_bytes());
    for (pgno, page) in tx.pages() {
        hash.update(pgno.to_be_bytes());
        hash.update(page);
    }
    hex(&hash.finalize())
}

// ---- files ----------------------------------------------------------------------------------

fn sidecar(file: &Path, suffix: &str) -> PathBuf {
    let mut name = file.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}
#[cfg(windows)]
fn redirected(info: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    info.file_attributes() & 0x400 != 0
}
#[cfg(not(windows))]
fn redirected(_info: &fs::Metadata) -> bool {
    false
}
#[cfg(unix)]
fn single_link(info: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    info.nlink() == 1
}
#[cfg(not(unix))]
fn single_link(_info: &fs::Metadata) -> bool {
    true
}
/// A regular, direct, singly linked file.
fn own_file(info: &fs::Metadata) -> bool {
    info.file_type().is_file() && !redirected(info) && single_link(info)
}
fn no_side_files(file: &Path) -> Result<(), StandbyError> {
    for suffix in ["-wal", "-shm", "-journal"] {
        match fs::symlink_metadata(sidecar(file, suffix)) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            _ => return Err(StandbyError::Unsafe),
        }
    }
    Ok(())
}

struct ImageInfo {
    page_size: u32,
    bytes: u64,
    pages: u32,
}
/// Plain-I/O identity and size checks of the image; SQLite is never involved.
fn inspect_image(
    image: &Path,
    expect_page_size: Option<u32>,
    max_bytes: u64,
) -> Result<ImageInfo, StandbyError> {
    let info = match fs::symlink_metadata(image) {
        Ok(info) => info,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(StandbyError::Missing)
        }
        Err(_) => return Err(StandbyError::Unsafe),
    };
    if !own_file(&info) {
        return Err(StandbyError::Unsafe);
    }
    no_side_files(image)?;
    let mut file = fs::File::open(image).map_err(|_| StandbyError::Unsafe)?;
    let bytes = file.metadata().map_err(|_| StandbyError::Unsafe)?.len();
    if bytes != info.len() {
        return Err(StandbyError::Unsafe);
    }
    let mut header = [0u8; 100];
    file.read_exact(&mut header)
        .map_err(|_| StandbyError::Corrupt)?;
    if &header[..16] != b"SQLite format 3\0"
        || !matches!(header[18], 1 | 2)
        || !matches!(header[19], 1 | 2)
    {
        return Err(StandbyError::Corrupt);
    }
    let page_size = match u32::from(u16::from_be_bytes([header[16], header[17]])) {
        1 => 65_536,
        other => other,
    };
    if !valid_page_size(page_size) {
        return Err(StandbyError::Corrupt);
    }
    if expect_page_size.is_some_and(|expected| expected != page_size) {
        return Err(StandbyError::Mismatch);
    }
    if bytes == 0 || bytes % u64::from(page_size) != 0 {
        return Err(StandbyError::Corrupt);
    }
    if bytes > max_bytes {
        return Err(StandbyError::Quota);
    }
    let pages = bytes / u64::from(page_size);
    if pages > u64::from(MAX_PGNO) {
        return Err(StandbyError::Quota);
    }
    // SQLite trusts the in-header page count only when the change counter agrees with the
    // version-valid-for field; then it must equal the real size.
    let be32 = |at: usize| {
        u32::from_be_bytes([header[at], header[at + 1], header[at + 2], header[at + 3]])
    };
    if be32(24) == be32(92) && be32(28) != 0 && u64::from(be32(28)) != pages {
        return Err(StandbyError::Corrupt);
    }
    Ok(ImageInfo {
        page_size,
        bytes,
        pages: pages as u32,
    })
}

/// What a recovery needs to be offered: the exact tips and shape of the pending transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Recovery {
    pub(crate) parent: Tip,
    pub(crate) end: Tip,
    pub(crate) tx_sha256: String,
    pub(crate) db_pages: u32,
    pub(crate) frames: u32,
}
/// The bounded result of the explicit final verification. Not an authority.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Verified {
    pub(crate) tip: Tip,
    pub(crate) pages: u32,
    pub(crate) page_size: u32,
    pub(crate) bytes: u64,
}

pub(crate) struct Standby {
    control: TakeoverStore,
    root: PathBuf,
    image: PathBuf,
    limits: Limits,
    page_size: u32,
    /// Mirrors the durable record after every successful commit.
    state: State,
    poisoned: bool,
    /// Test only: stop the image write with an I/O error after this many pages (simulated
    /// interruption; no power loss is claimed).
    #[cfg(test)]
    interrupt_after: Option<usize>,
}
impl std::fmt::Debug for Standby {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Standby")
            .field("applied_term", &self.state.applied.term)
            .field("applied_index", &self.state.applied.index)
            .field("pending", &self.state.pending.is_some())
            .field("poisoned", &self.poisoned)
            .finish_non_exhaustive()
    }
}

impl Standby {
    /// Creates the control record for an image that the caller placed at `root/IMAGE_NAME` from a
    /// verified complete P0. `applied` is the caller's verified tip of that image. Nothing existing
    /// is replaced, moved or removed; the image is only inspected.
    pub(crate) fn create_new(
        root: &Path,
        set: &AuthoritySet,
        member: &HostIdentity,
        applied: &Tip,
        limits: Limits,
    ) -> Result<Self, StandbyError> {
        if !limits.valid() {
            return Err(StandbyError::Invalid("limits"));
        }
        let context = check_context(set, member)?;
        applied
            .validate()
            .map_err(|_| StandbyError::Invalid("applied tip"))?;
        handover_target::private_directory(root).map_err(|_| StandbyError::Unsafe)?;
        let image = root.join(IMAGE_NAME);
        // The seed must already be there and sane, so a refused create leaves no control store.
        let seed = inspect_image(&image, None, limits.max_image_bytes)?;
        let mut control =
            TakeoverStore::create_new(&root.join(CONTROL_DIR), CONTROL_QUOTA).map_err(from_open)?;
        let state = State {
            version: STATE_VERSION,
            context,
            page_size: seed.page_size,
            applied: applied.clone(),
            pages: seed.pages,
            pending: None,
        };
        control
            .commit(None, &encode(&state)?)
            .map_err(|_| StandbyError::Failed)?;
        Ok(Self::assemble(control, root, limits, state))
    }

    /// Re-opens an existing standby. Never initializes, repairs or converts. A pending
    /// transaction is reported by [`Standby::recovery_needed`]; the image is not opened.
    pub(crate) fn open_existing(
        root: &Path,
        set: &AuthoritySet,
        member: &HostIdentity,
        limits: Limits,
    ) -> Result<Self, StandbyError> {
        if !limits.valid() {
            return Err(StandbyError::Invalid("limits"));
        }
        let context = check_context(set, member)?;
        match fs::symlink_metadata(root) {
            Ok(info) if info.file_type().is_dir() && !redirected(&info) => {}
            Ok(_) => return Err(StandbyError::Unsafe),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(StandbyError::Missing)
            }
            Err(_) => return Err(StandbyError::Unsafe),
        }
        // The folder exists, so this only validates it and re-applies its private permissions.
        handover_target::private_directory(root).map_err(|_| StandbyError::Unsafe)?;
        let image = root.join(IMAGE_NAME);
        let mut control = TakeoverStore::open_existing(&root.join(CONTROL_DIR), CONTROL_QUOTA)
            .map_err(from_open)?;
        let raw = control
            .load_state()
            .map_err(|_| StandbyError::Failed)?
            .ok_or(StandbyError::Corrupt)?;
        let state = decode(&raw)?;
        if state.context != context {
            return Err(StandbyError::Mismatch);
        }
        if state.pending.is_some() {
            let info = fs::symlink_metadata(&image).map_err(|_| StandbyError::Missing)?;
            if !own_file(&info) {
                return Err(StandbyError::Unsafe);
            }
            no_side_files(&image)?;
            if info.len() > limits.max_image_bytes {
                return Err(StandbyError::Quota);
            }
        } else {
            let seen = inspect_image(&image, Some(state.page_size), limits.max_image_bytes)?;
            if state.pages != seen.pages {
                return Err(StandbyError::Corrupt);
            }
        }
        if u64::from(state.pages) * u64::from(state.page_size) > limits.max_image_bytes {
            return Err(StandbyError::Quota);
        }
        Ok(Self::assemble(control, root, limits, state))
    }

    fn assemble(control: TakeoverStore, root: &Path, limits: Limits, state: State) -> Self {
        Self {
            control,
            root: root.to_owned(),
            image: root.join(IMAGE_NAME),
            limits,
            page_size: state.page_size,
            state,
            poisoned: false,
            #[cfg(test)]
            interrupt_after: None,
        }
    }

    /// The tip the image durably is at (the last applied end, or the seed tip).
    pub(crate) fn bound_to(&self, set: &AuthoritySet, member: &HostIdentity) -> bool {
        check_context(set, member).is_ok_and(|context| context == self.state.context)
    }

    /// Where the owned image lives. Callers copy it; they never open or move it.
    pub(crate) fn image_path(&self) -> &Path {
        &self.image
    }
    pub(crate) fn applied(&self) -> &Tip {
        &self.state.applied
    }
    pub(crate) fn page_size(&self) -> u32 {
        self.page_size
    }
    /// The image size in pages at the applied tip (meaningful while nothing is pending).
    pub(crate) fn pages(&self) -> u32 {
        self.state.pages
    }
    pub(crate) fn is_poisoned(&self) -> bool {
        self.poisoned
    }
    /// `Some` while a transaction was started but not durably finished: only [`Standby::recover`]
    /// with exactly that transaction may continue, and the image must not be trusted or opened.
    pub(crate) fn recovery_needed(&self) -> Option<Recovery> {
        self.state.pending.as_ref().map(|p| Recovery {
            parent: p.parent.clone(),
            end: p.end.clone(),
            tx_sha256: p.tx_sha256.clone(),
            db_pages: p.db_pages,
            frames: p.frames,
        })
    }

    fn usable(&self) -> Result<(), StandbyError> {
        if self.poisoned {
            Err(StandbyError::Poisoned)
        } else {
            Ok(())
        }
    }

    /// Everything that can be refused without touching the image or the control record.
    fn check_tx(&self, tx: &AssembledTx) -> Result<(), StandbyError> {
        if tx.page_size != self.page_size {
            return Err(StandbyError::Invalid("page size"));
        }
        if tx.db_pages == 0 || tx.db_pages > MAX_PGNO || tx.frames == 0 {
            return Err(StandbyError::Invalid("transaction shape"));
        }
        if tx.frames > self.limits.max_frames
            || u64::from(tx.db_pages) * u64::from(self.page_size) > self.limits.max_image_bytes
        {
            return Err(StandbyError::Quota);
        }
        let mut count: u64 = 0;
        for (pgno, page) in tx.pages() {
            count += 1;
            if count > u64::from(tx.frames)
                || pgno == 0
                || pgno > MAX_PGNO
                || page.len() != self.page_size as usize
            {
                return Err(StandbyError::Invalid("transaction page"));
            }
        }
        if count != u64::from(tx.frames) {
            return Err(StandbyError::Invalid("transaction frame count"));
        }
        Ok(())
    }

    fn check_tips(parent: &Tip, end: &Tip) -> Result<(), StandbyError> {
        if parent.validate().is_err() || end.validate().is_err() {
            return Err(StandbyError::Invalid("tip"));
        }
        if end.index <= parent.index || end.term < parent.term {
            return Err(StandbyError::Invalid("tip order"));
        }
        Ok(())
    }

    /// Applies ONE complete transaction at the exact applied tip. See the module docs for the
    /// durable order. `Err` before the pending record is committed changes nothing; `Err` after
    /// it leaves [`Standby::recovery_needed`] set. If `applied()` already equals `end` the
    /// transaction is done and must not be sent again.
    pub(crate) fn apply(
        &mut self,
        parent: &Tip,
        end: &Tip,
        tx: &AssembledTx,
    ) -> Result<(), StandbyError> {
        self.usable()?;
        if self.state.pending.is_some() {
            return Err(StandbyError::RecoveryNeeded);
        }
        Self::check_tips(parent, end)?;
        if *parent != self.state.applied {
            return Err(StandbyError::Parent);
        }
        self.check_tx(tx)?;
        let seen = inspect_image(
            &self.image,
            Some(self.page_size),
            self.limits.max_image_bytes,
        )?;
        if seen.pages != self.state.pages {
            return Err(StandbyError::Corrupt);
        }
        let mut next = self.state.clone();
        next.pending = Some(PendingRec {
            parent: parent.clone(),
            end: end.clone(),
            tx_sha256: tx_digest(tx),
            db_pages: tx.db_pages,
            frames: tx.frames,
        });
        // Durable BEFORE the first image byte moves.
        self.persist(&next)?;
        self.state = next;
        self.finish(tx)
    }

    /// Resumes the pending transaction with EXACTLY the same transaction and tips; anything else
    /// is `WrongRecovery` and clears nothing. All pages are rewritten (idempotent).
    pub(crate) fn recover(
        &mut self,
        parent: &Tip,
        end: &Tip,
        tx: &AssembledTx,
    ) -> Result<(), StandbyError> {
        self.usable()?;
        let Some(pending) = &self.state.pending else {
            return Err(StandbyError::NothingPending);
        };
        self.check_tx(tx)?;
        if pending.parent != *parent
            || pending.end != *end
            || pending.db_pages != tx.db_pages
            || pending.frames != tx.frames
            || pending.tx_sha256 != tx_digest(tx)
        {
            return Err(StandbyError::WrongRecovery);
        }
        self.finish(tx)
    }

    /// Writes the pending transaction into the image and durably advances `applied`.
    fn finish(&mut self, tx: &AssembledTx) -> Result<(), StandbyError> {
        let Some(pending) = self.state.pending.clone() else {
            return Err(StandbyError::NothingPending);
        };
        // A failure here keeps the pending record: the exact recoverable phase.
        self.write_image(tx)?;
        let done = State {
            applied: pending.end,
            pages: pending.db_pages,
            pending: None,
            ..self.state.clone()
        };
        self.persist(&done)?;
        self.state = done;
        Ok(())
    }

    /// One durable control commit. A failure leaves the durable outcome unknown, so the instance
    /// poisons; re-opening reads whichever record won.
    fn persist(&mut self, next: &State) -> Result<(), StandbyError> {
        let bytes = encode(next)?;
        match self.control.commit(None, &bytes) {
            Ok(()) => Ok(()),
            Err(_) => {
                self.poisoned = true;
                Err(StandbyError::Failed)
            }
        }
    }

    fn write_image(&mut self, tx: &AssembledTx) -> Result<(), StandbyError> {
        let info = fs::symlink_metadata(&self.image).map_err(|_| StandbyError::Unsafe)?;
        if !own_file(&info) {
            return Err(StandbyError::Unsafe);
        }
        no_side_files(&self.image)?;
        let mut file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.image)
            .map_err(|_| StandbyError::Failed)?;
        // The durable pending record and exact transaction digest govern redo.
        // Do not require a valid header halfway through writing that header.
        #[cfg(test)]
        let mut budget = self.interrupt_after.take();
        for (pgno, page) in tx.pages() {
            // An obsolete spilled page beyond the final size is dropped, never grown into.
            if pgno > tx.db_pages {
                continue;
            }
            #[cfg(test)]
            {
                if let Some(left) = budget.as_mut() {
                    if *left == 0 {
                        return Err(StandbyError::Failed);
                    }
                    *left -= 1;
                }
            }
            let at = u64::from(pgno - 1) * u64::from(self.page_size);
            file.seek(SeekFrom::Start(at))
                .and_then(|_| file.write_all(page))
                .map_err(|_| StandbyError::Failed)?;
        }
        file.set_len(u64::from(tx.db_pages) * u64::from(self.page_size))
            .and_then(|_| file.sync_all())
            .map_err(|_| StandbyError::Failed)
    }

    /// The explicit final verification: no pending work, exactly the requested applied tip, plain
    /// file checks, then `quick_check` and page-size/page-count agreement on a private scratch
    /// copy that is removed again. NOT writer or election authority.
    pub(crate) fn verify(&mut self, expected: &Tip) -> Result<Verified, StandbyError> {
        self.usable()?;
        if self.state.pending.is_some() {
            return Err(StandbyError::RecoveryNeeded);
        }
        if expected.validate().is_err() {
            return Err(StandbyError::Invalid("tip"));
        }
        if self.state.applied != *expected {
            return Err(StandbyError::Parent);
        }
        let seen = inspect_image(
            &self.image,
            Some(self.page_size),
            self.limits.max_image_bytes,
        )?;
        if seen.pages != self.state.pages {
            return Err(StandbyError::Corrupt);
        }
        let scratch = self.root.join(VERIFY_COPY);
        clear_scratch(&scratch)?;
        let outcome = self
            .copy_for_verification(&scratch, seen.bytes)
            .and_then(|()| check_copy(&scratch, seen.page_size, seen.pages));
        let removed = clear_scratch(&scratch);
        outcome?;
        removed?;
        // The image must not have changed or grown a side file meanwhile.
        let after = inspect_image(
            &self.image,
            Some(self.page_size),
            self.limits.max_image_bytes,
        )?;
        if after.bytes != seen.bytes {
            return Err(StandbyError::Corrupt);
        }
        Ok(Verified {
            tip: expected.clone(),
            pages: seen.pages,
            page_size: seen.page_size,
            bytes: seen.bytes,
        })
    }

    /// Streams the image into a NEW scratch file. Only header bytes 18 and 19 are changed, to the
    /// rollback-journal value, in the copy.
    fn copy_for_verification(&self, scratch: &Path, bytes: u64) -> Result<(), StandbyError> {
        let mut source = fs::File::open(&self.image).map_err(|_| StandbyError::Failed)?;
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut target = options.open(scratch).map_err(|_| StandbyError::Failed)?;
        let mut head = [0u8; 100];
        source
            .read_exact(&mut head)
            .map_err(|_| StandbyError::Corrupt)?;
        head[18] = 1;
        head[19] = 1;
        target.write_all(&head).map_err(|_| StandbyError::Failed)?;
        let mut copied = head.len() as u64;
        let mut buffer = vec![0u8; 1 << 20];
        loop {
            let n = source.read(&mut buffer).map_err(|_| StandbyError::Failed)?;
            if n == 0 {
                break;
            }
            copied += n as u64;
            if copied > bytes {
                return Err(StandbyError::Corrupt);
            }
            target
                .write_all(&buffer[..n])
                .map_err(|_| StandbyError::Failed)?;
        }
        if copied != bytes {
            return Err(StandbyError::Corrupt);
        }
        target.sync_all().map_err(|_| StandbyError::Failed)
    }
}

/// Removes a scratch file of ours; anything unusual at that name is refused, not followed.
fn clear_scratch(path: &Path) -> Result<(), StandbyError> {
    match fs::symlink_metadata(path) {
        Ok(info) if own_file(&info) => fs::remove_file(path).map_err(|_| StandbyError::Failed),
        Ok(_) => Err(StandbyError::Unsafe),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(StandbyError::Unsafe),
    }
}

/// Opens the rollback-format scratch copy read-only and runs the integrity checks.
fn check_copy(scratch: &Path, page_size: u32, pages: u32) -> Result<(), StandbyError> {
    let conn = Connection::open_with_flags(
        scratch,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(from_sqlite)?;
    conn.pragma_update(None, "trusted_schema", "OFF")
        .map_err(from_sqlite)?;
    {
        let mut statement = conn.prepare("PRAGMA quick_check").map_err(from_sqlite)?;
        let rows = statement
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(from_sqlite)?;
        let mut seen = Vec::new();
        for row in rows {
            seen.push(row.map_err(from_sqlite)?);
            if seen.len() > 1 {
                break;
            }
        }
        if seen.len() != 1 || seen[0] != "ok" {
            return Err(StandbyError::Corrupt);
        }
    }
    let count: i64 = conn
        .query_row("PRAGMA page_count", [], |r| r.get(0))
        .map_err(from_sqlite)?;
    let size: i64 = conn
        .query_row("PRAGMA page_size", [], |r| r.get(0))
        .map_err(from_sqlite)?;
    if count != i64::from(pages) || size != i64::from(page_size) {
        return Err(StandbyError::Corrupt);
    }
    Ok(())
}

#[cfg(test)]
mod tests;
