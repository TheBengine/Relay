//! Only the active account may publish or commit a transfer. IO runs off the worker.
use super::*;
use crate::history_transfer::{self as model, Code};
use std::io::Read;
use zeroize::Zeroizing;
pub(super) struct Job {
    origin: CommandOrigin,
    request_id: u64,
    token: Zeroizing<String>,
    device_id: String,
    cancel: Arc<AtomicBool>,
    result: mpsc::Receiver<Result<Outcome, String>>,
}
impl Drop for Job {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
    }
}
enum Outcome {
    Sent,
    Received(Box<(crate::e2ee::PreparedTransfer, RequestBuilder)>),
}
impl Worker {
    pub(super) fn start_history_transfer(
        &mut self,
        origin: CommandOrigin,
        request_id: u64,
        text: Zeroizing<String>,
        send: bool,
        cancel: Arc<AtomicBool>,
    ) -> Result<(), String> {
        if self.history_transfer_job.is_some() || self.archive_busy.load(Ordering::Acquire) {
            return Err("Another history operation is still running.".into());
        }
        if cancel.load(Ordering::Acquire) {
            return Err("History transfer was cancelled.".into());
        }
        let code = Code::parse(&text)?;
        drop(text);
        let c = self.command_connection(&origin)?;
        if !c.chat.supports(model::CAPABILITY) {
            return Err("Update this server before transferring history between PCs.".into());
        }
        crate::community::account_destination(&origin.url)?;
        let token = Zeroizing::new(c.session.token.clone());
        let device_id = c
            .crypto
            .as_ref()
            .ok_or("Encryption device is unavailable.")?
            .device
            .identity()
            .device_id
            .clone();
        let source = c
            .crypto
            .as_ref()
            .ok_or("Protected history is not ready on this PC.")?
            .transfer_source(&origin)?;
        let path = format!("/api/history-transfer/{}", code.id());
        let request = self
            .request(
                if send {
                    reqwest::Method::PUT
                } else {
                    reqwest::Method::GET
                },
                &path,
            )?
            .timeout(Duration::from_secs(45));
        let cleanup = self
            .request(reqwest::Method::DELETE, &path)?
            .timeout(Duration::from_secs(4));
        let (tx, rx) = mpsc::channel();
        let lease = shutdown::Lease::acquire(self.background_active.clone());
        let stopped = cancel.clone();
        let hold = self.work.clone();
        let wake = self.wake.clone();
        let target = origin.clone();
        std::thread::Builder::new().name("relay-history-transfer".into()).spawn(move || {
            let _lease=lease;
            let result=(|| {
                if stopped.load(Ordering::Acquire) || hold.hold.load(Ordering::Acquire) { return Err("History transfer was cancelled.".into()); }
                let request=if send {let snapshot=source.snapshot(&target,&stopped)?;request.header("Content-Type","application/octet-stream").body(model::seal(&snapshot,&code,&target)?)} else {request};
                if stopped.load(Ordering::Acquire) || hold.hold.load(Ordering::Acquire) { return Err("History transfer was cancelled.".into()); }
                let response=request.send().map_err(|_| "History transfer could not connect. Your existing messages are unchanged.")?;
                let status=response.status();
                if status==reqwest::StatusCode::NOT_FOUND { return Err("Approve this code on your original PC, then choose Receive history. Transfers expire after 15 minutes or a server restart.".into()); }
                if status==reqwest::StatusCode::CONFLICT { return Err("Another history transfer is pending. Finish it or wait 15 minutes before sending again.".into()); }
                if !status.is_success() { return Err(format!("History transfer failed (HTTP {}). Existing messages are unchanged.",status.as_u16())); }
                if send { return Ok(Outcome::Sent); }
                if response.content_length().is_some_and(|size| size > model::MAX_BYTES as u64) { return Err("History transfer is too large.".into()); }
                let mut wire=Vec::new(); response.take(model::MAX_BYTES as u64+1).read_to_end(&mut wire).map_err(|_| "History transfer was interrupted. Try again.")?;
                if stopped.load(Ordering::Acquire) || hold.hold.load(Ordering::Acquire) { return Err("History transfer was cancelled.".into()); }
                Ok(Outcome::Received(Box::new((source.stage(model::open(&wire,&code,&target)?,&target,&stopped)?,cleanup))))
            })();
            let _=tx.send(result); wake.notify();
        }).map_err(|_| "Cannot start history transfer.")?;
        self.history_transfer_job = Some(Job {
            origin,
            request_id,
            token,
            device_id,
            cancel,
            result: rx,
        });
        Ok(())
    }
    pub(super) fn poll_history_transfer(&mut self) {
        let Some(job) = self.history_transfer_job.as_ref() else {
            return;
        };
        if job.cancel.load(Ordering::Acquire)
            || self.command_connection(&job.origin).is_err()
            || self.connection.as_ref().is_none_or(|c| {
                c.session.token.as_str() != job.token.as_str()
                    || c.crypto
                        .as_ref()
                        .is_none_or(|crypto| crypto.device.identity().device_id != job.device_id)
            })
        {
            // Retain the job until its IO has stopped, so cancellation cannot start
            // an unbounded number of overlapping network workers.
            job.cancel.store(true, Ordering::Release);
        }
        let result = match job.result.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(_) => Err("History transfer stopped unexpectedly. Try again.".into()),
        };
        let job = self.history_transfer_job.take().unwrap();
        let active = !job.cancel.load(Ordering::Acquire)
            && !self.work.hold.load(Ordering::Acquire)
            && self
                .command_connection(&job.origin)
                .is_ok_and(|c| c.session.token.as_str() == job.token.as_str());
        if !active {
            return;
        }
        let result=result.and_then(|outcome| match outcome {
            Outcome::Sent=>Ok("History approved and sent. Choose Receive history on your new PC within 15 minutes.".into()),
            Outcome::Received(received)=>{
                let (snapshot,cleanup)=*received;
                let count=self.connection.as_mut().and_then(|c| c.crypto.as_mut()).ok_or("Protected history is not ready.")?.commit_transferred_history(snapshot,&job.cancel)?;
                // Consumption happens only AFTER the new checkpoint commits. Cleanup failure
                // cannot undo successful local import; server TTL still limits availability.
                let lease=shutdown::Lease::acquire(self.background_active.clone());
                let _=std::thread::Builder::new().name("relay-history-consumed".into()).spawn(move || {let _lease=lease;let _=cleanup.send();});
                let _=self.load_messages();
                Ok(format!("History received: {count} messages added. Existing messages and device verification were preserved."))
            }
        });
        self.emit(Update::HistoryTransferred {
            origin: job.origin.clone(),
            request_id: job.request_id,
            result,
        });
    }
}
