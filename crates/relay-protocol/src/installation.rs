//! Read-only startup inspection of public installer records.
//!
//! A marker records installation ownership and a rollback floor. Its UUID and
//! digest are syntax-checked here, never accepted as updater publisher authority.
//! Completion means no pending/temporary records remain. Startup does not hash
//! the payload, repair records, choose a profile, or launch another process.
use std::{
    fs::File,
    io::Read,
    path::{Component, Path, PathBuf},
};

#[cfg(any(test, target_os = "linux"))]
use std::fs;
#[cfg(windows)]
use std::fs::OpenOptions;

pub const MAX_RECORD_BYTES: usize = 4096;
const MAX_RELEASE_VERSION_BYTES: usize = 64;
const MARKER: &str = "relay-installation.ini";
const INCOMPLETE: [&str; 3] = [
    "relay-install-pending.ini",
    "relay-install-pending.tmp",
    "relay-installation.tmp",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallationRecord {
    pub app_id: String,
    pub version: String,
    pub sequence: String,
    pub payload_sha256: String,
    pub schema: Option<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallationState {
    Ready { record: Option<InstallationRecord> },
    RepairRequired { reason: &'static str },
}
impl InstallationState {
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready { .. })
    }
    /// Stable first word and a public, path-free explanation for diagnostics.
    pub fn diagnostic(&self) -> String {
        match self {
            Self::Ready { .. } => "ready".into(),
            Self::RepairRequired { reason } => format!("repair-required: {reason}"),
        }
    }
    fn repair(reason: &'static str) -> Self {
        Self::RepairRequired { reason }
    }
}

/// This is the first application operation after collecting command-line args.
/// No environment/profile override can relocate the public installation records.
pub fn inspect_startup(args: &[String]) -> InstallationState {
    if let Err(reason) = wait_from_args(args) {
        return InstallationState::repair(reason);
    }
    let Ok(executable) = std::env::current_exe() else {
        return InstallationState::repair("Cannot locate the running application.");
    };
    inspect_executable(&executable)
}

fn inspect_executable(executable: &Path) -> InstallationState {
    let Some(directory) = executable.parent() else {
        return InstallationState::repair("Cannot locate the installation directory.");
    };
    let local = inspect_directory(directory);
    if !local.is_ready()
        || !directory
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.eq_ignore_ascii_case("dist"))
    {
        return local;
    }
    // Current packages keep all executable helpers in dist, while the installer
    // owns the enclosing package. Check both locations so root-level pending
    // records block startup and older flat/dist installations remain supported.
    let Some(package) = directory.parent() else {
        return local;
    };
    match inspect_directory(package) {
        InstallationState::Ready { record: None } => local,
        state => state,
    }
}

/// Inspects only the supplied public installation directory; never mutates it.
/// Also used by isolated fixtures, without changing the application's path policy.
pub fn inspect_directory(path: &Path) -> InstallationState {
    let directory = match Directory::open(path) {
        Ok(directory) => directory,
        Err(_) => return InstallationState::repair("Unsafe or unreadable installation directory."),
    };
    let inspect = || -> Result<Option<InstallationRecord>, &'static str> {
        incomplete(&directory)?;
        let record = match directory.record() {
            Ok(Some(bytes)) => Some(parse_record(&bytes)?),
            Ok(None) => None, // Unmanaged/development and older installations remain supported.
            Err(_) => return Err("Unsafe or unreadable installation record."),
        };
        // Catch an installer beginning while the small public record was read.
        incomplete(&directory)?;
        directory
            .unchanged()
            .map_err(|_| "Installation directory changed during inspection.")?;
        Ok(record)
    };
    match inspect() {
        Ok(record) => InstallationState::Ready { record },
        Err(reason) => InstallationState::repair(reason),
    }
}
fn incomplete(directory: &Directory) -> Result<(), &'static str> {
    for name in INCOMPLETE {
        match directory.artifact_present(name) {
            Ok(false) => {}
            // Presence itself is sufficient, even for a directory, link, or invalid INI.
            Ok(true) => return Err("An installation is incomplete."),
            Err(_) => return Err("Unsafe or unreadable pending installation state."),
        }
    }
    Ok(())
}

