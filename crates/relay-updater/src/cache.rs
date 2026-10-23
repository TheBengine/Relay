use super::{sequence, storage, Release, Result, MAX_INSTALLER};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

pub(super) const MAX_CACHE: u64 = 2 * MAX_INSTALLER + 128 * 1024;
const MAX_ENTRIES: usize = 512;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    name: String,
    length: u64,
    sha256: String,
    sequence: String,
}
impl Receipt {
    fn from_release(release: &Release) -> Self {
        Self {
            name: format!("Relay-{}-{}-Setup.exe", release.version, release.sequence),
            length: release.installer_bytes,
            sha256: release.installer_sha256.clone(),
            sequence: release.sequence.clone(),
        }
    }
    fn validate(&self) -> Result<()> {
        sequence(&self.sequence)?;
        let version = self
            .name
            .strip_prefix("Relay-")
            .and_then(|v| v.strip_suffix("-Setup.exe"))
            .and_then(|v| v.rsplit_once('-'))
            .filter(|(_, s)| *s == self.sequence)
            .ok_or("Invalid installer cache name.")?
            .0;
        let parsed = semver::Version::parse(version).map_err(|e| e.to_string())?;
        if version.len() > 64
            || !parsed.build.is_empty()
            || self.length == 0
            || self.length > MAX_INSTALLER
            || self.sha256.len() != 64
            || !self
                .sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err("Invalid installer cache receipt.".into());
        }
        Ok(())
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    schema: u32,
    publisher: String,
    files: BTreeMap<String, Receipt>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Intent {
    schema: u32,
    publisher: String,
    name: String,
    maximum: u64,
    installer_sha256: String,
}

pub(super) struct Cache {
    directory: PathBuf,
    ledger: Ledger,
    limit: u64,
}
impl Cache {
    pub fn open(directory: PathBuf, publisher: &str) -> Result<Self> {
        storage::guard(&directory)?;
        let path = directory.join("installer-receipts.json");
        let ledger: Ledger = if path.exists() {
            serde_json::from_slice(&storage::read(&path, 64 * 1024)?).map_err(|e| e.to_string())?
        } else {
            Ledger {
                schema: 1,
                publisher: publisher.into(),
                files: BTreeMap::new(),
            }
        };
        if ledger.schema != 1 || ledger.publisher != publisher || ledger.files.len() > 16 {
            return Err("Unrecognized installer cache journal was preserved.".into());
        }
        for (name, receipt) in &ledger.files {
            receipt.validate()?;
            if name != &receipt.name {
                return Err("Invalid cache receipt binding.".into());
            }
        }
        let mut cache = Self {
            directory,
            ledger,
            limit: MAX_CACHE,
        };
        cache.recover()?;
        cache.inventory()?;
        Ok(cache)
    }
    fn journal(&self) -> PathBuf {
        self.directory.join("download-intent.json")
    }
    fn save(&self) -> Result<()> {
        storage::atomic_json(
            &self.directory.join("installer-receipts.json"),
            &self.ledger,
        )
    }
    fn inventory(&self) -> Result<u64> {
        self.inventory_with_reserved_entries(0)
    }
    fn inventory_with_reserved_entries(&self, reserved: usize) -> Result<u64> {
        let mut pending = vec![self.directory.clone()];
        let mut count = 0usize;
        let mut bytes = 0u64;
        while let Some(directory) = pending.pop() {
            storage::guard(&directory)?;
            for entry in fs::read_dir(directory).map_err(|e| e.to_string())? {
                let path = entry.map_err(|e| e.to_string())?.path();
                storage::guard(&path)?;
                count += 1;
                if count > MAX_ENTRIES.saturating_sub(reserved) {
                    return Err(
                        "Too many installer cache entries; unknown files were preserved.".into(),
                    );
                }
                let metadata = fs::metadata(&path).map_err(|e| e.to_string())?;
                if metadata.is_dir() {
                    pending.push(path);
                } else if metadata.is_file() {
                    bytes = bytes
                        .checked_add(metadata.len())
                        .ok_or("Cache size overflow.")?;
                } else {
                    return Err("Unrecognized cache entry was preserved.".into());
                }
            }
        }
        Ok(bytes)
    }
    fn recover(&mut self) -> Result<()> {
        let journal = self.journal();
        storage::guard(&journal)?;
        if !journal.exists() {
            return Ok(());
        }
        let intent: Intent =
            serde_json::from_slice(&storage::read(&journal, 4096)?).map_err(|e| e.to_string())?;
        let token = intent
            .name
            .strip_prefix("download-")
            .and_then(|s| s.strip_suffix(".part"));
        if intent.schema != 1
            || intent.publisher != self.ledger.publisher
            || intent.maximum == 0
            || intent.maximum > MAX_INSTALLER
            || !token.is_some_and(|v| {
                !v.is_empty() && v.len() <= 64 && v.bytes().all(|b| b.is_ascii_alphanumeric())
            })
            || intent.installer_sha256.len() != 64
            || !intent
                .installer_sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err("Unrecognized download intent was preserved.".into());
        }
        let path = self.directory.join(&intent.name);
        storage::guard(&path)?;
        if path.exists() {
            let metadata = fs::metadata(&path).map_err(|e| e.to_string())?;
            if !metadata.is_file() || metadata.len() > intent.maximum {
                return Err("Changed partial download was preserved.".into());
            }
            fs::remove_file(path).map_err(|e| e.to_string())?;
        }
        fs::remove_file(journal).map_err(|e| e.to_string())?;
        Ok(())
    }
    // Validate every planned deletion first. A receipt never authorizes removing
    // another name, a redirect/hardlink, or an installer whose bytes changed.
    fn remove(&mut self, names: &[String]) -> Result<()> {
        for name in names {
            let receipt = self
                .ledger
                .files
                .get(name)
                .ok_or("Missing cache receipt.")?;
            let path = self.directory.join(name);
            storage::guard(&path)?;
            if path.exists()
                && storage::file_digest(&path, MAX_INSTALLER)?
                    != (receipt.length, receipt.sha256.clone())
            {
                return Err("Changed cached installer was preserved.".into());
            }
        }
        for name in names {
            let path = self.directory.join(name);
            storage::guard(&path)?;
            if path.exists() {
                fs::remove_file(path).map_err(|e| e.to_string())?;
            }
            self.ledger.files.remove(name);
        }
        if !names.is_empty() {
            self.save()?;
        }
        Ok(())
    }
    fn make_room(&mut self, required: u64, keep: Option<&str>) -> Result<()> {
        // Reserve enough space for both atomic journals and their temporary files.
        let needed = required
            .checked_add(128 * 1024)
            .ok_or("Cache reservation overflow.")?;
        let used = self.inventory()?;
        if used.checked_add(needed).is_some_and(|n| n <= self.limit) {
            return Ok(());
        }
        let mut receipts: Vec<_> = self.ledger.files.values().cloned().collect();
        receipts.sort_by(|a, b| a.sequence.cmp(&b.sequence));
        let mut reclaim = 0u64;
        let mut names = vec![];
        for receipt in receipts {
            if keep == Some(receipt.name.as_str()) {
                continue;
            }
            let path = self.directory.join(&receipt.name);
            storage::guard(&path)?;
            if path.exists() {
                reclaim = reclaim
                    .checked_add(fs::metadata(path).map_err(|e| e.to_string())?.len())
                    .ok_or("Cache size overflow.")?;
            }
            names.push(receipt.name);
            if used
                .saturating_sub(reclaim)
                .checked_add(needed)
                .is_some_and(|n| n <= self.limit)
            {
                break;
            }
        }
        if !used
            .saturating_sub(reclaim)
            .checked_add(needed)
            .is_some_and(|n| n <= self.limit)
        {
            return Err("Installer cache is full; unknown files were preserved.".into());
        }
        self.remove(&names)?;
        if self
            .inventory()?
            .checked_add(needed)
            .is_none_or(|n| n > self.limit)
        {
            return Err("Installer cache changed during reservation.".into());
        }
        Ok(())
    }
    pub fn stage(&mut self, release: &Release) -> Result<Staging> {
        Receipt::from_release(release).validate()?;
        if self.journal().exists() {
            return Err("A cache download is already active.".into());
        }
        self.inventory_with_reserved_entries(3)?;
        self.make_room(release.installer_bytes, None)?;
        let temporary = tempfile::Builder::new()
            .prefix("download-")
            .suffix(".part")
            .tempfile_in(&self.directory)
            .map_err(|e| e.to_string())?;
        storage::checkpoint("download-created");
        let intent = Intent {
            schema: 1,
            publisher: self.ledger.publisher.clone(),
            name: temporary
                .path()
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or("Invalid temporary name.")?
                .into(),
            maximum: release.installer_bytes,
            installer_sha256: release.installer_sha256.clone(),
        };
        let journal = self.journal();
        storage::atomic_json(&journal, &intent)?;
        storage::checkpoint("download-journaled");
        let marker = serde_json::to_vec(&intent).map_err(|e| e.to_string())?;
        Ok(Staging {
            temporary: Some(temporary),
            journal,
            marker,
        })
    }
    pub fn record(&mut self, release: &Release, path: &Path) -> Result<()> {
        let receipt = Receipt::from_release(release);
        receipt.validate()?;
        if path != self.directory.join(&receipt.name)
            || storage::file_digest(path, MAX_INSTALLER)?
                != (receipt.length, receipt.sha256.clone())
        {
            return Err("Installer cache registration failed verification.".into());
        }
        if self
            .ledger
            .files
            .get(&receipt.name)
            .is_some_and(|old| old != &receipt)
        {
            return Err("Changed cache receipt was preserved.".into());
        }
        self.make_room(0, Some(receipt.name.as_str()))?;
        self.ledger
            .files
            .insert(receipt.name.clone(), receipt.clone());
        self.save()?;
        storage::checkpoint("download-receipted");
        let old: Vec<_> = self
            .ledger
            .files
            .keys()
            .filter(|name| *name != &receipt.name)
            .cloned()
            .collect();
        self.remove(&old)?;
        Ok(())
    }
}
pub(super) struct Staging {
    temporary: Option<tempfile::NamedTempFile>,
    journal: PathBuf,
    marker: Vec<u8>,
}
impl Staging {
    pub fn file(&mut self) -> &mut tempfile::NamedTempFile {
        self.temporary.as_mut().expect("Active staged download")
    }
    pub fn path(&self) -> &Path {
        self.temporary
            .as_ref()
            .expect("Active staged download")
            .path()
    }
    pub fn persist(&mut self, destination: &Path) -> Result<()> {
        storage::guard(destination)?;
        let file = self.temporary.take().ok_or("Download already persisted.")?;
        file.persist_noclobber(destination)
            .map_err(|e| e.to_string())?;
        storage::checkpoint("download-published");
        Ok(())
    }
}
impl Drop for Staging {
    fn drop(&mut self) {
        // First close/remove the partial file, then its intent. An interruption
        // between these steps leaves a recoverable intent with no partial file.
        drop(self.temporary.take());
        if storage::read(&self.journal, 4096).is_ok_and(|bytes| bytes == self.marker) {
            let _ = fs::remove_file(&self.journal);
        }
    }
}

#[cfg(test)]
mod tests;
