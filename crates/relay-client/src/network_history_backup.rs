//! Local-only history work. Errors never pause encryption or call the server.
use super::*;
use crate::recovery::{self, BackupResult};
use zeroize::Zeroizing;

impl Worker {
    pub(super) fn history_backup(
        &mut self,
        origin: Option<CommandOrigin>,
        path: PathBuf,
        password: Zeroizing<String>,
        export: bool,
        request_id: u64,
    ) {
        if self
            .archive_busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            self.emit(Update::HistoryBackup {
                origin,
                request_id,
                result: Err("Another history backup is still being processed.".into()),
            });
            return;
        }
        let snapshot = (|| {
            if let Some(origin) = &origin {
                self.command_connection(origin)?;
            }
            if export {
                if origin.is_none() {
                    return Err("Connect to your account before making a history backup.".into());
                }
                let c = self
                    .connection
                    .as_ref()
                    .ok_or("Connect to your account first.")?;
                let crypto = c
                    .crypto
                    .as_ref()
                    .ok_or("This device's protected history is unavailable.")?;
                if crypto.device.identity().server_id != c.server_id
                    || crypto.device.identity().user_id != c.session.user.id
                {
                    return Err("The history belongs to another account or community.".into());
                }
                Ok(Some(crypto.history_backup()?))
            } else {
                Ok(None)
            }
        })();
        let snapshot = match snapshot {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.archive_busy.store(false, Ordering::Release);
                self.emit(Update::HistoryBackup {
                    origin,
                    request_id,
                    result: Err(error),
                });
                return;
            }
        };
        let busy = self.archive_busy.clone();
        let updates = self.updates.clone();
        let ctx = self.ctx.clone();
        let response_origin = origin.clone();
        let task = std::thread::Builder::new()
            .name("relay-history-backup".into())
            .spawn(move || {
                struct Lease(Arc<AtomicBool>);
                impl Drop for Lease {
                    fn drop(&mut self) {
                        self.0.store(false, Ordering::Release);
                    }
                }
                let _lease = Lease(busy);
                let result = match snapshot {
                    Some(archive) => {
                        recovery::save(&path, &archive, &password).map(BackupResult::Saved)
                    }
                    None => {
                        recovery::open(&path, &password, origin.as_ref()).map(BackupResult::Opened)
                    }
                };
                drop(password);
                let _ = updates.send(Update::HistoryBackup {
                    origin,
                    request_id,
                    result,
                });
                ctx.request_repaint();
            });
        if task.is_err() {
            self.archive_busy.store(false, Ordering::Release);
            self.emit(Update::HistoryBackup {
                origin: response_origin,
                request_id,
                result: Err("The history backup worker could not start. Try again.".into()),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_or_stale_account_is_rejected_before_file_or_network_work() {
        let (tx, updates) = mpsc::sync_channel(16);
        let mut worker = Worker::new(
            tx,
            eframe::egui::Context::default(),
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        let path =
            std::env::temp_dir().join(format!("relay-history-rejected-{}", uuid::Uuid::new_v4()));
        let origin = CommandOrigin {
            url: "http://127.0.0.1:1".into(),
            server_id: "other".into(),
            user_id: "other".into(),
        };
        for (expected, export) in [
            (Some(origin.clone()), true),
            (Some(origin), false),
            (None, true),
        ] {
            worker.command(Command::HistoryBackup {
                origin: expected.clone(),
                path: path.clone(),
                password: Zeroizing::new("synthetic long passphrase".into()),
                export,
                request_id: 31,
            });
            let Update::HistoryBackup {
                origin,
                request_id,
                result,
            } = updates.try_recv().unwrap()
            else {
                panic!("wrong result")
            };
            assert_eq!(origin, expected);
            assert_eq!(request_id, 31);
            assert!(result.is_err());
            assert!(!worker.archive_busy.load(Ordering::Acquire));
            assert!(!path.exists());
        }
    }
}
