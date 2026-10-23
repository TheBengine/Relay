//! An explicitly chosen installer waits for consent and the exact parent exit.
//! No shell, installer flags that skip consent, force-close, or helper copy.
use crate::{storage, PreparedInstaller, Result, MAX_INSTALLER};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::Read,
    os::windows::{fs::OpenOptionsExt, process::CommandExt},
    process::{Child, Command, Stdio},
};
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, FILETIME, HANDLE, WAIT_OBJECT_0,
    },
    Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE,
    },
    System::Threading::{
        CreateEventW, GetCurrentProcess, GetProcessTimes, SetEvent, WaitForMultipleObjects,
        WaitForSingleObject, CREATE_NO_WINDOW,
    },
};

struct Event(HANDLE);
// An event is a kernel synchronization object; these handles are never inherited.
unsafe impl Send for Event {}
unsafe impl Sync for Event {}
impl Drop for Event {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}
impl Event {
    fn new(name: &str) -> Result<Self> {
        let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        let handle = unsafe { CreateEventW(std::ptr::null(), 1, 0, wide.as_ptr()) };
        if handle.is_null() {
            return Err("Couldn't prepare the installation handoff.".into());
        }
        let event = Self(handle);
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            return Err("An installation handoff name was already in use.".into());
        }
        Ok(event)
    }
    fn signal(&self) -> Result<()> {
        if unsafe { SetEvent(self.0) } == 0 {
            Err("Couldn't signal the installation handoff.".into())
        } else {
            Ok(())
        }
    }
}

pub struct Signals {
    name: String,
    ready: Event,
    commit: Event,
    cancel: Event,
}
impl Signals {
    pub fn new() -> Result<Self> {
        let mut nonce = [0u8; 32];
        getrandom::getrandom(&mut nonce).map_err(|_| "Installation randomness is unavailable.")?;
        let name = format!("Local\\RelayUpdate-{}", crate::digest(&nonce));
        Ok(Self {
            ready: Event::new(&format!("{name}-ready"))?,
            commit: Event::new(&format!("{name}-commit"))?,
            cancel: Event::new(&format!("{name}-cancel"))?,
            name,
        })
    }
    pub fn cancel(&self) {
        let _ = self.cancel.signal();
    }
    fn cancelled(&self) -> bool {
        unsafe { WaitForSingleObject(self.cancel.0, 0) == WAIT_OBJECT_0 }
    }
    fn await_ready(&self) -> Result<()> {
        let handles = [self.cancel.0, self.ready.0];
        match unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, 30_000) } {
            value if value == WAIT_OBJECT_0 + 1 => Ok(()),
            _ => Err("The installer didn't become ready. Relay was left open.".into()),
        }
    }
}

