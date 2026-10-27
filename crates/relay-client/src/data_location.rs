//! A user-chosen data location for the normal (personal) Relay profile.
//!
//! The choice is kept in a small bootstrap record that never moves
//! (`%LOCALAPPDATA%\Relay\data-location.json`). A chosen folder uses exactly the
//! `--profile-dir` layout; "Windows default folders" is the historical layout.
//! Data only moves at startup, before any worker opens it: copy with hashes,
//! verify the copy, switch, and keep the old copy until the user deletes it.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    ffi::OsString,
    fs,
    io::{Read, Write},
    path::{Component, Path, PathBuf, Prefix},
};

pub const BOOTSTRAP_FILE: &str = "data-location.json";
/// Identifies a finished data folder; its id must match the bootstrap record.
pub const MARKER: &str = "relay-data-location.json";
/// Present in every destination folder while a copy is incomplete.
pub const IN_PROGRESS: &str = "relay-migration.json";
/// Set together with the `--profile-dir` variables when a chosen folder is active.
pub const LOCATION_ENV: &str = "RELAY_DATA_LOCATION";
const VERSION: u32 = 1;
const MAX_ENTRIES: usize = 1_000_000;
const MAX_ATTEMPTS: u32 = 3;
const PROFILE_VARIABLES: [&str; 4] = [
    "RELAY_PROFILE_DIR",
    "RELAY_CONFIG_DIR",
    "RELAY_CRYPTO_DIR",
    "RELAY_SERVER_DATA_DIR",
];

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Place {
    /// The historical per-user Windows folders.
    #[default]
    Default,
    Folder {
        root: PathBuf,
        id: String,
        #[serde(default)]
        allow_removable: bool,
    },
}

impl Place {
    pub fn describe(&self) -> String {
        match self {
            Self::Default => "Windows default folders (AppData)".into(),
            Self::Folder { root, .. } => root.display().to_string(),
        }
    }
    fn id(&self) -> Option<&str> {
        match self {
            Self::Default => None,
            Self::Folder { id, .. } => Some(id),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pending {
    pub target: Place,
    /// Names this move's in-progress markers.
    pub id: String,
    #[serde(default)]
    pub attempts: u32,
    /// Old copies renamed out of the way while moving back onto them.
    #[serde(default)]
    pub set_aside: Vec<(PathBuf, PathBuf)>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Outcome {
    pub ok: bool,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bootstrap {
    pub version: u32,
    #[serde(default)]
    pub active: Place,
    /// The old copy, kept until the user deletes it or moves back onto it.
    #[serde(default)]
    pub previous: Option<Place>,
    #[serde(default)]
    pub pending: Option<Pending>,
    #[serde(default)]
    pub last_result: Option<Outcome>,
}

impl Default for Bootstrap {
    fn default() -> Self {
        Self {
            version: VERSION,
            active: Place::Default,
            previous: None,
            pending: None,
            last_result: None,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FolderMarker {
    version: u32,
    id: String,
}

/// The historical storage folders; they must match the resolvers that run
/// when no override is set (app.rs, e2ee.rs, local_host.rs, replica_copy.rs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DefaultFolders {
    pub config: PathBuf,
    pub local: PathBuf,
    pub server: PathBuf,
    pub replicas: PathBuf,
    /// Holds the bootstrap record; never part of any copy.
    pub bootstrap_dir: PathBuf,
}

impl DefaultFolders {
    pub fn current() -> Result<Self, String> {
        let dirs = directories::ProjectDirs::from("org", "Relay", "Relay")
            .ok_or("Cannot locate your per-user Relay folders.")?;
        #[cfg(windows)]
        let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
        #[cfg(not(windows))]
        let base = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".local/share")));
        let base = base
            .ok_or("Cannot locate your per-user Relay folders.")?
            .join("Relay");
        Ok(Self {
            config: dirs.config_dir().to_path_buf(),
            local: dirs.data_local_dir().to_path_buf(),
            server: base.join("server-data"),
            replicas: base.join("replica-copies"),
            bootstrap_dir: base,
        })
    }
    pub fn bootstrap(&self) -> PathBuf {
        self.bootstrap_dir.join(BOOTSTRAP_FILE)
    }
}

// ---------------------------------------------------------------------------
// Bootstrap record

pub fn load(path: &Path) -> Result<Bootstrap, String> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Bootstrap::default())
        }
        Err(error) => {
            return Err(format!(
                "Relay cannot read where your data is stored ({}): {error}",
                path.display()
            ))
        }
    };
    let value: Bootstrap = serde_json::from_slice(&bytes).map_err(|_| {
        format!(
            "Relay's record of where your data is stored is damaged: {}. Nothing was changed.",
            path.display()
        )
    })?;
    if value.version != VERSION {
        return Err(format!(
            "This version of Relay does not understand the data location record at {}.",
            path.display()
        ));
    }
    Ok(value)
}

/// Write-then-rename so a crash leaves either the old or the new record.
pub fn save(path: &Path, value: &Bootstrap) -> Result<(), String> {
    let failed = |error: std::io::Error| format!("Relay cannot save the data location: {error}");
    let parent = path.parent().ok_or("Invalid data location record path.")?;
    fs::create_dir_all(parent).map_err(failed)?;
    let bytes = serde_json::to_vec_pretty(value).map_err(|_| "Cannot encode the data location.")?;
    let scratch = parent.join(format!("{BOOTSTRAP_FILE}.{}.tmp", uuid::Uuid::new_v4()));
    let written = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&scratch)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&scratch, path)
    })();
    if written.is_err() {
        let _ = fs::remove_file(&scratch);
    }
    written.map_err(failed)
}

