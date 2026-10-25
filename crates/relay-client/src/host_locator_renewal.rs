//! A durable cursor for the operator's renewal of invitation locators.
//!
//! After a host moves, the owner's own app walks the host's local page endpoint,
//! four signed records at a time, and puts each record in the outbox for the
//! independent directory. This module is the checkpoint of that walk and the checks
//! on each page. It does no networking, starts no thread and keeps no timer: the
//! caller supplies the one fetch of a page and the outbox of the SAME service.
//!
//! Two walks use it, each with its own checkpoint file: one over the usable invitations and
//! one over the remembered valid sessions. A finished walk is not finished for good: it is
//! due again after a fixed interval, so invitations and sessions created later, and ones
//! nearing expiry, are found again. The caller's own clock (`now_ms`) decides; there is no
//! timer here.
//!
//! The checkpoint is one small file in the owned host-data folder. Its cursor holds an
//! invite verifier, so it stays in that private folder and never joins an outbox
//! record, the directory, a link, a debug line or an error. The file also holds only
//! hashes of the service and of the destination access, never the destination, an
//! admission capability, a bearer, a signing key or an invitation code.
use crate::host_locator_outbox::{service_address, Outbox};
use crate::host_route::{history_connects, valid_pin, verify};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use relay_protocol::{
    handover::{ActivationGrant, RoutePin},
    host_locator::{self as locator, Cursor, Destination, Page, Purpose, Record},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};

/// The invitation walk's checkpoint, as it has always been (schema 1).
const FILE: &str = "locator-renewal.json";
/// The session walk's own checkpoint: the two never share a file or a scratch name.
const SESSION_FILE: &str = "locator-session-renewal.json";
/// A finished walk starts again this long after it finished.
const REFRESH_MS: u64 = 15 * 60 * 1000;
const SCRATCH_SUFFIX: &str = ".new";
const SCRATCH_TRIES: usize = 4;
const MAX_FILE_BYTES: u64 = 8192;
const SCHEMA: u32 = 1;
const NONCE_BYTES: usize = 12;
const MIN_CIPHERTEXT_BYTES: usize = 16;
const FUTURE_SKEW_MS: u64 = 30_000;

pub(crate) const STORAGE: &str =
    "Relay could not keep the progress of renewing host route updates. Check the host's data folder.";
pub(crate) const UNREADABLE: &str = "Relay could not read the saved renewal progress. The file is damaged, too large or not Relay's, and was left unchanged.";
pub(crate) const FOREIGN: &str = "The saved renewal progress belongs to another community or to a newer host. It was left unchanged and nothing was renewed.";
pub(crate) const AUTHORITY: &str =
    "This host's route could not be verified, so nothing was renewed.";
pub(crate) const BAD_PAGE: &str = "The host returned a renewal page that does not match this community. It was ignored and nothing was saved.";
/// The progress file was replaced, but flushing its folder failed: the new position is
/// installed and kept, and whether it survives a power loss is not known.
pub(crate) const UNCONFIRMED: &str = "Relay saved the renewal progress, but could not confirm that it reached the disk. It is kept; continue to confirm it.";

fn file_of(purpose: Purpose) -> &'static str {
    match purpose {
        Purpose::Invitation => FILE,
        Purpose::Session => SESSION_FILE,
    }
}

fn scratch_prefix(purpose: Purpose) -> &'static str {
    match purpose {
        Purpose::Invitation => "locator-renewal.json.",
        Purpose::Session => "locator-session-renewal.json.",
    }
}

