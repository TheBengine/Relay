//! Recovery is RAM-only and bound to the active connection. There is no join
//! fallback when admission is absent, revoked, replaced or expired.
use super::*;
use base64::Engine as _;
use zeroize::{Zeroize, Zeroizing};

pub(super) struct Admission {
    token: Zeroizing<String>,
    pub(super) instance: String,
    pub(super) session: u64,
    device: String,
}
pub(super) struct Recovery {
    pub(super) deadline: Instant,
    prepared_instance: Option<String>,
    pub(super) admitted: bool,
}

fn admitted_presence(
    bootstrap: &Bootstrap,
    user: &str,
    channel: &str,
    admission: &Admission,
) -> bool {
    bootstrap.voice_members.iter().any(|v| {
        v.user.id == user
            && v.channel_id == channel
            && v.device_id.as_deref() == Some(admission.device.as_str())
            && v.session_id == admission.session
    })
}

impl Worker {
    pub(super) fn clear_voice_intent(&mut self) {
        if let Some(c) = &mut self.connection {
            c.voice_channel = None;
            c.voice_attempt = None;
            c.voice_admission = None;
            c.voice_recovery = None;
            c.crypto_pending_join = None;
            c.crypto_call_epoch = None;
            c.frame_ids.clear();
            c.sharing = false;
            c.camera = false;
        }
        self.live.epoch.fetch_add(1, Ordering::SeqCst);
        *self.live.context.lock().unwrap_or_else(|e| e.into_inner()) = None;
        self.live.activity.notify_all();
    }

    pub(super) fn remember_voice_admission(&mut self, ticket: &VoiceTicket) {
        let Some(c) = self.connection.as_mut() else {
            return;
        };
        c.voice_admission = ticket.resume.as_ref().and_then(|r| {
            let device = c.voice_device_id()?;
            if r.instance_id.is_empty()
                || r.instance_id.len() > 128
                || r.token.len() != 43
                || base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .decode(&r.token)
                    .ok()?
                    .len()
                    != 32
            {
                return None;
            }
            Some(Admission {
                token: Zeroizing::new(r.token.clone()),
                instance: r.instance_id.clone(),
                session: ticket.session_id,
                device,
            })
        });
    }

    fn finish_voice_recovery(&mut self, error: &str) {
        let pending = self
            .connection
            .as_ref()
            .and_then(|c| c.crypto_pending_join.clone());
        let active = self.connection.as_ref().and_then(|c| {
            Some((
                CommandOrigin::from(c),
                c.voice_channel.clone()?,
                c.voice_attempt?,
            ))
        });
        self.clear_voice_intent();
        if let Some((channel_id, request_id)) = pending {
            // The UI ignores unscoped VoiceLeft while a join is pending, so an
            // earlier leave cannot cancel a newer request. End this exact join.
            self.emit(Update::VoiceJoinFailed {
                channel_id,
                request_id,
                error: error.into(),
            });
        } else if let Some((origin, channel_id, request_id)) = active {
            self.emit(Update::VoiceCallFailed {
                origin,
                channel_id,
                request_id,
                error: error.into(),
            });
        } else {
            self.emit(Update::VoiceLeft);
            self.emit(Update::Error(error.into()));
        }
    }

    pub(super) fn voice_recovery_expired(&mut self) -> bool {
        if self
            .connection
            .as_ref()
            .and_then(|c| c.voice_recovery.as_ref())
            .is_some_and(|r| Instant::now() >= r.deadline)
        {
            self.finish_voice_recovery(
                "The call could not reconnect. Join the room again to retry.",
            );
            true
        } else {
            false
        }
    }

    pub(super) fn pause_voice_for_outage(&mut self) {
        if self.voice_recovery_expired() {
            return;
        }
        let Some(c) = self.connection.as_mut() else {
            return;
        };
        let Some(channel_id) = c.voice_channel.clone() else {
            return;
        };
        if c.voice_recovery.is_some() {
            return;
        }
        if c.voice_admission.is_none() {
            self.finish_voice_recovery("The call disconnected. Join the room again to reconnect.");
            return;
        }
        c.voice_recovery = Some(Recovery {
            deadline: Instant::now() + Duration::from_secs(30),
            prepared_instance: None,
            admitted: false,
        });
        c.crypto_call_epoch = None;
        c.sharing = false;
        c.camera = false;
        c.frame_ids.clear();
        self.live.epoch.fetch_add(1, Ordering::SeqCst);
        *self.live.context.lock().unwrap_or_else(|e| e.into_inner()) = None;
        self.live.activity.notify_all();
        self.emit(Update::VoiceEncryptionPaused {
            channel_id,
            error: "Reconnecting your call. Camera and screen sharing have stopped.".into(),
            reconnecting: true,
        });
    }

