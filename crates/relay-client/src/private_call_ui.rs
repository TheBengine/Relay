//! Call invitations never bypass the ordinary voice/protection join path.
use super::*;
use relay_protocol::PrivateCall;
use serde_json::{json, Value};

pub(super) struct Intent {
    origin: CommandOrigin,
    id: String,
    deadline: Instant,
}

pub(super) struct Active {
    origin: CommandOrigin,
    id: String,
    channel: String,
    peer: String,
    request: u64,
}

#[derive(Default)]
pub(super) struct State {
    pub(super) intent: Option<Intent>,
    active: Option<Active>,
    dismissed: HashSet<String>,
    origin: Option<CommandOrigin>,
    pending: Option<(String, CommandOrigin)>,
}

impl State {
    fn bind_origin(&mut self, origin: Option<CommandOrigin>) {
        if self.origin != origin {
            *self = Self { origin, ..Default::default() };
        }
    }
}

#[cfg(test)]
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn peer<'a>(call: &'a PrivateCall, user: &str) -> Option<&'a str> {
    if call.caller_id == user {
        Some(&call.recipient_id)
    } else if call.recipient_id == user {
        Some(&call.caller_id)
    } else {
        None
    }
}

pub(super) fn valid_for(call: &PrivateCall, bootstrap: &Bootstrap) -> bool {
    let Some(peer) = peer(call, &bootstrap.user.id) else {
        return false;
    };
    peer != bootstrap.user.id
        && bootstrap.social.friends.iter().any(|id| id == peer)
        && !bootstrap.social.blocked.iter().any(|id| id == peer)
        && bootstrap.members.iter().any(|member| member.id == peer)
        && uuid::Uuid::parse_str(&call.id).is_ok()
        && matches!(
            call.state.as_str(),
            "ringing" | "accepted" | "declined" | "cancelled" | "missed" | "ended"
        )
}

pub(super) fn channel_available(call: &PrivateCall, bootstrap: &Bootstrap) -> bool {
    pair_channel_available(
        &call.channel_id,
        &call.caller_id,
        &call.recipient_id,
        bootstrap,
    )
}

fn pair_channel_available(
    channel_id: &str,
    caller_id: &str,
    recipient_id: &str,
    bootstrap: &Bootstrap,
) -> bool {
    let Some(voice) = bootstrap
        .conversations
        .iter()
        .find(|room| room.channel_id == channel_id && room.kind == "group_voice" && !room.archived)
    else {
        return false;
    };
    bootstrap
        .channels
        .iter()
        .any(|channel| channel.id == channel_id && channel.kind == "voice" && channel.can_join)
        && bootstrap.conversations.iter().any(|group| {
            Some(&group.channel_id) == voice.parent_id.as_ref()
                && group.kind == "group"
                && !group.archived
                && group.member_ids.len() == 2
                && group.member_ids.iter().any(|id| id == caller_id)
                && group.member_ids.iter().any(|id| id == recipient_id)
        })
}

impl RelayApp {
    pub(super) fn adopt_switched_private_call(
        &mut self,
        call: &PrivateCall,
        origin: CommandOrigin,
        request: u64,
    ) {
        self.private_calls = State {
            origin: Some(origin.clone()),
            active: Some(Active {
                peer: peer(call, &origin.user_id).unwrap_or_default().into(),
                origin,
                id: call.id.clone(),
                channel: call.channel_id.clone(),
                request,
            }),
            ..Default::default()
        };
    }
    pub(super) fn finish_private_call(&mut self) {
        let Some(active) = self.private_calls.active.take() else {
            return;
        };
        if self.command_origin().as_ref() != Some(&active.origin) {
            return;
        }
        let key = format!("private-call:end:{}", active.id);
        self.admin_request(
            AdminMethod::Post,
            format!("/api/private-calls/{}", active.id),
            json!({"action":"end"}),
            key.clone(),
        );
        self.private_calls.pending = Some((key, active.origin));
    }