/// What one call to `advance` did.
pub(crate) enum Advance {
    /// A page was queued durably and the checkpoint moved; `more` says another page follows.
    Queued { more: bool },
    /// The walk is finished and not yet due again. This is not delivery to the directory.
    Complete,
    /// The outbox has no room for a whole page; nothing was fetched.
    Full,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Stored {
    schema: u32,
    current: RoutePin,
    /// Domain-separated SHA-256 of the canonical service address.
    service: String,
    /// Domain-separated SHA-256 of the serialised destination access.
    destination: String,
    cursor: Option<Cursor>,
    complete: bool,
    /// When the walk finished, by the caller's clock. Absent in older files (due at once)
    /// and while the walk is still going.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    completed_ms: Option<u64>,
}

/// Where the walk stands for one authority, service and destination access.
#[derive(Clone, PartialEq, Eq)]
struct Progress {
    cursor: Option<(String, u64)>,
    complete: bool,
    completed_ms: Option<u64>,
}

pub(crate) struct Renewal {
    purpose: Purpose,
    dir: PathBuf,
    current: RoutePin,
    history: Vec<ActivationGrant>,
    /// Every pin a holder of an existing invitation may have, oldest first, then current.
    lineage: Vec<RoutePin>,
    service: String,
    destination: String,
    /// Flushes the folder after a replacement; a seam so the failure can be tested.
    sync: fn(&Path) -> std::io::Result<()>,
}

// ---- Storage: direct paths, bounded reads, unique scratch, atomic flushed replacement ----

/// An absolute path none of whose existing parts is a link or a reparse point.
fn direct(path: &Path) -> Result<(), String> {
    if !path.is_absolute() {
        return Err(STORAGE.into());
    }
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
                    return Err(STORAGE.into());
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(STORAGE.into()),
        }
    }
    Ok(())
}

/// The file's bytes, `None` if absent. Anything that is not a plain small file is an
/// error and is never touched.
fn read(path: &Path) -> Result<Option<Vec<u8>>, String> {
    direct(path)?;
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(UNREADABLE.into()),
    };
    let info = file.metadata().map_err(|_| UNREADABLE)?;
    if !info.is_file() || info.len() > MAX_FILE_BYTES {
        return Err(UNREADABLE.into());
    }
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| UNREADABLE)?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(UNREADABLE.into());
    }
    Ok(Some(bytes))
}

/// Flush the folder entry on Unix; other systems have no equivalent step here.
fn sync_directory(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        fs::File::open(dir).and_then(|v| v.sync_all())
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

/// Write the whole file under a scratch name created for this write alone, flush it,
/// replace the file in one step, and report whether the folder flush was confirmed.
/// Only the scratch file this call created is ever removed.
fn write(
    dir: &Path,
    purpose: Purpose,
    stored: &Stored,
    sync: fn(&Path) -> std::io::Result<()>,
) -> Result<bool, String> {
    let bytes = serde_json::to_vec(stored).map_err(|_| STORAGE)?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(STORAGE.into());
    }
    let path = dir.join(file_of(purpose));
    direct(&path)?;
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut created = None;
    for _ in 0..SCRATCH_TRIES {
        let scratch = dir.join(format!(
            "{}{}{SCRATCH_SUFFIX}",
            scratch_prefix(purpose),
            uuid::Uuid::new_v4().simple()
        ));
        direct(&scratch)?;
        match options.open(&scratch) {
            Ok(file) => {
                created = Some((scratch, file));
                break;
            }
            // That name exists and is not ours: leave it and pick another.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return Err(STORAGE.into()),
        }
    }
    let Some((scratch, mut file)) = created else {
        return Err(STORAGE.into());
    };
    let flushed = file.write_all(&bytes).and_then(|_| file.sync_all());
    drop(file);
    if flushed.is_err() || fs::rename(&scratch, &path).is_err() {
        let _ = fs::remove_file(&scratch);
        return Err(STORAGE.into());
    }
    // Installed from here on: a failed folder flush is reported as such, never undone.
    Ok(sync(dir).is_ok())
}

// ---- Shapes ----

