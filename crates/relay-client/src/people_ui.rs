//! Incoming account-wide calls remain visible while another server is selected.
use super::*;

#[path = "people_answer.rs"]
mod answer;

#[cfg(all(test, windows))]
#[path = "people_native_fixture.rs"]
mod native_fixture;

#[derive(Clone)]
struct Incoming {
    origin: CommandOrigin,
    id: String,
    name: String,
    expires: u64,
}

struct PendingDecline {
    origin: CommandOrigin,
    call_id: String,
    request_id: u64,
}

#[derive(Default)]
pub(super) struct State {
    incoming: Option<Incoming>,
    dismissed: HashSet<String>,
    pending: Option<PendingDecline>,
    error: Option<(CommandOrigin, String, String)>,
    answer: Option<answer::Intent>,
}

fn incoming(url: &str, b: &Bootstrap, timestamp: u64) -> Option<Incoming> {
    if !b.capabilities.iter().any(|c| c == "people-v1") {
        return None;
    }
    b.social.calls.iter().find_map(|call| {
        if call.recipient_id != b.user.id
            || call.state != "ringing"
            || call.expires_at <= timestamp
            || !b.social.friends.contains(&call.caller_id)
            || b.social.blocked.contains(&call.caller_id)
            || uuid::Uuid::parse_str(&call.id).is_err()
        {
            return None;
        }
        let peer = b.members.iter().find(|u| u.id == call.caller_id)?;
        Some(Incoming {
            origin: CommandOrigin {
                url: url.into(),
                server_id: b.server_id.clone(),
                user_id: b.user.id.clone(),
            },
            id: call.id.clone(),
            name: profile_ui::display_name(peer).into(),
            expires: call.expires_at,
        })
    })
}

impl RelayApp {
    fn decline_background_people_call(&mut self, call: &Incoming) {
        if self.people_ui.pending.is_some() {
            return;
        }
        self.people_ui.error = None;
        self.call_alerts.silence(&call.origin, &call.id);
        let request_id = self.next_request();
        match self.network.send(Command::DeclinePeopleCall {
            origin: call.origin.clone(),
            call_id: call.id.clone(),
            request_id,
        }) {
            Ok(()) => {
                self.people_ui.pending = Some(PendingDecline {
                    origin: call.origin.clone(),
                    call_id: call.id.clone(),
                    request_id,
                });
            }
            Err(error) => {
                self.people_ui.error = Some((call.origin.clone(), call.id.clone(), error));
            }
        }
    }

    pub(super) fn people_call_declined(
        &mut self,
        origin: CommandOrigin,
        call_id: String,
        request_id: u64,
        result: Result<(), String>,
    ) {
        if self.call_alerts.clock(&origin).is_none()
            || !self.people_ui.pending.as_ref().is_some_and(|p| {
                p.origin == origin && p.call_id == call_id && p.request_id == request_id
            })
        {
            // A late result from a signed-out/replaced account owns no UI here.
            return;
        }
        self.people_ui.pending = None;
        match result {
            Ok(()) => {
                self.people_ui.dismissed.insert(call_id);
                self.people_ui.error = None;
            }
            Err(error) => self.people_ui.error = Some((origin, call_id, error)),
        }
    }

    pub(super) fn remember_people_call(&mut self, url: &str, b: &Bootstrap) {
        if self.people_url().as_deref() != Some(url) {
            return;
        }
        if self.central_account.account.as_ref().is_none_or(|a| {
            !b.linked_identities
                .iter()
                .any(|l| l.user_id == b.user.id && l.issuer == a.issuer && l.handle == a.handle)
        }) {
            self.people_ui = Default::default();
            return;
        }
        let origin = CommandOrigin {
            url: url.into(),
            server_id: b.server_id.clone(),
            user_id: b.user.id.clone(),
        };
        let Some(clock) = self.call_alerts.clock(&origin) else {
            self.people_ui.incoming = None;
            return;
        };
        let timestamp = clock.unix_seconds;
        self.people_ui.incoming = incoming(url, b, timestamp)
            .filter(|call| self.call_alerts.ringing(&call.origin, &call.id));
        self.people_ui.dismissed.retain(|id| {
            b.social
                .calls
                .iter()
                .any(|c| &c.id == id && c.expires_at > timestamp)
        });
    }