// ---------------------------------------------------------------------------
// Paths

fn key(path: &Path) -> String {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let text = absolute.to_string_lossy().replace('/', "\\");
    let text = text.strip_prefix(r"\\?\").unwrap_or(&text);
    text.trim_end_matches('\\').to_lowercase()
}

/// True when `child` is `parent` or lies inside it (case-insensitive, lexical).
pub fn within(child: &Path, parent: &Path) -> bool {
    let (child, parent) = (key(child), key(parent));
    child == parent || child.starts_with(&format!("{parent}\\"))
}

fn is_link(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return true;
        }
    }
    metadata.file_type().is_symlink()
}

fn non_empty(path: &Path) -> Result<bool, String> {
    match fs::read_dir(path) {
        Ok(mut entries) => Ok(entries.next().is_some()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("Cannot read {}: {error}", path.display())),
    }
}

// ---------------------------------------------------------------------------
// Layouts

struct Entry {
    logical: PathBuf,
    source: PathBuf,
    dir: bool,
    len: u64,
}

enum Layout<'a> {
    Default(&'a DefaultFolders),
    Folder(&'a Path),
}

/// Folders that live in the profile root, not in `config`, under `--profile-dir`.
fn root_level_config_item(name: &str) -> bool {
    name == "host-moves" || name.starts_with("host-locator-")
}
const RESERVED_TOP: [&str; 6] = [
    "config",
    "server-data",
    "replica-copies",
    "updates",
    MARKER,
    IN_PROGRESS,
];

impl<'a> Layout<'a> {
    fn of(place: &'a Place, defaults: &'a DefaultFolders) -> Self {
        match place {
            Place::Default => Self::Default(defaults),
            Place::Folder { root, .. } => Self::Folder(root),
        }
    }
    fn slots(&self) -> Vec<PathBuf> {
        match self {
            Self::Default(d) => vec![
                d.config.clone(),
                d.local.clone(),
                d.server.clone(),
                d.replicas.clone(),
            ],
            Self::Folder(root) => vec![root.to_path_buf()],
        }
    }
    /// Where a profile-layout relative path lives in this layout.
    fn physical(&self, logical: &Path) -> PathBuf {
        let Self::Default(d) = self else {
            let Self::Folder(root) = self else {
                unreachable!()
            };
            return root.join(logical);
        };
        let mut parts = logical.components();
        let first = parts
            .next()
            .map(|part| part.as_os_str().to_string_lossy().into_owned())
            .unwrap_or_default();
        let rest = parts.as_path();
        match first.as_str() {
            "config" => d.config.join(rest),
            "server-data" => d.server.join(rest),
            "replica-copies" => d.replicas.join(rest),
            name if root_level_config_item(name) => d.config.join(logical),
            _ => d.local.join(logical),
        }
    }
    /// Everything to copy, as profile-layout relative paths.
    fn entries(&self) -> Result<Vec<Entry>, String> {
        let mut out = Vec::new();
        match self {
            Self::Folder(root) => walk(root, &mut out, &|rel| {
                let top = top_name(rel);
                (![MARKER, IN_PROGRESS, "updates"].contains(&top.as_str()))
                    .then(|| rel.to_path_buf())
            })?,
            Self::Default(d) => {
                walk(&d.config, &mut out, &|rel| {
                    let top = top_name(rel);
                    if top == IN_PROGRESS {
                        None
                    } else if root_level_config_item(&top) {
                        Some(rel.to_path_buf())
                    } else {
                        Some(Path::new("config").join(rel))
                    }
                })?;
                let mut local = Vec::new();
                walk(&d.local, &mut local, &|rel| {
                    (top_name(rel) != IN_PROGRESS).then(|| rel.to_path_buf())
                })?;
                if let Some(clash) = local.iter().find(|entry| {
                    let top = top_name(&entry.logical);
                    RESERVED_TOP.contains(&top.as_str()) || root_level_config_item(&top)
                }) {
                    return Err(format!(
                        "Relay found an unexpected item it cannot place safely: {}. Nothing was moved.",
                        clash.source.display()
                    ));
                }
                out.extend(local);
                for (slot, name) in [(&d.server, "server-data"), (&d.replicas, "replica-copies")] {
                    walk(slot, &mut out, &|rel| {
                        (top_name(rel) != IN_PROGRESS).then(|| Path::new(name).join(rel))
                    })?;
                }
            }
        }
        if out.len() > MAX_ENTRIES {
            return Err("Your Relay data has too many files to move safely.".into());
        }
        out.sort_by(|a, b| a.logical.cmp(&b.logical));
        Ok(out)
    }
    fn is_empty(&self) -> Result<bool, String> {
        for slot in self.slots() {
            if non_empty(&slot)? {
                return Ok(false);
            }
        }
        Ok(true)
    }
    /// Removes only folders this move marked as its own incomplete copy.
    fn remove_partial(&self, id: &str) -> Result<(), String> {
        for slot in self.slots() {
            let marker = slot.join(IN_PROGRESS);
            let ours = fs::read(&marker)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<FolderMarker>(&bytes).ok())
                .is_some_and(|marker| marker.id == id);
            if ours {
                fs::remove_dir_all(&slot).map_err(|error| {
                    format!(
                        "Relay could not remove an incomplete copy at {}: {error}",
                        slot.display()
                    )
                })?;
            }
        }
        Ok(())
    }
}

