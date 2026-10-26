use super::*;

pub(super) fn message_toolbar_area(message_id: &str, position: Pos2) -> egui::Area {
    // Built-in menus use Foreground. The toolbar must stay below its menu so
    // the menu can receive clicks outside the original message/toolbar bounds.
    egui::Area::new(egui::Id::new(("message-toolbar", message_id)))
        .order(egui::Order::Middle)
        .fixed_pos(position)
}

pub(super) struct ReactionSnapshot<'a> {
    pub message_id: &'a str,
    pub target_client_id: Option<&'a str>,
    pub rows: &'a [relay_protocol::Reaction],
}

pub(super) fn apply_reaction_snapshot(
    message: &mut Message,
    target: &Target,
    own_id: &str,
    snapshot: &ReactionSnapshot<'_>,
    protection: Option<&crate::e2ee::MessageProtection>,
) -> bool {
    if message.id != snapshot.message_id {
        return false;
    }
    let in_target = match target {
        Target::Channel(id) => {
            message.channel_id.as_ref() == Some(id) && message.recipient_id.is_none()
        }
        Target::Direct(peer) => {
            message.channel_id.is_none()
                && ((message.author.id == own_id && message.recipient_id.as_ref() == Some(peer))
                    || (message.author.id == *peer
                        && message.recipient_id.as_deref() == Some(own_id)))
        }
    };
    if !in_target {
        return false;
    }
    match snapshot.target_client_id {
        Some(id)
            if message.client_id.as_deref() == Some(id)
                && !matches!(
                    protection,
                    Some(crate::e2ee::MessageProtection::Unavailable(_))
                ) => {}
        None if matches!(protection, Some(crate::e2ee::MessageProtection::Legacy)) => {}
        _ => return false,
    }
    if message.reactions.len() == snapshot.rows.len()
        && message
            .reactions
            .iter()
            .zip(snapshot.rows)
            .all(|(a, b)| a.emoji == b.emoji && a.user_ids == b.user_ids)
    {
        return false;
    }
    message.reactions = snapshot.rows.to_vec();
    true
}

fn reply_mentions(
    mut explicit: Vec<String>,
    reply: Option<&ReplyDraft>,
    members: &[User],
    own_id: &str,
) -> Vec<String> {
    if let Some(author_id) = reply
        .filter(|reply| reply.notify_author)
        .and_then(|reply| reply.author_id.as_ref())
    {
        if author_id != own_id
            && members.iter().any(|user| user.id == *author_id)
            && !explicit.contains(author_id)
        {
            explicit.push(author_id.clone());
        }
    }
    explicit
}

// A retry must preserve the exact request, including reply notifications and
// mention IDs captured before a member was renamed. Re-parsing the composer can
// change those fields and turn an uncertain acknowledgement into a duplicate.
pub(super) fn outgoing_retry(
    target: &Target,
    message: &Message,
    origin: CommandOrigin,
) -> Option<(SendAttempt, Command)> {
    let client_id = message.client_id.as_ref()?;
    let matches_target = match target {
        Target::Channel(id) => {
            message.channel_id.as_ref() == Some(id) && message.recipient_id.is_none()
        }
        Target::Direct(id) => {
            message.recipient_id.as_ref() == Some(id) && message.channel_id.is_none()
        }
    };
    if client_id.is_empty()
        || message.id != format!("local:{client_id}")
        || message.author.id != origin.user_id
        || !matches_target
    {
        return None;
    }
    let attempt = SendAttempt {
        client_id: client_id.clone(),
        content: message.content.clone(),
        attachments: message
            .attachments
            .iter()
            .map(|file| file.id.clone())
            .collect(),
        reply_to_id: message.reply_to_id.clone(),
        mentions: message.mentions.clone(),
    };
    let command = Command::SendRich {
        binding: None,
        origin,
        target: target.clone(),
        content: attempt.content.clone(),
        attachment_ids: attempt.attachments.clone(),
        client_id: attempt.client_id.clone(),
        reply_to_id: attempt.reply_to_id.clone(),
        mentions: attempt.mentions.clone(),
    };
    Some((attempt, command))
}

pub(super) fn dismiss_inline_on_escape(
    closing_overlay: bool,
    editing: &mut Option<(String, String)>,
    reply: &mut Option<ReplyDraft>,
) {
    if !closing_overlay {
        // Cancelling an edit must not discard the reply being composed below it.
        // A second Escape can dismiss that reply after the editor has closed.
        if editing.take().is_none() {
            *reply = None;
        }
    }
}

