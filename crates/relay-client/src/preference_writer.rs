//! Preferences are protected before any disk write. Rapid edits replace pending
//! snapshots, but a confirmed snapshot is an ordered, non-coalescing barrier.
use eframe::egui;
use serde::de::DeserializeOwned;
use std::{
    collections::VecDeque,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{mpsc, Arc, Condvar, Mutex},
    thread::{self, JoinHandle},
};
use zeroize::Zeroizing;

const MAGIC: &[u8] = b"RLYPREF1";
const MAX_WIRE_BYTES: u64 = 5 * 1024 * 1024;
const MAX_PLAIN_BYTES: usize = 4 * 1024 * 1024;
const MAX_CONFIRMED_SNAPSHOTS: usize = 8;

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
    snapshots: VecDeque<Snapshot>,
    confirmed: usize,
    stopped: bool,
}

struct Snapshot {
    bytes: Zeroizing<Vec<u8>>,
    receipt: Option<mpsc::Sender<Result<(), String>>>,
}

impl Pending {
    fn save(&mut self, bytes: Vec<u8>) {
        // Never replace or move a snapshot across a persistence barrier.
        if let Some(snapshot) = self
            .snapshots
            .back_mut()
            .filter(|last| last.receipt.is_none())
        {
            snapshot.bytes = Zeroizing::new(bytes);
        } else {
            self.snapshots.push_back(Snapshot {
                bytes: Zeroizing::new(bytes),
                receipt: None,
            });
        }
    }

    fn save_confirmed(
        &mut self,
        bytes: Vec<u8>,
    ) -> Result<mpsc::Receiver<Result<(), String>>, String> {
        if self.stopped {
            return Err(
                "Local preferences storage is shutting down. Your message was not sent.".into(),
            );
        }
        if bytes.len() > MAX_PLAIN_BYTES {
            return Err("Local preferences storage is full. Your message was not sent.".into());
        }
        if self.confirmed >= MAX_CONFIRMED_SNAPSHOTS {
            return Err("Local preferences storage is busy. Try sending again in a moment.".into());
        }
        let (sender, receipt) = mpsc::channel();
        self.snapshots.push_back(Snapshot {
            bytes: Zeroizing::new(bytes),
            receipt: Some(sender),
        });
        self.confirmed += 1;
        Ok(receipt)
    }
}

pub(super) struct PreferenceWriter {
    shared: Arc<(Mutex<Pending>, Condvar)>,
    worker: Option<JoinHandle<()>>,
    errors: mpsc::Receiver<String>,
    writable: bool,
}

impl PreferenceWriter {
    pub fn new(path: PathBuf, ctx: egui::Context, writable: bool) -> Self {
        Self::with_commit(ctx, writable, move |bytes| commit(&path, bytes))
    }