fn top_name(rel: &Path) -> String {
    rel.components()
        .next()
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn walk(
    root: &Path,
    out: &mut Vec<Entry>,
    map: &dyn Fn(&Path) -> Option<PathBuf>,
) -> Result<(), String> {
    let metadata = match fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("Cannot read {}: {error}", root.display())),
    };
    if is_link(&metadata) || !metadata.is_dir() {
        return Err(format!(
            "{} is a link or not a folder. Relay does not move linked storage.",
            root.display()
        ));
    }
    let mut pending = vec![root.to_path_buf()];
    while let Some(folder) = pending.pop() {
        let entries =
            fs::read_dir(&folder).map_err(|e| format!("Cannot read {}: {e}", folder.display()))?;
        for entry in entries {
            let entry = entry.map_err(|e| format!("Cannot read {}: {e}", folder.display()))?;
            let path = entry.path();
            let rel = path.strip_prefix(root).map_err(|_| "Invalid data path.")?;
            let Some(logical) = map(rel) else { continue };
            let metadata = fs::symlink_metadata(&path)
                .map_err(|e| format!("Cannot read {}: {e}", path.display()))?;
            if is_link(&metadata) {
                return Err(format!(
                    "{} is a link. Relay does not move linked storage.",
                    path.display()
                ));
            }
            if metadata.is_dir() {
                pending.push(path.clone());
                // Storage-area folders exist in one layout and are separate
                // roots in the other; only their contents are compared.
                if ["config", "server-data", "replica-copies"]
                    .iter()
                    .any(|name| logical == Path::new(name))
                {
                    continue;
                }
            } else if !metadata.is_file() {
                return Err(format!("{} is not an ordinary file.", path.display()));
            }
            out.push(Entry {
                logical,
                source: path,
                dir: metadata.is_dir(),
                len: if metadata.is_dir() { 0 } else { metadata.len() },
            });
            if out.len() > MAX_ENTRIES {
                return Err("Your Relay data has too many files to move safely.".into());
            }
        }
    }
    Ok(())
}

/// Total bytes the current data location would copy.
pub fn size_of(place: &Place, defaults: &DefaultFolders) -> Result<u64, String> {
    Ok(Layout::of(place, defaults)
        .entries()?
        .iter()
        .map(|entry| entry.len)
        .sum())
}

// ---------------------------------------------------------------------------
// Drives and validation

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DriveKind {
    Fixed,
    Removable,
    Network,
    Missing,
    /// CD/DVD, RAM disk or unknown: never suitable.
    Unsuitable,
}

#[derive(Clone, Debug)]
pub struct DriveInfo {
    pub kind: DriveKind,
    pub file_system: String,
    pub free_bytes: u64,
}

pub trait Probe {
    fn drive(&self, path: &Path) -> DriveInfo;
}

pub struct SystemProbe;

#[cfg(windows)]
impl Probe for SystemProbe {
    fn drive(&self, path: &Path) -> DriveInfo {
        use std::os::windows::ffi::OsStrExt;
        use windows::{
            core::PCWSTR,
            Win32::Storage::FileSystem::{
                GetDiskFreeSpaceExW, GetDriveTypeW, GetVolumeInformationW,
            },
        };
        let missing = DriveInfo {
            kind: DriveKind::Missing,
            file_system: String::new(),
            free_bytes: 0,
        };
        let Some(root) = drive_root(path) else {
            return missing;
        };
        let wide: Vec<u16> = root.as_os_str().encode_wide().chain(Some(0)).collect();
        // DRIVE_REMOVABLE 2, DRIVE_FIXED 3, DRIVE_REMOTE 4; 0/1 = unknown/no root.
        let kind = match unsafe { GetDriveTypeW(PCWSTR(wide.as_ptr())) } {
            0 | 1 => return missing,
            2 => DriveKind::Removable,
            3 => DriveKind::Fixed,
            4 => DriveKind::Network,
            _ => DriveKind::Unsuitable,
        };
        let kind = if matches!(
            path.components().next(),
            Some(Component::Prefix(p)) if matches!(p.kind(), Prefix::UNC(..) | Prefix::VerbatimUNC(..))
        ) {
            DriveKind::Network
        } else {
            kind
        };
        let mut name = [0u16; 64];
        let file_system = unsafe {
            GetVolumeInformationW(
                PCWSTR(wide.as_ptr()),
                None,
                None,
                None,
                None,
                Some(&mut name),
            )
        }
        .map(|_| String::from_utf16_lossy(&name[..name.iter().position(|c| *c == 0).unwrap_or(0)]))
        .unwrap_or_default();
        let mut free = 0u64;
        let existing = path
            .ancestors()
            .find(|p| p.exists())
            .unwrap_or(root.as_path());
        let existing: Vec<u16> = existing.as_os_str().encode_wide().chain(Some(0)).collect();
        let _ = unsafe {
            GetDiskFreeSpaceExW(
                PCWSTR(existing.as_ptr()),
                Some(&mut free as *mut u64),
                None,
                None,
            )
        };
        DriveInfo {
            kind,
            file_system,
            free_bytes: free,
        }
    }
}

#[cfg(not(windows))]
impl Probe for SystemProbe {
    fn drive(&self, _path: &Path) -> DriveInfo {
        DriveInfo {
            kind: DriveKind::Fixed,
            file_system: "NTFS".into(),
            free_bytes: u64::MAX,
        }
    }
}

/// `D:\` for `D:\x\y`, `\\server\share\` for UNC paths.
pub fn drive_root(path: &Path) -> Option<PathBuf> {
    match path.components().next()? {
        Component::Prefix(prefix) => {
            let mut root = PathBuf::from(prefix.as_os_str());
            root.push("\\");
            Some(root)
        }
        _ => None,
    }
}

fn drive_label(path: &Path) -> String {
    drive_root(path)
        .map(|root| {
            root.display()
                .to_string()
                .trim_end_matches('\\')
                .to_string()
        })
        .unwrap_or_else(|| path.display().to_string())
}