/// Strict, bounded ASCII INI: one section, required known fields exactly once,
/// and a final line terminator so a truncated final record is not accepted.
pub fn parse_record(bytes: &[u8]) -> Result<InstallationRecord, &'static str> {
    if bytes.is_empty() || bytes.len() > MAX_RECORD_BYTES || !bytes.ends_with(b"\n") {
        return Err("Installation record is empty, truncated, or too large.");
    }
    if !bytes.is_ascii() || bytes.contains(&0) {
        return Err("Installation record has invalid encoding.");
    }
    let text =
        std::str::from_utf8(bytes).map_err(|_| "Installation record has invalid encoding.")?;
    let mut lines = text.split_terminator('\n');
    if lines
        .next()
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        != Some("[Relay]")
    {
        return Err("Installation record has an invalid section.");
    }
    let mut app_id = None;
    let mut version = None;
    let mut sequence = None;
    let mut payload_sha256 = None;
    let mut schema = None;
    for raw in lines {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        let (key, value) = line
            .split_once('=')
            .ok_or("Installation record has a malformed field.")?;
        let slot = match key {
            "AppId" => &mut app_id,
            "Version" => &mut version,
            "Sequence" => &mut sequence,
            "PayloadSHA256" => &mut payload_sha256,
            "Schema" => &mut schema,
            _ => return Err("Installation record has an unknown field."),
        };
        if slot.replace(value).is_some() {
            return Err("Installation record has a duplicate field.");
        }
    }
    let app_id = app_id
        .filter(|value| uuid_syntax(value))
        .ok_or("Installation record has an invalid AppId.")?;
    let version = version
        .filter(|value| release_version_syntax(value))
        .ok_or("Installation record has an invalid Version.")?;
    let sequence = sequence
        .filter(|value| value.len() == 14 && value.bytes().all(|byte| byte.is_ascii_digit()))
        .ok_or("Installation record has an invalid Sequence.")?;
    let payload_sha256 = payload_sha256
        .filter(|value| value.len() == 64 && value.bytes().all(lower_hex))
        .ok_or("Installation record has an invalid PayloadSHA256.")?;
    if schema.is_some_and(|value| value != "1") {
        return Err("Installation record has an unsupported Schema.");
    }
    Ok(InstallationRecord {
        app_id: app_id.into(),
        version: version.into(),
        sequence: sequence.into(),
        payload_sha256: payload_sha256.into(),
        schema: schema.map(|_| 1),
    })
}
/// Installer release grammar shared with Recovery.iss and the release builder:
/// at most 64 ASCII bytes; three u64 decimal components without leading zeros;
/// optional SemVer prerelease identifiers. Build metadata is excluded, matching
/// the updater's stable/beta release policy. This checks syntax, not authority
/// or ordering: only the installer sequence/hash establishes its rollback floor.
fn release_version_syntax(value: &str) -> bool {
    if value.is_empty() || value.len() > MAX_RELEASE_VERSION_BYTES || !value.is_ascii() {
        return false;
    }
    let (core, prerelease) = match value.split_once('-') {
        Some((core, prerelease)) => (core, Some(prerelease)),
        None => (value, None),
    };
    let mut components = core.split('.');
    for _ in 0..3 {
        let Some(component) = components.next() else {
            return false;
        };
        if component.is_empty()
            || !component.bytes().all(|byte| byte.is_ascii_digit())
            || component.len() > 1 && component.starts_with('0')
            || component.parse::<u64>().is_err()
        {
            return false;
        }
    }
    if components.next().is_some() {
        return false;
    }
    prerelease.is_none_or(|prerelease| {
        prerelease.split('.').all(|identifier| {
            !identifier.is_empty()
                && identifier
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                && !(identifier.len() > 1
                    && identifier.starts_with('0')
                    && identifier.bytes().all(|byte| byte.is_ascii_digit()))
        })
    })
}
fn lower_hex(byte: u8) -> bool {
    byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
}
fn uuid_syntax(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                lower_hex(byte)
            }
        })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProcessIdentity {
    pid: u32,
    creation_low: u32,
    creation_high: u32,
}
impl ProcessIdentity {
    fn parse(value: &str) -> Result<Self, &'static str> {
        if value.len() > 42 {
            return Err("Invalid installer process identity.");
        }
        let parts: Vec<_> = value.split('-').collect();
        if parts.len() != 3
            || parts.iter().any(|part| {
                part.is_empty()
                    || !part.bytes().all(|byte| byte.is_ascii_digit())
                    || (part.len() > 1 && part.starts_with('0'))
            })
        {
            return Err("Invalid installer process identity.");
        }
        let identity = Self {
            pid: parts[0]
                .parse()
                .map_err(|_| "Invalid installer process identity.")?,
            creation_low: parts[1]
                .parse()
                .map_err(|_| "Invalid installer process identity.")?,
            creation_high: parts[2]
                .parse()
                .map_err(|_| "Invalid installer process identity.")?,
        };
        if identity.pid == 0 || identity.creation_low == 0 && identity.creation_high == 0 {
            return Err("Invalid installer process identity.");
        }
        Ok(identity)
    }
    #[cfg(any(windows, test))]
    fn matches_creation(self, low: u32, high: u32) -> bool {
        self.creation_low == low && self.creation_high == high
    }
}
fn wait_from_args(args: &[String]) -> Result<(), &'static str> {
    let mut identity = None;
    for argument in args.iter().skip(1) {
        if argument.starts_with("--wait-for-installer") {
            let value = argument
                .strip_prefix("--wait-for-installer=")
                .ok_or("Invalid installer wait argument.")?;
            if identity.replace(ProcessIdentity::parse(value)?).is_some() {
                return Err("Duplicate installer wait argument.");
            }
        }
    }
    match identity {
        Some(identity) => wait_for_process(identity),
        None => Ok(()),
    }
}
#[cfg(windows)]
fn wait_for_process(identity: ProcessIdentity) -> Result<(), &'static str> {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, FILETIME, WAIT_OBJECT_0},
        System::Threading::{
            GetProcessTimes, OpenProcess, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION,
        },
    };
    // SYNCHRONIZE and limited query only: never terminate, inject, or grant trust.
    let handle = unsafe {
        OpenProcess(
            0x0010_0000 | PROCESS_QUERY_LIMITED_INFORMATION,
            0,
            identity.pid,
        )
    };
    if handle.is_null() {
        return Err("Cannot observe the requested installer process.");
    }
    let result = (|| {
        let mut times = [FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        }; 4];
        let [creation, exit, kernel, user] = &mut times;
        if unsafe { GetProcessTimes(handle, creation, exit, kernel, user) } == 0
            || !identity.matches_creation(creation.dwLowDateTime, creation.dwHighDateTime)
        {
            return Err("Installer process identity did not match.");
        }
        if unsafe { WaitForSingleObject(handle, 10_000) } != WAIT_OBJECT_0 {
            return Err("The installer has not exited.");
        }
        Ok(())
    })();
    unsafe { CloseHandle(handle) };
    result
}
#[cfg(not(windows))]
fn wait_for_process(_: ProcessIdentity) -> Result<(), &'static str> {
    Err("Installer process waiting is unavailable on this platform.")
}