    /// False withholds a pre-admission bootstrap so its missing old presence
    /// cannot erase the UI's bounded pending call. Identity is checked first.
    pub(super) fn recover_voice(&mut self, bootstrap: &mut Bootstrap) -> bool {
        if self.voice_recovery_expired() {
            return true;
        }
        let Some(c) = self.connection.as_ref() else {
            return false;
        };
        let (Some(channel), Some(admission)) = (&c.voice_channel, &c.voice_admission) else {
            return true;
        };
        let channel = channel.clone();
        let changed = admission.instance != bootstrap.instance_id;
        let matches_presence =
            admitted_presence(bootstrap, &c.session.user.id, &channel, admission);
        if !changed && !matches_presence {
            self.finish_voice_recovery(
                "Your call ended or moved to another device. Join the room again to reconnect.",
            );
            return true;
        }
        if changed {
            self.pause_voice_for_outage();
        }
        if self
            .connection
            .as_ref()
            .and_then(|c| c.voice_recovery.as_ref())
            .is_none()
        {
            return true;
        }
        let c = self.connection.as_ref().unwrap();
        if bootstrap.instance_id.is_empty()
            || !c.chat.supports("voice-restart-resume-v1")
            || c.voice_device_id().as_deref()
                != c.voice_admission.as_ref().map(|a| a.device.as_str())
        {
            self.finish_voice_recovery(
                "This call cannot reconnect safely. Join the room again to retry.",
            );
            return true;
        }
        if changed {
            let a = c.voice_admission.as_ref().unwrap();
            let mut input = VoiceResume {
                open_revision: c.chat.open_revision(),
                channel_id: channel.clone(),
                device_id: a.device.clone(),
                resume_token: (*a.token).clone(),
            };
            let remaining = c
                .voice_recovery
                .as_ref()
                .unwrap()
                .deadline
                .saturating_duration_since(Instant::now());
            let response = self
                .request(reqwest::Method::POST, "/api/voice/resume")
                .map(|request| {
                    request
                        .timeout(remaining.min(Duration::from_secs(2)))
                        .json(&input)
                        .send()
                });
            input.resume_token.zeroize();
            let ticket = match response {
                Ok(Ok(response)) if matches!(response.status().as_u16(), 401 | 403 | 404 | 409) => {
                    self.finish_voice_recovery(
                        "The previous call is no longer available. Join the room again to retry.",
                    );
                    return true;
                }
                Ok(Ok(response)) => parse::<VoiceTicket>(response),
                Ok(Err(error)) => Err(network_error(error)),
                Err(error) => Err(error),
            };
            let Ok(ticket) = ticket else {
                self.voice_recovery_expired();
                return false;
            };
            if ticket
                .resume
                .as_ref()
                .is_none_or(|r| r.instance_id != bootstrap.instance_id)
            {
                self.finish_voice_recovery(
                    "The server changed while reconnecting. Join the room again to retry.",
                );
                return true;
            }
            self.remember_voice_admission(&ticket);
            if self
                .connection
                .as_ref()
                .is_none_or(|c| c.voice_admission.is_none())
            {
                self.finish_voice_recovery(
                    "The call could not reconnect safely. Join the room again to retry.",
                );
                return true;
            }
            // Re-read actual presence after admission. Never fabricate it or
            // continue to an endpoint whose community/account has changed.
            let fresh = self.get::<Bootstrap>("/api/bootstrap");
            let Ok(fresh) = fresh else {
                self.voice_recovery_expired();
                return false;
            };
            let c = self.connection.as_ref().unwrap();
            if let Err(error) =
                validate_bootstrap_identity(&c.server_id, &c.session.user.id, &fresh)
            {
                self.reject_active_identity(error);
                return false;
            }
            *bootstrap = fresh;
            if self.voice_recovery_expired() {
                return true;
            }
            let c = self.connection.as_ref().unwrap();
            let admission = c.voice_admission.as_ref().unwrap();
            if bootstrap.instance_id != admission.instance {
                // One resume transaction per refresh. Repeated server changes
                // keep the original deadline and wait for the next refresh,
                // instead of recursively consuming the network worker's stack.
                return false;
            }
            if !admitted_presence(bootstrap, &c.session.user.id, &channel, admission) {
                self.finish_voice_recovery(
                    "Your call ended or moved to another device. Join the room again to reconnect.",
                );
                return true;
            }
        }
        if self.voice_recovery_expired() {
            return true;
        }
        let c = self.connection.as_mut().unwrap();
        let recovery = c.voice_recovery.as_mut().unwrap();
        if recovery.prepared_instance.as_ref() != Some(&bootstrap.instance_id) {
            if let Some(crypto) = c.crypto.as_mut() {
                let scope = format!("{}/channel/{channel}", c.server_id);
                crypto.invalidate_call_freshness(&scope);
            }
            recovery.prepared_instance = Some(bootstrap.instance_id.clone());
        }
        recovery.admitted = true;
        true
    }
}