/// Folders a data location must neither be inside nor contain.
fn protected_paths(defaults: &DefaultFolders, boot: &Bootstrap) -> Vec<PathBuf> {
    let mut list: Vec<PathBuf> = [
        "SystemRoot",
        "ProgramFiles",
        "ProgramFiles(x86)",
        "ProgramData",
    ]
    .into_iter()
    .filter_map(std::env::var_os)
    .map(PathBuf::from)
    .collect();
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
    {
        list.push(dir);
    }
    list.extend([
        defaults.config.clone(),
        defaults.local.clone(),
        defaults.server.clone(),
        defaults.replicas.clone(),
        defaults.bootstrap_dir.clone(),
    ]);
    for place in [Some(&boot.active), boot.previous.as_ref()]
        .into_iter()
        .flatten()
    {
        if let Place::Folder { root, .. } = place {
            list.push(root.clone());
        }
    }
    list
}

/// Checks a folder chosen as the new data location. A missing folder is
/// created to prove it is writable and removed again unless `keep` is set.
pub fn validate_folder(
    root: &Path,
    allow_removable: bool,
    needed_bytes: u64,
    defaults: &DefaultFolders,
    boot: &Bootstrap,
    probe: &dyn Probe,
    keep: bool,
) -> Result<(), String> {
    if !root.is_absolute() {
        return Err("Choose a complete folder path, such as D:\\Relay Data.".into());
    }
    let network = match root.components().next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::Disk(_) | Prefix::VerbatimDisk(_) => false,
            Prefix::UNC(..) | Prefix::VerbatimUNC(..) => true,
            _ => return Err("Choose a folder on a drive, not a device path.".into()),
        },
        _ if cfg!(windows) => return Err("Choose a folder on a drive.".into()),
        _ => false,
    };
    if root.parent().is_none() || root.file_name().is_none() {
        return Err("Choose a folder, not a whole drive.".into());
    }
    let drive = probe.drive(root);
    match drive.kind {
        DriveKind::Missing => {
            return Err(format!("Drive {} is not available.", drive_label(root)))
        }
        DriveKind::Unsuitable => {
            return Err(format!(
                "Drive {} cannot hold Relay data (CD/DVD, RAM disk or unknown drive).",
                drive_label(root)
            ))
        }
        DriveKind::Removable | DriveKind::Network if !allow_removable => {
            return Err(format!(
                "{} is a {} drive. Relay will not start while it is disconnected. Tick \"Allow a removable or network drive\" if you accept that.",
                drive_label(root),
                if network || drive.kind == DriveKind::Network { "network" } else { "removable" }
            ))
        }
        _ => {}
    }
    if network && !allow_removable {
        return Err("Network folders need \"Allow a removable or network drive\".".into());
    }
    let file_system = drive.file_system.to_ascii_uppercase();
    if file_system != "NTFS" && file_system != "REFS" {
        return Err(format!(
            "Drive {} uses {}. Relay needs NTFS so it can keep your data private to your Windows account.",
            drive_label(root),
            if drive.file_system.is_empty() { "an unknown file system" } else { &drive.file_system }
        ));
    }
    for protected in protected_paths(defaults, boot) {
        if within(root, &protected) || within(&protected, root) {
            return Err(format!(
                "Choose a separate folder. {} overlaps {}.",
                root.display(),
                protected.display()
            ));
        }
    }
    for ancestor in root.ancestors() {
        if let Ok(metadata) = fs::symlink_metadata(ancestor) {
            if is_link(&metadata) {
                return Err(format!(
                    "{} is a link or junction. Choose an ordinary folder.",
                    ancestor.display()
                ));
            }
        }
    }
    for ancestor in root.ancestors().skip(1) {
        if ancestor.join(MARKER).exists()
            || (ancestor.join("config").is_dir() && ancestor.join("encryption").is_dir())
        {
            return Err(format!(
                "{} is inside another Relay profile ({}).",
                root.display(),
                ancestor.display()
            ));
        }
    }
    if root.exists() {
        if !root.is_dir() {
            return Err(format!("{} is a file, not a folder.", root.display()));
        }
        if root.join(MARKER).exists() {
            return Err(format!(
                "{} already contains Relay data. Choose an empty folder; Relay never merges two copies.",
                root.display()
            ));
        }
        if non_empty(root)? {
            return Err(format!(
                "{} is not empty. Choose an empty or new folder.",
                root.display()
            ));
        }
    }
    let margin = (needed_bytes / 10).max(256 * 1024 * 1024);
    if drive.free_bytes < needed_bytes.saturating_add(margin) {
        return Err(format!(
            "Drive {} has {} free; Relay needs about {} for your data.",
            drive_label(root),
            human_size(drive.free_bytes),
            human_size(needed_bytes.saturating_add(margin))
        ));
    }
    writable(root, keep)
}

/// Moving back to the Windows default folders needs them empty (or the kept
/// old copy, which is set aside during the move) and enough space.
pub fn validate_default(
    needed_bytes: u64,
    defaults: &DefaultFolders,
    boot: &Bootstrap,
    probe: &dyn Probe,
) -> Result<(), String> {
    let replacing = boot.previous == Some(Place::Default);
    if !replacing && !Layout::Default(defaults).is_empty()? {
        return Err("The Windows default folders already contain Relay data. Relay never merges two copies.".into());
    }
    let drive = probe.drive(&defaults.local);
    let margin = (needed_bytes / 10).max(256 * 1024 * 1024);
    if drive.free_bytes < needed_bytes.saturating_add(margin) {
        return Err(format!(
            "Drive {} has {} free; Relay needs about {} for your data.",
            drive_label(&defaults.local),
            human_size(drive.free_bytes),
            human_size(needed_bytes.saturating_add(margin))
        ));
    }
    Ok(())
}

