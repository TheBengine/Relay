//! One explicit answer survives only its own identity-checked account switch.
//! Acceptance and protected voice still use the ordinary private-call path.
use super::*;
use relay_protocol::PrivateCall;

pub(super) struct Intent {
    origin: CommandOrigin,
    call: String,
    previous: Option<CommandOrigin>,
    issuer: String,
    subject: String,
    handle: String,
    generation: u64,
    deadline: Instant,
}

struct Context<'a> {
    previous: Option<&'a CommandOrigin>,
    account: Option<&'a crate::central_account::Account>,
    generation: Option<u64>,
    ringing: bool,
    idle: bool,
    navigation_idle: bool,
    now: Instant,
}

impl Intent {
    fn current(&self, c: &Context<'_>) -> bool {
        c.now < self.deadline
            && c.previous == self.previous.as_ref()
            && c.generation == Some(self.generation)
            && c.ringing
            && c.idle
            && c.navigation_idle
            && c.account.is_some_and(|a| {
                a.issuer == self.issuer && a.subject == self.subject && a.handle == self.handle
            })
    }

    fn call(&self, c: &Context<'_>, url: &str, b: &Bootstrap, at: u64) -> Option<PrivateCall> {
        if !self.current(c)
            || url != self.origin.url
            || b.server_id != self.origin.server_id
            || b.user.id != self.origin.user_id
            || !b.capabilities.iter().any(|cap| cap == "people-v1")
            || !b.capabilities.iter().any(|cap| cap == "private-calls-v1")
            || !b.linked_identities.iter().any(|link| {
                link.user_id == b.user.id
                    && link.issuer == self.issuer
                    && link.handle == self.handle
            })
        {
            return None;
        }
        b.social
            .calls
            .iter()
            .find(|call| {
                call.id == self.call
                    && call.state == "ringing"
                    && call.recipient_id == b.user.id
                    && call.expires_at > at
                    && private_call_ui::valid_for(call, b)
                    && private_call_ui::channel_available(call, b)
            })
            .cloned()
    }
}

impl RelayApp {
    pub(in super::super) fn cancel_background_people_answer(&mut self) {
        self.people_ui.answer = None;
    }

    fn background_answer_context<'a>(
        &'a self,
        intent: &Intent,
        previous: Option<&'a CommandOrigin>,
    ) -> Context<'a> {
        Context {
            previous,
            account: self.central_account.account.as_ref(),
            generation: self.call_alerts.generation(&intent.origin),
            ringing: self.call_alerts.ringing(&intent.origin, &intent.call),
            idle: self.voice_channel.is_none()
                && self.joining_voice.is_none()
                && self.private_calls.intent.is_none()
                && !self.call_switch_pending()
                && !self
                    .admin_requests
                    .values()
                    .any(|key| key.starts_with("private-call:")),
            navigation_idle: !self.exiting
                && !self.account_start.home
                && self.account_start.pending.is_none()
                && !self.add_community_open,
            now: Instant::now(),
        }
    }

    pub(super) fn answer_background_people_call(&mut self, call: &Incoming) {
        if self.people_ui.answer.is_some() || self.people_ui.pending.is_some() {
            return;
        }
        let Some(account) = self.central_account.account.as_ref() else {
            return;
        };
        let Some(generation) = self.call_alerts.generation(&call.origin) else {
            return;
        };
        let Some(clock) = self.call_alerts.clock(&call.origin) else {
            return;
        };
        let now = Instant::now();
        let Some(deadline) = clock.deadline(call.expires, now) else {
            return;
        };
        let previous = self.command_origin();
        let intent = Intent {
            origin: call.origin.clone(),
            call: call.id.clone(),
            previous: previous.clone(),
            issuer: account.issuer.clone(),
            subject: account.subject.clone(),
            handle: account.handle.clone(),
            generation,
            deadline: deadline.min(now + Duration::from_secs(20)),
        };
        let was_home = self.account_start.home;
        self.account_start.home = false;
        if !intent.current(&self.background_answer_context(&intent, previous.as_ref())) {
            self.account_start.home = was_home;
            return;
        }
        if !self.communities.iter().any(|c| c.url == call.origin.url)
            || self.community_status(&call.origin.url) == ConnectionStatus::SignInRequired
        {
            self.people_ui.error = Some((
                call.origin.clone(),
                call.id.clone(),
                "Open Friends and sign in again before answering this call.".into(),
            ));
            self.account_start.home = was_home;
            return;
        }
        self.save_preferences();
        self.server_actions.cancel_if_other(&call.origin.url);
        match self.network.send(Command::SwitchCommunity {
            url: call.origin.url.clone(),
        }) {
            Ok(()) => {
                self.people_ui.error = None;
                self.people_ui.answer = Some(intent);
            }
            Err(error) => {
                self.account_start.home = was_home;
                self.people_ui.error = Some((call.origin.clone(), call.id.clone(), error));
            }
        }
    }

    pub(in super::super) fn poll_background_people_answer(&mut self, ctx: &egui::Context) {
        let previous = self.command_origin();
        if self.people_ui.answer.as_ref().is_some_and(|intent| {
            !intent.current(&self.background_answer_context(intent, previous.as_ref()))
        }) {
            self.people_ui.answer = None;
            self.notice = Some(("The call changed or could not be opened in time. Open Friends to check its status.".into(), Instant::now()));
        }
        if self.people_ui.answer.is_some() {
            ctx.request_repaint_after(Duration::from_millis(200));
        }
    }

    pub(in super::super) fn take_background_people_answer(
        &mut self,
        url: &str,
        b: &Bootstrap,
    ) -> Option<PrivateCall> {
        let intent = self.people_ui.answer.take()?;
        let previous = self.command_origin();
        let c = self.background_answer_context(&intent, previous.as_ref());
        let call = self
            .call_alerts
            .clock(&intent.origin)
            .and_then(|clock| intent.call(&c, url, b, clock.unix_seconds));
        if call.is_none() && url == intent.origin.url {
            self.notice = Some((
                "This call is no longer available to answer. Check Friends for its current status."
                    .into(),
                Instant::now(),
            ));
        }
        call
    }
}

#[cfg(test)]
#[path = "people_answer_tests.rs"]
mod tests;

#[cfg(all(test, windows))]
#[path = "people_answer_native_fixture.rs"]
mod native_fixture;
