//! Remembered reconnect uses public key proofs before a token changes hosts.
use super::*;
impl Worker {
    pub(super) fn resume_same_host(
        &mut self,
        url: String,
        remembered: crate::credential_vault::RememberedSession,
        expected_server_id: Option<String>,
    ) -> Result<(), String> {
        let (session, bootstrap, call_received, alert_cursor) =
            match resume_remembered(&self.http, &url, &remembered, expected_server_id.as_deref()) {
                Ok(value) => value,
                Err(error) => {
                    if error == "Sign in again" {
                        let _ = crate::credential_vault::forget(&url);
                    }
                    return Err(error);
                }
            };
        let authority = crate::host_route::bootstrap_pin(
            &url,
            &bootstrap.server_id,
            bootstrap.host_authority.as_ref(),
            None,
        )?;
        if remembered.server_id.is_none() || (remembered.authority.is_none() && authority.is_some())
        {
            // Upgrade only after known community and account match.
            // A failed vault write must preserve the current UI.
            crate::credential_vault::save_bound(
                &url,
                &bootstrap.server_id,
                &session,
                authority
                    .as_ref()
                    .map(|pin| crate::host_route::RememberedAuthority {
                        pin: pin.clone(),
                        account_provider: bootstrap.account_provider.clone(),
                        previous_urls: Vec::new(),
                    })
                    .as_ref(),
            )?;
        }
        self.live.epoch.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut context) = self.live.context.lock() {
            *context = None;
        }
        self.park_connection(&url);
        self.chat_media_epoch.fetch_add(1, Ordering::SeqCst);
        self.chat_media_workers = None;
        self.legacy_reactions.clear();
        self.socket_stop = Arc::new(AtomicBool::new(false));
        self.refresh_pending = Arc::new(AtomicBool::new(false));
        self.connection = Some(Connection {
            url: url.clone(),
            session: session.clone(),
            current: None,
            voice_channel: None,
            voice_attempt: None,
            voice_admission: None,
            voice_recovery: None,
            frame_ids: HashMap::new(),
            sharing: false,
            camera: false,
            server_id: bootstrap.server_id.clone(),
            name: bootstrap.server_name.clone(),
            change_cursor: bootstrap.latest_change_sequence,
            protocol_version: bootstrap.protocol_version,
            remembered: true,
            chat: extended::ChatSession::new(&bootstrap),
            crypto: None,
            crypto_registered: false,
            crypto_call_epoch: None,
            open_call_context: None,
            crypto_pending_join: None,
        });
        self.connection.as_mut().unwrap().chat.authority = authority;
        self.connection.as_mut().unwrap().chat.authority_aliases = remembered
            .authority
            .as_ref()
            .map(|a| a.previous_urls.clone())
            .unwrap_or_default();
        self.sync_live_transport()?;
        spawn_events_with_alerts(
            url.clone(),
            session.token.clone(),
            self.refresh_pending.clone(),
            self.socket_stop.clone(),
            self.connection.as_ref().unwrap().chat.events.clone(),
            self.wake.clone(),
            self.alert_watch(&bootstrap, call_received, alert_cursor),
        );
        self.emit(Update::Connected {
            url: url.clone(),
            session: session.clone(),
            bootstrap: Box::new(bootstrap),
        });
        self.publish_communities();
        Ok(())
    }
}

impl Worker {
    pub(super) fn begin_host_resume(
        &mut self,
        url: String,
        remembered: crate::credential_vault::RememberedSession,
    ) -> Result<(), String> {
        let saved = remembered
            .authority
            .as_ref()
            .ok_or(crate::host_route::REFUSED)?;
        let origin = CommandOrigin {
            url: url.clone(),
            server_id: saved.pin.server_id.clone(),
            user_id: remembered.session.user.id.clone(),
        };
        let wake = self.wake.clone();
        let ctx = self.ctx.clone();
        let job = crate::host_route::Job::start(
            url,
            saved.pin.clone(),
            crate::host_locator::ReadCapability::session(&saved.pin, &remembered.session.token),
            self.background_active.clone(),
            move || {
                wake.notify();
                ctx.request_repaint();
            },
        )?;
        self.pending_host_resume = Some(PendingHostResume {
            origin,
            remembered,
            job,
        });
        self.emit(Update::Notice(
            "Checking this community's current host…".into(),
        ));
        Ok(())
    }
    pub(super) fn poll_host_resume(&mut self) {
        let Some(pending) = self.pending_host_resume.take() else {
            return;
        };
        let Some(answer) = pending.job.poll() else {
            self.pending_host_resume = Some(pending);
            return;
        };
        if self.live.shutdown.load(Ordering::Acquire) {
            return;
        }
        if let Err(error) = self.finish_host_resume(pending, answer) {
            self.emit(Update::Error(error));
        }
    }
    pub(super) fn finish_host_resume(
        &mut self,
        pending: PendingHostResume,
        answer: Result<Option<crate::host_route::VerifiedMove>, String>,
    ) -> Result<(), String> {
        // The pinned host itself is paused for a move: it can still be reopened
        // with this remembered login, so the move can be finished or cancelled.
        let next = match answer {
            Err(error) if error == crate::host_route::PAUSED => None,
            other => other?,
        };
        if let Some(next) = next {
            if self.saved_connections.contains_key(&next.url) {
                return Err(
                    "The new host is already open. Switch to that server session to continue."
                        .into(),
                );
            }
            // A reconnecting background server must not implicitly leave a
            // different community's call while its address is being checked.
            if self
                .connection
                .as_ref()
                .is_some_and(|c| c.url != pending.origin.url && c.voice_channel.is_some())
            {
                return Err("The community moved. Finish your current call, then open this community again.".into());
            }
            let saved = pending
                .remembered
                .authority
                .as_ref()
                .ok_or(crate::host_route::REFUSED)?;
            let moving = MovingSession {
                aliases: crate::host_route::alias_history(
                    &pending.origin.url,
                    &saved.previous_urls,
                ),
                origin: pending.origin,
                pin: next.pin,
                provider: saved.account_provider.clone(),
                remembered: true,
                resuming: true,
            };
            self.adopt_session_checked(
                next.url,
                Some(moving.origin.server_id.clone()),
                pending.remembered.session,
                None,
                Some(moving),
                None,
            )?;
        } else {
            self.resume_same_host(
                pending.origin.url,
                pending.remembered,
                Some(pending.origin.server_id),
            )?;
        }
        Ok(())
    }
}
