//! Preferences are protected before any disk write. Rapid edits replace pending snapshots.
use eframe::egui;
use serde::de::DeserializeOwned;
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{mpsc, Arc, Condvar, Mutex},
    thread::{self, JoinHandle},
};
use zeroize::Zeroizing;

const MAGIC: &[u8] = b"RLYPREF1";
const MAX_WIRE_BYTES: u64 = 5 * 1024 * 1024;

/// Migrate valid legacy JSON directly to protected storage. An unreadable
/// protected file is returned as an error and remains untouched.
pub(super) fn load<T: DeserializeOwned + Default>(path: &Path) -> Result<T, String> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(T::default()),
        Err(_) => {
            return Err(
                "Cannot read your local preferences. The existing file was preserved.".into(),
            )
        }
    };
    let mut wire = Zeroizing::new(Vec::new());
    file.take(MAX_WIRE_BYTES + 1)
        .read_to_end(&mut wire)
        .map_err(|_| "Cannot read your local preferences. The existing file was preserved.")?;
    if wire.len() as u64 > MAX_WIRE_BYTES {
        return Err(
            "The local preferences file exceeds its limit. The existing file was preserved.".into(),
        );
    }
    let bytes = crate::e2ee::unprotect_local_preferences(&wire)?;
    let preferences = serde_json::from_slice(&bytes)
        .map_err(|_| "Cannot read your local preferences. The existing file was preserved.")?;
    if !wire.starts_with(MAGIC) {
        commit(path, &bytes)?;
    }
    Ok(preferences)
}

#[derive(Default)]
struct Pending {
    bytes: Option<Zeroizing<Vec<u8>>>,
    stopped: bool,
}

pub(super) struct PreferenceWriter {
    shared: Arc<(Mutex<Pending>, Condvar)>,
    worker: Option<JoinHandle<()>>,
    errors: mpsc::Receiver<String>,
    writable: bool,
}

impl PreferenceWriter {
    pub fn new(path: PathBuf, ctx: egui::Context, writable: bool) -> Self {
        let shared = Arc::new((Mutex::new(Pending::default()), Condvar::new()));
        let (error_tx, errors) = mpsc::sync_channel(1);
        let worker = if writable {
            let worker_shared = shared.clone();
            Some(thread::spawn(move || {
                let (lock, wake) = &*worker_shared;
                loop {
                    let bytes = {
                        let mut pending = lock.lock().unwrap_or_else(|error| error.into_inner());
                        while pending.bytes.is_none() && !pending.stopped {
                            pending = wake
                                .wait(pending)
                                .unwrap_or_else(|error| error.into_inner());
                        }
                        match pending.bytes.take() {
                            Some(bytes) => bytes,
                            None => break,
                        }
                    };
                    if let Err(error) = commit(&path, &bytes) {
                        let _ = error_tx.try_send(error);
                        ctx.request_repaint();
                    }
                }
            }))
        } else {
            None
        };
        Self {
            shared,
            worker,
            errors,
            writable,
        }
    }
    pub fn save(&self, bytes: Vec<u8>) {
        if !self.writable {
            return;
        }
        let mut pending = self
            .shared
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        pending.bytes = Some(Zeroizing::new(bytes));
        self.shared.1.notify_one();
    }
    pub fn take_error(&self) -> Option<String> {
        self.errors.try_recv().ok()
    }
}

impl Drop for PreferenceWriter {
    fn drop(&mut self) {
        self.shared
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .stopped = true;
        self.shared.1.notify_one();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn commit(path: &Path, plain: &[u8]) -> Result<(), String> {
    commit_with(path, plain, crate::e2ee::protect_local_preferences)
}

fn commit_with(
    path: &Path,
    plain: &[u8],
    protect: impl FnOnce(&[u8]) -> Result<Vec<u8>, String>,
) -> Result<(), String> {
    // No plaintext file or fallback is created if OS protection fails.
    let wire = protect(plain)?;
    let parent = path.parent().ok_or("Invalid preferences path.")?;
    std::fs::create_dir_all(parent).map_err(|_| "Cannot create local preferences storage.")?;
    let temporary = parent.join(format!("{}.preferences-pending", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
            .map_err(|_| "Cannot create a protected preferences snapshot.")?;
        file.write_all(&wire)
            .and_then(|_| file.sync_all())
            .map_err(|_| "Cannot save protected preferences. The previous file was preserved.")?;
        drop(file);
        replace(&temporary, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}

#[cfg(windows)]
fn replace(source: &Path, destination: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows::{
        core::PCWSTR,
        Win32::Storage::FileSystem::{
            MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
        },
    };
    let from: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    unsafe {
        MoveFileExW(
            PCWSTR(from.as_ptr()),
            PCWSTR(to.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    }
    .map_err(|_| "Cannot commit protected preferences. The previous file was preserved.".into())
}

#[cfg(not(windows))]
fn replace(source: &Path, destination: &Path) -> Result<(), String> {
    std::fs::rename(source, destination)
        .map_err(|_| "Cannot commit protected preferences. The previous file was preserved.".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn folder() -> PathBuf {
        let folder = std::env::var_os("RELAY_TEST_WORK_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("relay-tests"))
            .join(format!("protected-preferences-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&folder).unwrap();
        folder
    }

    #[test]
    fn protection_failure_does_not_replace_the_original_or_create_a_plaintext_fallback() {
        let folder = folder();
        let path = folder.join("preferences.json");
        std::fs::write(&path, b"existing protected snapshot").unwrap();
        assert!(commit_with(&path, b"a private new draft", |_| Err(
            "synthetic protection failure".into()
        ))
        .is_err());
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"existing protected snapshot"
        );
        assert_eq!(std::fs::read_dir(&folder).unwrap().count(), 1);
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(folder).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn legacy_private_drafts_migrate_and_shutdown_flushes_only_protected_latest_snapshots() {
        let folder = folder();
        let path = folder.join("preferences.json");
        let original = json!({"remember_drafts":true,"draft":{"content":"private synthetic draft","reply":{"excerpt":"private received excerpt"},"attachments":[{"name":"private filename"}]}});
        std::fs::write(&path, serde_json::to_vec(&original).unwrap()).unwrap();
        assert_eq!(load::<Value>(&path).unwrap(), original);
        let wire = std::fs::read(&path).unwrap();
        assert!(wire.starts_with(MAGIC));
        assert!(!wire.windows(7).any(|window| window == b"private"));
        {
            let writer = PreferenceWriter::new(path.clone(), egui::Context::default(), true);
            for id in 0..100 {
                writer.save(
                    serde_json::to_vec(&json!({"edit":id,"content":"private synthetic draft"}))
                        .unwrap(),
                );
            }
        }
        assert_eq!(
            load::<Value>(&path).unwrap(),
            json!({"edit":99,"content":"private synthetic draft"})
        );
        assert!(!std::fs::read(&path)
            .unwrap()
            .windows(7)
            .any(|window| window == b"private"));
        assert_eq!(std::fs::read_dir(&folder).unwrap().count(), 1);
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(folder).unwrap();
    }

    #[test]
    fn unreadable_protected_preferences_are_never_overwritten_with_defaults() {
        let folder = folder();
        let path = folder.join("preferences.json");
        let original = b"RLYPREF1invalid protected bytes";
        std::fs::write(&path, original).unwrap();
        assert!(load::<Value>(&path).is_err());
        {
            let writer = PreferenceWriter::new(path.clone(), egui::Context::default(), false);
            writer.save(serde_json::to_vec(&json!({"default":true})).unwrap());
        }
        assert_eq!(std::fs::read(&path).unwrap(), original);
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(folder).unwrap();
    }
}