    fn with_commit(
        ctx: egui::Context,
        writable: bool,
        commit: impl Fn(&[u8]) -> Result<(), String> + Send + 'static,
    ) -> Self {
        let shared = Arc::new((Mutex::new(Pending::default()), Condvar::new()));
        let (error_tx, errors) = mpsc::sync_channel(1);
        let worker = if writable {
            let worker_shared = shared.clone();
            Some(thread::spawn(move || {
                let (lock, wake) = &*worker_shared;
                loop {
                    let snapshot = {
                        let mut pending = lock.lock().unwrap_or_else(|error| error.into_inner());
                        while pending.snapshots.is_empty() && !pending.stopped {
                            pending = wake
                                .wait(pending)
                                .unwrap_or_else(|error| error.into_inner());
                        }
                        match pending.snapshots.pop_front() {
                            Some(snapshot) => snapshot,
                            None => break,
                        }
                    };
                    let result = commit(&snapshot.bytes);
                    if let Some(receipt) = snapshot.receipt {
                        lock.lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .confirmed -= 1;
                        let _ = receipt.send(result);
                        // The UI must wake on success too, before dispatching
                        // any action gated on this exact durable snapshot.
                        ctx.request_repaint();
                    } else if let Err(error) = result {
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
        if pending.stopped {
            return;
        }
        pending.save(bytes);
        self.shared.1.notify_one();
    }

    /// A receipt acknowledges only these bytes after the protected atomic
    /// commit. Later settings edits cannot acknowledge an earlier failed send.
    /// Callers must retain every staged outgoing message in subsequent snapshots
    /// until it is acknowledged by the server or explicitly discarded.
    pub fn save_confirmed(
        &self,
        bytes: Vec<u8>,
    ) -> Result<mpsc::Receiver<Result<(), String>>, String> {
        if !self.writable {
            return Err("Cannot save local preferences. The existing file was preserved and your message was not sent.".into());
        }
        let receipt = self
            .shared
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .save_confirmed(bytes)?;
        self.shared.1.notify_one();
        Ok(receipt)
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

    #[test]
    fn ordinary_edits_coalesce_only_between_exact_confirmation_barriers() {
        let mut pending = Pending::default();
        pending.save(b"settings-old".to_vec());
        pending.save(b"settings-latest".to_vec());
        let _first = pending.save_confirmed(b"first-send".to_vec()).unwrap();
        pending.save(b"draft-old".to_vec());
        pending.save(b"draft-latest".to_vec());
        let _second = pending.save_confirmed(b"second-send".to_vec()).unwrap();
        pending.save(b"later-settings".to_vec());
        let bytes: Vec<&[u8]> = pending
            .snapshots
            .iter()
            .map(|item| item.bytes.as_slice())
            .collect();
        assert_eq!(
            bytes,
            vec![
                b"settings-latest".as_slice(),
                b"first-send",
                b"draft-latest",
                b"second-send",
                b"later-settings"
            ]
        );
        assert_eq!(pending.confirmed, 2);
    }

    #[test]
    fn later_success_cannot_acknowledge_a_failed_snapshot() {
        let committed = Arc::new(Mutex::new(Vec::new()));
        let observed = committed.clone();
        let writer = PreferenceWriter::with_commit(egui::Context::default(), true, move |bytes| {
            observed.lock().unwrap().push(bytes.to_vec());
            if bytes == b"failed-send" {
                Err("synthetic failure".into())
            } else {
                Ok(())
            }
        });
        let failed = writer.save_confirmed(b"failed-send".to_vec()).unwrap();
        let success = writer.save_confirmed(b"later-send".to_vec()).unwrap();
        drop(writer); // Flush both exact snapshots and close the worker.
        assert_eq!(failed.recv().unwrap(), Err("synthetic failure".into()));
        assert_eq!(success.recv().unwrap(), Ok(()));
        assert_eq!(
            *committed.lock().unwrap(),
            vec![b"failed-send".to_vec(), b"later-send".to_vec()]
        );
    }

    #[test]
    fn confirmed_snapshot_is_not_acknowledged_until_commit_finishes() {
        let (started_tx, started) = mpsc::channel();
        let (release, blocked) = mpsc::channel();
        let writer = PreferenceWriter::with_commit(egui::Context::default(), true, move |_| {
            started_tx.send(()).unwrap();
            blocked
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
            Ok(())
        });
        let receipt = writer.save_confirmed(b"send".to_vec()).unwrap();
        started
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        assert!(matches!(receipt.try_recv(), Err(mpsc::TryRecvError::Empty)));
        release.send(()).unwrap();
        assert_eq!(
            receipt
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap(),
            Ok(())
        );
    }

    #[test]
    fn confirmed_queue_is_bounded_and_refuses_oversize_or_read_only_storage() {
        let mut pending = Pending::default();
        for _ in 0..MAX_CONFIRMED_SNAPSHOTS {
            let _receipt = pending.save_confirmed(b"send".to_vec()).unwrap();
        }
        assert!(pending.save_confirmed(b"one-too-many".to_vec()).is_err());
        assert_eq!(pending.snapshots.len(), MAX_CONFIRMED_SNAPSHOTS);
        assert!(Pending::default()
            .save_confirmed(vec![0; MAX_PLAIN_BYTES + 1])
            .is_err());
        let writer = PreferenceWriter::with_commit(egui::Context::default(), false, |_| {
            panic!("must not write")
        });
        assert!(writer.save_confirmed(b"send".to_vec()).is_err());
    }

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