struct Directory {
    #[cfg(any(windows, target_os = "linux"))]
    handles: Vec<(PathBuf, File)>,
}
impl Directory {
    #[cfg(windows)]
    fn open(path: &Path) -> std::io::Result<Self> {
        use std::os::windows::{ffi::OsStrExt, fs::OpenOptionsExt};
        use windows_sys::Win32::Storage::FileSystem::{
            GetDriveTypeW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
            FILE_READ_ATTRIBUTES, FILE_SHARE_READ,
        };
        direct_absolute(path)?;
        let Some(Component::Prefix(prefix)) = path.components().next() else {
            return Err(unsafe_path());
        };
        let letter = match prefix.kind() {
            std::path::Prefix::Disk(letter) | std::path::Prefix::VerbatimDisk(letter) => letter,
            _ => return Err(unsafe_path()),
        };
        let root = format!("{}:\\", letter as char);
        let wide: Vec<u16> = std::ffi::OsStr::new(&root)
            .encode_wide()
            .chain(Some(0))
            .collect();
        // Ordinary fixed local drives are supported at runtime, never mapped shares.
        if unsafe { GetDriveTypeW(wide.as_ptr()) } != 3 {
            return Err(unsafe_path());
        }
        let mut handles = Vec::new();
        for ancestor in path.ancestors().collect::<Vec<_>>().into_iter().rev() {
            let file = OpenOptions::new()
                .access_mode(FILE_READ_ATTRIBUTES)
                // Pin the directory names, and refuse concurrent reparse modification.
                .share_mode(FILE_SHARE_READ)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
                .open(ancestor)?;
            windows_information(&file, true)?;
            handles.push((ancestor.to_owned(), file));
        }
        Ok(Self { handles })
    }
    #[cfg(target_os = "linux")]
    fn open(path: &Path) -> std::io::Result<Self> {
        direct_absolute(path)?;
        let root = linux_openat(-100, std::ffi::OsStr::new("/"), true)?;
        let mut handles = vec![(PathBuf::from("/"), root)];
        for component in path.components() {
            if let Component::Normal(name) = component {
                use std::os::fd::AsRawFd;
                let parent = handles.last().ok_or_else(unsafe_path)?;
                let file = linux_openat(parent.1.as_raw_fd(), name, true)?;
                let next = parent.0.join(name);
                handles.push((next, file));
            }
        }
        Ok(Self { handles })
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    fn open(_: &Path) -> std::io::Result<Self> {
        Err(unsafe_path())
    }

    #[cfg(windows)]
    fn open_name(&self, name: &str, attributes_only: bool) -> std::io::Result<File> {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ,
        };
        let mut options = OpenOptions::new();
        if attributes_only {
            options.access_mode(FILE_READ_ATTRIBUTES);
        } else {
            options.read(true);
        }
        options
            .share_mode(FILE_SHARE_READ)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(self.handles.last().ok_or_else(unsafe_path)?.0.join(name))
    }
    #[cfg(target_os = "linux")]
    fn open_name(&self, name: &str, _: bool) -> std::io::Result<File> {
        use std::os::fd::AsRawFd;
        linux_openat(
            self.handles.last().ok_or_else(unsafe_path)?.1.as_raw_fd(),
            std::ffi::OsStr::new(name),
            false,
        )
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    fn open_name(&self, _: &str, _: bool) -> std::io::Result<File> {
        Err(unsafe_path())
    }

    fn artifact_present(&self, name: &str) -> std::io::Result<bool> {
        match self.open_name(name, true) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }
    fn record(&self) -> std::io::Result<Option<Vec<u8>>> {
        let file = match self.open_name(MARKER, false) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        #[cfg(windows)]
        windows_information(&file, false)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let info = file.metadata()?;
            if !info.is_file() || info.nlink() != 1 {
                return Err(unsafe_path());
            }
        }
        if file.metadata()?.len() > MAX_RECORD_BYTES as u64 {
            return Err(unsafe_path());
        }
        let mut bytes = Vec::with_capacity(MAX_RECORD_BYTES + 1);
        file.take(MAX_RECORD_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > MAX_RECORD_BYTES {
            return Err(unsafe_path());
        }
        Ok(Some(bytes))
    }
    fn unchanged(&self) -> std::io::Result<()> {
        #[cfg(windows)]
        for (_, handle) in &self.handles {
            windows_information(handle, true)?;
        }
        #[cfg(target_os = "linux")]
        for (path, handle) in &self.handles {
            use std::os::unix::fs::MetadataExt;
            let named = fs::symlink_metadata(path)?;
            let opened = handle.metadata()?;
            if !named.is_dir() || named.dev() != opened.dev() || named.ino() != opened.ino() {
                return Err(unsafe_path());
            }
        }
        Ok(())
    }
}
fn direct_absolute(path: &Path) -> std::io::Result<()> {
    if !path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
    {
        return Err(unsafe_path());
    }
    #[cfg(windows)]
    for component in path.components() {
        if let Component::Normal(name) = component {
            let Some(name) = name.to_str() else {
                return Err(unsafe_path());
            };
            if name.ends_with('.')
                || name.ends_with(' ')
                || name
                    .chars()
                    .any(|character| matches!(character, ':' | '*' | '?' | '\0'))
            {
                return Err(unsafe_path());
            }
        }
    }
    Ok(())
}
fn unsafe_path() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "Unsafe public installation path or record",
    )
}
#[cfg(windows)]
fn windows_information(file: &File, directory: bool) -> std::io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, GetFileType, BY_HANDLE_FILE_INFORMATION,
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_TYPE_DISK,
    };
    let handle = file.as_raw_handle() as _;
    let mut info = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::zeroed();
    if unsafe { GetFileInformationByHandle(handle, info.as_mut_ptr()) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let info = unsafe { info.assume_init() };
    if unsafe { GetFileType(handle) } != FILE_TYPE_DISK
        || info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || (info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0) != directory
        || (!directory && info.nNumberOfLinks != 1)
    {
        return Err(unsafe_path());
    }
    Ok(())
}
#[cfg(target_os = "linux")]
fn linux_openat(parent: i32, name: &std::ffi::OsStr, directory: bool) -> std::io::Result<File> {
    // These flags use the Linux x86/x64/AArch64 ABI; other architectures refuse
    // inspection rather than guessing O_NOFOLLOW values.
    #[cfg(not(any(target_arch = "x86", target_arch = "x86_64", target_arch = "aarch64")))]
    {
        let _ = (parent, name, directory);
        return Err(unsafe_path());
    }
    #[cfg(any(target_arch = "x86", target_arch = "x86_64", target_arch = "aarch64"))]
    {
        use std::os::{fd::FromRawFd, unix::ffi::OsStrExt};
        // Linux ABI constants: O_RDONLY | O_NONBLOCK | O_NOFOLLOW | O_CLOEXEC,
        // plus O_DIRECTORY for the anchored ancestor walk. No symlink is followed.
        const SAFE_READ: i32 = 0x800 | 0x20000 | 0x80000;
        extern "C" {
            fn openat(dirfd: i32, path: *const std::ffi::c_char, flags: i32, ...) -> i32;
        }
        let name = std::ffi::CString::new(name.as_bytes()).map_err(|_| unsafe_path())?;
        let raw = unsafe {
            openat(
                parent,
                name.as_ptr(),
                SAFE_READ | if directory { 0x10000 } else { 0 },
            )
        };
        if raw < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(unsafe { File::from_raw_fd(raw) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn marker(schema: bool) -> String {
        format!("[Relay]\r\n{}AppId=12345678-1234-1234-1234-123456789abc\r\nVersion=0.10.0\r\nSequence=20261009123456\r\nPayloadSHA256={}\r\n",
            if schema { "Schema=1\r\n" } else { "" }, "a".repeat(64))
    }
    #[test]
    fn legacy_and_schema_one_records_are_valid() {
        let legacy = parse_record(marker(false).as_bytes()).unwrap();
        assert_eq!(legacy.schema, None);
        assert_eq!(legacy.sequence, "20261009123456");
        assert_eq!(
            parse_record(marker(true).as_bytes()).unwrap().schema,
            Some(1)
        );
        assert!(parse_record(marker(true).replace("\r\n", "\n").as_bytes()).is_ok());
        // Lexical fixture UUID validity does not establish publisher trust.
        assert!(uuid_syntax("12345678-1234-1234-1234-123456789abc"));
    }
    #[test]
    fn old_future_and_prerelease_versions_do_not_define_marker_authority() {
        for schema in [false, true] {
            for version in [
                "0.9.9",
                "0.10.0",
                "0.10.1",
                "1.0.0-beta.1",
                "2.0.0-rc.0.alpha-1",
                "18446744073709551615.0.0",
            ] {
                let bytes = marker(schema).replace("Version=0.10.0", &format!("Version={version}"));
                let parsed = parse_record(bytes.as_bytes()).unwrap();
                assert_eq!(parsed.version, version);
                assert_eq!(parsed.sequence, "20261009123456");
                assert_eq!(parsed.payload_sha256, "a".repeat(64));
                assert_eq!(parsed.schema, schema.then_some(1));
            }
        }
        let longest = format!("1.2.3-{}", "a".repeat(MAX_RELEASE_VERSION_BYTES - 6));
        assert!(release_version_syntax(&longest));
        assert!(!release_version_syntax(&format!("{longest}a")));
    }
    #[test]
    fn release_version_grammar_refuses_ambiguous_or_unbounded_versions() {
        for version in [
            "",
            "0.10",
            "0.10.1.0",
            "v0.10.1",
            "01.10.1",
            "0.010.1",
            "0.10.01",
            "18446744073709551616.0.0",
            "0.10.1-",
            "0.10.1-beta..1",
            "0.10.1-.beta",
            "0.10.1-beta.",
            "0.10.1-beta.01",
            "0.10.1-beta_1",
            "0.10.1+build.1",
            "0.10.1-beta.1+build.1",
            "0.10.1 beta",
            "0.10.1\r",
            "0.10.1-\u{00e9}",
        ] {
            assert!(!release_version_syntax(version), "accepted {version:?}");
            let record = marker(true).replace("Version=0.10.0", &format!("Version={version}"));
            assert!(
                parse_record(record.as_bytes()).is_err(),
                "accepted {version:?}"
            );
        }
    }
    #[test]
    fn malformed_duplicate_unknown_and_truncated_records_are_refused() {
        let valid = marker(true);
        for bad in [
            String::new(),
            valid.trim_end().to_owned(),
            valid.replace("[Relay]", "[Other]"),
            valid.replace("[Relay]", "[Relay]\r\n[Relay]"),
            valid.replace("Schema=1", "Schema=2"),
            valid.replace("Schema=1", "Schema=1\r\nSchema=1"),
            valid.replace("Version=0.10.0", "Version=0.10.0\r\nVersion=0.10.0"),
            valid.replace("Version=0.10.0", "Version=0.10.01"),
            valid.replace("Version=0.10.0", "Version =0.10.0"),
            valid.replace("Version=0.10.0", "Version=0.10.0\r\nUnknown=1"),
            valid.replace("Version=0.10.0\r\n", ""),
            valid.replace(
                "AppId=12345678-1234-1234-1234-123456789abc",
                "AppId=12345678-1234-1234-1234-123456789abC",
            ),
            valid.replace(
                "12345678-1234-1234-1234-123456789abc",
                "123456781234-1234-1234-123456789abc",
            ),
            valid.replace("Sequence=20261009123456", "Sequence=2026100912345"),
            valid.replace("Sequence=20261009123456", "Sequence=2026100912345x"),
            valid.replace("PayloadSHA256=", "PayloadSHA256=A"),
            valid.replace(&"a".repeat(64), &"a".repeat(63)),
            valid.replace("Version=", "Version=\0"),
            valid.replace("Version=", "Version=é"),
            format!("{valid}\r\n"),
            format!("{valid}{}\n", "x".repeat(MAX_RECORD_BYTES)),
        ] {
            assert!(
                parse_record(bad.as_bytes()).is_err(),
                "accepted malformed fixture: {bad:?}"
            );
        }
        for size in [MAX_RECORD_BYTES, MAX_RECORD_BYTES + 1] {
            let mut bytes = vec![b'x'; size];
            *bytes.last_mut().unwrap() = b'\n';
            assert!(parse_record(&bytes).is_err());
        }
    }
    #[test]
    fn installer_identity_is_bounded_exact_decimal_and_creation_scoped() {
        let identity = ProcessIdentity::parse("123-4294967295-1").unwrap();
        assert!(identity.matches_creation(u32::MAX, 1));
        assert!(!identity.matches_creation(u32::MAX - 1, 1));
        assert!(!identity.matches_creation(u32::MAX, 2));
        assert!(ProcessIdentity::parse("4294967295-0-4294967295").is_ok());
        for value in [
            "",
            "0-1-1",
            "1-0-0",
            "1-2",
            "1-2-3-4",
            "+1-2-3",
            "1-2- 3",
            "01-2-3",
            "1-02-3",
            "1-2-x",
            "4294967296-1-1",
            "1-4294967296-1",
            "1-1-4294967296",
        ] {
            assert!(ProcessIdentity::parse(value).is_err(), "accepted {value:?}");
        }
        assert!(ProcessIdentity::parse(&"1".repeat(43)).is_err());
        assert!(wait_from_args(&["relay".into()]).is_ok());
        for arguments in [
            vec!["relay", "--wait-for-installer", "1-2-3"],
            vec!["relay", "--wait-for-installer-extra=1-2-3"],
            vec!["relay", "--wait-for-installer=bad"],
            vec![
                "relay",
                "--wait-for-installer=1-2-3",
                "--wait-for-installer=1-2-3",
            ],
        ] {
            let arguments: Vec<String> = arguments.into_iter().map(str::to_owned).collect();
            // These cases are rejected during parsing, without opening a process.
            assert!(wait_from_args(&arguments).is_err());
        }
    }

    #[test]
    fn direct_paths_refuse_relative_and_parent_components() {
        assert!(direct_absolute(Path::new("relative-installation")).is_err());
        #[cfg(windows)]
        for path in [
            r"C:\fixture\..\installation",
            r"C:\fixture.\installation",
            r"C:\fixture:stream\installation",
        ] {
            assert!(direct_absolute(Path::new(path)).is_err());
        }
        #[cfg(target_os = "linux")]
        assert!(direct_absolute(Path::new("/fixture/../installation")).is_err());
    }

    /// All filesystem fixtures are under an explicitly supplied, pre-existing,
    /// direct local test root. There is no system temp/profile fallback or cleanup
    /// recursion: fixtures remain available for the parent acceptance evidence.
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let root = std::env::var_os("RELAY_TEST_WORK_ROOT")
                .map(PathBuf::from)
                .expect("Set RELAY_TEST_WORK_ROOT to an isolated existing local fixture directory");
            #[cfg(windows)]
            {
                let Some(Component::Prefix(prefix)) = root.components().next() else {
                    panic!("Use local C:/D: fixtures");
                };
                assert!(
                    matches!(
                        prefix.kind(),
                        std::path::Prefix::Disk(b'C' | b'c' | b'D' | b'd')
                            | std::path::Prefix::VerbatimDisk(b'C' | b'c' | b'D' | b'd')
                    ),
                    "Use local C:/D: fixtures"
                );
            }
            let _guard = Directory::open(&root)
                .expect("Fixture root must be direct, local, and free of redirects");
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let name = format!(
                "installation-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            );
            let path = root.join(name);
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn write(&self, name: &str, bytes: &[u8]) {
            fs::write(self.0.join(name), bytes).unwrap();
        }
        fn inspect(&self) -> InstallationState {
            inspect_directory(&self.0)
        }
    }
    #[test]
    fn dist_executables_observe_package_repair_state_and_keep_flat_compatibility() {
        let fixture = Fixture::new();
        let dist = fixture.0.join("dist");
        fs::create_dir(&dist).unwrap();
        let exe = dist.join("relay.exe");
        assert!(inspect_executable(&exe).is_ready());
        fixture.write(MARKER, marker(true).as_bytes());
        assert!(matches!(
            inspect_executable(&exe),
            InstallationState::Ready { record: Some(_) }
        ));
        for name in INCOMPLETE {
            fixture.write(name, marker(true).as_bytes());
            assert!(!inspect_executable(&exe).is_ready());
            fs::remove_file(fixture.0.join(name)).unwrap();
            fs::write(dist.join(name), marker(true)).unwrap();
            assert!(!inspect_executable(&exe).is_ready());
            fs::remove_file(dist.join(name)).unwrap();
        }
        fixture.write(MARKER, b"invalid");
        assert!(!inspect_executable(&exe).is_ready());
        assert!(!inspect_executable(&fixture.0.join("relay.exe")).is_ready());
        fs::remove_file(fixture.0.join(MARKER)).unwrap();
        fs::write(dist.join(MARKER), marker(true)).unwrap();
        assert!(matches!(
            inspect_executable(&exe),
            InstallationState::Ready { record: Some(_) }
        ));
    }

    #[test]
    fn absent_legacy_and_new_public_installations_are_ready() {
        let fixture = Fixture::new();
        assert_eq!(fixture.inspect(), InstallationState::Ready { record: None });
        fixture.write(MARKER, marker(false).as_bytes());
        assert!(fixture.inspect().is_ready());
        fixture.write(MARKER, marker(true).as_bytes());
        assert!(fixture.inspect().is_ready());
        fixture.write(
            MARKER,
            marker(true)
                .replace("Version=0.10.0", "Version=0.10.1")
                .as_bytes(),
        );
        assert!(fixture.inspect().is_ready());
    }
    #[test]
    fn future_release_markers_do_not_bypass_pending_state() {
        for version in ["0.10.1", "0.11.0-beta.1"] {
            let record = marker(true).replace("Version=0.10.0", &format!("Version={version}"));
            for name in INCOMPLETE {
                let fixture = Fixture::new();
                fixture.write(MARKER, record.as_bytes());
                fixture.write(name, record.as_bytes());
                assert!(!fixture.inspect().is_ready());
                assert_eq!(fs::read(fixture.0.join(MARKER)).unwrap(), record.as_bytes());
                assert_eq!(fs::read(fixture.0.join(name)).unwrap(), record.as_bytes());
            }
        }
    }
    #[test]
    fn every_pending_or_temporary_artifact_refuses_without_mutation() {
        for name in INCOMPLETE {
            let fixture = Fixture::new();
            fixture.write(MARKER, marker(true).as_bytes());
            fixture.write(name, b"incomplete\0record");
            assert!(!fixture.inspect().is_ready());
            assert_eq!(
                fs::read(fixture.0.join(name)).unwrap(),
                b"incomplete\0record"
            );
            assert_eq!(
                fs::read(fixture.0.join(MARKER)).unwrap(),
                marker(true).as_bytes()
            );
            let directory = Fixture::new();
            fs::create_dir(directory.0.join(name)).unwrap();
            assert!(!directory.inspect().is_ready());
            assert!(directory.0.join(name).is_dir());
            let linked = Fixture::new();
            linked.write("source", b"owned fixture");
            fs::hard_link(linked.0.join("source"), linked.0.join(name)).unwrap();
            assert!(!linked.inspect().is_ready());
            assert_eq!(fs::read(linked.0.join("source")).unwrap(), b"owned fixture");
        }
    }
    #[test]
    fn unsafe_or_malformed_marker_refuses() {
        let directory = Fixture::new();
        fs::create_dir(directory.0.join(MARKER)).unwrap();
        assert!(!directory.inspect().is_ready());
        let linked = Fixture::new();
        linked.write("source", marker(true).as_bytes());
        fs::hard_link(linked.0.join("source"), linked.0.join(MARKER)).unwrap();
        assert!(!linked.inspect().is_ready());
        for bytes in [
            b"[Relay]\n".to_vec(),
            vec![b'x'; MAX_RECORD_BYTES + 1],
            marker(true).trim_end().as_bytes().to_vec(),
        ] {
            let fixture = Fixture::new();
            fixture.write(MARKER, &bytes);
            assert!(!fixture.inspect().is_ready());
            assert_eq!(fs::read(fixture.0.join(MARKER)).unwrap(), bytes);
        }
    }
    #[cfg(any(windows, target_os = "linux"))]
    fn symlink_file(target: &Path, link: &Path) -> std::io::Result<()> {
        #[cfg(windows)]
        {
            std::os::windows::fs::symlink_file(target, link)
        }
        #[cfg(target_os = "linux")]
        {
            std::os::unix::fs::symlink(target, link)
        }
    }
    #[cfg(any(windows, target_os = "linux"))]
    #[test]
    fn file_symlinks_and_dangling_pending_links_refuse() {
        for name in [MARKER, INCOMPLETE[0], INCOMPLETE[1], INCOMPLETE[2]] {
            let fixture = Fixture::new();
            fixture.write("source", marker(true).as_bytes());
            if let Err(error) = symlink_file(&fixture.0.join("source"), &fixture.0.join(name)) {
                if error.kind() == std::io::ErrorKind::PermissionDenied {
                    eprintln!("Symlink fixture skipped: platform did not grant symlink creation.");
                    return;
                }
                panic!("Cannot create owned symlink fixture: {error}");
            }
            assert!(!fixture.inspect().is_ready());
            assert_eq!(
                fs::read(fixture.0.join("source")).unwrap(),
                marker(true).as_bytes()
            );
        }
        let fixture = Fixture::new();
        symlink_file(&fixture.0.join("missing"), &fixture.0.join(INCOMPLETE[0])).unwrap();
        assert!(!fixture.inspect().is_ready());
    }
    #[cfg(any(windows, target_os = "linux"))]
    #[test]
    fn redirected_installation_directory_refuses() {
        let fixture = Fixture::new();
        fs::create_dir(fixture.0.join("source")).unwrap();
        #[cfg(windows)]
        let link =
            std::os::windows::fs::symlink_dir(fixture.0.join("source"), fixture.0.join("redirect"));
        #[cfg(target_os = "linux")]
        let link = std::os::unix::fs::symlink(fixture.0.join("source"), fixture.0.join("redirect"));
        if let Err(error) = link {
            if error.kind() == std::io::ErrorKind::PermissionDenied {
                eprintln!(
                    "Directory symlink fixture skipped: platform did not grant symlink creation."
                );
                return;
            }
            panic!("Cannot create owned directory symlink fixture: {error}");
        }
        assert!(!inspect_directory(&fixture.0.join("redirect")).is_ready());
    }
}
