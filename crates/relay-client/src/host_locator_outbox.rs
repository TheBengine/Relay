//! Durable retry of host-locator publication. When a host moves, the signed,
//! encrypted `Record` that tells members where it went must reach the independent
//! directory even if that service is down at that moment. This keeps those records
//! on disk, and nothing else, until they are published.
//!
//! What is stored: the `Record` (opaque ciphertext and public signatures) and the
//! retry bookkeeping. Never an invite code or verifier, an encryption key, a
//! community or central-account bearer, or a plaintext destination or admission
//! token: the record does not contain them and this module never sees them.
//!
//! One queue belongs to one Friends service: the caller names the service once and
//! publishes to that same address, so a saved record can never be sent to a
//! different provider that is configured later.
//!
//! No timers, threads or network here. The caller asks `due`, tries to publish,
//! then reports `sent` or `failed`. Errors never carry a capability or record text.
use crate::host_route::{history_connects, valid_pin, verify};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use relay_protocol::host_locator::{self as locator, Record};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};

/// Records waiting at once, and the largest file that may hold them.
const MAX_PENDING: usize = 32;
const MAX_FILE_BYTES: u64 = 2_621_440;
const FILE: &str = "host-locator-outbox.json";
/// Every write uses its own new scratch name beside the file: this prefix plus a random suffix.
const SCRATCH_PREFIX: &str = "host-locator-outbox.json.";
const SCRATCH_SUFFIX: &str = ".new";
const SCRATCH_TRIES: usize = 4;
const LOCK: &str = "host-locator-outbox.lock";
const VERSION: u32 = 1;
const FUTURE_SKEW_MS: u64 = 30_000;
const FIRST_RETRY_MS: u64 = 5_000;
const LAST_RETRY_MS: u64 = 30 * 60 * 1000;
const NONCE_BYTES: usize = 12;
const MIN_CIPHERTEXT_BYTES: usize = 16;

pub(crate) const INVALID: &str = "This host route update is not valid and was not queued.";
pub(crate) const CONFLICT: &str =
    "A different or newer host route update is already waiting for this community.";
pub(crate) const FULL: &str =
    "Too many host route updates are waiting to be sent. They stay saved; try again later.";
pub(crate) const UNREADABLE: &str = "Relay could not read the saved host route updates. The saved file is damaged, too large or not Relay's, and was left unchanged.";
pub(crate) const UNSAVED: &str =
    "Relay could not save the pending host route update. Check this profile's storage and try again.";
pub(crate) const NO_SERVICE: &str =
    "Relay has no Friends service to keep host route updates for. Nothing was saved.";
/// The new file replaced the old one, but flushing its folder to disk failed. The
/// result is installed and kept; whether it survives a power loss is not known.
pub(crate) const UNCONFIRMED: &str = "Relay saved the pending host route update, but could not confirm that it reached the disk. It is kept; call again to confirm it.";
pub(crate) const BUSY: &str =
    "Another Relay window is saving host route updates. Try again in a moment.";

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Item {
    record: Record,
    attempts: u32,
    next_ms: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Stored {
    version: u32,
    items: Vec<Item>,
}

#[derive(Serialize)]
struct Written<'a> {
    version: u32,
    items: &'a [Item],
}

