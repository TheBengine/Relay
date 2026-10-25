//! Account-scoped handover jobs do not block calls or the control worker.
use super::*;
use crate::handover_client::{Action, Fault, Job};

impl Worker {
    pub(super) fn start_handover(
        &mut self,
        origin: CommandOrigin,
        action: Action,
        request_id: u64,
    ) {
        // A click from a replaced account cannot cancel or replace its job.
        let context = self.command_connection(&origin).ok().and_then(|c| {
            c.chat
                .authority
                .clone()
                .map(|pin| (pin, zeroize::Zeroizing::new(c.session.token.clone())))
        });
        let Some((pin, token)) = context else {
            self.emit(Update::Handover(Job::failure(
                origin,
                request_id,
                Fault::Binding,
            )));
            return;
        };
        if matches!(&action, Action::RunDestination) && self.handover_host_busy() {
            self.emit(Update::Handover(Job::failure(
                origin,
                request_id,
                Fault::HostBusy,
            )));
            return;
        }
        // Never run two local companions against the same keys/cache. Stop must
        // finish before another operation is allowed, including account switches.
        if self.handover_job.is_some() {
            self.emit(Update::Handover(Job::failure(
                origin,
                request_id,
                Fault::Connection,
            )));
            return;
        }
        let wake = self.wake.clone();
        let ctx = self.ctx.clone();
        match Job::start(
            origin.clone(),
            pin,
            token,
            action,
            request_id,
            self.work.background.clone(),
            move || {
                wake.notify();
                ctx.request_repaint();
            },
        ) {
            Ok(job) => self.handover_job = Some((job, 0)),
            Err(fault) => self.emit(Update::Handover(Job::failure(origin, request_id, fault))),
        }
    }

    pub(super) fn poll_handover(&mut self) {
        if self
            .handover_runtime
            .as_mut()
            .is_some_and(|r| !r.is_alive())
        {
            self.handover_runtime = None;
            self.emit(Update::LocalStopped);
            self.emit(Update::Notice("This PC's transferred host stopped. Its data and keys were kept; restart that host to continue.".into()));
        }
        let Some((job, sequence)) = &mut self.handover_job else {
            return;
        };
        let view = job.view();
        if view.sequence == *sequence {
            return;
        }
        *sequence = view.sequence;
        let current = self
            .connection
            .as_ref()
            .is_some_and(|c| CommandOrigin::from(c) == view.origin);
        let finished = view.finished;
        let runtime = if finished { job.take_runtime() } else { None };
        let restoring = job.is_restore();
        if finished {
            self.handover_job = None;
        }
        if let Some(runtime) = runtime {
            if self.handover_runtime.is_some() {
                // Never replace a live owned host, even if an earlier job's
                // completion was queued before the currently running host.
                drop(runtime);
                self.emit(Update::Error(Fault::HostBusy.message().into()));
                return;
            }
            let remembered = runtime.recipe().and_then(|r| r.remember()).is_ok();
            self.handover_runtime = Some(runtime);
            self.emit(Update::HandoverHostReady { remembered });
        } else if restoring && finished {
            if let Some(fault) = view.fault {
                self.emit(Update::Error(format!(
                    "The transferred host could not restart. {}",
                    fault.message()
                )));
            }
        }
        if current {
            self.emit(Update::Handover(view));
        }
    }

    fn handover_host_busy(&self) -> bool {
        self.handover_runtime.is_some()
            || self.local_server.is_some()
            || self.owned_local.is_some()
            || self.local_server_ready()
    }
    pub(super) fn restore_handover_host(&mut self) -> Result<(), String> {
        if self.handover_runtime.is_some() {
            self.emit(Update::HandoverHostReady { remembered: true });
            return Ok(());
        }
        if self.handover_job.is_some() || self.handover_host_busy() {
            return Err(
                "Another local host is already running or being prepared. It was left untouched."
                    .into(),
            );
        }
        let recipe = crate::handover_runtime::remembered()
            .map_err(|f| f.message().to_owned())?
            .ok_or("No transferred host is selected on this PC.")?;
        let wake = self.wake.clone();
        let ctx = self.ctx.clone();
        let job = Job::restore(recipe, self.work.background.clone(), move || {
            wake.notify();
            ctx.request_repaint();
        })
        .map_err(|f| f.message().to_owned())?;
        self.handover_job = Some((job, 0));
        self.emit(Update::Notice(
            "Resuming this PC's transferred community…".into(),
        ));
        Ok(())
    }
    pub(super) fn share_handover_host(&mut self) -> Result<(), String> {
        let id=self.handover_runtime.as_ref().ok_or("No transferred host is running.")?.server_id().to_owned();
        let connection=self.connection.as_ref().filter(|c|
            self.handover_runtime.as_ref().is_some_and(|r|r.matches_connection(&c.server_id,&c.url)))
            .ok_or("Sign in to this PC's transferred community before inviting friends.")?;
        let user=connection.session.user.id.clone();
        let bootstrap:Bootstrap=self.get("/api/bootstrap")?;
        validate_bootstrap_identity(&id,&user,&bootstrap)?;
        if !bootstrap.has_permission(PERMISSION_ADMIN) {
            return Err("Only a server administrator can share this host.".into());
        }
        let route=crate::iroh_route::hosted(&id)
            .ok_or("This transferred host has no running account-free connection. Restart its saved host to reconnect.")?;
        // Reuse the granted runtime's scoped endpoint and current admission.
        // Never launch the default data folder or replace the existing lease.
        self.emit(Update::IrohReady{route,only:true});
        Ok(())
    }
    pub(super) fn stop_handover_host(&mut self) {
        let Some(mut host) = self.handover_runtime.take() else {
            return;
        };
        let id = host.server_id().to_owned();
        host.shutdown();
        drop(host);
        self.saved_connections.retain(|_, s| {
            if s.connection.server_id == id {
                s.stop.store(true, Ordering::Release);
                false
            } else {
                true
            }
        });
        if self.connection.as_ref().is_some_and(|c| c.server_id == id) {
            self.cancel_transfers();
            self.connection = None;
            self.chat_media_epoch.fetch_add(1, Ordering::SeqCst);
            self.chat_media_workers = None;
            self.legacy_reactions.clear();
            self.socket_stop.store(true, Ordering::Release);
            self.emit(Update::Disconnected);
        }
        self.emit(Update::LocalStopped);
        self.publish_communities();
    }
}
