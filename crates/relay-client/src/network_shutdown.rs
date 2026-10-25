//! Normal close and updater close share an acknowledged, queue-independent drain.
//! Public completion describes server acknowledgements, never a best-effort logout.
use super::*;

pub(super) const HTTP_BUDGET: Duration = Duration::from_secs(2);
const WORKER_BUDGET: Duration = Duration::from_secs(12);

#[derive(Default)]
pub(super) struct Work {
    pub(super) commands: AtomicUsize,
    pub(super) control: AtomicBool,
    pub(super) archive: Arc<AtomicBool>,
    pub(super) transfers: Arc<AtomicUsize>,
    pub(super) background: Arc<AtomicUsize>,
    pub(super) server_check: Arc<AtomicBool>,
    pub(super) media: Arc<AtomicUsize>,
    pub(super) hold: AtomicBool,
}

impl Work {
    pub(super) fn busy(&self) -> bool {
        self.commands.load(Ordering::Acquire) != 0
            || self.control.load(Ordering::Acquire)
            || self.archive.load(Ordering::Acquire)
            || self.transfers.load(Ordering::Acquire) != 0
            || self.background.load(Ordering::Acquire) != 0
            || self.server_check.load(Ordering::Acquire)
            || self.media.load(Ordering::Acquire) != 0
    }
}