#[derive(Default)]
pub(super) struct PinsView {
    pub open: bool,
    pub items: Vec<PinnedMessage>,
    pub request: Option<(u64, String)>,
    pub action: Option<(u64, String)>,
}

impl PinsView {
    /// The caller has accepted this change for the visible conversation. Keep
    /// existing pin metadata while updating the message snapshot from that feed.
    pub(super) fn apply_change(&mut self, change: &MessageChange) {
        self.items.retain_mut(|pin| {
            if pin.message.id != change.message_id
                || pin.message.channel_id != change.channel_id
                || change.recipient_id.is_some()
            {
                return true;
            }
            if change.deleted {
                return false;
            }
            if let Some(message) = &change.message {
                if message.id == change.message_id
                    && message.channel_id == pin.message.channel_id
                    && message.recipient_id.is_none()
                {
                    if message.pinned_at.is_none() {
                        return false;
                    }
                    pin.message = message.clone();
                }
            }
            true
        });
    }
}

impl RelayApp {
    pub(super) fn history_search_panel(&mut self, ui: &mut egui::Ui) {
        egui::TopBottomPanel::top("history-search-panel")
            .frame(
                egui::Frame::none()
                    .fill(c_panel())
                    .inner_margin(Margin::symmetric(16.0, 6.0)),
            )
            .show_inside(ui, |ui| {
                ui.horizontal(|ui| {
                    let response = ui.add_sized(
                        [(ui.available_width() - 76.0).max(1.0), 30.0],
                        egui::TextEdit::singleline(&mut self.search)
                            .id(egui::Id::new("history-search"))
                            .hint_text("Search this conversation")
                            .margin(Vec2::new(7.0, 5.0)),
                    );
                    if response.changed() {
                        self.search_changed_at = Some(Instant::now());
                        if self.search.trim().is_empty() {
                            self.search_history(false);
                        }
                        ui.ctx().request_repaint_after(Duration::from_millis(350));
                    }
                    if icon_button(ui, Icon::Filter, false, "Search filters and more history")
                        .clicked()
                    {
                        self.open_filtered_history_search();
                    }
                    if icon_button(ui, Icon::Close, false, "Close search").clicked() {
                        self.history_search_open = false;
                        self.search.clear();
                        self.search_history(false);
                        ui.memory_mut(|memory| {
                            memory.request_focus(egui::Id::new((
                                "composer-editor",
                                self.current.clone(),
                            )))
                        });
                    }
                });
            });
    }

    pub(super) fn has_chat_capability(&self, capability: &str) -> bool {
        self.bootstrap
            .as_ref()
            .is_some_and(|b| b.capabilities.iter().any(|c| c == capability))
    }

    pub(super) fn request_pins(&mut self) {
        let Some(Target::Channel(channel_id)) = self.current.clone() else {
            return;
        };
        if !self.has_chat_capability("pins-v1") {
            return;
        }
        let request_id = self.next_request();
        self.pins.request = Some((request_id, channel_id.clone()));
        if let Err(error) = self.network.send(Command::FetchPins {
            channel_id,
            request_id,
        }) {
            self.pins.request = None;
            self.error = Some(error);
        }
    }

    pub(super) fn can_pin(&self, message: &Message) -> bool {
        message.channel_id.as_ref().is_some_and(|id| {
            self.bootstrap
                .as_ref()
                .is_some_and(|b| b.channels.iter().any(|c| c.id == *id && c.can_manage_pins))
        }) && self.has_chat_capability("pins-v1")
    }

    pub(super) fn set_message_pin(&mut self, message: &Message, pinned: bool) {
        let Some(channel_id) = message.channel_id.clone() else {
            return;
        };
        if !self.can_pin(message) || self.pins.action.is_some() {
            return;
        }
        let request_id = self.next_request();
        self.pins.action = Some((request_id, message.id.clone()));
        if let Err(error) = self.network.send(Command::SetPin {
            channel_id,
            message_id: message.id.clone(),
            pinned,
            request_id,
        }) {
            self.pins.action = None;
            self.error = Some(error);
        }
    }