pub(crate) struct Outbox {
    dir: PathBuf,
    items: Vec<Item>,
    /// A write was installed but its folder flush failed: the next change, or
    /// `settle`, writes again until the flush is confirmed.
    unconfirmed: bool,
    /// Flushes the folder after a replacement; a seam so the failure can be tested.
    sync: fn(&Path) -> std::io::Result<()>,
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

// ---- Storage: profile selection, redirect checks, bounded reads, atomic writes ----

/// The profile folder, chosen the same way as the other profile-local stores.
fn profile_dir() -> Result<PathBuf, String> {
    std::env::var_os("RELAY_PROFILE_DIR")
        .or_else(|| std::env::var_os("RELAY_CONFIG_DIR"))
        .map(PathBuf::from)
        .or_else(|| {
            directories::ProjectDirs::from("org", "Relay", "Relay")
                .map(|v| v.config_dir().to_owned())
        })
        .ok_or_else(|| UNSAVED.to_string())
}

/// The one Friends service a queue belongs to, as the app provider rules accept it:
/// HTTPS, a host, no credentials, query or fragment, bounded; normalised so that
/// equal addresses give the same text.
pub(crate) fn service_address(base: &str) -> Result<String, String> {
    let parsed = url::Url::parse(base.trim()).map_err(|_| NO_SERVICE)?;
    if parsed.scheme() != "https"
        || parsed.host_str().is_none()
        || parsed.as_str().len() > 4096
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.fragment().is_some()
        || parsed.query().is_some()
    {
        return Err(NO_SERVICE.into());
    }
    Ok(parsed.as_str().trim_end_matches('/').to_owned())
}

/// The folder of one service's queue below `profile`: a domain-separated hash of its
/// address, so different services never share a file and nothing is enumerated.
fn scoped(profile: &Path, base: &str) -> Result<PathBuf, String> {
    let address = service_address(base)?;
    let mut hash = Sha256::new();
    hash.update(b"relay-host-locator-outbox-service-v1\0");
    hash.update((address.len() as u64).to_be_bytes());
    hash.update(address.as_bytes());
    Ok(profile.join(format!("host-locator-{:x}", hash.finalize())))
}

/// An absolute path none of whose existing parts is a link or a reparse point.
fn direct(path: &Path) -> Result<(), String> {
    if !path.is_absolute() {
        return Err(UNSAVED.into());
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
                    return Err(UNSAVED.into());
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(UNSAVED.into()),
        }
    }
    Ok(())
}

/// The file's bytes, or `None` if there is no file. Anything that is not a plain
/// file within the limit is an error and is never touched.
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

/// Write the whole file under a scratch name created for this write alone, flush it,
/// then replace the file in one step. Returns whether the folder flush afterwards
/// was confirmed. Only the scratch file this call created is ever removed: nothing
/// else in the folder is looked at or deleted.
fn write(
    dir: &Path,
    items: &[Item],
    sync: fn(&Path) -> std::io::Result<()>,
) -> Result<bool, String> {
    let bytes = serde_json::to_vec(&Written {
        version: VERSION,
        items,
    })
    .map_err(|_| UNSAVED)?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(FULL.into());
    }
    let path = dir.join(FILE);
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
            "{SCRATCH_PREFIX}{}{SCRATCH_SUFFIX}",
            uuid::Uuid::new_v4().simple()
        ));
        direct(&scratch)?;
        match options.open(&scratch) {
            Ok(file) => {
                created = Some((scratch, file));
                break;
            }
            // That name already exists (not ours): leave it and pick another.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return Err(UNSAVED.into()),
        }
    }
    let Some((scratch, mut file)) = created else {
        return Err(UNSAVED.into());
    };
    let flushed = file.write_all(&bytes).and_then(|_| file.sync_all());
    drop(file);
    if flushed.is_err() || fs::rename(&scratch, &path).is_err() {
        // The file is unchanged; only our own scratch goes.
        let _ = fs::remove_file(&scratch);
        return Err(UNSAVED.into());
    }
    // From here the new file is installed. A failed folder flush is reported as such,
    // never as "unchanged", and nothing is undone or rewritten.
    Ok(sync(dir).is_ok())
}

// ---- What a queued record must be ----

