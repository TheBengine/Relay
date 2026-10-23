//! Explicit confirmation and account-scoped continuation for a call handoff.
use super::*;
use crate::network::call_switch::{Failure, Ready, Request};

struct Offer {
    request: Request,
    name: String,
}
#[derive(Default)]
pub(super) struct State {
    offer: Option<Offer>,
    pending: Option<Request>,
}
fn matches_source(
    r: &Request,
    origin: Option<&CommandOrigin>,
    channel: Option<&str>,
    attempt: Option<u64>,
    epoch: u64,
) -> bool {
    origin == Some(&r.previous)
        && channel == Some(r.previous_channel.as_str())
        && attempt == Some(r.previous_attempt)
        && epoch == r.media_generation
}
fn matches_continuation(
    r: &Request,
    pending: Option<&Request>,
    origin: Option<&CommandOrigin>,
    handoff_generation: u64,
    current_generation: u64,
) -> bool {
    let expected = handoff_generation.wrapping_add(u64::from(r.origin != r.previous));
    pending.is_some_and(|p| {
        p.operation == r.operation && p.origin == r.origin && p.call_id == r.call_id
    }) && origin == Some(&r.origin)
        && current_generation == expected
}
impl RelayApp {
    pub(super) fn call_switch_pending(&self) -> bool {
        self.call_switch.pending.is_some()
    }
    pub(super) fn offer_leave_and_answer(
        &mut self,
        origin: CommandOrigin,
        call_id: String,
        name: String,
    ) {
        if self.call_switch.pending.is_some() || self.call_switch.offer.is_some() {
            return;
        }
        let (Some(previous), Some(channel), Some(attempt)) = (
            self.command_origin(),
            self.voice_channel.clone(),
            self.voice_join_attempt,
        ) else {
            self.notice = Some((
                "Wait for your current call to connect before answering another.".into(),
                Instant::now(),
            ));
            return;
        };
        if !self.call_alerts.ringing(&origin, &call_id) {
            return;
        }
        let operation = self.next_request();
        self.call_switch.offer = Some(Offer {
            name,
            request: Request {
                origin,
                call_id,
                previous,
                previous_channel: channel,
                previous_attempt: attempt,
                media_generation: self.network.live_media_generation(),
                operation,
                voice_attempt: self.voice_request.wrapping_add(1),
                state: VoiceState {
                    muted: self.muted,
                    deafened: self.deafened,
                    ..Default::default()
                },
            },
        });
    }
    fn current_call_switch(&self, r: &Request) -> bool {
        matches_source(
            r,
            self.command_origin().as_ref(),
            self.voice_channel.as_deref(),
            self.voice_join_attempt,
            self.network.live_media_generation(),
        )
    }
    pub(super) fn call_switch_dialog(&mut self, ctx: &egui::Context) {
        let Some(offer) = self.call_switch.offer.take() else {
            return;
        };
        if !self.current_call_switch(&offer.request)
            || !self
                .call_alerts
                .ringing(&offer.request.origin, &offer.request.call_id)
        {
            return;
        }
        let mut confirm = false;
        let mut cancel = false;
        egui::Window::new("Leave and answer?")
            .id(egui::Id::new("confirm-call-switch"))
            .collapsible(false).resizable(false)
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(format!("Leave your current call and answer {}?", offer.name));
                ui.label(RichText::new(
                    "Relay checks the invitation and prepares its connection before leaving your current call."
                ).color(c_muted()));
                ui.horizontal(|ui| {
                    confirm = ui.add(primary("Leave and answer")).clicked();
                    cancel = ui.add(secondary("Keep current call")).clicked();
                });
            });
        if confirm {
            let mut request = offer.request;
            request.state = VoiceState {
                muted: self.muted,
                deafened: self.deafened,
                ..Default::default()
            };
            match self.network.send(Command::LeaveAndAnswer(request.clone())) {
                Ok(()) => {
                    self.call_alerts.silence(&request.origin, &request.call_id);
                    self.call_switch.pending = Some(request);
                }
                Err(error) => self.error = Some(error),
            }
        } else if !cancel {
            self.call_switch.offer = Some(offer);
        }
    }
    pub(super) fn call_switch_result(
        &mut self,
        request: Request,
        result: Result<Box<Ready>, Failure>,
        queue: &mut std::collections::VecDeque<Update>,
    ) {
        let owns = self.call_switch.pending.as_ref().is_some_and(|p| {
            p.operation == request.operation
                && p.origin == request.origin
                && p.call_id == request.call_id
        });
        let expected_epoch = match &result {
            Ok(ready) => ready.media_generation,
            Err(failure) => failure.media_generation,
        };
        let mut confirmed = request.clone();
        confirmed.media_generation = expected_epoch;
        let current = owns && self.current_call_switch(&confirmed);
        match result {
            Err(failure) => {
                if owns {
                    self.call_switch.pending = None;
                }
                if current {
                    if !failure.previous_kept {
                        self.stop_media();
                        self.private_calls = Default::default();
                    }
                    self.error = Some(failure.message);
                }
            }
            Ok(ready) => {
                if !current {
                    if owns {
                        self.call_switch.pending = None;
                    }
                    self.cancel_call_switch_admission(&ready);
                    return;
                }
                if ready.request.origin != request.origin
                    || ready.request.call_id != request.call_id
                    || ready.request.operation != request.operation
                {
                    self.call_switch.pending = None;
                    self.cancel_call_switch_admission(&ready);
                    return;
                }
                if request.origin != request.previous {
                    queue.push_back(Update::Connected {
                        url: request.origin.url.clone(),
                        session: ready.session.clone(),
                        bootstrap: Box::new(ready.bootstrap.clone()),
                    });
                }
                queue.push_back(Update::CallSwitchStart(ready));
            }
        }
    }
    fn cancel_call_switch_admission(&mut self, ready: &Ready) {
        let _ = self.network.send(Command::CancelSwitchedCall {
            origin: ready.request.origin.clone(),
            instance: ready
                .admission
                .ticket
                .resume
                .as_ref()
                .map(|v| v.instance_id.clone())
                .unwrap_or_default(),
            session: ready.admission.ticket.session_id,
            channel: ready.admission.call.channel_id.clone(),
            device: ready.keys.local_device_id.clone(),
            open_revision: (ready.bootstrap.protection.level
                == relay_protocol::ProtectionLevel::Open)
                .then_some(ready.bootstrap.protection.revision),
        });
    }
    pub(super) fn start_switched_call(&mut self, ready: Box<Ready>) {
        let owns = self.call_switch.pending.as_ref().is_some_and(|p| {
            p.operation == ready.request.operation
                && p.origin == ready.request.origin
                && p.call_id == ready.request.call_id
        });
        if !matches_continuation(
            &ready.request,
            self.call_switch.pending.as_ref(),
            self.command_origin().as_ref(),
            ready.media_generation,
            self.network.live_media_generation(),
        ) {
            if owns {
                self.call_switch.pending = None;
            }
            self.cancel_call_switch_admission(&ready);
            return;
        }
        self.call_switch.pending = None;
        self.bootstrap = Some(Arc::new(ready.bootstrap.clone()));
        if self.voice_channel.is_some() || self.joining_voice.is_some() {
            self.stop_media();
        }
        self.voice_request = ready.request.voice_attempt;
        self.voice_join_attempt = Some(self.voice_request);
        self.voice_channel = Some(ready.admission.call.channel_id.clone());
        self.joining_voice = None;
        self.sound_check_since.clear();
        self.error = None;
        self.home_page = None;
        self.select(Target::Channel(ready.admission.call.channel_id.clone()));
        self.adopt_switched_private_call(
            &ready.admission.call,
            ready.request.origin.clone(),
            self.voice_request,
        );
        self.start_protected_voice(
            &ready.request.origin.url,
            ready.admission.ticket,
            ready.keys,
        );
        self.sync_voice_state();
        self.request_video_connection();
        if let Some(warning) = ready.warning {
            self.notice = Some((warning, Instant::now()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request() -> Request {
        let origin = CommandOrigin {
            url: "https://owned.invalid".into(),
            server_id: "service".into(),
            user_id: "user".into(),
        };
        Request {
            origin: origin.clone(),
            previous: origin,
            call_id: uuid::Uuid::new_v4().to_string(),
            previous_channel: "old".into(),
            previous_attempt: 7,
            media_generation: 9,
            operation: 11,
            voice_attempt: 8,
            state: VoiceState::default(),
        }
    }
    #[test]
    fn confirmation_is_bound_to_the_exact_account_attempt_channel_and_media_generation() {
        let r = request();
        assert!(matches_source(
            &r,
            Some(&r.previous),
            Some("old"),
            Some(7),
            9
        ));
        assert!(!matches_source(
            &r,
            Some(&r.previous),
            Some("new"),
            Some(7),
            9
        ));
        assert!(!matches_source(
            &r,
            Some(&r.previous),
            Some("old"),
            Some(8),
            9
        ));
        assert!(!matches_source(
            &r,
            Some(&r.previous),
            Some("old"),
            Some(7),
            10
        ));
        let other = CommandOrigin {
            user_id: "other".into(),
            ..r.previous.clone()
        };
        assert!(!matches_source(&r, Some(&other), Some("old"), Some(7), 9));
        assert!(!matches_source(&r, None, None, None, 9));
    }
    #[test]
    fn continuation_allows_only_expected_teardown_and_rejects_logout_or_changed_generation() {
        let mut r = request();
        r.origin.url = "https://destination.invalid".into();
        assert!(matches_continuation(&r, Some(&r), Some(&r.origin), 10, 11));
        assert!(!matches_continuation(&r, Some(&r), Some(&r.origin), 10, 10));
        assert!(!matches_continuation(&r, Some(&r), Some(&r.origin), 10, 12));
        assert!(!matches_continuation(&r, None, Some(&r.origin), 10, 11));
        assert!(!matches_continuation(&r, Some(&r), None, 10, 11));
        assert!(!matches_continuation(
            &r,
            Some(&r),
            Some(&r.previous),
            10,
            11
        ));
        let other = CommandOrigin {
            user_id: "different-login".into(),
            ..r.origin.clone()
        };
        assert!(!matches_continuation(&r, Some(&r), Some(&other), 10, 11));
        r.origin = r.previous.clone();
        assert!(matches_continuation(&r, Some(&r), Some(&r.origin), 10, 10));
        assert!(!matches_continuation(&r, Some(&r), Some(&r.origin), 10, 11));
    }
}