    pub(super) fn pins_dialog(&mut self, ctx: &egui::Context) {
        if !self.pins.open {
            return;
        }
        let mut open = true;
        let window = desktop_ui::dialog(ctx, "Pinned Messages")
            .open(&mut open)
            .collapsible(false);
        // The shared header (title, Close, Escape) stays above the scrolling body.
        if host_setup_ui::show_with_header(ctx, window, "Pinned Messages", |ui| {
            ui.label(RichText::new("Keep the important messages easy to find.").color(c_muted()));
            ui.horizontal_wrapped(|ui| {
                if ui
                    .add_enabled(self.pins.request.is_none(), secondary("Refresh"))
                    .clicked()
                {
                    self.request_pins();
                }
                if self.pins.request.is_some() || self.pins.action.is_some() {
                    ui.spinner();
                }
            });
            if let Some(error) = self.error.clone() {
                ui.add(egui::Label::new(RichText::new(error).color(c_bad())).wrap(true));
                if ui.small_button("Dismiss error").clicked() {
                    self.error = None;
                }
            }
            ui.separator();
            if self.pins.items.is_empty() {
                ui.label(if self.pins.request.is_some() {
                    "Loading pinned messages…"
                } else {
                    "No pinned messages yet."
                });
                ui.label(
                    RichText::new("Moderators can pin a message from its menu.").color(c_muted()),
                );
            }
            for pin in self.pins.items.clone() {
                egui::Frame::none()
                    .fill(c_card())
                    .rounding(6.0)
                    .inner_margin(Margin::same(12.0))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(
                            RichText::new(profile_ui::display_name(&pin.message.author)).strong(),
                        );
                        self.message_protection_label(ui, &pin.message.id);
                        self.message_preview(ui, &pin.message);
                        for file in &pin.message.attachments {
                            ui.label(RichText::new(&file.name).color(c_muted()));
                        }
                        ui.label(
                            RichText::new(format!(
                                "Pinned by {} · {} {}",
                                profile_ui::display_name(&pin.pinned_by),
                                day_label(&pin.pinned_at),
                                display_time(&pin.pinned_at)
                            ))
                            .size(font_size(TEXT_CAPTION))
                            .color(c_muted()),
                        );
                        ui.horizontal_wrapped(|ui| {
                            if ui.add(secondary("Jump to Message")).clicked() {
                                self.reveal_message(pin.message.id.clone());
                                self.pins.open = false;
                            }
                            if self.can_pin(&pin.message)
                                && ui
                                    .add_enabled(self.pins.action.is_none(), secondary("Unpin"))
                                    .clicked()
                            {
                                self.set_message_pin(&pin.message, false);
                            }
                        });
                    });
                ui.add_space(8.0);
            }
        }) {
            open = false;
        }
        self.pins.open &= open;
    }

    pub(super) fn stop_typing(&mut self) {
        if let Some((target, _)) = self.typing_sent.take() {
            self.send(Command::Typing {
                target,
                typing: false,
            });
        }
        self.typing_activity = None;
    }

    pub(super) fn typing_heartbeat(&mut self, ctx: &egui::Context) {
        let now = Instant::now();
        let active = self.session.is_some()
            && !self.settings_open
            && self.connection_status == ConnectionStatus::Connected
            && ctx.input(|i| i.focused)
            && ctx
                .memory(|m| m.has_focus(egui::Id::new(("composer-editor", self.current.clone()))))
            && self.has_chat_capability("typing-v1")
            && !self.draft.trim().is_empty()
            && self.typing_activity.as_ref().is_some_and(|(target, at)| {
                Some(target) == self.current.as_ref()
                    && now.duration_since(*at) < Duration::from_secs(5)
            });
        if !active {
            self.stop_typing();
            return;
        }
        let Some(target) = self.current.clone() else {
            return;
        };
        if self.typing_sent.as_ref().is_none_or(|(sent, at)| {
            sent != &target || now.duration_since(*at) >= Duration::from_secs(3)
        }) {
            self.send(Command::Typing {
                target: target.clone(),
                typing: true,
            });
            self.typing_sent = Some((target, now));
        }
        ctx.request_repaint_after(Duration::from_secs(1));
    }

    pub(super) fn slowmode_for(&self, target: &Target) -> u32 {
        match target {
            Target::Channel(id) => self
                .bootstrap
                .as_ref()
                .and_then(|b| b.channels.iter().find(|c| c.id == *id))
                .filter(|channel| !channel.slowmode_bypass)
                .map(|channel| channel.slowmode_seconds)
                .unwrap_or(0),
            Target::Direct(_) => 0,
        }
    }

    pub(super) fn composer_activity(&mut self, ui: &mut egui::Ui) {
        let now = Instant::now();
        if let Some(target) = &self.current {
            if let Some(wait) = self.cooldowns.remaining(target, now) {
                let text = if wait.slowmode {
                    format!(
                        "Slowmode · You can send again in {}s. Your draft is safe.",
                        wait.seconds
                    )
                } else {
                    format!(
                        "Slow down · Try again in {}s. Your draft is safe.",
                        wait.seconds
                    )
                };
                ui.add(
                    egui::Label::new(RichText::new(text).color(c_accent()).size(font_size(12.0)))
                        .wrap(true),
                );
                ui.ctx().request_repaint_after(Duration::from_millis(250));
            } else {
                let slowmode = self.slowmode_for(target);
                if slowmode > 0 {
                    ui.add(
                        egui::Label::new(
                            RichText::new(format!(
                                "Slowmode is on · One message every {slowmode}s."
                            ))
                            .color(c_muted())
                            .size(font_size(TEXT_CAPTION)),
                        )
                        .wrap(true),
                    );
                }
            }
        }
        if let Some(text) = self.typing_users.text(now) {
            ui.add(
                egui::Label::new(RichText::new(text).color(c_muted()).size(font_size(12.0)))
                    .wrap(true),
            );
            ui.ctx().request_repaint_after(Duration::from_secs(1));
        }
    }

    pub(super) fn fail_outgoing(&mut self, target: &Target) {
        if let Some(attempt) = self.send_ids.get(target) {
            for (conversation, message, failed, _) in &mut self.outgoing_messages {
                if conversation == target
                    && message.client_id.as_deref() == Some(&attempt.client_id)
                {
                    *failed = true;
                }
            }
        }
    }

    pub(super) fn can_send_message(&self, target: &Target) -> bool {
        self.bootstrap.as_ref().is_some_and(|b| match target {
            Target::Direct(id) => b
                .members
                .iter()
                .any(|member| member.id == *id && crate::direct_targets::actionable(b, id)),
            Target::Channel(id) => b
                .channels
                .iter()
                .any(|channel| channel.id == *id && channel.kind == "text" && channel.can_send),
        }) && !self.pending_sends.contains(target)
            && self.cooldowns.remaining(target, Instant::now()).is_none()
    }

    pub(super) fn retry_outgoing(&mut self, message_id: &str) {
        let Some(target) = self
            .current
            .clone()
            .filter(|target| self.can_send_message(target))
        else {
            return;
        };
        let Some((_, message, _, _)) =
            self.outgoing_messages
                .iter()
                .find(|(saved, message, failed, confirmed)| {
                    saved == &target && message.id == message_id && *failed && !*confirmed
                })
        else {
            return;
        };
        // Persist the original request without touching a newer composer draft.
        self.stage_outgoing(target, message.clone(), false);
    }

    pub(super) fn submit_composer(&mut self) {
        self.submit_message_content(self.draft.clone(), self.attachments.clone(), true);
    }

    /// GIF actions pass their own payload without temporarily replacing the
    /// editor. The persistence barrier must see the user's real newer draft.
    pub(super) fn submit_message_content(
        &mut self,
        content: String,
        attachments: Vec<Attachment>,
        clear_composer: bool,
    ) -> bool {
        let Some(target) = self.current.clone() else {
            return false;
        };
        if !self.can_send_message(&target)
            || (content.trim().is_empty() && attachments.is_empty())
        {
            return false;
        }
        let content = self.expand_message_shortcodes(content.trim());
        let reply_to_id = self.reply.as_ref().map(|reply| reply.message_id.clone());
        let mentions = self
            .bootstrap
            .as_ref()
            .map(|b| {
                reply_mentions(
                    mentioned_users(&content, &b.members),
                    self.reply.as_ref(),
                    &b.members,
                    &b.user.id,
                )
            })
            .unwrap_or_default();
        let client_id = crate::network::operation_id();
        if self.command_origin().is_none() {
            return false;
        }
        let message = Message {
            id: format!("local:{client_id}"),
            client_id: Some(client_id),
            channel_id: match &target {
                Target::Channel(id) => Some(id.clone()),
                _ => None,
            },
            recipient_id: match &target {
                Target::Direct(id) => Some(id.clone()),
                _ => None,
            },
            author: self.bootstrap.as_ref().unwrap().user.clone(),
            content,
            attachments,
            created_at: chrono::Utc::now().to_rfc3339(),
            reply_to_id,
            mentions,
            ..Default::default()
        };
        self.stage_outgoing(target.clone(), message, clear_composer);
        self.pending_sends.contains(&target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outgoing_retry_preserves_reply_notification_and_original_request_identity() {
        let origin = CommandOrigin {
            url: "https://relay.example".into(),
            server_id: "server".into(),
            user_id: "self".into(),
        };
        let message = Message {
            id: "local:uncertain-send".into(),
            client_id: Some("uncertain-send".into()),
            channel_id: Some("general".into()),
            author: User {
                id: "self".into(),
                ..Default::default()
            },
            content: "Yes, @old-name :wave:".into(),
            reply_to_id: Some("original-message".into()),
            mentions: vec!["renamed-member".into(), "reply-author".into()],
            attachments: vec![Attachment {
                id: "uploaded-file".into(),
                name: "image.png".into(),
                mime: "image/png".into(),
                size: 12,
                url: "opaque-file".into(),
            }],
            ..Default::default()
        };
        // The old composer round-trip could no longer find @old-name and lost
        // the reply's notification setting. Neither may change the saved send.
        let renamed = vec![User {
            id: "renamed-member".into(),
            username: "new-name".into(),
            ..Default::default()
        }];
        assert!(mentioned_users(&message.content, &renamed).is_empty());
        for target in [
            Target::Channel("general".into()),
            Target::Direct("friend".into()),
        ] {
            let mut message = message.clone();
            if let Target::Direct(id) = &target {
                message.channel_id = None;
                message.recipient_id = Some(id.clone());
            }
            for _ in 0..2 {
                let (attempt, command) = outgoing_retry(&target, &message, origin.clone()).unwrap();
                assert_eq!(attempt.client_id, "uncertain-send");
                let Command::SendRich {
                    origin: actual_origin,
                    target: actual_target,
                    content,
                    attachment_ids,
                    client_id,
                    reply_to_id,
                    mentions,
                    ..
                } = command
                else {
                    panic!("Retry must use the idempotent rich-message request");
                };
                assert_eq!(actual_origin, origin);
                assert_eq!(actual_target, target);
                assert_eq!(client_id, "uncertain-send");
                assert_eq!(content, "Yes, @old-name :wave:");
                assert_eq!(attachment_ids, ["uploaded-file"]);
                assert_eq!(reply_to_id.as_deref(), Some("original-message"));
                assert_eq!(mentions, ["renamed-member", "reply-author"]);
            }
        }
    }

    #[test]
    fn outgoing_retry_rejects_messages_from_other_accounts_or_conversations() {
        let origin = CommandOrigin {
            url: "https://relay.example".into(),
            server_id: "server".into(),
            user_id: "self".into(),
        };
        let target = Target::Channel("general".into());
        let message = Message {
            id: "local:send".into(),
            client_id: Some("send".into()),
            channel_id: Some("general".into()),
            author: User {
                id: "self".into(),
                ..Default::default()
            },
            content: "Message".into(),
            ..Default::default()
        };
        assert!(outgoing_retry(&target, &message, origin.clone()).is_some());
        assert!(
            outgoing_retry(&Target::Direct("general".into()), &message, origin.clone()).is_none()
        );
        assert!(
            outgoing_retry(&Target::Channel("other".into()), &message, origin.clone()).is_none()
        );
        for invalid in 0..5 {
            let mut message = message.clone();
            match invalid {
                0 => message.author.id = "another-account".into(),
                1 => message.client_id = None,
                2 => message.client_id = Some(String::new()),
                3 => message.id = "server-confirmed-message".into(),
                _ => message.recipient_id = Some("ambiguous-target".into()),
            }
            assert!(outgoing_retry(&target, &message, origin.clone()).is_none());
        }
    }

    #[test]
    fn message_menu_lower_action_stays_above_other_message_toolbars() {
        struct Fixture {
            time: f64,
            more: Rect,
            lower: Rect,
            underlying: Rect,
            chosen: usize,
            underlying_clicks: usize,
        }
        impl Fixture {
            fn frame(&mut self, ctx: &egui::Context, events: Vec<egui::Event>) {
                let input = egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(640.0, 480.0))),
                    focused: true,
                    time: Some(self.time),
                    events,
                    ..Default::default()
                };
                self.time += 0.05;
                let _ = ctx.run(input, |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        ui.label("First message");
                    });
                    message_toolbar_area("first", Pos2::new(300.0, 30.0)).show(ctx, |ui| {
                        self.more = ui
                            .menu_button("More", |ui| {
                                for label in ["Reply", "Like", "Copy text", "Edit message"] {
                                    let _ = ui.button(label);
                                }
                                let lower = ui.button("Delete message");
                                self.lower = lower.rect;
                                if lower.clicked() {
                                    self.chosen += 1;
                                    ui.close_menu();
                                }
                            })
                            .response
                            .rect;
                    });
                    // A later message's hover toolbar can overlap an open menu.
                    // It must neither paint above it nor intercept its pointer.
                    let position = if self.lower.is_positive() {
                        self.lower.left_top()
                    } else {
                        Pos2::new(300.0, 200.0)
                    };
                    message_toolbar_area("later", position).show(ctx, |ui| {
                        let response = ui.button("Another message toolbar");
                        self.underlying = response.rect;
                        self.underlying_clicks += usize::from(response.clicked());
                    });
                });
            }

            fn click(&mut self, ctx: &egui::Context, position: Pos2) {
                self.frame(ctx, vec![egui::Event::PointerMoved(position)]);
                for pressed in [true, false] {
                    self.frame(
                        ctx,
                        vec![egui::Event::PointerButton {
                            pos: position,
                            button: egui::PointerButton::Primary,
                            pressed,
                            modifiers: egui::Modifiers::NONE,
                        }],
                    );
                }
            }
        }
        let ctx = egui::Context::default();
        let mut fixture = Fixture {
            time: 0.0,
            more: Rect::NOTHING,
            lower: Rect::NOTHING,
            underlying: Rect::NOTHING,
            chosen: 0,
            underlying_clicks: 0,
        };
        for _ in 0..3 {
            fixture.frame(&ctx, vec![]);
        }
        fixture.click(&ctx, fixture.more.center());
        for _ in 0..3 {
            fixture.frame(&ctx, vec![]);
        }
        assert!(fixture.lower.is_positive(), "The message menu must open");
        assert!(
            fixture.lower.top() > 60.0,
            "The action is beyond the original message"
        );
        let position = fixture.lower.center();
        assert!(fixture.underlying.contains(position));
        fixture.click(&ctx, position);
        assert_eq!(
            fixture.chosen, 1,
            "The lower menu action must receive the click"
        );
        assert_eq!(
            fixture.underlying_clicks, 0,
            "The menu blocks the underlying toolbar"
        );
    }

    #[test]
    fn authenticated_reaction_snapshots_update_older_and_pinned_rows_without_changing_message_content(
    ) {
        let mut message = Message {
            id: "old-message".into(),
            channel_id: Some("room".into()),
            client_id: Some("signed-original-id".into()),
            content: "old private text".into(),
            sequence: 2,
            pinned_at: Some("pin-time".into()),
            ..Default::default()
        };
        let rows = vec![relay_protocol::Reaction {
            emoji: "<3".into(),
            user_ids: vec!["viewer".into()],
        }];
        let snapshot = ReactionSnapshot {
            message_id: "old-message",
            target_client_id: Some("signed-original-id"),
            rows: &rows,
        };
        let target = Target::Channel("room".into());
        assert!(apply_reaction_snapshot(
            &mut message,
            &target,
            "own-user",
            &snapshot,
            Some(&crate::e2ee::MessageProtection::Protected)
        ));
        assert_eq!(message.reactions[0].user_ids, vec!["viewer"]);
        assert_eq!(message.content, "old private text");
        assert_eq!(message.sequence, 2);
        assert_eq!(message.pinned_at.as_deref(), Some("pin-time"));
        assert!(
            !apply_reaction_snapshot(&mut message, &target, "own-user", &snapshot, None),
            "an identical refresh should not invalidate row layout"
        );
        let wrong = ReactionSnapshot {
            target_client_id: Some("different-signed-original"),
            ..snapshot
        };
        assert!(!apply_reaction_snapshot(
            &mut message,
            &target,
            "own-user",
            &wrong,
            None
        ));
        assert!(!apply_reaction_snapshot(
            &mut message,
            &Target::Channel("other".into()),
            "own-user",
            &snapshot,
            None
        ));
    }

    #[test]
    fn legacy_and_direct_reaction_snapshots_cannot_be_applied_to_wrong_or_unavailable_messages() {
        let rows = vec![relay_protocol::Reaction {
            emoji: "+1".into(),
            user_ids: vec!["viewer".into()],
        }];
        let snapshot = ReactionSnapshot {
            message_id: "direct",
            target_client_id: None,
            rows: &rows,
        };
        let mut message = Message {
            id: "direct".into(),
            author: User {
                id: "peer".into(),
                ..Default::default()
            },
            recipient_id: Some("own-user".into()),
            ..Default::default()
        };
        let target = Target::Direct("peer".into());
        assert!(!apply_reaction_snapshot(
            &mut message,
            &target,
            "own-user",
            &snapshot,
            None
        ));
        assert!(!apply_reaction_snapshot(
            &mut message,
            &target,
            "own-user",
            &snapshot,
            Some(&crate::e2ee::MessageProtection::Protected)
        ));
        assert!(apply_reaction_snapshot(
            &mut message,
            &target,
            "own-user",
            &snapshot,
            Some(&crate::e2ee::MessageProtection::Legacy)
        ));
        message.reactions.clear();
        message.recipient_id = Some("other-account".into());
        assert!(!apply_reaction_snapshot(
            &mut message,
            &target,
            "own-user",
            &snapshot,
            Some(&crate::e2ee::MessageProtection::Legacy)
        ));
        message.recipient_id = Some("own-user".into());
        message.client_id = Some("protected-original".into());
        let protected = ReactionSnapshot {
            target_client_id: Some("protected-original"),
            ..snapshot
        };
        assert!(!apply_reaction_snapshot(
            &mut message,
            &target,
            "own-user",
            &protected,
            Some(&crate::e2ee::MessageProtection::Unavailable(
                "Missing key".into()
            ))
        ));
    }

    #[test]
    fn reply_notification_is_opt_in_and_never_removes_explicit_mentions() {
        let members = vec![User {
            id: "original-author".into(),
            ..Default::default()
        }];
        let mut reply = ReplyDraft {
            author_id: Some("original-author".into()),
            ..Default::default()
        };
        assert!(reply_mentions(vec![], Some(&reply), &members, "me").is_empty());
        let explicit = vec!["original-author".into()];
        assert_eq!(
            reply_mentions(explicit.clone(), Some(&reply), &members, "me"),
            explicit
        );
        reply.notify_author = true;
        assert_eq!(
            reply_mentions(vec![], Some(&reply), &members, "me"),
            explicit
        );
        assert_eq!(
            reply_mentions(explicit.clone(), Some(&reply), &members, "me"),
            explicit
        );
        assert!(reply_mentions(vec![], Some(&reply), &members, "original-author").is_empty());
        assert!(reply_mentions(vec![], Some(&reply), &[], "me").is_empty());
    }

    #[test]
    fn old_saved_replies_do_not_create_implicit_notifications_after_upgrade() {
        let reply: ReplyDraft =
            serde_json::from_str(r#"{"message_id":"old","author":"Mira","excerpt":"Original"}"#)
                .unwrap();
        assert!(!reply.notify_author);
        assert!(reply.author_id.is_none());
        let restored: ReplyDraft =
            serde_json::from_str(&serde_json::to_string(&reply).unwrap()).unwrap();
        assert_eq!(restored, reply);
    }

    #[test]
    fn escaping_a_pin_list_or_other_dialog_keeps_reply_and_edit_context() {
        let mut editing = Some(("edited-message".into(), "unsaved edit".into()));
        let mut reply = Some(ReplyDraft {
            message_id: "replied-message".into(),
            author: "Mira".into(),
            excerpt: "reply context".into(),
            ..Default::default()
        });
        dismiss_inline_on_escape(true, &mut editing, &mut reply);
        assert_eq!(editing.as_ref().unwrap().1, "unsaved edit");
        assert_eq!(reply.as_ref().unwrap().message_id, "replied-message");
        assert_eq!(reply.as_ref().unwrap().excerpt, "reply context");
    }

    #[test]
    fn escaping_an_edit_preserves_the_reply_until_a_separate_escape() {
        let mut editing = Some(("edited-message".into(), "unsaved edit".into()));
        let mut reply = Some(ReplyDraft {
            message_id: "replied-message".into(),
            author: "Mira".into(),
            excerpt: "reply context".into(),
            ..Default::default()
        });
        dismiss_inline_on_escape(false, &mut editing, &mut reply);
        assert!(editing.is_none());
        assert_eq!(reply.as_ref().unwrap().message_id, "replied-message");
        assert_eq!(reply.as_ref().unwrap().excerpt, "reply context");
        dismiss_inline_on_escape(false, &mut editing, &mut reply);
        assert!(reply.is_none());
    }

    fn pinned() -> PinnedMessage {
        PinnedMessage {
            message: Message {
                id: "pinned-message".into(),
                channel_id: Some("visible-room".into()),
                content: "original text".into(),
                pinned_at: Some("2026-10-04T01:00:00Z".into()),
                ..Default::default()
            },
            pinned_by: User {
                id: "moderator".into(),
                username: "Mira".into(),
                role: "moderator".into(),
                ..Default::default()
            },
            pinned_at: "2026-10-04T01:00:00Z".into(),
        }
    }

    fn change(message: Message, deleted: bool) -> MessageChange {
        MessageChange {
            message_id: message.id.clone(),
            channel_id: message.channel_id.clone(),
            deleted,
            kind: if deleted { "deleted" } else { "updated" }.into(),
            message: (!deleted).then_some(message),
            ..Default::default()
        }
    }

    #[test]
    fn pinned_message_edits_update_text_attachments_and_reactions_preserving_pin_metadata() {
        let pin = pinned();
        let mut message = pin.message.clone();
        message.content = "edited text".into();
        message.edited_at = Some("2026-10-04T01:01:00Z".into());
        message.attachments.push(Attachment {
            id: "attachment".into(),
            name: "notes.txt".into(),
            mime: "text/plain".into(),
            size: 8,
            url: "/api/uploads/attachment".into(),
        });
        message.reactions.push(Reaction {
            emoji: "+1".into(),
            user_ids: vec!["viewer".into()],
        });
        let mut view = PinsView {
            items: vec![pin],
            ..Default::default()
        };
        view.apply_change(&change(message, false));
        assert_eq!(view.items.len(), 1);
        let updated = &view.items[0];
        assert_eq!(updated.message.content, "edited text");
        assert!(updated.message.edited_at.is_some());
        assert_eq!(updated.message.attachments[0].id, "attachment");
        assert_eq!(updated.message.reactions[0].user_ids, ["viewer"]);
        assert_eq!(updated.pinned_by.id, "moderator");
        assert_eq!(updated.pinned_by.username, "Mira");
        assert_eq!(updated.pinned_at, "2026-10-04T01:00:00Z");
    }

    #[test]
    fn deleted_and_unpinned_messages_leave_the_pin_list_without_a_manual_refresh() {
        for deleted in [true, false] {
            let pin = pinned();
            let mut message = pin.message.clone();
            if !deleted {
                message.pinned_at = None;
            }
            let update = change(message, deleted);
            let mut view = PinsView {
                items: vec![pin],
                ..Default::default()
            };
            view.apply_change(&update);
            assert!(view.items.is_empty());
        }
    }

    #[test]
    fn unrelated_ids_or_conversation_payloads_cannot_replace_or_delete_a_visible_pin() {
        let original = pinned();
        let mut other_id = original.message.clone();
        other_id.id = "other-message".into();
        let mut other_room = original.message.clone();
        other_room.channel_id = Some("private-room".into());
        let mut direct = original.message.clone();
        direct.recipient_id = Some("private-peer".into());
        let mut mismatched_body = change(original.message.clone(), false);
        mismatched_body.message.as_mut().unwrap().channel_id = Some("private-room".into());
        for update in [
            change(other_id, true),
            change(other_room, true),
            change(direct, false),
            mismatched_body,
        ] {
            let mut view = PinsView {
                items: vec![original.clone()],
                ..Default::default()
            };
            view.apply_change(&update);
            assert_eq!(view.items.len(), 1);
            assert_eq!(view.items[0].message.content, "original text");
            assert_eq!(
                view.items[0].message.channel_id.as_deref(),
                Some("visible-room")
            );
            assert_eq!(view.items[0].pinned_by.id, "moderator");
        }
    }
}