    fn join_private_call(&mut self, call: &PrivateCall, origin: CommandOrigin) {
        if self
            .call_alerts
            .clock(&origin)
            .is_none_or(|clock| call.expires_at <= clock.unix_seconds)
        {
            return;
        }
        self.private_calls.intent = None;
        self.home_page = None;
        self.select(Target::Channel(call.channel_id.clone()));
        self.join_voice(call.channel_id.clone());
        if self.joining_voice.as_deref() == Some(call.channel_id.as_str()) {
            let peer = peer(call, &origin.user_id).unwrap_or_default().to_owned();
            self.private_calls.active = Some(Active {
                origin,
                id: call.id.clone(),
                channel: call.channel_id.clone(),
                peer,
                request: self.voice_request,
            });
        }
    }
    pub(super) fn start_friend_call(&mut self, peer_id: &str) {
        self.cancel_background_people_answer();
        let Some(bootstrap) = &self.bootstrap else {
            return;
        };
        if !bootstrap
            .capabilities
            .iter()
            .any(|cap| cap == "private-calls-v1")
        {
            self.open_friend_voice_room(peer_id);
            return;
        }
        if self.voice_channel.is_some()
            || self.joining_voice.is_some()
            || self.private_calls.intent.is_some()
        {
            self.notice = Some((
                "Finish your current call before calling someone else.".into(),
                Instant::now(),
            ));
            return;
        }
        if !bootstrap.social.friends.iter().any(|id| id == peer_id)
            || bootstrap.social.blocked.iter().any(|id| id == peer_id)
        {
            return;
        }
        if self
            .admin_requests
            .values()
            .any(|key| key.starts_with("private-call:"))
        {
            return;
        }
        let Some(origin) = self.command_origin() else {
            return;
        };
        let key = format!("private-call:start:{peer_id}");
        self.admin_request(
            AdminMethod::Post,
            "/api/private-calls".into(),
            json!({"peer_id":peer_id}),
            key.clone(),
        );
        self.private_calls.origin = Some(origin.clone());
        self.private_calls.pending = Some((key, origin));
    }

    pub(super) fn answer_private_call(&mut self, call: &PrivateCall, action: &str) {
        self.cancel_background_people_answer();
        if self
            .admin_requests
            .values()
            .any(|key| key.starts_with("private-call:"))
        {
            return;
        }
        if action != "accept" {
            self.private_calls.intent = None;
        }
        let Some(origin) = self.command_origin() else {
            return;
        };
        if action == "accept"
            && (!self.call_alerts.ringing(&origin, &call.id)
                || self
                    .call_alerts
                    .clock(&origin)
                    .is_none_or(|clock| call.expires_at <= clock.unix_seconds))
        {
            return;
        }
        self.call_alerts.silence(&origin, &call.id);
        // A background answer can arrive before the destination panel's first
        // frame. Bind now so that frame cannot discard its correlated reply.
        self.private_calls.bind_origin(Some(origin.clone()));
        let key = format!("private-call:{action}:{}", call.id);
        self.admin_request(
            AdminMethod::Post,
            format!("/api/private-calls/{}", call.id),
            json!({"action":action}),
            key.clone(),
        );
        self.private_calls.pending = Some((key, origin));
    }

    pub(super) fn private_call_result(&mut self, key: &str, data: Value) {
        let Some(origin) = self.command_origin() else {
            return;
        };
        if self
            .private_calls
            .pending
            .as_ref()
            .is_none_or(|(pending_key, pending_origin)| {
                pending_key != key || pending_origin != &origin
            })
        {
            return;
        }
        self.private_calls.pending = None;
        let Ok(call) = serde_json::from_value::<PrivateCall>(data) else {
            return;
        };
        let Some(bootstrap) = &self.bootstrap else {
            return;
        };
        if !valid_for(&call, bootstrap) {
            return;
        }
        let start_matches = key
            .strip_prefix("private-call:start:")
            .is_some_and(|peer| call.caller_id == bootstrap.user.id && call.recipient_id == peer);
        let accept_matches = key
            .strip_prefix("private-call:accept:")
            .is_some_and(|id| call.recipient_id == bootstrap.user.id && call.id == id);
        let Some(clock) = self.call_alerts.clock(&origin) else {
            return;
        };
        if (start_matches || accept_matches)
            && call.expires_at > clock.unix_seconds
            && matches!(call.state.as_str(), "ringing" | "accepted")
        {
            self.private_calls.intent = Some(Intent {
                origin: origin.clone(),
                id: call.id.clone(),
                deadline: clock
                    .deadline(call.expires_at.saturating_add(30), Instant::now())
                    .unwrap_or_else(Instant::now),
            });
            self.private_calls.origin = Some(origin);
        }
        if let Some(bootstrap) = self.bootstrap.as_mut().map(Arc::make_mut) {
            bootstrap.social.calls.retain(|entry| entry.id != call.id);
            bootstrap.social.calls.push(call);
        }
    }