/// Pins every parent name against renaming, and the file against writing/deleting.
/// Hash the actual open file; a path check alone is insufficient before execution.
struct Pinned {
    _file: File,
    _directories: Vec<File>,
}
fn pin(ready: &PreparedInstaller, signals: &Signals) -> Result<Pinned> {
    expiry(ready)?;
    storage::guard(&ready.path)?;
    let mut directories = Vec::new();
    for parent in ready
        .path
        .ancestors()
        .skip(1)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
    {
        let directory = OpenOptions::new()
            .access_mode(0x80) // FILE_READ_ATTRIBUTES
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE) // deny deletion/rename
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(parent)
            .map_err(|e| e.to_string())?;
        information(&directory, true)?;
        directories.push(directory);
    }
    let mut file = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(&ready.path)
        .map_err(|_| "Close programs using the downloaded installer and try again.")?;
    information(&file, false)?;
    let mut buffer = [0u8; 64 * 1024];
    let mut bytes = 0u64;
    let mut hash = Sha256::new();
    loop {
        if signals.cancelled() {
            return Err("Installation cancelled.".into());
        }
        let count = file.read(&mut buffer).map_err(|e| e.to_string())?;
        if count == 0 {
            break;
        }
        bytes += count as u64;
        if bytes > MAX_INSTALLER || bytes > ready.release.installer_bytes {
            return Err("The downloaded installer changed.".into());
        }
        hash.update(&buffer[..count]);
    }
    if bytes != ready.release.installer_bytes
        || format!("{:x}", hash.finalize()) != ready.release.installer_sha256
    {
        return Err("The downloaded installer changed.".into());
    }
    expiry(ready)?;
    Ok(Pinned {
        _file: file,
        _directories: directories,
    })
}
fn information(file: &File, directory: bool) -> Result<()> {
    use std::os::windows::io::AsRawHandle;
    let mut info = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::zeroed();
    if unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, info.as_mut_ptr()) } == 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let info = unsafe { info.assume_init() };
    if info.dwFileAttributes & 0x400 != 0
        || (info.dwFileAttributes & 0x10 != 0) != directory
        || (!directory && info.nNumberOfLinks != 1)
    {
        return Err("Redirected or hard-linked installer paths are refused.".into());
    }
    Ok(())
}
fn expiry(ready: &PreparedInstaller) -> Result<()> {
    if ready.release.handoff.as_deref() != Some("wait-parent-v1") {
        return Err("This installer doesn't support a verified handoff. Use Show download.".into());
    }
    if chrono::Utc::now().timestamp() + 90 >= ready.valid_until {
        return Err("Update approval has expired or is about to expire. Check again.".into());
    }
    Ok(())
}
fn parent_identity() -> Result<String> {
    let mut times = [FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    }; 4];
    let [creation, exit, kernel, user] = &mut times;
    if unsafe { GetProcessTimes(GetCurrentProcess(), creation, exit, kernel, user) } == 0 {
        return Err("Couldn't identify this Relay process.".into());
    }
    Ok(format!(
        "{}-{}-{}",
        std::process::id(),
        creation.dwLowDateTime,
        creation.dwHighDateTime
    ))
}

pub struct WaitingInstaller {
    _child: Child,
    _pinned: Pinned,
    signals: std::sync::Arc<Signals>,
    committed: bool,
    valid_until: i64,
}
impl WaitingInstaller {
    /// The caller rechecks current call/host state, then closes Relay normally.
    pub fn commit(mut self) -> Result<()> {
        if self.signals.cancelled() {
            return Err("Installation was cancelled.".into());
        }
        if self
            ._child
            .try_wait()
            .map_err(|_| "Couldn't confirm that the installer is still waiting.")?
            .is_some()
        {
            return Err("The installer closed before Relay. Check the update again.".into());
        }
        if chrono::Utc::now().timestamp() + 60 >= self.valid_until {
            return Err("Update approval is about to expire. Check again.".into());
        }
        self.signals.commit.signal()?;
        self.committed = true;
        Ok(())
    }
}
impl Drop for WaitingInstaller {
    fn drop(&mut self) {
        if !self.committed {
            self.signals.cancel();
        }
    }
}

pub fn prepare(
    ready: &PreparedInstaller,
    signals: std::sync::Arc<Signals>,
) -> Result<WaitingInstaller> {
    prepare_inner(ready, signals, &[])
}

fn prepare_inner(
    ready: &PreparedInstaller,
    signals: std::sync::Arc<Signals>,
    extra: &[std::ffi::OsString],
) -> Result<WaitingInstaller> {
    let pinned = pin(ready, &signals)?;
    let identity = parent_identity()?;
    if signals.cancelled() {
        return Err("Installation cancelled.".into());
    }
    // The current Relay UI is English. A valid Inno language skips only the
    // pre-readiness picker; all ordinary wizard pages and final consent remain.
    // No silent flags, custom directory, shell, elevation or automatic app run.
    let child = Command::new(&ready.path)
        .arg("/LANG=english")
        .arg(format!("/RELAYPARENT={identity}"))
        .arg(format!("/RELAYHANDOFF={}", signals.name))
        .args(extra)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|_| "Couldn't start the verified installer. Relay was left open.")?;
    // Retain path/file pins through extraction, readiness and final consent.
    let waiting = WaitingInstaller {
        _child: child,
        _pinned: pinned,
        signals,
        committed: false,
        valid_until: ready.valid_until,
    };
    waiting.signals.await_ready()?;
    Ok(waiting)
}

#[cfg(test)]
mod tests;