pub(super) struct Lease(Arc<AtomicUsize>);
impl Lease {
    pub(super) fn acquire(active: Arc<AtomicUsize>) -> Self {
        active.fetch_add(1, Ordering::AcqRel);
        Self(active)
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Report {
    pub(super) voice_confirmed: usize,
    pub(super) revoked_confirmed: usize,
    pub(super) failed_requests: usize,
}

pub(crate) enum Poll {
    Pending(Duration),
    Finished(Result<(), String>),
}

impl Network {
    pub(crate) fn update_busy(&self) -> bool {
        self.work.busy()
    }

    pub(crate) fn set_update_hold(&self, held: bool) {
        if self.work.hold.swap(held, Ordering::AcqRel) != held {
            self.wake.notify();
        }
    }

    pub(crate) fn try_update(&mut self) -> Result<Update, mpsc::TryRecvError> {
        self.pending_updates
            .pop_front()
            .map_or_else(|| self.rx.try_recv(), Ok)
    }

    pub(crate) fn poll_shutdown(&mut self) -> Poll {
        let started = *self.shutdown_started.get_or_insert_with(Instant::now);
        self.shutdown.store(true, Ordering::Release);
        self.wake.notify();
        self.receive_completion();
        if let Some(result) = &self.shutdown_result {
            return Poll::Finished(result.as_ref().map(|_| ()).map_err(Clone::clone));
        }
        if started.elapsed() >= WORKER_BUDGET {
            let error = "Relay could not finish closing its connection in time. Installation was cancelled; Relay stays open. Reconnect after the connection stops.".to_string();
            self.shutdown_result = Some(Err(error.clone()));
            return Poll::Finished(Err(error));
        }
        Poll::Pending(WORKER_BUDGET.saturating_sub(started.elapsed()))
    }

    fn receive_completion(&mut self) {
        // Receiving an acknowledgement alone is insufficient: the worker has
        // dropped its account state and must also have actually returned.
        if !self
            .worker
            .as_ref()
            .is_some_and(|worker| worker.is_finished())
        {
            return;
        }
        let joined = self.worker.take().unwrap().join();
        let report = joined
            .map_err(|_| "Relay's connection worker stopped unexpectedly. Installation was cancelled.".to_string())
            .and_then(|_| self.completed.try_recv().map_err(|_| "Relay did not acknowledge closing its connection. Installation was cancelled.".into()))
            .and_then(|report| {
                if report.failed_requests != 0 {
                    Err("The server did not confirm closing this connection. Installation was cancelled; Relay stays open. Reconnect and try again.".into())
                } else {
                    Ok(report)
                }
            });
        // A timeout remains a failure even if the thread returns immediately
        // afterwards. It cannot silently turn an already cancelled install into
        // a committed one.
        if self.shutdown_result.is_none() {
            self.shutdown_result = Some(report);
        }
    }

    pub(crate) fn reopen_if_stopped(&mut self, ctx: &eframe::egui::Context) {
        if self.shutdown_started.is_none() {
            return;
        }
        // Continue draining acknowledgements while an unusually slow command
        // returns. Never spawn a second account worker beside an unfinished one.
        self.receive_completion();
        if self.worker.is_some() {
            // The completion repaint can precede the thread's actual return.
            // Finish a cancelled shutdown without needing another user action.
            ctx.request_repaint_after(Duration::from_millis(100));
            return;
        }
        let mut replacement = Self::with_call_alerts(ctx.clone(), self.call_alerts.clone());
        replacement.pending_updates.push_back(Update::Disconnected);
        replacement
            .pending_updates
            .push_back(Update::Communities(Vec::new()));
        *self = replacement;
    }

    pub(crate) fn finish_drop(&mut self) {
        if self.worker.is_none() {
            return;
        }
        // Drop no longer relies on placing Logout in a potentially full queue.
        // Draining releases any blocked bounded UI acknowledgement sender.
        let start = Instant::now();
        loop {
            while self.rx.try_recv().is_ok() {}
            match self.poll_shutdown() {
                Poll::Finished(result) => {
                    if let Err(error) = result {
                        eprintln!("{error}");
                    }
                    break;
                }
                Poll::Pending(remaining) => {
                    if let Some(worker) = &self.worker {
                        if worker.is_finished() {
                            continue;
                        }
                    }
                    // A blocking receive is also the acknowledgement drain.
                    // Short, bounded waits cover the worker-return/channel race.
                    let _ = self
                        .rx
                        .recv_timeout(remaining.min(Duration::from_millis(10)));
                }
            }
            if start.elapsed() >= WORKER_BUDGET {
                break;
            }
        }
    }
}

impl Worker {
    pub(super) fn finish_normal(&mut self, deadline: Instant) -> Report {
        if let Some((job,_))=&self.handover_job {job.cancel();}
        self.pending_host_resume = None;
        self.cancel_transfers();
        self.socket_stop.store(true, Ordering::Release);
        self.live.shutdown.store(true, Ordering::Release);
        self.live.epoch.fetch_add(1, Ordering::AcqRel);
        self.live.activity.notify_all();
        self.chat_media_epoch.fetch_add(1, Ordering::AcqRel);
        self.chat_media_workers = None;
        for saved in self.saved_connections.values() {
            saved.stop.store(true, Ordering::Release);
        }
        let mut report = Report::default();
        let mut close = |connection: &Connection, current: bool| {
            let request = |path: &str| {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return false;
                }
                self.http
                    .post(crate::embedded_network::endpoint(&format!(
                        "{}{path}",
                        connection.url
                    )))
                    .bearer_auth(&connection.session.token)
                    .json(&serde_json::json!({}))
                    .timeout(remaining.min(Duration::from_millis(750)))
                    .send()
                    .is_ok_and(|response| response.status().is_success())
            };
            if current {
                if request("/api/voice/leave") {
                    report.voice_confirmed += 1;
                } else {
                    report.failed_requests += 1;
                }
            }
            if !connection.remembered {
                if request("/api/logout") {
                    report.revoked_confirmed += 1;
                } else {
                    report.failed_requests += 1;
                }
            }
        };
        if let Some(connection) = self.connection.as_ref() {
            close(connection, true);
        }
        for saved in self.saved_connections.values() {
            close(&saved.connection, false);
        }
        if let Some(mut host) = self.handover_runtime.take() {
            host.shutdown();
        }
        if let Err(error) = self.close_app_host() {
            eprintln!("{error}");
            report.failed_requests += 1;
        }
        // No credential-vault forget, key reset or persistent-history mutation.
        // Explicit Command::Logout retains its separate user-requested meaning.
        report
    }
}
