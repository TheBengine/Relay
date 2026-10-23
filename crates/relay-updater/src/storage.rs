use super::{digest, Release, Result, MAX_METADATA};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};

pub(super) fn guard(path: &Path) -> Result<()> {
    if !path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir))
    {
        return Err("Use a direct absolute update directory.".into());
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let Some(Component::Prefix(prefix)) = path.components().next() else {
            return Err("Invalid local update path.".into());
        };
        let letter = match prefix.kind() {
            std::path::Prefix::Disk(letter) | std::path::Prefix::VerbatimDisk(letter) => letter,
            _ => return Err("Network and device update paths are refused.".into()),
        };
        let drive = PathBuf::from(format!("{}:\\", letter as char));
        let wide: Vec<u16> = drive.as_os_str().encode_wide().chain(Some(0)).collect();
        if unsafe { windows_sys::Win32::Storage::FileSystem::GetDriveTypeW(wide.as_ptr()) } != 3 {
            return Err("Updates require a fixed local drive.".into());
        }
    }
    for ancestor in path.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(info) => {
                if info.file_type().is_symlink() {
                    return Err("Update paths may not contain symbolic links.".into());
                }
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    if info.file_attributes() & 0x400 != 0 {
                        return Err(
                            "Update paths may not contain junctions or other redirects.".into()
                        );
                    }
                }
                if info.is_file() {
                    single_link(ancestor)?;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.to_string()),
        }
    }
    Ok(())
}
fn single_link(path: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        };
        let file = File::open(path).map_err(|e| e.to_string())?;
        let mut info = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::zeroed();
        if unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, info.as_mut_ptr()) } == 0
        {
            return Err(std::io::Error::last_os_error().to_string());
        }
        if unsafe { info.assume_init() }.nNumberOfLinks != 1 {
            return Err("Hard-linked update files are refused.".into());
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if fs::metadata(path).map_err(|e| e.to_string())?.nlink() != 1 {
            return Err("Hard-linked update files are refused.".into());
        }
    }
    Ok(())
}
pub(super) fn file_digest(path: &Path, max: u64) -> Result<(u64, String)> {
    guard(path)?;
    let mut file = File::open(path).map_err(|e| e.to_string())?;
    if file.metadata().map_err(|e| e.to_string())?.len() > max {
        return Err("Update file exceeds its size limit.".into());
    }
    let mut buffer = [0u8; 64 * 1024];
    let mut count = 0u64;
    let mut hash = Sha256::new();
    loop {
        let length = file.read(&mut buffer).map_err(|e| e.to_string())?;
        if length == 0 {
            break;
        }
        count = count
            .checked_add(length as u64)
            .ok_or("File length overflow.")?;
        if count > max {
            return Err("Update file exceeds its size limit.".into());
        }
        hash.update(&buffer[..length]);
    }
    Ok((count, format!("{:x}", hash.finalize())))
}
pub(super) fn read(path: &Path, max: u64) -> Result<Vec<u8>> {
    guard(path)?;
    let mut bytes = Vec::new();
    File::open(path)
        .map_err(|e| e.to_string())?
        .take(max + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > max {
        return Err("Metadata file is too large.".into());
    }
    Ok(bytes)
}
fn metadata_name(name: &str) -> bool {
    matches!(
        name,
        "root.json"
            | "targets.json"
            | "snapshot.json"
            | "timestamp.json"
            | "latest_known_time.json"
            | "generation-owner.json"
    ) || name
        .strip_prefix("chain-")
        .and_then(|s| s.strip_suffix(".json"))
        .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
}

// Creating this policy-bound directory is an atomic initial ownership intent.
// A process interruption cannot leave its policy identifier half-written.
fn bootstrap_owner(
    directory: &Path,
    publisher: &str,
    owner: &Path,
    marker: &serde_json::Value,
) -> Result<File> {
    if publisher.len() != 64
        || !publisher
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return Err("Invalid publisher storage identity.".into());
    }
    let intent = directory.join(format!("bootstrap-{publisher}.owner"));
    guard(&intent)?;
    let separate = || -> Result<()> {
        for entry in fs::read_dir(directory).map_err(|e| e.to_string())? {
            let path = entry.map_err(|e| e.to_string())?.path();
            if path != intent && path != owner {
                return Err("Choose a separate empty directory for update storage.".into());
            }
        }
        Ok(())
    };
    separate()?;
    match fs::create_dir(&intent) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && intent.is_dir() => {}
        Err(e) => return Err(e.to_string()),
    }
    checkpoint("bootstrap-intent");
    let lock_path = intent.join("engine.lock");
    guard(&lock_path)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|e| e.to_string())?;
    fs2::FileExt::try_lock_exclusive(&lock)
        .map_err(|_| "Update storage initialization is already running.")?;
    separate()?;
    let mut count = 0;
    for entry in fs::read_dir(&intent).map_err(|e| e.to_string())? {
        let path = entry.map_err(|e| e.to_string())?.path();
        guard(&path)?;
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or("Invalid initialization entry.")?;
        count += 1;
        if count > 32
            || !path.is_file()
            || (path != lock_path && !(name.starts_with("owner-") && name.ends_with(".tmp")))
            || path.metadata().map_err(|e| e.to_string())?.len()
                > if path == lock_path { 0 } else { 4096 }
        {
            return Err("Unrecognized initialization files were preserved.".into());
        }
    }
    if !owner.exists() {
        let bytes = serde_json::to_vec(marker).map_err(|e| e.to_string())?;
        let mut staged = tempfile::Builder::new()
            .prefix("owner-")
            .suffix(".tmp")
            .tempfile_in(&intent)
            .map_err(|e| e.to_string())?;
        staged
            .write_all(&bytes[..bytes.len() / 2])
            .map_err(|e| e.to_string())?;
        staged.flush().map_err(|e| e.to_string())?;
        checkpoint("owner-partial");
        staged
            .write_all(&bytes[bytes.len() / 2..])
            .map_err(|e| e.to_string())?;
        staged.flush().map_err(|e| e.to_string())?;
        staged.as_file().sync_all().map_err(|e| e.to_string())?;
        checkpoint("owner-synced");
        guard(owner)?;
        staged.persist_noclobber(owner).map_err(|e| e.to_string())?;
        checkpoint("owner-published");
    }
    Ok(lock)
}

