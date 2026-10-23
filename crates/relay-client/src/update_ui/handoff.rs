use relay_updater::{
    handoff::{self, Signals, WaitingInstaller},
    PreparedInstaller,
};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    thread::JoinHandle,
};

pub(super) enum Poll {
    Running,
    Completing,
    Finished(Result<WaitingInstaller, String>),
}

pub(super) struct Job {
    signals: Arc<Signals>,
    stopped: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    result: mpsc::Receiver<Result<WaitingInstaller, String>>,
    pending: Option<Result<WaitingInstaller, String>>,
    cancel_on_drop: bool,
}
impl Job {
    pub fn start(ready: PreparedInstaller, ctx: &eframe::egui::Context) -> Result<Self, String> {
        let signals = Arc::new(Signals::new()?);
        let stopped = Arc::new(AtomicBool::new(false));
        let (tx, result) = mpsc::sync_channel(1);
        let wake = ctx.clone();
        let worker_signals = signals.clone();
        let worker_stopped = stopped.clone();
        let thread = std::thread::Builder::new()
            .name("relay-update-handoff".into())
            .spawn(move || {
                let prepared = handoff::prepare(&ready, worker_signals);
                let _ = tx.send(prepared);
                if !worker_stopped.load(Ordering::Acquire) {
                    wake.request_repaint();
                }
            })
            .map_err(|_| "Couldn't prepare the installation. Relay was left open.")?;
        Ok(Self {
            signals,
            stopped,
            thread: Some(thread),
            result,
            pending: None,
            cancel_on_drop: true,
        })
    }
    pub fn cancel(&self) {
        self.signals.cancel();
    }
    pub fn poll(&mut self) -> Poll {
        let Some(thread) = self.thread.as_ref() else {
            return Poll::Running;
        };
        if self.pending.is_none() {
            match self.result.try_recv() {
                Ok(result) => self.pending = Some(result),
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.pending = Some(Err("Installation preparation failed.".into()))
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if !thread.is_finished() {
            // This rare completion race is the only UI polling timer.
            return if self.pending.is_some() {
                Poll::Completing
            } else {
                Poll::Running
            };
        }
        let joined = self.thread.take().unwrap().join();
        let result = joined
            .map_err(|_| "Installation preparation failed.".to_owned())
            .and_then(|_| {
                self.pending
                    .take()
                    .or_else(|| self.result.try_recv().ok())
                    .ok_or_else(|| "Installation preparation failed.".to_owned())
            })
            .and_then(|result| result);
        if result.is_ok() {
            self.cancel_on_drop = false;
        }
        Poll::Finished(result)
    }
}
impl Drop for Job {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        if self.cancel_on_drop {
            self.signals.cancel();
        }
    }
}