fn writable(root: &Path, keep: bool) -> Result<(), String> {
    let created_from = root.ancestors().find(|p| p.exists()).map(Path::to_path_buf);
    let failed = |error: std::io::Error| {
        format!(
            "Relay cannot write to {}: {error}. Check the folder's permissions.",
            root.display()
        )
    };
    let result = (|| {
        fs::create_dir_all(root)?;
        let probe = root.join(format!(".relay-write-test-{}", uuid::Uuid::new_v4()));
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&probe)?;
        let written = file.write_all(b"relay").and_then(|_| file.sync_all());
        drop(file);
        let removed = fs::remove_file(&probe);
        written.and(removed)
    })();
    if result.is_err() || !keep {
        // Undo only folders this check created; each must still be empty.
        if let Some(existing) = created_from {
            let mut folder = root.to_path_buf();
            while folder != existing && fs::remove_dir(&folder).is_ok() {
                let Some(parent) = folder.parent() else { break };
                folder = parent.to_path_buf();
            }
        }
    }
    result.map_err(failed)
}

pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

// ---------------------------------------------------------------------------
// Copy and verify

fn open_source(path: &Path) -> Result<fs::File, String> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // Others may read, nobody may write while the copy is taken.
        options.share_mode(0x1);
    }
    options.open(path).map_err(|error| {
        if error.raw_os_error() == Some(32) {
            format!(
                "{} is in use. Quit Relay completely (including servers hosted on this PC) and try again.",
                path.display()
            )
        } else {
            format!("Cannot read {}: {error}", path.display())
        }
    })
}

fn hash_file(path: &Path) -> Result<(u64, [u8; 32]), String> {
    let mut file =
        fs::File::open(path).map_err(|e| format!("Cannot verify {}: {e}", path.display()))?;
    hash_reader(&mut file, None).map_err(|e| format!("Cannot verify {}: {e}", path.display()))
}

fn hash_reader(
    input: &mut dyn Read,
    mut copy: Option<&mut fs::File>,
) -> std::io::Result<(u64, [u8; 32])> {
    let mut hash = Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    let mut total = 0u64;
    loop {
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hash.update(&buffer[..read]);
        if let Some(output) = copy.as_deref_mut() {
            output.write_all(&buffer[..read])?;
        }
        total += read as u64;
    }
    Ok((total, hash.finalize().into()))
}

/// Copies one file and proves the bytes on disk match what was read.
fn copy_verified(source: &Path, destination: &Path, corrupt: bool) -> Result<(), String> {
    let mut input = open_source(source)?;
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("Cannot create {}: {e}", parent.display()))?;
    }
    let mut output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .map_err(|e| format!("Cannot create {}: {e}", destination.display()))?;
    let expected = hash_reader(&mut input, Some(&mut output))
        .and_then(|value| output.sync_all().map(|_| value))
        .map_err(|e| format!("Cannot copy {}: {e}", source.display()))?;
    drop(output);
    if corrupt {
        let mut bytes = fs::read(destination).unwrap_or_default();
        bytes.push(0);
        let _ = fs::write(destination, bytes);
    }
    if hash_file(destination)? != expected {
        return Err(format!(
            "The copy of {} did not match the original. Nothing was switched.",
            source.display()
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn make_private(root: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows::{
        core::{PCWSTR, PWSTR},
        Win32::{
            Foundation::{CloseHandle, LocalFree, BOOL, HANDLE, HLOCAL},
            Security::{
                Authorization::{
                    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
                    SetNamedSecurityInfoW, SE_FILE_OBJECT,
                },
                GetSecurityDescriptorDacl, GetTokenInformation, TokenUser, ACL,
                DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
                PSECURITY_DESCRIPTOR, PSID, TOKEN_QUERY, TOKEN_USER,
            },
            System::Threading::{GetCurrentProcess, OpenProcessToken},
        },
    };
    const FAILED: &str =
        "Relay could not make the new data folder private to your Windows account.";
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).map_err(|_| FAILED)?;
        let mut needed = 0;
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut needed);
        let mut buffer = vec![
            0usize;
            (needed as usize)
                .div_ceil(std::mem::size_of::<usize>())
                .max(1)
        ];
        let info = GetTokenInformation(
            token,
            TokenUser,
            Some(buffer.as_mut_ptr().cast()),
            needed,
            &mut needed,
        );
        let _ = CloseHandle(token);
        info.map_err(|_| FAILED)?;
        let user = &*buffer.as_ptr().cast::<TOKEN_USER>();
        let mut text = PWSTR::null();
        ConvertSidToStringSidW(user.User.Sid, &mut text).map_err(|_| FAILED)?;
        let sid = text.to_string().map_err(|_| FAILED);
        let _ = LocalFree(HLOCAL(text.0.cast()));
        let sddl: Vec<u16> = format!("D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;{})", sid?)
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl.as_ptr()),
            1,
            &mut descriptor,
            None,
        )
        .map_err(|_| FAILED)?;
        let mut present = BOOL(0);
        let mut defaulted = BOOL(0);
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let result = GetSecurityDescriptorDacl(descriptor, &mut present, &mut dacl, &mut defaulted)
            .map_err(|_| FAILED)
            .and_then(|_| {
                let path: Vec<u16> = root.as_os_str().encode_wide().chain(Some(0)).collect();
                SetNamedSecurityInfoW(
                    PCWSTR(path.as_ptr()),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                    PSID::default(),
                    PSID::default(),
                    Some(dacl),
                    None,
                )
                .ok()
                .map_err(|_| FAILED)
            });
        let _ = LocalFree(HLOCAL(descriptor.0));
        result.map_err(String::from)
    }
}
#[cfg(not(windows))]
fn make_private(_: &Path) -> Result<(), String> {
    Ok(())
}