    pub(super) fn people_call_notice(&mut self, ctx: &egui::Context) {
        let Some(call) = self.people_ui.incoming.clone().filter(|c| {
            self.call_alerts
                .clock(&c.origin)
                .is_some_and(|clock| c.expires > clock.unix_seconds)
                && self.call_alerts.ringing(&c.origin, &c.id)
                && c.origin.url != self.login.url
                && !self.people_ui.dismissed.contains(&c.id)
                && self.central_account.account.is_some()
        }) else {
            return;
        };
        let Some(clock) = self.call_alerts.clock(&call.origin) else {
            return;
        };
        let switching = self.call_switch_pending();
        let answering = self.people_ui.answer.is_some();
        let pending = self.people_ui.pending.is_some() || switching || answering;
        let can_answer = self.voice_channel.is_none() && self.joining_voice.is_none();
        egui::Window::new("Incoming Relay call")
            .id(egui::Id::new("background-people-call"))
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::RIGHT_BOTTOM, [-20.0, -20.0])
            .show(ctx, |ui| {
                let label = ui.label(format!("{} is calling", call.name));
                if clock.server_sample_age.is_none() {
                    label.on_hover_text("This older server uses your PC's clock for call timing. Ask the host to update Relay.");
                }
                ui.label(
                    RichText::new(if can_answer {
                        "Answer here, or decline this call."
                    } else {
                        "You can leave your current call to answer, or decline here."
                    })
                    .color(c_muted()),
                );
                if let Some((_, _, error)) = self
                    .people_ui
                    .error
                    .as_ref()
                    .filter(|(origin, id, _)| origin == &call.origin && id == &call.id)
                {
                    ui.colored_label(c_bad(), error);
                }
                ui.horizontal(|ui| {
                    if !can_answer && ui.add_enabled(!pending, primary(
                        if switching { "Preparing call…" } else { "Leave and answer" }
                    )).clicked() {
                        self.offer_leave_and_answer(call.origin.clone(), call.id.clone(), call.name.clone());
                    }
                    if ui
                        .add_enabled(can_answer && !pending, primary(if answering { "Answering…" } else { "Answer" }))
                        .clicked()
                    {
                        self.answer_background_people_call(&call);
                    }
                    if ui
                        .add_enabled(
                            !pending,
                            secondary(if pending && !switching { "Declining…" } else { "Decline" }),
                        )
                        .clicked()
                    {
                        self.decline_background_people_call(&call);
                    }
                    if ui.button("Dismiss").clicked() {
                        self.cancel_background_people_answer();
                        self.call_alerts.silence(&call.origin, &call.id);
                        self.people_ui.dismissed.insert(call.id.clone());
                    }
                });
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn people_incoming_calls_require_current_friend_recipient_and_fresh_ring() {
        let mut b = Bootstrap::default();
        b.user.id = "me".into();
        b.server_id = "service".into();
        b.capabilities.push("people-v1".into());
        b.members.push(User {
            id: "friend".into(),
            username: "alex".into(),
            ..Default::default()
        });
        b.social.friends.push("friend".into());
        b.social.calls.push(PrivateCall {
            id: uuid::Uuid::new_v4().to_string(),
            caller_id: "friend".into(),
            recipient_id: "me".into(),
            state: "ringing".into(),
            expires_at: 200,
            channel_id: "owned-pair-room".into(),
        });
        assert!(incoming("https://accounts.invalid/people", &b, 100).is_some());
        assert!(incoming("https://accounts.invalid/people", &b, 200).is_none());
        b.social.blocked.push("friend".into());
        assert!(incoming("https://accounts.invalid/people", &b, 100).is_none());
        b.social.blocked.clear();
        b.social.calls[0].recipient_id = "other".into();
        assert!(incoming("https://accounts.invalid/people", &b, 100).is_none());
        b.social.calls[0].recipient_id = "me".into();
        b.social.friends.clear();
        assert!(incoming("https://accounts.invalid/people", &b, 100).is_none());
    }
}