fn hex64(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
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

fn text(record: &Record) -> Option<String> {
    serde_json::to_string(record).ok()
}

/// Everything about a record that does not depend on the clock: size, shape,
/// pins, the complete signed chain from the anchor to exactly the signer, and the
/// signer's signature. The same bounds the directory enforces.
fn sound(record: &Record) -> bool {
    serde_json::to_vec(record).is_ok_and(|v| v.len() <= locator::MAX_RECORD_BYTES)
        && hex64(&record.id)
        && canonical(&record.nonce, NONCE_BYTES, NONCE_BYTES)
        && canonical(
            &record.ciphertext,
            MIN_CIPHERTEXT_BYTES,
            locator::MAX_CIPHERTEXT_BYTES,
        )
        && record.issued_ms > 0
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

/// A record that may be queued now: sound, and neither from the future nor expired.
fn fresh(record: &Record, now_ms: u64) -> bool {
    sound(record)
        && record.issued_ms <= now_ms.saturating_add(FUTURE_SKEW_MS)
        && record.expires_ms > now_ms
}

fn parse(bytes: &[u8]) -> Result<Vec<Item>, String> {
    let stored: Stored = serde_json::from_slice(bytes).map_err(|_| UNREADABLE)?;
    let mut ids = std::collections::HashSet::new();
    if stored.version != VERSION
        || stored.items.len() > MAX_PENDING
        || !stored
            .items
            .iter()
            .all(|item| sound(&item.record) && ids.insert(item.record.id.clone()))
    {
        return Err(UNREADABLE.into());
    }
    Ok(stored.items)
}

/// How an existing item for the same ID answers a new record.
enum Change {
    /// The very same record again: nothing changes, its retry schedule stays.
    Same,
    Replace,
}

/// A queued record is replaced only under the anchor it already has, and then
/// only by the identical record, a strictly later revision from the same authority
/// over the same history, or a higher-term authority whose complete history
/// extends the stored one (so the stored authority is verifiably its ancestor).
fn replacement(old: &Record, new: &Record) -> Option<Change> {
    if old.anchor != new.anchor {
        return None;
    }
    if text(old)? == text(new)? {
        return Some(Change::Same);
    }
    if new.current == old.current {
        return (new.history == old.history && new.issued_ms > old.issued_ms)
            .then_some(Change::Replace);
    }
    let extends = new.history.len() > old.history.len()
        && new.history[..old.history.len()] == old.history[..];
    (new.current.term > old.current.term && extends).then_some(Change::Replace)
}

/// Wall-clock milliseconds; zero if the clock is unusable, which expires nothing.
fn clock() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or(0)
}

/// 5 s after the first failure, doubling, never more than 30 minutes.
fn backoff_ms(attempts: u32) -> u64 {
    (FIRST_RETRY_MS << attempts.saturating_sub(1).min(20)).min(LAST_RETRY_MS)
}

impl Outbox {
    /// The outbox of this profile. A missing file is an empty outbox; a damaged,
    /// oversized, redirected or foreign one is an error and stays as it is.
    ///
    /// The queue of the Friends service the app is configured with right now.
    /// A caller that publishes later should capture that address once and use
    /// `for_service` with it, so the queue and the publication cannot differ.
    pub(crate) fn for_profile() -> Result<Self, String> {
        let base = crate::host_locator::directory().ok_or(NO_SERVICE)?;
        Self::for_service(&base)
    }

    /// The queue that belongs to exactly this Friends service (`https://...`), kept
    /// in its own folder below the profile. Another service never sees it, and an
    /// address that a community supplied must not be passed here.
    #[cfg(test)]
    pub(crate) fn at_service(profile:PathBuf,base:&str)->Result<Self,String> {
        Self::open(scoped(&profile,base)?)
    }

    pub(crate) fn for_service(base: &str) -> Result<Self, String> {
        Self::open(scoped(&profile_dir()?, base)?)
    }

    fn open(dir: PathBuf) -> Result<Self, String> {
        let path = dir.join(FILE);
        let items = match read(&path)? {
            Some(bytes) => parse(&bytes)?,
            None => vec![],
        };
        direct(&dir)?;
        Ok(Self {
            dir,
            items,
            unconfirmed: false,
            sync: sync_directory,
        })
    }

    /// Reload under an exclusive lease, apply `change`, drop expired records, and
    /// write. Memory follows only a successful write; a failure before the file is
    /// replaced leaves both the file and this outbox exactly as they were. If the
    /// file was replaced but flushing its folder failed, the result stays installed
    /// and in memory and `UNCONFIRMED` is returned (see `settle`). `change` says
    /// whether it changed anything.
    fn mutate<R>(
        &mut self,
        now_ms: u64,
        change: impl FnOnce(&mut Vec<Item>) -> Result<(R, bool), String>,
    ) -> Result<R, String> {
        direct(&self.dir)?;
        fs::create_dir_all(&self.dir).map_err(|_| UNSAVED)?;
        let lock = self.dir.join(LOCK);
        direct(&lock)?;
        let lease = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock)
            .map_err(|_| UNSAVED)?;
        lease.try_lock().map_err(|_| BUSY)?;
        let mut items = match read(&self.dir.join(FILE))? {
            Some(bytes) => parse(&bytes)?,
            None => vec![],
        };
        let before = items.len();
        items.retain(|item| item.record.expires_ms > now_ms);
        let (result, changed) = change(&mut items)?;
        if changed || items.len() != before || self.unconfirmed {
            if !write(&self.dir, &items, self.sync)? {
                // Installed but not confirmed on disk: keep what is there, as it
                // is, and say so; the next call writes it again.
                self.items = items;
                self.unconfirmed = true;
                return Err(UNCONFIRMED.into());
            }
            self.unconfirmed = false;
        }
        self.items = items;
        Ok(result)
    }

    /// After `UNCONFIRMED`: reload what is on disk and write it again until the
    /// folder flush is confirmed. Changes nothing otherwise.
    pub(crate) fn settle(&mut self) -> Result<(), String> {
        self.mutate(clock(), |_| Ok(((), false)))
    }

    /// Save a record before any attempt to publish it. It is due immediately.
    pub(crate) fn enqueue(&mut self, record: Record, now_ms: u64) -> Result<(), String> {
        if !fresh(&record, now_ms) {
            return Err(INVALID.into());
        }
        self.mutate(now_ms, |items| {
            match items.iter().position(|item| item.record.id == record.id) {
                Some(index) => match replacement(&items[index].record, &record) {
                    Some(Change::Same) => Ok(((), false)),
                    Some(Change::Replace) => {
                        items[index] = Item {
                            record,
                            attempts: 0,
                            next_ms: now_ms,
                        };
                        Ok(((), true))
                    }
                    None => Err(CONFLICT.into()),
                },
                None => {
                    // Never drops an unexpired record to make room.
                    if items.len() >= MAX_PENDING {
                        return Err(FULL.into());
                    }
                    items.push(Item {
                        record,
                        attempts: 0,
                        next_ms: now_ms,
                    });
                    Ok(((), true))
                }
            }
        })
    }

    /// The record that has waited longest and may be tried now, if any.
    /// A renewal page must fit durably before the operator advances its cursor.
    pub(crate) fn room(&self, now_ms:u64) -> usize {
        MAX_PENDING.saturating_sub(self.items.iter().filter(|item|item.record.expires_ms>now_ms).count())
    }

    pub(crate) fn due(&self, now_ms: u64) -> Option<Record> {
        self.items
            .iter()
            .filter(|item| item.next_ms <= now_ms && item.record.expires_ms > now_ms)
            .min_by_key(|item| item.next_ms)
            .map(|item| item.record.clone())
    }

    /// This exact revision was published: forget it, and only it. A newer record
    /// for the same ID, or none, is left alone.
    pub(crate) fn sent(&mut self, record: &Record) -> Result<(), String> {
        let exact = text(record).ok_or_else(|| INVALID.to_string())?;
        // `sent` takes no time from the caller; expired records still go on this write.
        self.mutate(clock(), |items| {
            let before = items.len();
            items.retain(|item| text(&item.record).as_deref() != Some(exact.as_str()));
            Ok(((), items.len() != before))
        })
    }

    /// Publishing this exact revision failed: back off before the next try. Only
    /// that revision is touched.
    pub(crate) fn failed(&mut self, record: &Record, now_ms: u64) -> Result<(), String> {
        let exact = text(record).ok_or_else(|| INVALID.to_string())?;
        self.mutate(now_ms, |items| {
            let mut changed = false;
            for item in items
                .iter_mut()
                .filter(|item| text(&item.record).as_deref() == Some(exact.as_str()))
            {
                item.attempts = item.attempts.saturating_add(1);
                item.next_ms = now_ms.saturating_add(backoff_ms(item.attempts));
                changed = true;
            }
            Ok(((), changed))
        })
    }
}

#[cfg(test)]
#[path = "host_locator_outbox_tests.rs"]
mod tests;