// ---------------------------------------------------------------------------
// Startup move

/// Test-only fault injection; production passes `Faults::default()`.
#[derive(Default)]
pub struct Faults {
    /// Stop after this many files as if the process had died.
    pub crash_after: Option<usize>,
    /// Fail after this many files with an ordinary error.
    pub fail_after: Option<usize>,
    /// Damage the first copied file before it is verified.
    pub corrupt: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum MoveResult {
    Nothing,
    Moved,
    Failed(String),
    /// Only produced by `Faults::crash_after`.
    Interrupted,
}

const CRASH: &str = "\u{0}simulated crash";

/// Runs a scheduled move. Called once at startup before any storage is opened.
/// `Err` means the bootstrap record itself could not be read or written.
pub fn run_pending(
    defaults: &DefaultFolders,
    probe: &dyn Probe,
    faults: &Faults,
) -> Result<MoveResult, String> {
    let path = defaults.bootstrap();
    let mut boot = load(&path)?;
    let Some(mut pending) = boot.pending.clone() else {
        return Ok(MoveResult::Nothing);
    };
    if pending.target == boot.active {
        boot.pending = None;
        save(&path, &boot)?;
        return Ok(MoveResult::Nothing);
    }
    pending.attempts += 1;
    boot.pending = Some(pending.clone());
    save(&path, &boot)?;
    let (active, target) = (boot.active.clone(), pending.target.clone());
    let source = Layout::of(&active, defaults);
    let destination = Layout::of(&target, defaults);
    let result = if pending.attempts > MAX_ATTEMPTS {
        Err("The move was interrupted several times, so Relay stopped trying. Your data was left where it was.".to_string())
    } else {
        destination.remove_partial(&pending.id).and_then(|_| {
            copy_all(
                &boot,
                &mut pending,
                &source,
                &destination,
                defaults,
                probe,
                faults,
                &path,
            )
        })
    };
    match result {
        Ok(()) => {
            let old = std::mem::replace(&mut boot.active, pending.target.clone());
            boot.previous = Some(old.clone());
            boot.pending = None;
            boot.last_result = Some(Outcome {
                ok: true,
                message: format!(
                    "Your data now lives in {}. The old copy is kept in {} until you delete it.",
                    boot.active.describe(),
                    old.describe()
                ),
            });
            save(&path, &boot)?;
            // Committed: the markers and set-aside stale copies are leftovers now.
            for slot in destination.slots() {
                let _ = fs::remove_file(slot.join(IN_PROGRESS));
            }
            for (_, aside) in &pending.set_aside {
                let _ = fs::remove_dir_all(aside);
            }
            Ok(MoveResult::Moved)
        }
        Err(error) if error == CRASH => Ok(MoveResult::Interrupted),
        Err(error) => {
            let cleanup = destination.remove_partial(&pending.id);
            let mut restore = Ok(());
            for (original, aside) in pending.set_aside.iter().rev() {
                if aside.exists() && !original.exists() {
                    if let Err(e) = fs::rename(aside, original) {
                        restore = Err(format!(
                            "Relay could not put the old copy back from {}: {e}",
                            aside.display()
                        ));
                    }
                }
            }
            let mut message = format!("Relay could not move your data: {error}");
            for problem in [cleanup.err(), restore.err()].into_iter().flatten() {
                message.push(' ');
                message.push_str(&problem);
            }
            boot.pending = None;
            boot.last_result = Some(Outcome {
                ok: false,
                message: message.clone(),
            });
            save(&path, &boot)?;
            Ok(MoveResult::Failed(message))
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn copy_all(
    boot: &Bootstrap,
    pending: &mut Pending,
    source: &Layout,
    destination: &Layout,
    defaults: &DefaultFolders,
    probe: &dyn Probe,
    faults: &Faults,
    bootstrap_path: &Path,
) -> Result<(), String> {
    if let Place::Folder { root, id, .. } = &boot.active {
        check_marker(root, id)?;
    }
    // Moving back onto the kept old copy: that copy is stale, set it aside.
    let replacing = boot.previous.as_ref() == Some(&pending.target);
    if replacing && pending.set_aside.is_empty() && !destination.is_empty()? {
        if let Place::Folder { root, id, .. } = &pending.target {
            check_marker(root, id)?;
        }
        for slot in destination.slots() {
            if !slot.exists() {
                continue;
            }
            let name = slot.file_name().ok_or("Invalid data folder.")?;
            let mut aside_name = OsString::from(name);
            aside_name.push(format!(
                ".relay-old-{}",
                &pending.id[..8.min(pending.id.len())]
            ));
            let aside = slot.with_file_name(aside_name);
            fs::rename(&slot, &aside)
                .map_err(|e| format!("Cannot set the old copy at {} aside: {e}", slot.display()))?;
            pending.set_aside.push((slot, aside));
            let mut saved = boot.clone();
            saved.pending = Some(pending.clone());
            save(bootstrap_path, &saved)?;
        }
    }
    let entries = source.entries()?;
    let needed: u64 = entries.iter().map(|entry| entry.len).sum();
    let mut checked = boot.clone();
    if replacing {
        // The stale copy is aside; it is no longer an obstacle or a reserved path.
        checked.previous = None;
    }
    match &pending.target {
        Place::Default => validate_default(needed, defaults, &checked, probe)?,
        Place::Folder {
            root,
            allow_removable,
            ..
        } => validate_folder(
            root,
            *allow_removable,
            needed,
            defaults,
            &checked,
            probe,
            true,
        )?,
    }
    let marker = serde_json::to_vec(&FolderMarker {
        version: VERSION,
        id: pending.id.clone(),
    })
    .map_err(|_| "Cannot encode the move marker.")?;
    for slot in destination.slots() {
        fs::create_dir_all(&slot).map_err(|e| format!("Cannot create {}: {e}", slot.display()))?;
        if matches!(destination, Layout::Folder(_)) {
            make_private(&slot)?;
        }
        fs::write(slot.join(IN_PROGRESS), &marker)
            .map_err(|e| format!("Cannot write to {}: {e}", slot.display()))?;
    }
    let mut files = 0usize;
    for entry in &entries {
        let target = destination.physical(&entry.logical);
        if entry.dir {
            fs::create_dir_all(&target)
                .map_err(|e| format!("Cannot create {}: {e}", target.display()))?;
            continue;
        }
        if faults.crash_after == Some(files) {
            return Err(CRASH.into());
        }
        if faults.fail_after == Some(files) {
            return Err("simulated failure".into());
        }
        copy_verified(&entry.source, &target, faults.corrupt && files == 0)?;
        files += 1;
    }
    // Prove the destination, read back through its own layout, is the same set.
    let copied = destination.entries()?;
    let summary = |list: &[Entry]| -> Vec<(PathBuf, bool, u64)> {
        list.iter()
            .map(|e| (e.logical.clone(), e.dir, e.len))
            .collect()
    };
    if summary(&copied) != summary(&entries) {
        return Err("The copied folders did not match the original. Nothing was switched.".into());
    }
    if let (Layout::Folder(root), Some(id)) = (destination, pending.target.id()) {
        let bytes = serde_json::to_vec_pretty(&FolderMarker {
            version: VERSION,
            id: id.to_string(),
        })
        .map_err(|_| "Cannot encode the data folder marker.")?;
        let mut file = fs::File::create(root.join(MARKER))
            .map_err(|e| format!("Cannot write to {}: {e}", root.display()))?;
        file.write_all(&bytes)
            .and_then(|_| file.sync_all())
            .map_err(|e| format!("Cannot write to {}: {e}", root.display()))?;
    }
    Ok(())
}

fn check_marker(root: &Path, id: &str) -> Result<(), String> {
    let marker = fs::read(root.join(MARKER))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<FolderMarker>(&bytes).ok());
    match marker {
        Some(marker) if marker.id == id => Ok(()),
        _ => Err(format!(
            "{} does not contain the Relay data that was moved there.",
            root.display()
        )),
    }
}

// ---------------------------------------------------------------------------
// Startup

#[derive(Debug, PartialEq, Eq)]
pub enum Startup {
    DefaultFolders,
    Folder(PathBuf),
}

/// Where this start's data lives. Never creates a missing data folder: an
/// unavailable drive is an error, not a new empty profile.
pub fn resolve(defaults: &DefaultFolders) -> Result<Startup, String> {
    let boot = load(&defaults.bootstrap())?;
    match boot.active {
        Place::Default => Ok(Startup::DefaultFolders),
        Place::Folder { root, id, .. } => {
            if !root.is_dir() {
                let drive_present = drive_root(&root).is_some_and(|drive| drive.exists());
                return Err(if drive_present {
                    format!(
                        "Your Relay data folder {} is missing. Relay will not start with an empty profile. Make the folder available again, then choose Retry.",
                        root.display()
                    )
                } else {
                    format!(
                        "Your Relay data is on drive {}, which is not available. Connect the drive, then choose Retry. Relay will not start with an empty profile.",
                        drive_label(&root)
                    )
                });
            }
            check_marker(&root, &id).map_err(|error| {
                format!("{error} Relay will not start with other or empty data. Restore the folder, then choose Retry.")
            })?;
            Ok(Startup::Folder(root))
        }
    }
}

/// Points this process at a chosen folder using the `--profile-dir` layout.
pub fn apply(root: &Path) -> Result<(), String> {
    for name in ["config", "encryption", "server-data"] {
        fs::create_dir_all(root.join(name)).map_err(|e| format!("Create profile storage: {e}"))?;
    }
    // Leftover of a move that committed just before an interruption.
    let _ = fs::remove_file(root.join(IN_PROGRESS));
    // Called once in main before eframe, devices or network workers exist.
    std::env::set_var("RELAY_CONFIG_DIR", root.join("config"));
    std::env::set_var("RELAY_CRYPTO_DIR", root.join("encryption"));
    std::env::set_var("RELAY_SERVER_DATA_DIR", root.join("server-data"));
    std::env::set_var("RELAY_PROFILE_DIR", root);
    std::env::set_var(LOCATION_ENV, root);
    Ok(())
}

/// A Relay started by this process (for example after an update) re-reads the
/// bootstrap itself; inherited location variables must not make it an isolated profile.
pub fn forget_inherited() {
    let Some(location) = std::env::var_os(LOCATION_ENV) else {
        return;
    };
    if std::env::var_os("RELAY_PROFILE_DIR").as_ref() == Some(&location) {
        for name in PROFILE_VARIABLES {
            std::env::remove_var(name);
        }
    }
    std::env::remove_var(LOCATION_ENV);
}

/// Saved logins of the normal profile keep their names wherever its data lives;
/// only an explicit isolated profile gets path-scoped vault entries.
pub fn credential_scope(profile: Option<OsString>, location: Option<OsString>) -> Option<OsString> {
    match (profile, location) {
        (Some(profile), Some(location)) if profile == location => None,
        (profile, _) => profile,
    }
}

static MANAGED: std::sync::OnceLock<DefaultFolders> = std::sync::OnceLock::new();

/// The default folders when this process uses the user's normal profile;
/// `None` for an isolated `--profile-dir` or environment override.
pub fn managed() -> Option<&'static DefaultFolders> {
    MANAGED.get()
}

/// Startup for the normal profile: run a scheduled move (only for the single
/// owner instance), then resolve the location, asking to retry while it is unavailable.
pub fn startup(allow_move: bool) -> Result<(), String> {
    let defaults = DefaultFolders::current()?;
    if allow_move {
        if let MoveResult::Failed(message) =
            run_pending(&defaults, &SystemProbe, &Faults::default())?
        {
            notify(&format!(
                "{message}\n\nRelay will start from your existing data."
            ));
        }
    }
    loop {
        match resolve(&defaults) {
            Ok(Startup::DefaultFolders) => break,
            Ok(Startup::Folder(root)) => {
                apply(&root)?;
                break;
            }
            Err(message) => {
                if !ask_retry(&message) {
                    return Err(message);
                }
            }
        }
    }
    let _ = MANAGED.set(defaults);
    Ok(())
}

#[cfg(windows)]
fn message_box(
    text: &str,
    style: windows::Win32::UI::WindowsAndMessaging::MESSAGEBOX_STYLE,
) -> windows::Win32::UI::WindowsAndMessaging::MESSAGEBOX_RESULT {
    use windows::{core::PCWSTR, Win32::UI::WindowsAndMessaging::MessageBoxW};
    let text: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
    let title: Vec<u16> = "Relay data location"
        .encode_utf16()
        .chain(Some(0))
        .collect();
    unsafe { MessageBoxW(None, PCWSTR(text.as_ptr()), PCWSTR(title.as_ptr()), style) }
}

fn ask_retry(message: &str) -> bool {
    eprintln!("{message}");
    #[cfg(windows)]
    {
        use windows::Win32::UI::WindowsAndMessaging::{IDRETRY, MB_ICONWARNING, MB_RETRYCANCEL};
        message_box(message, MB_RETRYCANCEL | MB_ICONWARNING) == IDRETRY
    }
    #[cfg(not(windows))]
    false
}

fn notify(message: &str) {
    eprintln!("{message}");
    #[cfg(windows)]
    {
        use windows::Win32::UI::WindowsAndMessaging::{MB_ICONWARNING, MB_OK};
        message_box(message, MB_OK | MB_ICONWARNING);
    }
}

// ---------------------------------------------------------------------------
// Settings actions (normal profile only)

/// Schedules a move to `target`; it runs at the next start.
pub fn schedule(defaults: &DefaultFolders, target: Place) -> Result<(), String> {
    let path = defaults.bootstrap();
    let mut boot = load(&path)?;
    if boot.pending.is_some() {
        return Err("A move is already scheduled.".into());
    }
    if target == boot.active {
        return Err("Your data is already there.".into());
    }
    if boot.previous.is_some() && boot.previous.as_ref() != Some(&target) {
        return Err(
            "Delete or forget the old copy first. Relay keeps only one old copy at a time.".into(),
        );
    }
    let id = target
        .id()
        .map(str::to_string)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    boot.pending = Some(Pending {
        target,
        id,
        attempts: 0,
        set_aside: Vec::new(),
    });
    boot.last_result = None;
    save(&path, &boot)
}

pub fn new_folder_place(root: PathBuf, allow_removable: bool) -> Place {
    Place::Folder {
        root,
        id: uuid::Uuid::new_v4().to_string(),
        allow_removable,
    }
}

pub fn cancel(defaults: &DefaultFolders) -> Result<(), String> {
    let path = defaults.bootstrap();
    let mut boot = load(&path)?;
    boot.pending = None;
    save(&path, &boot)
}

pub fn dismiss_result(defaults: &DefaultFolders) -> Result<(), String> {
    let path = defaults.bootstrap();
    let mut boot = load(&path)?;
    boot.last_result = None;
    save(&path, &boot)
}

/// Stops tracking the old copy without deleting it (for example on a drive
/// that is gone). Its files stay where they are.
pub fn forget_previous(defaults: &DefaultFolders) -> Result<(), String> {
    let path = defaults.bootstrap();
    let mut boot = load(&path)?;
    if boot.pending.is_some() {
        return Err("Cancel the scheduled move first.".into());
    }
    boot.previous = None;
    save(&path, &boot)
}

/// Permanently deletes the kept old copy after the user confirmed it.
pub fn delete_previous(defaults: &DefaultFolders) -> Result<(), String> {
    let path = defaults.bootstrap();
    let mut boot = load(&path)?;
    if boot.pending.is_some() {
        return Err("Cancel the scheduled move first.".into());
    }
    let Some(previous) = boot.previous.clone() else {
        return Ok(());
    };
    if previous == boot.active {
        return Err("The old copy is the data Relay is using now.".into());
    }
    if let Place::Folder { root, id, .. } = &previous {
        if root.exists() {
            check_marker(root, id)?;
        }
    }
    for slot in Layout::of(&previous, defaults).slots() {
        // Never delete what the running profile uses.
        if let Place::Folder { root, .. } = &boot.active {
            if within(&slot, root) || within(root, &slot) {
                return Err("The old copy overlaps the data Relay is using now.".into());
            }
        }
        match fs::remove_dir_all(&slot) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "Relay could not delete {} completely: {error}",
                    slot.display()
                ))
            }
        }
    }
    boot.previous = None;
    save(&path, &boot)
}

#[cfg(test)]
#[path = "data_location_tests.rs"]
mod tests;