fn hex64(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn hash_bytes(value: &str) -> Option<[u8; 32]> {
    if !hex64(value) {
        return None;
    }
    let mut out = [0; 32];
    for (at, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        let digit = |v: u8| match v {
            b'0'..=b'9' => Some(v - b'0'),
            _ => Some(v - b'a' + 10),
        };
        out[at] = digit(pair[0])? * 16 + digit(pair[1])?;
    }
    Some(out)
}

fn hex(bytes: impl AsRef<[u8]>) -> String {
    bytes.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

fn scoped(label: &[u8], value: &[u8]) -> String {
    let mut hash = Sha256::new();
    hash.update(label);
    hash.update((value.len() as u64).to_be_bytes());
    hash.update(value);
    hex(hash.finalize())
}

/// Canonical base64url holding `min..=max` bytes.
fn canonical(value: &str, min: usize, max: usize) -> bool {
    if value.len() > max / 3 * 4 + 4 {
        return false;
    }
    URL_SAFE_NO_PAD
        .decode(value)
        .is_ok_and(|raw| (min..=max).contains(&raw.len()) && URL_SAFE_NO_PAD.encode(&raw) == value)
}

/// The record's own shape and signatures, the same bounds the outbox and the directory
/// enforce, so a whole page can be judged before any of it is queued.
fn acceptable(record: &Record, now_ms: u64) -> bool {
    serde_json::to_vec(record).is_ok_and(|v| v.len() <= locator::MAX_RECORD_BYTES)
        && hex64(&record.id)
        && canonical(&record.nonce, NONCE_BYTES, NONCE_BYTES)
        && canonical(
            &record.ciphertext,
            MIN_CIPHERTEXT_BYTES,
            locator::MAX_CIPHERTEXT_BYTES,
        )
        && record.issued_ms > 0
        && record.issued_ms <= now_ms.saturating_add(FUTURE_SKEW_MS)
        && record.expires_ms > now_ms
        && record.expires_ms >= record.issued_ms
        && record.expires_ms - record.issued_ms <= locator::MAX_LIFETIME_MS
        && valid_pin(&record.anchor)
        && valid_pin(&record.current)
        && history_connects(&record.anchor, &record.current, &record.history)
        && verify(
            &record.current.host,
            &record.signature,
            &locator::record_message(record),
        )
}

impl Renewal {
    /// The renewal of one authority, for one service and one destination access.
    /// `data` is the owned host-data folder, `history` the complete verified source-to-
    /// current history of this host. Reads the saved checkpoint now, so a damaged or
    /// foreign one fails here and stays as it is; writes nothing.
    pub(crate) fn new(
        data: PathBuf,
        current: RoutePin,
        history: Vec<ActivationGrant>,
        service: &str,
        destination: &Destination,
    ) -> Result<Self, String> {
        Self::build(
            Purpose::Invitation,
            data,
            current,
            history,
            service,
            destination,
        )
    }

    /// The same walk over the remembered valid sessions, with its own checkpoint file,
    /// its own lookup material and the same checks and durability rules.
    pub(crate) fn new_session(
        data: PathBuf,
        current: RoutePin,
        history: Vec<ActivationGrant>,
        service: &str,
        destination: &Destination,
    ) -> Result<Self, String> {
        Self::build(
            Purpose::Session,
            data,
            current,
            history,
            service,
            destination,
        )
    }

    fn build(
        purpose: Purpose,
        data: PathBuf,
        current: RoutePin,
        history: Vec<ActivationGrant>,
        service: &str,
        destination: &Destination,
    ) -> Result<Self, String> {
        direct(&data)?;
        if !fs::metadata(&data).is_ok_and(|info| info.is_dir()) {
            return Err(STORAGE.into());
        }
        let first = history.first().map_or_else(
            || current.clone(),
            |grant| RoutePin {
                server_id: current.server_id.clone(),
                host: grant.source_identity.clone(),
                term: grant.receipt.body.term,
            },
        );
        if !valid_pin(&current) || !history_connects(&first, &current, &history) {
            return Err(AUTHORITY.into());
        }
        let mut lineage: Vec<RoutePin> = history
            .iter()
            .map(|grant| RoutePin {
                server_id: current.server_id.clone(),
                host: grant.source_identity.clone(),
                term: grant.receipt.body.term,
            })
            .collect();
        lineage.sort_by_key(|pin| pin.term);
        lineage.dedup_by_key(|pin| pin.term);
        lineage.push(current.clone());
        let address = service_address(service)?;
        let destination = serde_json::to_vec(destination).map_err(|_| STORAGE)?;
        // The invitation labels are the ones existing files were written with; sessions
        // have their own, so one purpose's file is never read as the other's progress.
        let (service_label, destination_label): (&[u8], &[u8]) = match purpose {
            Purpose::Invitation => (
                b"relay-locator-renewal-service-v1\0",
                b"relay-locator-renewal-destination-v1\0",
            ),
            Purpose::Session => (
                b"relay-locator-session-renewal-service-v1\0",
                b"relay-locator-session-renewal-destination-v1\0",
            ),
        };
        let renewal = Self {
            purpose,
            dir: data,
            current,
            history,
            lineage,
            service: scoped(service_label, address.as_bytes()),
            destination: scoped(destination_label, &destination),
            sync: sync_directory,
        };
        renewal.load()?;
        Ok(renewal)
    }

    /// The saved position for exactly this scope, or the start. A different service or
    /// destination access starts over; so does an older verified authority on this very
    /// lineage. A newer, unrelated or foreign one is refused and left alone.
    fn load(&self) -> Result<Progress, String> {
        let start = Progress {
            cursor: None,
            complete: false,
            completed_ms: None,
        };
        let Some(bytes) = read(&self.dir.join(file_of(self.purpose)))? else {
            return Ok(start);
        };
        let stored: Stored = serde_json::from_slice(&bytes).map_err(|_| UNREADABLE)?;
        if stored.schema != SCHEMA
            || !valid_pin(&stored.current)
            || !hex64(&stored.service)
            || !hex64(&stored.destination)
            || stored.complete && stored.cursor.is_some()
            || stored.completed_ms.is_some() && !stored.complete
            || stored
                .cursor
                .as_ref()
                .is_some_and(|c| hash_bytes(&c.invite_id).is_none())
        {
            return Err(UNREADABLE.into());
        }
        if stored.current != self.current {
            // Only a verified ancestor of this very lineage may be left behind.
            let ancestor = stored.current.term < self.current.term
                && self.lineage[..self.lineage.len() - 1].contains(&stored.current);
            return if ancestor {
                Ok(start)
            } else {
                Err(FOREIGN.into())
            };
        }
        if stored
            .cursor
            .as_ref()
            .is_some_and(|c| !self.lineage.iter().any(|pin| pin.term == c.anchor_term))
        {
            return Err(UNREADABLE.into());
        }
        if stored.service != self.service || stored.destination != self.destination {
            return Ok(start);
        }
        Ok(Progress {
            cursor: stored.cursor.map(|c| (c.invite_id, c.anchor_term)),
            complete: stored.complete,
            completed_ms: stored.completed_ms,
        })
    }

    /// Replace the checkpoint. `Ok(false)` means installed but not confirmed on disk.
    fn save(&self, progress: &Progress) -> Result<bool, String> {
        direct(&self.dir)?;
        let stored = Stored {
            schema: SCHEMA,
            current: self.current.clone(),
            service: self.service.clone(),
            destination: self.destination.clone(),
            cursor: progress.cursor.as_ref().map(|(hash, term)| Cursor {
                invite_id: hash.clone(),
                anchor_term: *term,
            }),
            complete: progress.complete,
            completed_ms: progress.completed_ms,
        };
        write(&self.dir, self.purpose, &stored, self.sync)
    }

    /// Judge a whole page against the saved position before anything is queued.
    fn check(&self, page: &Page, before: &Progress, now_ms: u64) -> Result<(), String> {
        let count = page.records.len();
        if page.current != self.current
            || count > locator::PAGE_RECORDS
            || (count == 0 && page.next.is_some())
            || (page.next.is_some() != (count == locator::PAGE_RECORDS))
        {
            return Err(BAD_PAGE.into());
        }
        let mut ids = std::collections::HashSet::new();
        for record in &page.records {
            // Our own authority signs every record, under an anchor of this lineage and
            // this history; and each ID appears once.
            if record.current != self.current
                || record.history != self.history
                || !self.lineage.contains(&record.anchor)
                || !ids.insert(record.id.as_str())
                || !acceptable(record, now_ms)
            {
                return Err(BAD_PAGE.into());
            }
        }
        let Some(next) = &page.next else {
            return Ok(());
        };
        let hash = hash_bytes(&next.invite_id).ok_or(BAD_PAGE)?;
        let last = page.records.last().ok_or(BAD_PAGE)?;
        // A cursor that moves backwards or stays put would loop the walk.
        let advances = before.cursor.as_ref().is_none_or(|(old, term)| {
            (next.invite_id.as_str(), next.anchor_term) > (old.as_str(), *term)
        });
        // The cursor must be the one the last record was made for, so a false cursor
        // cannot skip invitations without notice.
        // The ID of the other purpose's record for the same bytes is different, so a page of
        // the wrong kind does not pass.
        let material = match self.purpose {
            Purpose::Invitation => locator::capability_material(&last.anchor, &hash, false),
            Purpose::Session => locator::session_material(&last.anchor, &hash, false),
        };
        let derived = hex(Sha256::digest(material));
        if !advances
            || !self.lineage.iter().any(|pin| pin.term == next.anchor_term)
            || last.anchor.term != next.anchor_term
            || last.id != derived
        {
            return Err(BAD_PAGE.into());
        }
        Ok(())
    }

    /// Take one step of the walk. Loads the checkpoint first. A walk that finished less
    /// than the refresh interval ago returns `Complete`, and a full outbox returns `Full`,
    /// both without calling `fetch` and without writing. A finished walk that is due (or
    /// has no recorded time) starts again from the beginning, also after a restart.
    /// Otherwise `fetch` is called once with the saved position, the whole page is
    /// checked, every record is put in the outbox durably, and only then does the
    /// checkpoint move. Any failure keeps the old position; repeating a page is safe.
    pub(crate) fn advance(
        &self,
        queue: &mut Outbox,
        now_ms: u64,
        fetch: impl FnOnce(Option<Cursor>) -> Result<Page, String>,
    ) -> Result<Advance, String> {
        let mut before = self.load()?;
        if before.complete {
            if let Some(done) = before.completed_ms {
                // A completion time from the far future is damage, not a reason to wait
                // forever; the file is kept as it is.
                if done > now_ms.saturating_add(REFRESH_MS) {
                    return Err(UNREADABLE.into());
                }
                if now_ms < done.saturating_add(REFRESH_MS) {
                    return Ok(Advance::Complete);
                }
            }
            // Due: the next page is the first one. Nothing is written until it is queued.
            before = Progress {
                cursor: None,
                complete: false,
                completed_ms: None,
            };
        }
        if queue.room(now_ms) < locator::PAGE_RECORDS {
            return Ok(Advance::Full);
        }
        let page = fetch(
            before
                .cursor
                .clone()
                .map(|(invite_id, anchor_term)| Cursor {
                    invite_id,
                    anchor_term,
                }),
        )?;
        self.check(&page, &before, now_ms)?;
        let more = page.next.is_some();
        let empty = page.records.is_empty();
        for record in page.records {
            queue.enqueue(record, now_ms)?;
        }
        let after = Progress {
            cursor: page.next.map(|c| (c.invite_id, c.anchor_term)),
            complete: !more,
            completed_ms: (!more).then_some(now_ms),
        };
        if !self.save(&after)? {
            return Err(UNCONFIRMED.into());
        }
        Ok(if empty {
            Advance::Complete
        } else {
            Advance::Queued { more }
        })
    }
}

#[cfg(test)]
#[path = "host_locator_renewal_tests.rs"]
mod tests;
