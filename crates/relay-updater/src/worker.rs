use super::{Check, Engine, PreparedInstaller, Release, Result};
use std::{
    future::Future,
    sync::{mpsc, Arc},
    thread::{self, JoinHandle},
};

#[derive(Debug)]
pub enum Outcome {
    Checked(Check),
    Downloaded(Box<PreparedInstaller>),
    Cancelled,
}

pub enum Poll {
    Idle,
    Running,
    /// The result has arrived; one short repaint can collect the exiting thread.
    Completing,
    Finished(Result<Outcome>),
}

struct Job {
    handle: JoinHandle<()>,
    receiver: mpsc::Receiver<Result<Outcome>>,
    received: Option<Result<Outcome>>,
    cancel: Option<tokio::sync::oneshot::Sender<()>>,
}

/// A single cancellable operation. No runtime, timer, socket or thread exists
/// while idle; the UI is notified only when a job has produced its final result.
pub struct Worker {
    engine: Arc<Engine>,
    job: Option<Job>,
}

impl Worker {
    pub fn new(engine: Engine) -> Self {
        Self {
            engine: Arc::new(engine),
            job: None,
        }
    }
    pub fn running(&self) -> bool {
        self.job.is_some()
    }

    pub fn check(&mut self, notify: impl FnOnce() + Send + 'static) -> Result<()> {
        let engine = self.engine.clone();
        self.start(
            move || async move { engine.check().await.map(Outcome::Checked) },
            notify,
        )
    }

    pub fn download(
        &mut self,
        approved: Release,
        notify: impl FnOnce() + Send + 'static,
    ) -> Result<()> {
        let engine = self.engine.clone();
        self.start(
            move || async move {
                engine
                    .download(&approved)
                    .await
                    .map(|ready| Outcome::Downloaded(Box::new(ready)))
            },
            notify,
        )
    }

    pub fn cancel(&mut self) {
        if let Some(cancel) = self.job.as_mut().and_then(|job| job.cancel.take()) {
            let _ = cancel.send(());
        }
    }

    pub fn poll(&mut self) -> Poll {
        let Some(job) = self.job.as_mut() else {
            return Poll::Idle;
        };
        if job.received.is_none() {
            match job.receiver.try_recv() {
                Ok(result) => job.received = Some(result),
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    job.received = Some(Err(
                        "The update worker stopped unexpectedly. Check again.".into(),
                    ));
                }
            }
        }
        if !job.handle.is_finished() {
            return if job.received.is_some() {
                Poll::Completing
            } else {
                Poll::Running
            };
        }
        let job = self.job.take().expect("Observed update job disappeared");
        let result = job.received.unwrap_or_else(|| {
            job.receiver
                .try_recv()
                .unwrap_or_else(|_| Err("The update worker returned no result.".into()))
        });
        if job.handle.join().is_err() {
            return Poll::Finished(Err(
                "The update worker stopped unexpectedly. Check again.".into()
            ));
        }
        Poll::Finished(result)
    }

    pub(super) fn start<F, Fut>(
        &mut self,
        operation: F,
        notify: impl FnOnce() + Send + 'static,
    ) -> Result<()>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = Result<Outcome>> + 'static,
    {
        if self.job.is_some() {
            return Err("An update operation is already running.".into());
        }
        let (sender, receiver) = mpsc::channel();
        let (cancel, cancelled) = tokio::sync::oneshot::channel();
        let handle = thread::Builder::new()
            .name("relay-update".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    match tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                    {
                        Ok(runtime) => {
                            let result = runtime.block_on(async {
                                tokio::select! {
                                    biased;
                                    _ = cancelled => Ok(Outcome::Cancelled),
                                    result = operation() => result,
                                }
                            });
                            // Shutdown HTTP/DNS/runtime resources before waking the UI.
                            drop(runtime);
                            result
                        }
                        Err(error) => Err(format!("Cannot start the update worker: {error}")),
                    }
                }))
                .unwrap_or_else(|_| {
                    Err("The update worker stopped unexpectedly. Check again.".into())
                });
                if sender.send(result).is_ok() {
                    notify();
                }
            })
            .map_err(|error| format!("Cannot start the update worker: {error}"))?;
        self.job = Some(Job {
            handle,
            receiver,
            received: None,
            cancel: Some(cancel),
        });
        Ok(())
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        if let Some(job) = self.job.take() {
            // Close delivery before waking cancellation so an exiting UI receives no new result.
            drop(job.receiver);
            if let Some(cancel) = job.cancel {
                let _ = cancel.send(());
            }
            // Detach only for shutdown; cancellation closes the operation/runtime off the UI thread.
            drop(job.handle);
        }
    }
}