#[cfg(test)]
pub(super) fn checkpoint(point: &str) {
    if std::env::var("RELAY_UPDATER_FIXTURE_BREAK").as_deref() == Ok(point) {
        std::process::exit(77);
    }
}
#[cfg(not(test))]
pub(super) fn checkpoint(_point: &str) {}

pub(super) fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    guard(path)?;
    let directory = path.parent().ok_or("Missing journal directory.")?;
    let bytes = serde_json::to_vec(value).map_err(|e| e.to_string())?;
    if bytes.len() > 64 * 1024 {
        return Err("Update journal exceeds its limit.".into());
    }
    let mut staged = tempfile::Builder::new()
        .prefix("journal-")
        .suffix(".tmp")
        .tempfile_in(directory)
        .map_err(|e| e.to_string())?;
    staged.write_all(&bytes).map_err(|e| e.to_string())?;
    staged.flush().map_err(|e| e.to_string())?;
    staged.as_file().sync_all().map_err(|e| e.to_string())?;
    staged.persist(path).map_err(|e| e.to_string())?;
    Ok(())
}
fn tree(path: &Path) -> Result<()> {
    guard(path)?;
    let mut pending = vec![path.to_path_buf()];
    let mut count = 0;
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory).map_err(|e| e.to_string())? {
            let path = entry.map_err(|e| e.to_string())?.path();
            guard(&path)?;
            count += 1;
            if count > 4096 {
                return Err("Update storage contains too many entries.".into());
            }
            if path.is_dir() {
                pending.push(path);
            }
        }
    }
    Ok(())
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Head {
    schema: u32,
    publisher: String,
    generation: String,
    metadata: BTreeMap<String, String>,
    pub highest: Option<Release>,
}
pub(super) struct Store {
    directory: PathBuf,
    publisher: String,
    head: Option<Head>,
    _lock: File,
}
impl Store {
    pub fn open(directory: &Path, publisher: &str) -> Result<Self> {
        guard(directory)?;
        if !directory.exists() {
            fs::create_dir_all(directory).map_err(|e| e.to_string())?;
        }
        tree(directory)?;
        let owner = directory.join("publisher.json");
        guard(&owner)?;
        let marker = serde_json::json!({"schema":1,"publisher":publisher});
        let bootstrap_lock = if !owner.exists() {
            Some(bootstrap_owner(directory, publisher, &owner, &marker)?)
        } else {
            None
        };
        if owner.exists() {
            let old: serde_json::Value =
                serde_json::from_slice(&read(&owner, 4096)?).map_err(|e| e.to_string())?;
            if old != marker {
                return Err(
                    "This update directory belongs to a different publisher policy.".into(),
                );
            }
        } else {
            return Err("Update ownership was not published.".into());
        }
        let lock_path = directory.join("engine.lock");
        guard(&lock_path)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)
            .map_err(|e| e.to_string())?;
        fs2::FileExt::try_lock_exclusive(&lock)
            .map_err(|_| "An update check or download is already running.")?;
        drop(bootstrap_lock);
        let pointer = directory.join("head.json");
        guard(&pointer)?;
        let head: Option<Head> = if pointer.exists() {
            Some(serde_json::from_slice(&read(&pointer, 64 * 1024)?).map_err(|e| e.to_string())?)
        } else {
            None
        };
        if let Some(head) = &head {
            if head.schema != 1
                || head.publisher != publisher
                || !head.generation.starts_with("metadata-")
                || !head
                    .generation
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                || head.metadata.len() > 512
            {
                return Err("The saved update checkpoint is invalid.".into());
            }
            let generation = directory.join(&head.generation);
            guard(&generation)?;
            for (name, expected) in &head.metadata {
                if !metadata_name(name)
                    || file_digest(&generation.join(name), MAX_METADATA)?.1 != *expected
                {
                    return Err(
                        "The saved update metadata changed; its rollback protection was retained."
                            .into(),
                    );
                }
            }
        }
        let store = Self {
            directory: directory.into(),
            publisher: publisher.into(),
            head,
            _lock: lock,
        };
        store.clean_orphans()?;
        Ok(store)
    }
    pub fn head(&self) -> Option<&Head> {
        self.head.as_ref()
    }
    pub fn candidate(&self) -> Result<tempfile::TempDir> {
        let candidate = tempfile::Builder::new()
            .prefix("metadata-")
            .tempdir_in(&self.directory)
            .map_err(|e| e.to_string())?;
        if let Some(head) = &self.head {
            for name in head.metadata.keys() {
                let path = candidate.path().join(name);
                guard(&path)?;
                fs::write(
                    path,
                    read(
                        &self.directory.join(&head.generation).join(name),
                        MAX_METADATA,
                    )?,
                )
                .map_err(|e| e.to_string())?;
            }
        }
        fs::write(
            candidate.path().join("generation-owner.json"),
            serde_json::to_vec(&serde_json::json!({"schema":1,"publisher":self.publisher}))
                .map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        Ok(candidate)
    }
    pub fn downloads(&self) -> Result<PathBuf> {
        let path = self.directory.join("downloads");
        guard(&path)?;
        fs::create_dir_all(&path).map_err(|e| e.to_string())?;
        tree(&path)?;
        Ok(path)
    }
    pub fn commit(&self, candidate: tempfile::TempDir, highest: &Release) -> Result<()> {
        self.commit_retaining(candidate, highest, None)
    }
    // tough can update latest_known_time during later target reads. Its working
    // datastore must stay separate from the immutable rollback checkpoint.
    pub fn checkpoint(&self, working: &tempfile::TempDir, highest: &Release) -> Result<()> {
        tree(working.path())?;
        let snapshot = tempfile::Builder::new()
            .prefix("metadata-")
            .tempdir_in(&self.directory)
            .map_err(|e| e.to_string())?;
        let mut total = 0usize;
        let mut count = 0usize;
        for entry in fs::read_dir(working.path()).map_err(|e| e.to_string())? {
            let path = entry.map_err(|e| e.to_string())?.path();
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or("Invalid checkpoint entry.")?;
            if !metadata_name(name) || !path.is_file() {
                return Err("Unexpected checkpoint working file.".into());
            }
            let bytes = read(&path, MAX_METADATA)?;
            total = total
                .checked_add(bytes.len())
                .ok_or("Checkpoint overflow.")?;
            count += 1;
            if total > 32 * 1024 * 1024 || count > 512 {
                return Err("Update metadata exceeds its storage limit.".into());
            }
            let destination = snapshot.path().join(name);
            guard(&destination)?;
            fs::write(destination, bytes).map_err(|e| e.to_string())?;
        }
        self.commit_retaining(snapshot, highest, Some(working.path()))
    }
    fn commit_retaining(
        &self,
        candidate: tempfile::TempDir,
        highest: &Release,
        keep: Option<&Path>,
    ) -> Result<()> {
        tree(candidate.path())?;
        let mut metadata = BTreeMap::new();
        let mut total = 0u64;
        for entry in fs::read_dir(candidate.path()).map_err(|e| e.to_string())? {
            let path = entry.map_err(|e| e.to_string())?.path();
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or("Invalid metadata name.")?;
            if !metadata_name(name) || !path.is_file() {
                return Err("Unexpected file in update metadata generation.".into());
            }
            let bytes = read(&path, MAX_METADATA)?;
            let _: serde_json::Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
            total = total
                .checked_add(bytes.len() as u64)
                .ok_or("Metadata size overflow.")?;
            if total > 32 * 1024 * 1024 || metadata.len() >= 512 {
                return Err("Update metadata exceeds its storage limit.".into());
            }
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .map_err(|e| e.to_string())?
                .sync_all()
                .map_err(|e| e.to_string())?;
            metadata.insert(name.into(), digest(&bytes));
        }
        let head = Head {
            schema: 1,
            publisher: self.publisher.clone(),
            generation: candidate
                .path()
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or("Invalid metadata directory.")?
                .into(),
            metadata,
            highest: Some(highest.clone()),
        };
        let bytes = serde_json::to_vec(&head).map_err(|e| e.to_string())?;
        if bytes.len() > 64 * 1024 {
            return Err("Update checkpoint is too large.".into());
        }
        // Keep a complete immutable generation before atomically replacing its pointer.
        let _retained_generation = candidate.keep();
        let mut pointer = tempfile::Builder::new()
            .prefix("head-")
            .suffix(".tmp")
            .tempfile_in(&self.directory)
            .map_err(|e| e.to_string())?;
        pointer.write_all(&bytes).map_err(|e| e.to_string())?;
        pointer.flush().map_err(|e| e.to_string())?;
        pointer.as_file().sync_all().map_err(|e| e.to_string())?;
        let path = self.directory.join("head.json");
        guard(&path)?;
        pointer.persist(&path).map_err(|e| e.to_string())?;
        // Cleanup reads the committed pointer, never the stale in-memory head.
        self.clean_orphans_retaining(keep)?;
        Ok(())
    }
    fn clean_orphans(&self) -> Result<()> {
        self.clean_orphans_retaining(None)
    }
    fn clean_orphans_retaining(&self, keep: Option<&Path>) -> Result<()> {
        let pointer = self.directory.join("head.json");
        let active = if pointer.exists() {
            let head: Head =
                serde_json::from_slice(&read(&pointer, 64 * 1024)?).map_err(|e| e.to_string())?;
            Some(head.generation)
        } else {
            None
        };
        for entry in fs::read_dir(&self.directory).map_err(|e| e.to_string())? {
            let path = entry.map_err(|e| e.to_string())?.path();
            guard(&path)?;
            if keep == Some(path.as_path()) {
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !path.is_dir() || !name.starts_with("metadata-") || active.as_deref() == Some(name) {
                continue;
            }
            let owner = path.join("generation-owner.json");
            if !owner.exists() {
                continue;
            }
            // A crash while writing an ownership marker cannot block the last good checkpoint.
            // Without a valid marker, preserve the unrecognized directory.
            let Ok(value) = read(&owner, 4096).and_then(|bytes| {
                serde_json::from_slice::<serde_json::Value>(&bytes).map_err(|e| e.to_string())
            }) else {
                continue;
            };
            if value != serde_json::json!({"schema":1,"publisher":self.publisher}) {
                continue;
            }
            tree(&path)?;
            let mut removable = true;
            for entry in fs::read_dir(&path).map_err(|e| e.to_string())? {
                let path = entry.map_err(|e| e.to_string())?.path();
                if !path.is_file()
                    || !path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(metadata_name)
                {
                    removable = false;
                    break;
                }
            }
            // Only a checked, directly owned generation with exclusively metadata files is removed.
            if removable {
                fs::remove_dir_all(&path).map_err(|e| e.to_string())?;
            }
        }
        Ok(())
    }
}

pub(super) fn cached_root(original: &[u8], directory: &Path) -> Result<Vec<u8>> {
    let mut trusted: tough::schema::Signed<tough::schema::Root> =
        serde_json::from_slice(original).map_err(|e| e.to_string())?;
    let mut chain = BTreeMap::new();
    for entry in fs::read_dir(directory).map_err(|e| e.to_string())? {
        let path = entry.map_err(|e| e.to_string())?.path();
        guard(&path)?;
        if let Some(version) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_prefix("chain-"))
            .and_then(|n| n.strip_suffix(".json"))
            .and_then(|n| n.parse::<u64>().ok())
        {
            if chain.insert(version, path).is_some() {
                return Err("Duplicate cached publisher root version.".into());
            }
        }
    }
    for (version, path) in chain {
        let next: tough::schema::Signed<tough::schema::Root> =
            serde_json::from_slice(&read(&path, MAX_METADATA)?).map_err(|e| e.to_string())?;
        if trusted.signed.version.get().checked_add(1) != Some(version)
            || next.signed.version.get() != version
        {
            return Err("Cached publisher roots are not a continuous trust chain.".into());
        }
        trusted
            .signed
            .verify_role(&next)
            .map_err(|e| e.to_string())?;
        next.signed.verify_role(&next).map_err(|e| e.to_string())?;
        trusted = next;
    }
    let latest = directory.join("root.json");
    if latest.exists() {
        let cached: tough::schema::Signed<tough::schema::Root> =
            serde_json::from_slice(&read(&latest, MAX_METADATA)?).map_err(|e| e.to_string())?;
        if serde_json::to_value(&cached).map_err(|e| e.to_string())?
            != serde_json::to_value(&trusted).map_err(|e| e.to_string())?
        {
            return Err("Cached publisher root does not match its signed transition chain.".into());
        }
    }
    serde_json::to_vec(&trusted).map_err(|e| e.to_string())
}