    pub(super) fn private_call_panel(&mut self, ctx: &egui::Context) {
        let origin = self.command_origin();
        self.private_calls.bind_origin(origin.clone());
        let Some(bootstrap) = self.bootstrap.clone() else {
            return;
        };
        if self.connection_status != ConnectionStatus::Connected {
            return;
        }
        let Some(clock) = origin
            .as_ref()
            .and_then(|origin| self.call_alerts.clock(origin))
        else {
            return;
        };
        let timestamp = clock.unix_seconds;
        if let Some(active) = &self.private_calls.active {
            let same_attempt = self.voice_join_attempt == Some(active.request)
                || (self.joining_voice.as_deref() == Some(active.channel.as_str())
                    && self.voice_request == active.request);
            let call = bootstrap
                .social
                .calls
                .iter()
                .find(|call| call.id == active.id);
            let revoked = !pair_channel_available(
                &active.channel,
                &bootstrap.user.id,
                &active.peer,
                &bootstrap,
            ) || !bootstrap.social.friends.contains(&active.peer)
                || bootstrap.social.blocked.contains(&active.peer)
                || call.is_some_and(|call| call.state == "ended" || !valid_for(call, &bootstrap));
            if same_attempt && revoked {
                self.private_calls.active = None;
                self.leave_voice();
                self.notice = Some(("Private call ended.".into(), Instant::now()));
            } else if !same_attempt && self.joining_voice.is_none() {
                self.private_calls.active = None;
            }
        }
        self.private_calls
            .dismissed
            .retain(|id| bootstrap.social.calls.iter().any(|call| &call.id == id));
        if let Some(intent) = &self.private_calls.intent {
            let call = bootstrap
                .social
                .calls
                .iter()
                .find(|call| call.id == intent.id && valid_for(call, &bootstrap));
            if origin.as_ref() != Some(&intent.origin)
                || Instant::now() >= intent.deadline
                || call.is_some_and(|call| {
                    call.expires_at <= timestamp
                        || !matches!(call.state.as_str(), "ringing" | "accepted")
                })
            {
                self.private_calls.intent = None;
            } else if let Some(call) = call.filter(|call| call.state == "accepted") {
                // Do not replace a call the user has joined in the meantime.
                let channel_available = channel_available(call, &bootstrap);
                if self.voice_channel.is_some() || self.joining_voice.is_some() {
                    self.private_calls.intent = None;
                } else if channel_available && !self.busy && !self.call_needs_open_consent() {
                    if let Some(origin) = origin.clone() {
                        self.join_private_call(call, origin);
                    }
                }
            }
        }
        let call = bootstrap
            .social
            .calls
            .iter()
            .filter(|call| valid_for(call, &bootstrap))
            .filter(|call| !self.private_calls.dismissed.contains(&call.id))
            .filter(|call| timestamp < call.expires_at.saturating_add(30))
            .filter(|call| {
                !(call.state == "accepted"
                    && (self.voice_channel.as_deref() == Some(call.channel_id.as_str())
                        || self.joining_voice.as_deref() == Some(call.channel_id.as_str())))
            })
            .max_by_key(|call| (call.state == "ringing", call.expires_at))
            .cloned();
        let Some(call) = call else {
            return;
        };
        let incoming = call.recipient_id == bootstrap.user.id;
        let peer_id = peer(&call, &bootstrap.user.id).unwrap_or_default();
        let name = bootstrap
            .members
            .iter()
            .find(|member| member.id == peer_id)
            .map(profile_ui::display_name)
            .unwrap_or("Friend");
        let ringing = call.state == "ringing"
            && call.expires_at > timestamp
            && origin
                .as_ref()
                .is_some_and(|origin| self.call_alerts.ringing(origin, &call.id));
        let text = match call.state.as_str() {
            "ringing" | "missed" if !incoming && !ringing => format!("No answer · {name}"),
            "ringing" if !ringing => format!("Missed call · {name}"),
            "ringing" if incoming => format!("Incoming call from {name}"),
            "ringing" => format!("Calling {name}…"),
            "accepted" => format!("Call accepted · {name}"),
            "declined" => format!("Call declined · {name}"),
            "cancelled" => format!("Call cancelled · {name}"),
            "ended" => format!("Call ended · {name}"),
            _ => format!("Missed call · {name}"),
        };
        let switching = self.call_switch_pending();
        let pending = switching || self.admin_requests.values()
            .any(|key| key.starts_with("private-call:"));
        egui::TopBottomPanel::top("private-call-invitation")
            .frame(
                egui::Frame::none()
                    .fill(c_card())
                    .inner_margin(Margin::symmetric(16.0, 12.0)),
            )
            .show(ctx, |ui| {
                ui.set_enabled(!self.modal_open());
                ui.horizontal_wrapped(|ui| {
                    let label = ui.label(RichText::new(text).strong());
                    if clock.server_sample_age.is_none() {
                        label.on_hover_text("This older server uses your PC's clock for call timing. Ask the host to update Relay.");
                    }
                    if ringing && incoming {
                        let idle = self.voice_channel.is_none() && self.joining_voice.is_none();
                        if !idle && ui.add_enabled(!pending, primary(
                            if switching { "Preparing call…" } else { "Leave and answer" }
                        )).clicked() {
                            if let Some(origin) = origin.clone() {
                                self.offer_leave_and_answer(origin, call.id.clone(), name.into());
                            }
                        }
                        if idle && ui.add_enabled(!pending, primary("Accept call"))
                            .on_disabled_hover_text("Another call action is still being prepared.")
                            .clicked()
                        {
                            self.answer_private_call(&call, "accept");
                        }
                        if ui.add_enabled(!pending, secondary("Decline")).clicked() {
                            self.answer_private_call(&call, "decline");
                        }
                    } else if ringing {
                        if ui.add_enabled(!pending, secondary("Cancel call")).clicked() {
                            self.answer_private_call(&call, "cancel");
                        }
                    } else if call.state == "accepted"
                        && self.private_calls.intent.is_some()
                        && self.call_needs_open_consent()
                    {
                        if ui.button("Review call protection").clicked() {
                            self.home_page = None;
                            self.select(Target::Channel(call.channel_id.clone()));
                        }
                    } else if call.state == "accepted"
                        && self.voice_channel.is_none()
                        && self.joining_voice.is_none()
                    {
                        if ui
                            .add_enabled(
                                channel_available(&call, &bootstrap),
                                secondary("Join call"),
                            )
                            .clicked()
                        {
                            if let Some(origin) = origin.clone() {
                                self.join_private_call(&call, origin);
                            }
                        }
                        if icon_button(ui, Icon::Close, false, "Dismiss call notice").clicked() {
                            self.private_calls.dismissed.insert(call.id.clone());
                        }
                    } else if icon_button(ui, Icon::Close, false, "Dismiss call notice").clicked() {
                        self.private_calls.dismissed.insert(call.id.clone());
                    }
                });
            });
        ctx.request_repaint_after(Duration::from_secs(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn background_answer_reply_survives_first_panel_but_not_a_replaced_account() {
        let origin = CommandOrigin { url: "https://account.invalid/people".into(), server_id: "service".into(), user_id: "me".into() };
        let mut state = State::default();
        state.bind_origin(Some(origin.clone()));
        state.pending = Some(("private-call:accept:owned-call".into(), origin.clone()));
        state.bind_origin(Some(origin.clone()));
        assert_eq!(state.pending.as_ref().map(|p| p.0.as_str()), Some("private-call:accept:owned-call"));
        state.bind_origin(Some(CommandOrigin { user_id: "another-account".into(), ..origin }));
        assert!(state.pending.is_none());
    }
    #[test]
    fn a_private_call_requires_current_friendship_exact_pair_and_live_join_permission() {
        let call = PrivateCall {
            id: uuid::Uuid::new_v4().to_string(),
            channel_id: "voice".into(),
            caller_id: "alice".into(),
            recipient_id: "bob".into(),
            state: "accepted".into(),
            expires_at: unix_now() + 30,
        };
        let mut b = Bootstrap {
            user: User {
                id: "bob".into(),
                ..Default::default()
            },
            members: vec![User {
                id: "alice".into(),
                ..Default::default()
            }],
            social: SocialState {
                friends: vec!["alice".into()],
                ..Default::default()
            },
            channels: vec![Channel {
                id: "voice".into(),
                kind: "voice".into(),
                can_join: true,
                ..Default::default()
            }],
            conversations: vec![
                ConversationInfo {
                    channel_id: "voice".into(),
                    kind: "group_voice".into(),
                    parent_id: Some("group".into()),
                    ..Default::default()
                },
                ConversationInfo {
                    channel_id: "group".into(),
                    kind: "group".into(),
                    member_ids: vec!["alice".into(), "bob".into()],
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        assert!(valid_for(&call, &b));
        assert!(channel_available(&call, &b));
        b.conversations[1].member_ids.push("outsider".into());
        assert!(!channel_available(&call, &b));
        b.conversations[1].member_ids.pop();
        b.channels[0].can_join = false;
        assert!(!channel_available(&call, &b));
        b.channels[0].can_join = true;
        b.social.blocked.push("alice".into());
        assert!(!valid_for(&call, &b));
        b.social.blocked.clear();
        b.social.friends.clear();
        assert!(!valid_for(&call, &b));
    }
}
