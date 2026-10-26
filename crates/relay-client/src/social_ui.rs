// Included in admin_ui so all requests use the same account and community binding.
fn friend_matches_view(b: &Bootstrap, member: &User, tab: u8, connected: bool) -> bool {
    if member.id == b.user.id {
        return false;
    }
    // A cached friend/presence entry must not override a newer block or make
    // disconnected people look online.
    if b.social.blocked.contains(&member.id) {
        return tab == 3;
    }
    match tab {
        0 => {
            connected
                && b.social.friends.contains(&member.id)
                && b.presence.iter().any(|p| {
                    p.user_id == member.id && matches!(p.status.as_str(), "online" | "away" | "dnd")
                })
        }
        1 => b.social.friends.contains(&member.id),
        2 => b.social.incoming.contains(&member.id) || b.social.outgoing.contains(&member.id),
        3 => false,
        _ => !b.social.friends.contains(&member.id),
    }
}

fn friend_search_query(value: &str) -> String {
    value.trim().trim_start_matches('@').to_lowercase()
}

/// Reserve an action column on wide lists; on narrow lists each part gets a
/// whole row, letting horizontal_wrapped move the actions below the identity.
fn friend_row_widths(ui: &egui::Ui) -> (f32, f32) {
    let width = ui.available_width().max(1.0);
    let scale = font_size(14.0) / 14.0;
    let actions = (190.0 * scale).min(width);
    let identity = width - actions - ui.spacing().item_spacing.x;
    if identity < 200.0 * scale {
        (width, width)
    } else {
        (identity, actions)
    }
}

impl ServerUi {
    fn friend_input(&mut self, adding_relay_friend: bool) -> &mut String {
        if adding_relay_friend {
            &mut self.friend_request_name
        } else {
            &mut self.friend_search
        }
    }
}

fn inbox_conversation_label(bootstrap: &Bootstrap, target: &Target) -> Option<String> {
    match target {
        Target::Channel(id) => {
            let channel = bootstrap
                .channels
                .iter()
                .find(|channel| channel.id == *id && channel.kind == "text")?;
            if let Some(group) = bootstrap
                .conversations
                .iter()
                .find(|conversation| conversation.channel_id == *id && conversation.kind == "group")
            {
                if !group.member_ids.contains(&bootstrap.user.id) {
                    return None;
                }
                Some(format!("Group · {}", channel.name))
            } else {
                Some(format!("#{}", channel.name))
            }
        }
        Target::Direct(id) => {
            if *id == bootstrap.user.id || bootstrap.social.blocked.contains(id) {
                return None;
            }
            let peer = bootstrap.members.iter().find(|member| member.id == *id)?;
            let name = profile_ui::display_name(peer);
            Some(if name == peer.username {
                format!("@{name}")
            } else {
                format!("{name} (@{})", peer.username)
            })
        }
    }
}

fn inbox_message_order(message: &Message) -> (i64, u64, String) {
    let timestamp = chrono::DateTime::parse_from_rfc3339(&message.created_at)
        .map(|date| date.timestamp_millis())
        .unwrap_or(i64::MIN);
    (timestamp, message.sequence, message.id.clone())
}

struct InboxUnreadRow {
    target: Target,
    label: String,
    unread: u64,
    mentions: u64,
}

/// Send bulk inbox actions one at a time; the network queue has a fixed capacity.
#[derive(Default)]
struct InboxReadQueue {
    pending: Option<Target>,
    remaining: std::collections::VecDeque<Target>,
}

impl InboxReadQueue {
    fn start(&mut self, targets: Vec<Target>) {
        if self.pending.is_some() {
            return;
        }
        let mut seen = HashSet::new();
        self.remaining = targets
            .into_iter()
            .filter(|target| seen.insert(target.clone()))
            .collect();
        self.pending = self.remaining.pop_front();
    }

    fn complete(&mut self, target: &Target, succeeded: bool) -> bool {
        if self.pending.as_ref() != Some(target) {
            return false;
        }
        if !succeeded {
            self.remaining.clear();
        }
        self.pending = self.remaining.pop_front();
        true
    }

    fn clear(&mut self) {
        self.pending = None;
        self.remaining.clear();
    }
}

fn inbox_unread_rows(bootstrap: &Bootstrap, query: &str) -> Vec<InboxUnreadRow> {
    let query = query.trim().to_lowercase();
    let mut rows: HashMap<Target, InboxUnreadRow> = HashMap::new();
    for read in &bootstrap.read_states {
        if read.unread_count == 0 {
            continue;
        }
        let target = match (&read.channel_id, &read.peer_id) {
            (Some(id), None) => Target::Channel(id.clone()),
            (None, Some(id)) => Target::Direct(id.clone()),
            _ => continue,
        };
        let Some(label) = inbox_conversation_label(bootstrap, &target) else {
            continue;
        };
        if !label.to_lowercase().contains(&query) {
            continue;
        }
        let row = rows.entry(target.clone()).or_insert(InboxUnreadRow {
            target,
            label,
            unread: 0,
            mentions: 0,
        });
        row.unread = row.unread.max(read.unread_count);
        row.mentions = row.mentions.max(read.mention_count.min(read.unread_count));
    }
    let mut rows: Vec<_> = rows.into_values().collect();
    rows.sort_by_cached_key(|row| (row.label.to_lowercase(), format!("{:?}", row.target)));
    rows
}

impl RelayApp {
    fn begin_inbox_read(&mut self, targets: Vec<Target>) {
        if self.admin_ui.inbox_read_queue.pending.is_some()
            || self.connection_status != ConnectionStatus::Connected
        {
            return;
        }
        self.admin_ui.inbox_read_queue.start(targets);
        self.send_inbox_read();
    }

    fn send_inbox_read(&mut self) {
        let Some(target) = self.admin_ui.inbox_read_queue.pending.clone() else {
            return;
        };
        let Some(origin) = self.command_origin() else {
            self.admin_ui.inbox_read_queue.clear();
            return;
        };
        if self
            .bootstrap
            .as_ref()
            .is_none_or(|bootstrap| inbox_conversation_label(bootstrap, &target).is_none())
        {
            self.admin_ui.inbox_read_queue.clear();
            self.error = Some(
                "That conversation is no longer available. Refresh Inbox and try again.".into(),
            );
            return;
        }
        if let Err(error) = self.network.send(Command::MarkRead {
            origin,
            target,
            last_read_sequence: u64::MAX,
        }) {
            self.admin_ui.inbox_read_queue.clear();
            self.error = Some(error);
        }
    }

    pub(super) fn finish_inbox_read(&mut self, target: &Target, succeeded: bool) {
        if self.admin_ui.inbox_read_queue.complete(target, succeeded) {
            self.send_inbox_read();
        }
    }

    fn social_action_pending(&self) -> bool {
        self.admin_requests
            .values()
            .any(|key| key == "action:social" || key == "action:people-request")
    }

    fn social_actions_available(&self) -> bool {
        self.connection_status == ConnectionStatus::Connected && !self.social_action_pending()
    }

    pub(super) fn people_request_failed(&mut self, error: String) {
        // Keep the entered name for correction/retry. No friendship is assumed.
        self.admin_ui.friend_request_feedback = Some((false, error));
    }

    pub(super) fn apply_mention_inbox(
        &mut self,
        request_id: u64,
        messages: Vec<(Target, Message)>,
        remaining: Vec<(Target, Option<u64>)>,
        errors: Vec<String>,
    ) {
        if self.admin_requests.remove(&request_id).as_deref() != Some("inbox") {
            return;
        }
        for row in messages {
            if let Some(old) = self
                .admin_ui
                .inbox_messages
                .iter_mut()
                .find(|(target, m)| *target == row.0 && m.id == row.1.id)
            {
                *old = row;
            } else {
                self.admin_ui.inbox_messages.push(row);
            }
        }
        self.admin_ui
            .inbox_messages
            .sort_by_key(|row| std::cmp::Reverse(inbox_message_order(&row.1)));
        self.admin_ui.inbox_remaining = remaining;
        self.admin_ui.inbox_errors.extend(errors);
    }
    fn load_mention_inbox(&mut self, refresh: bool) {
        if self.admin_requests.values().any(|key| key == "inbox") {
            return;
        }
        let Some(origin) = self.command_origin() else {
            return;
        };
        let request_id = self.next_request();
        let remaining = if refresh {
            self.admin_ui.inbox_messages.clear();
            self.admin_ui.inbox_errors.clear();
            None
        } else {
            Some(self.admin_ui.inbox_remaining.clone())
        };
        self.admin_ui.inbox_loaded = true;
        if let Err(error) = self.network.send(Command::MentionInbox {
            origin,
            remaining,
            request_id,
        }) {
            self.error = Some(error);
        } else {
            self.admin_requests.insert(request_id, "inbox".into());
        }
    }
    pub(super) fn mention_inbox_page(&mut self, ui: &mut egui::Ui) {
        let Some(bootstrap) = self.bootstrap.clone() else {
            return;
        };
        // The window and navigation already name this page. Keep the controls
        // compact so enlarged text still leaves room for the message list.
        let pending = self.admin_requests.values().any(|key| {
            key == if self.admin_ui.inbox_tab == 0 {
                "inbox-read-states"
            } else {
                "inbox"
            }
        });
        ui.horizontal_wrapped(|ui| {
            for (tab, label) in [(0, "Unreads"), (1, "Mentions")] {
                if crate::settings_ui::top_tab(
                    ui,
                    ("inbox-tab", tab),
                    label,
                    self.admin_ui.inbox_tab == tab,
                )
                .clicked()
                {
                    self.admin_ui.inbox_tab = tab;
                }
            }
            if ui.add_enabled(!pending, secondary("Refresh")).clicked() {
                if self.admin_ui.inbox_tab == 0 {
                    self.refresh_admin_data("inbox-read-states", "/api/read-state");
                } else {
                    self.load_mention_inbox(true);
                }
            }
            if pending {
                ui.spinner().on_hover_text("Updating this Inbox view");
            }
        });
        ui.separator();
        let filter = ui.add_sized(
            [ui.available_width(), 34.0],
            egui::TextEdit::singleline(&mut self.admin_ui.inbox_filter)
                .hint_text(if self.admin_ui.inbox_tab == 0 {
                    "Filter conversations by name"
                } else {
                    "Filter by conversation or author"
                })
                .char_limit(100),
        );
        ui.ctx().accesskit_node_builder(filter.id, |builder| {
            builder.set_name("Filter Inbox conversations")
        });
        ui.add_space(8.0);
        if self.admin_ui.inbox_tab == 0 {
            self.unread_inbox_page(ui, &bootstrap);
            return;
        }
        if !self.admin_ui.inbox_loaded {
            self.load_mention_inbox(true);
        }
        let pending = self.admin_requests.values().any(|key| key == "inbox");
        let rows: Vec<_> = self
            .admin_ui
            .inbox_messages
            .iter()
            .filter_map(|(target, message)| {
                if self.is_blocked(&message.author.id) {
                    return None;
                }
                let label = inbox_conversation_label(&bootstrap, target)?;
                let query = self.admin_ui.inbox_filter.trim().to_lowercase();
                if !label.to_lowercase().contains(&query)
                    && !profile_ui::display_name(&message.author)
                        .to_lowercase()
                        .contains(&query)
                    && !message.author.username.to_lowercase().contains(&query)
                {
                    return None;
                }
                Some((target.clone(), message.clone(), label))
            })
            .collect();
        let empty = rows.is_empty();
        ui.label(
            RichText::new(format!(
                "{} {} in the history checked",
                rows.len(),
                if rows.len() == 1 {
                    "mention"
                } else {
                    "mentions"
                }
            ))
            .size(font_size(TEXT_CAPTION))
            .color(c_muted()),
        );
        let list_height = ui.available_height().max(0.0);
        egui::ScrollArea::vertical()
            .id_source("inbox-mention-results")
            .max_height(list_height)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                for (target, message, label) in rows {
                    ui.push_id((&target, &message.id), |ui| {
                        egui::Frame::none()
                            .fill(c_card())
                            .rounding(8.0)
                            .inner_margin(Margin::same(14.0))
                            .show(ui, |ui| {
                                ui.set_min_width(ui.available_width());
                                let context = crate::settings_ui::single_line(
                                    ui,
                                    &label,
                                    12.0,
                                    c_muted(),
                                    ui.available_width(),
                                );
                                ui.label(context).on_hover_text(&label);
                                ui.horizontal(|ui| {
                                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                        if ui.button("Jump to Message").clicked() {
                                            self.select(target.clone());
                                            self.reveal_message(message.id.clone());
                                            self.settings_open = false;
                                        }
                                        let author = profile_ui::display_name(&message.author);
                                        ui.allocate_ui_with_layout(
                                            Vec2::new(
                                                ui.available_width(),
                                                ui.spacing().interact_size.y,
                                            ),
                                            Layout::left_to_right(Align::Center),
                                            |ui| {
                                                let name = crate::settings_ui::single_line(
                                                    ui,
                                                    author,
                                                    14.0,
                                                    c_text(),
                                                    ui.available_width(),
                                                );
                                                ui.label(name).on_hover_text(author);
                                            },
                                        );
                                    });
                                });
                                ui.label(
                                    RichText::new(format!(
                                        "{} · {}",
                                        day_label(&message.created_at),
                                        display_time(&message.created_at)
                                    ))
                                    .size(font_size(11.0))
                                    .color(c_muted()),
                                )
                                .on_hover_text(&message.created_at);
                                self.message_preview(ui, &message);
                            });
                        ui.add_space(8.0);
                    });
                }
                if !self.admin_ui.inbox_remaining.is_empty()
                    && ui
                        .add_enabled(!pending, secondary("Load More History"))
                        .clicked()
                {
                    self.load_mention_inbox(false);
                }
                if self.admin_ui.inbox_loaded && !pending && empty {
                    ui.label(if self.admin_ui.inbox_filter.trim().is_empty() {
                        "No mentions found in the history checked so far."
                    } else {
                        "No mentions match that conversation or author name."
                    });
                }
                if !self.admin_ui.inbox_errors.is_empty() {
                    ui.collapsing("Some conversations could not be read", |ui| {
                        for error in &self.admin_ui.inbox_errors {
                            ui.label(error);
                        }
                        if ui.button("Review Encryption").clicked() {
                            self.settings_tab = SettingsTab::Encryption;
                        }
                    });
                }
            });
    }
    fn unread_inbox_page(&mut self, ui: &mut egui::Ui, bootstrap: &Bootstrap) {
        let rows = inbox_unread_rows(bootstrap, &self.admin_ui.inbox_filter);
        let marking = self.admin_ui.inbox_read_queue.pending.is_some();
        let can_mark = self.connection_status == ConnectionStatus::Connected && !marking;
        ui.horizontal_wrapped(|ui| {
            ui.label(
                RichText::new(format!(
                    "{} unread {}",
                    rows.len(),
                    if rows.len() == 1 {
                        "conversation"
                    } else {
                        "conversations"
                    }
                ))
                .size(font_size(TEXT_CAPTION))
                .color(c_muted()),
            );
            let filtered = !self.admin_ui.inbox_filter.trim().is_empty();
            if ui
                .add_enabled(
                    can_mark && !rows.is_empty(),
                    secondary(if filtered {
                        "Mark filtered as read"
                    } else {
                        "Mark all as read"
                    }),
                )
                .on_hover_text(if filtered {
                    "Mark only the conversations matching this filter as read."
                } else {
                    "Mark all unread conversations in this community as read."
                })
                .clicked()
            {
                self.begin_inbox_read(rows.iter().map(|row| row.target.clone()).collect());
            }
            if marking {
                ui.spinner().on_hover_text("Saving read status");
            }
        });
        let list_height = ui.available_height().max(0.0);
        egui::ScrollArea::vertical()
            .id_source("inbox-unread-results")
            .max_height(list_height)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                if rows.is_empty() {
                    ui.label(if self.admin_ui.inbox_filter.trim().is_empty() {
                        "You're all caught up in this community."
                    } else {
                        "No unread conversations match that name."
                    });
                }
                for row in rows {
                    ui.push_id(&row.target, |ui| {
                        egui::Frame::none()
                            .fill(c_card())
                            .rounding(8.0)
                            .inner_margin(Margin::same(14.0))
                            .show(ui, |ui| {
                                ui.set_min_width(ui.available_width());
                                let label = crate::settings_ui::single_line(
                                    ui,
                                    &row.label,
                                    14.0,
                                    c_text(),
                                    ui.available_width(),
                                );
                                ui.label(label).on_hover_text(&row.label);
                                ui.horizontal_wrapped(|ui| {
                                    if ui.add(secondary("View Unreads")).clicked() {
                                        self.select_at(row.target.clone(), true);
                                        self.settings_open = false;
                                    }
                                    if ui
                                        .add_enabled(can_mark, secondary("Mark as Read"))
                                        .clicked()
                                    {
                                        self.begin_inbox_read(vec![row.target.clone()]);
                                    }
                                    ui.label(format!("{} unread", row.unread));
                                    if row.mentions > 0 {
                                        ui.label(
                                            RichText::new(format!(
                                                "{} {}",
                                                row.mentions,
                                                if row.mentions == 1 {
                                                    "mention"
                                                } else {
                                                    "mentions"
                                                }
                                            ))
                                            .color(c_accent()),
                                        );
                                    }
                                });
                            });
                        ui.add_space(8.0);
                    });
                }
            });
    }
    fn notification_policy_key(&self, target: Option<&Target>) -> Option<String> {
        let user = &self.session.as_ref()?.user.id;
        Some(format!(
            "{}|{}",
            ConversationMemory::scope(&self.login.url, user),
            target
                .map(|t| format!("{t:?}"))
                .unwrap_or_else(|| "community".into())
        ))
    }
    pub(super) fn message_notification_allowed(
        &self,
        url: &str,
        user: &str,
        message: &Message,
        target: &Target,
        direct: bool,
    ) -> bool {
        if url == self.login.url {
            if self.is_blocked(&message.author.id)
                || self.bootstrap.as_ref().is_some_and(|b| {
                    b.presence
                        .iter()
                        .any(|p| p.user_id == user && p.status == "dnd")
                })
            {
                return false;
            }
        } else if let Some((owner, social, status)) = self.community_social.get(url) {
            if owner != user || social.blocked.contains(&message.author.id) || status == "dnd" {
                return false;
            }
        } else {
            return false;
        }
        let scope = ConversationMemory::scope(url, user);
        let key = format!("{scope}|{target:?}");
        let community = format!("{scope}|community");
        let fallback = NotificationPolicy::default();
        let community_policy = self
            .prefs
            .notification_policies
            .get(&community)
            .unwrap_or(&fallback);
        if community_policy.muted_until > unix_time() {
            return false;
        }
        let policy = self
            .prefs
            .notification_policies
            .get(&key)
            .filter(|p| !p.mode.is_empty() || p.muted_until > unix_time())
            .unwrap_or(community_policy);
        let mentioned = message.mentions.iter().any(|id| id == user);
        if let Target::Channel(channel) = target {
            if self.friends_group_channel(url, channel) {
                return policy.allows_group(mentioned, unix_time());
            }
        }
        policy.allows(mentioned, direct, self.prefs.mentions_only, unix_time())
    }
    pub(super) fn notification_scopes_ui(&mut self, ui: &mut egui::Ui) {
        let Some(b) = self.bootstrap.clone() else {
            return;
        };
        ui.separator();
        crate::settings_ui::section_heading(
            ui,
            "Community & Channels",
            "Notification choices are saved for this account on this device.",
        );
        let mut scopes = vec![(None, b.server_name.clone())];
        scopes.extend(
            b.channels
                .iter()
                .filter(|c| c.kind == "text")
                .map(|c| (Some(Target::Channel(c.id.clone())), format!("# {}", c.name))),
        );
        for (target, label) in scopes {
            let Some(key) = self.notification_policy_key(target.as_ref()) else {
                continue;
            };
            let before = self
                .prefs
                .notification_policies
                .get(&key)
                .cloned()
                .unwrap_or_default();
            let policy = self
                .prefs
                .notification_policies
                .entry(key.clone())
                .or_default();
            ui.push_id(&key, |ui| {
                ui.collapsing(label, |ui| {
                    egui::ComboBox::from_id_source("mode")
                        .selected_text(match policy.mode.as_str() {
                            "all" => "All Messages",
                            "mentions" => "Only Mentions",
                            "none" => "Nothing",
                            _ => "Use Default",
                        })
                        .show_ui(ui, |ui| {
                            for (value, label) in [
                                ("", "Use Default"),
                                ("all", "All Messages"),
                                ("mentions", "Only Mentions"),
                                ("none", "Nothing"),
                            ] {
                                ui.selectable_value(&mut policy.mode, value.into(), label);
                            }
                        });
                    if policy.muted_until > unix_time() {
                        ui.label("Muted");
                        if ui.button("Unmute").clicked() {
                            policy.muted_until = 0;
                        }
                    }
                    ui.menu_button("Mute…", |ui| {
                        for (seconds, label) in [
                            (900, "15 minutes"),
                            (3600, "1 hour"),
                            (28800, "8 hours"),
                            (86400, "24 hours"),
                            (u64::MAX, "Until I turn it back on"),
                        ] {
                            if ui.button(label).clicked() {
                                policy.muted_until = unix_time().saturating_add(seconds);
                                ui.close_menu();
                            }
                        }
                    });
                });
            });
            if *policy != before {
                self.defer_preferences();
            }
        }
    }
    pub(super) fn is_blocked(&self, user: &str) -> bool {
        self.bootstrap
            .as_ref()
            .is_some_and(|b| b.social.blocked.iter().any(|id| id == user))
    }
    /// A private message may be opened or sent only to who `direct_targets` allows.
    pub(super) fn can_message(&self, user: &str) -> bool {
        self.bootstrap
            .as_ref()
            .is_some_and(|b| crate::direct_targets::actionable(b, user))
    }
    fn friend_action_button(
        &mut self,
        ui: &mut egui::Ui,
        user: &User,
        social: &relay_protocol::SocialState,
    ) {
        if social.blocked.contains(&user.id) {
            return;
        }
        let (label, method) = if social.friends.contains(&user.id) {
            ("Remove Friend", AdminMethod::Delete)
        } else if social.incoming.contains(&user.id) {
            ("Accept Friend", AdminMethod::Post)
        } else if social.outgoing.contains(&user.id) {
            ("Cancel Request", AdminMethod::Delete)
        } else {
            ("Add Friend", AdminMethod::Post)
        };
        if ui
            .add_enabled(self.social_actions_available(), secondary(label))
            .clicked()
        {
            self.admin_request(
                method,
                format!("/api/friends/{}", user.id),
                Value::Null,
                "action:social".into(),
            );
        }
    }
    pub(super) fn social_buttons(&mut self, ui: &mut egui::Ui, user: &User) {
        let Some(b) = self.bootstrap.clone() else {
            return;
        };
        if b.user.id == user.id || !b.capabilities.iter().any(|c| c == "social-v1") {
            return;
        }
        let blocked = b.social.blocked.contains(&user.id);
        ui.horizontal_wrapped(|ui| {
            self.friend_action_button(ui, user, &b.social);
            if ui
                .add_enabled(
                    self.social_actions_available(),
                    secondary(if blocked { "Unblock" } else { "Block" }),
                )
                .clicked()
            {
                self.admin_request(
                    if blocked {
                        AdminMethod::Delete
                    } else {
                        AdminMethod::Post
                    },
                    format!("/api/blocks/{}", user.id),
                    Value::Null,
                    "action:social".into(),
                );
            }
            if ui.button("Report").clicked() {
                self.begin_report(user.clone(), String::new());
            }
        });
    }
    pub(super) fn begin_report(&mut self, user: User, evidence: String) {
        self.profile_ui.card = None;
        self.profile_ui.card_request = None;
        self.admin_ui.report_target = Some(user);
        self.admin_ui.report_reason.clear();
        self.admin_ui.report_evidence = evidence;
        self.admin_ui.share_report_evidence = false;
    }
    pub(super) fn social_dialogs(&mut self, ctx: &egui::Context) {
        let Some(target) = self.admin_ui.report_target.clone() else {
            return;
        };
        let mut open = true;
        let window = desktop_ui::dialog(ctx, "Report Member")
            .id(egui::Id::new("report-member"))
            .open(&mut open)
            .collapsible(false)
            .default_width(420.0);
        // The shared header (title, Close, Escape) stays above the scrolling body.
        if host_setup_ui::show_with_header(ctx, window, "Report Member", |ui| {
            ui.label(format!(
                "Report {} to this community's moderators",
                profile_ui::display_name(&target)
            ));
            ui.label("Reason");
            ui.add(
                egui::TextEdit::multiline(&mut self.admin_ui.report_reason)
                    .char_limit(1000)
                    .desired_width(f32::INFINITY),
            );
            ui.checkbox(
                &mut self.admin_ui.share_report_evidence,
                "Share the following decrypted evidence with moderators",
            );
            ui.add(
                egui::TextEdit::multiline(&mut self.admin_ui.report_evidence)
                    .char_limit(4000)
                    .desired_width(f32::INFINITY)
                    .hint_text("Optional evidence — review before sharing"),
            );
            ui.label(
                RichText::new("Only this report is shared. Other conversations remain encrypted.")
                    .color(c_muted()),
            );
            let pending = self.admin_requests.values().any(|v| v == "action:report");
            if ui
                .add_enabled(
                    !pending && !self.admin_ui.report_reason.trim().is_empty(),
                    primary("Submit Report"),
                )
                .clicked()
            {
                self.admin_request(AdminMethod::Post,"/api/reports".into(),json!({"reported_id":target.id,"reason":self.admin_ui.report_reason,"evidence":if self.admin_ui.share_report_evidence {self.admin_ui.report_evidence.clone()} else {String::new()},"share_evidence":self.admin_ui.share_report_evidence}),"action:report".into());
            }
        }) {
            open = false;
        }
        if !open {
            self.admin_ui.report_target = None;
            self.admin_ui.report_evidence.clear();
        }
    }
    pub(super) fn friends_page(&mut self, ui: &mut egui::Ui) {
        let people = self.in_people();
        ui.horizontal_wrapped(|ui| {
            if crate::settings_ui::top_tab(
                ui,
                "friend-scope-current",
                if people {
                    "Relay friends"
                } else {
                    "This server"
                },
                !self.admin_ui.friend_all_servers,
            )
            .clicked()
            {
                self.admin_ui.friend_all_servers = false;
            }
            if crate::settings_ui::top_tab(
                ui,
                "friend-scope-all",
                "All my servers",
                self.admin_ui.friend_all_servers,
            )
            .clicked()
            {
                self.admin_ui.friend_all_servers = true;
                self.admin_ui.friend_tab = self.admin_ui.friend_tab.min(3);
            }
        });
        ui.separator();
        ui.add_space(10.0);
        if self.admin_ui.friend_all_servers {
            self.all_server_friends(ui);
            return;
        }
        let Some(b) = self.bootstrap.clone() else {
            return;
        };
        if !b.capabilities.iter().any(|c| c == "social-v1") {
            ui.label("Update the community server to use friends and status.");
            return;
        }
        let connected = self.connection_status == ConnectionStatus::Connected;
        self.group_invitations_ui(ui, &b);
        ui.horizontal_wrapped(|ui| {
            icon(ui, Icon::Users, 22.0, c_muted());
            ui.label(
                RichText::new(if people {
                    "Relay friends".into()
                } else {
                    format!("Friends on {}", b.server_name)
                })
                .size(font_size(16.0))
                .strong(),
            );
            ui.separator();
            for (value, label) in [
                (0, "Online"),
                (1, "All"),
                (2, "Pending"),
                (3, "Blocked"),
                (4, "Add Friend"),
            ] {
                if crate::settings_ui::top_tab(
                    ui,
                    ("friends-tab", value),
                    label,
                    self.admin_ui.friend_tab == value,
                )
                .clicked()
                {
                    self.admin_ui.friend_tab = value;
                }
            }
            let refreshing = self.admin_requests.values().any(|key| key == "social");
            if ui
                .add_enabled_ui(connected && !refreshing, |ui| {
                    icon_button(ui, Icon::More, false, "Refresh friends").clicked()
                })
                .inner
            {
                self.refresh_admin_data("social", "/api/social");
            }
        });
        ui.separator();
        ui.add_space(12.0);
        if !connected {
            ui.label(RichText::new(match self.connection_status {
                ConnectionStatus::SignInRequired => "Sign in again to update Friends. Your last known list is shown below.",
                ConnectionStatus::Reconnecting => "Reconnecting… Your last known friends are still under All. Calls and requests will be available when connected.",
                _ => "Friends is unavailable. Relay is retrying. Your last known friends are still under All.",
            }).color(c_accent()));
            if self.admin_ui.friend_tab == 0 && ui.button("Show all friends").clicked() {
                self.admin_ui.friend_tab = 1;
            }
            ui.add_space(8.0);
        }
        ui.label(
            RichText::new(
                if people { "Add a friend by their exact Relay name. You can message and call without a shared server." } else { "Find people on this server. Friend requests and private groups stay here." },
            )
            .color(c_muted()),
        );
        let adding = self.admin_ui.friend_tab == 4;
        let pending = self.social_action_pending();
        let search_field = ui.add_sized(
            [ui.available_width(), 34.0],
            egui::TextEdit::singleline(self.admin_ui.friend_input(adding && people))
                .id_source(if adding && people {
                    "friend-request-name"
                } else {
                    "friend-list-search"
                })
                .interactive(!(adding && people && pending))
                .hint_text(if adding {
                    if people {
                        "Enter your friend’s exact @RelayName"
                    } else {
                        "Find a member to add"
                    }
                } else {
                    "Search friends"
                }),
        );
        if adding && people {
            if search_field.changed() && !pending {
                self.admin_ui.friend_request_feedback = None;
            }
            let send = ui
                .add_enabled(
                    connected && !pending && !self.admin_ui.friend_request_name.trim().is_empty(),
                    primary(if pending {
                        "Sending…"
                    } else {
                        "Send friend request"
                    }),
                )
                .clicked();
            let enter = search_field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if (send || enter)
                && connected
                && !pending
                && !self.admin_ui.friend_request_name.trim().is_empty()
            {
                self.admin_ui.friend_request_feedback = None;
                self.admin_request(
                    AdminMethod::Post,
                    "/api/people/requests".into(),
                    json!({"handle":self.admin_ui.friend_request_name.trim()}),
                    "action:people-request".into(),
                );
            }
            if let Some((ok, message)) = self.admin_ui.friend_request_feedback.clone() {
                ui.add(
                    egui::Label::new(RichText::new(message).color(if ok {
                        c_good()
                    } else {
                        c_bad()
                    }))
                    .wrap(true),
                );
                if ok {
                    ui.horizontal(|ui| {
                        for (tab, label) in [(2, "View pending requests"), (1, "All friends")] {
                            if ui.button(label).clicked() {
                                self.admin_ui.friend_tab = tab;
                                self.admin_ui.friend_search.clear();
                                self.admin_ui.friend_request_feedback = None;
                            }
                        }
                    });
                }
            }
            ui.label(
                RichText::new(
                    "For example @alex. Your friend needs to have opened Friends in Relay once.",
                )
                .color(c_muted()),
            );
            return;
        }
        ui.add_space(12.0);
        let search = friend_search_query(&self.admin_ui.friend_search);
        let tab = self.admin_ui.friend_tab;
        let rows: Vec<_> = b
            .members
            .iter()
            .filter(|member| {
                friend_matches_view(&b, member, tab, connected)
                    && (member.username.to_lowercase().contains(&search)
                        || profile_ui::display_name(member)
                            .to_lowercase()
                            .contains(&search)
                        || account_link_ui::linked_name(
                            &b,
                            self.central_account.provider.as_ref(),
                            &member.id,
                        )
                        .is_some_and(|link| link.handle.to_lowercase().contains(&search)))
            })
            .cloned()
            .collect();
        crate::settings_ui::nav_group(
            ui,
            &format!(
                "{} — {}",
                ["ONLINE", "ALL FRIENDS", "PENDING", "BLOCKED", "ADD FRIEND"]
                    [usize::from(tab.min(4))],
                rows.len()
            ),
        );
        if rows.is_empty() {
            ui.label(if tab == 0 && !connected {
                "Online status is unavailable until Relay reconnects."
            } else if !search.is_empty() {
                "No matches. Try another name."
            } else {
                match tab {
                    0 => "No friends online. Choose All to see your friends, or Add Friend to find someone.",
                    1 if people => "No friends yet. Choose Add Friend and enter their exact Relay name.",
                    1 => "No friends yet. Choose Add Friend to find people on this server.",
                    2 => "No pending friend requests.",
                    3 => "You have not blocked anyone.",
                    _ => "No new people to add on this server.",
                }
            });
        }
        if rows.len() > 100 {
            ui.label(
                RichText::new(format!(
                    "Showing 100 of {} matches. Search for a name to narrow the list.",
                    rows.len()
                ))
                .color(c_muted()),
            );
        }
        if tab <= 1 && !rows.is_empty() {
            ui.add(egui::Label::new(RichText::new(if b.capabilities.iter().any(|cap| cap == "private-calls-v1") { "Call a friend here. They choose whether to accept; incoming calls never open your microphone." } else { "Private voice opens a room. Both people join it themselves; update this server to ring a friend." }).color(c_muted())).wrap(true));
            ui.add_space(8.0);
        }
        for member in rows.into_iter().take(100) {
            ui.push_id(&member.id, |ui| {
                let (identity_width, action_width) = friend_row_widths(ui);
                ui.horizontal_wrapped(|ui| {
                    ui.allocate_ui_with_layout(Vec2::new(identity_width, 0.0), Layout::left_to_right(Align::Center), |ui| {
                    self.user_avatar(ui, &member, 36.0, false);
                    ui.vertical(|ui| {
                        let name = profile_ui::display_name(&member);
                        ui.add(egui::Label::new(RichText::new(name).strong()).truncate(true))
                            .on_hover_text(name);
                        self.linked_account_label(ui,&member.id);
                        if tab == 2 {
                            ui.label(if b.social.incoming.contains(&member.id) {
                                "Incoming friend request"
                            } else {
                                "Outgoing friend request"
                            });
                        } else if !connected {
                            ui.label(RichText::new("Status unavailable").color(c_muted()));
                        } else {
                            self.presence_label(ui, &member.id);
                        }
                    });
                    });
                    ui.allocate_ui_with_layout(Vec2::new(action_width, 0.0), Layout::right_to_left(Align::Center), |ui| {
                        ui.menu_button("•••", |ui| {
                            self.social_buttons(ui, &member);
                        });
                        if tab <= 1 && icon_button(ui, Icon::Chat, false, "Message").clicked() {
                            self.select(Target::Direct(member.id.clone()));
                        }
                        if tab <= 1 && ui.add_enabled(connected, secondary(if b.capabilities.iter().any(|cap| cap == "private-calls-v1") { "Call" } else { "Private voice" }))
                            .on_hover_text("Call this friend, or open the private room on an older server.")
                            .clicked() {
                            self.start_friend_call(&member.id);
                        }
                        if (tab >= 2) && tab != 3 {
                            self.friend_action_button(ui, &member, &b.social);
                        }
                        if tab == 3 && ui.add_enabled(self.social_actions_available(), secondary("Unblock")).clicked() {
                            self.admin_request(AdminMethod::Delete, format!("/api/blocks/{}", member.id), Value::Null, "action:social".into());
                        }
                        if tab == 2
                            && b.social.incoming.contains(&member.id)
                            && ui.add_enabled_ui(self.social_actions_available(), |ui| {
                                icon_button(ui, Icon::Close, false, "Decline friend request").clicked()
                            }).inner
                        {
                            self.admin_request(
                                AdminMethod::Delete,
                                format!("/api/friends/{}", member.id),
                                Value::Null,
                                "action:social".into(),
                            );
                        }
                    });
                });
                ui.add_space(8.0);
                ui.separator();
                ui.add_space(8.0);
            });
        }
    }
    pub(super) fn presence_editor(&mut self, ui: &mut egui::Ui) {
        let Some(b) = self.bootstrap.clone() else {
            return;
        };
        ui.separator();
        crate::settings_ui::section_heading(
            ui,
            "Status",
            "Choose how you appear to this community.",
        );
        let own = b
            .presence
            .iter()
            .find(|p| p.user_id == b.user.id)
            .cloned()
            .unwrap_or(UserPresence {
                user_id: b.user.id.clone(),
                status: "online".into(),
                ..Default::default()
            });
        let draft = self.admin_ui.presence_draft.get_or_insert(own.clone());
        egui::ComboBox::from_id_source("presence-status")
            .selected_text(presence_name(&draft.status))
            .show_ui(ui, |ui| {
                for (value, label) in [
                    ("online", "Online"),
                    ("away", "Idle"),
                    ("dnd", "Do Not Disturb"),
                    ("invisible", "Invisible"),
                ] {
                    ui.selectable_value(&mut draft.status, value.into(), label);
                }
            });
        ui.add(
            egui::TextEdit::singleline(&mut draft.text)
                .char_limit(128)
                .hint_text("What's happening?")
                .desired_width(f32::INFINITY),
        );
        let mut expiry = draft
            .expires_at
            .map(|at| at.saturating_sub(unix_time()))
            .unwrap_or(0);
        egui::ComboBox::from_id_source("status-expiry")
            .selected_text(if draft.expires_at.is_none() {
                "Don't clear"
            } else {
                "Clear automatically"
            })
            .show_ui(ui, |ui| {
                for (seconds, label) in [
                    (0, "Don't clear"),
                    (3600, "1 hour"),
                    (14400, "4 hours"),
                    (86400, "1 day"),
                ] {
                    if ui.selectable_value(&mut expiry, seconds, label).changed() {
                        draft.expires_at = if seconds == 0 {
                            None
                        } else {
                            Some(unix_time() + seconds)
                        };
                    }
                }
            });
        let submitted = draft.clone();
        let dirty = submitted != own;
        ui.horizontal(|ui| {
            if ui.add_enabled(dirty, primary("Save Status")).clicked() {
                self.admin_request(
                    AdminMethod::Put,
                    "/api/presence".into(),
                    serde_json::to_value(&submitted).unwrap(),
                    "action:presence".into(),
                );
            }
            if ui.add_enabled(dirty, secondary("Reset")).clicked() {
                self.admin_ui.presence_draft = None;
            }
        });
    }
    pub(super) fn presence_label(&self, ui: &mut egui::Ui, user: &str) {
        if let Some(p) = self
            .bootstrap
            .as_ref()
            .and_then(|b| b.presence.iter().find(|p| p.user_id == user))
        {
            let color = match p.status.as_str() {
                "online" => c_good(),
                "dnd" => c_bad(),
                _ => c_muted(),
            };
            ui.label(
                RichText::new(format!(
                    "● {}{}",
                    presence_name(&p.status),
                    if p.text.is_empty() {
                        String::new()
                    } else {
                        format!(" · {}", p.text)
                    }
                ))
                .size(font_size(TEXT_CAPTION))
                .color(color),
            );
            // Opt-in game activity: only what the Friends service shows this account.
            if let Some(activity) = &p.activity {
                let own = self.bootstrap.as_ref().is_some_and(|b| b.user.id == user);
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!(
                            "{} {}",
                            if own { "Sharing" } else { "Playing" },
                            activity.name
                        ))
                        .size(font_size(TEXT_CAPTION))
                        .color(c_good()),
                    );
                    if let (false, Some(link)) = (own, &activity.join) {
                        if ui
                            .small_button("Join…")
                            .on_hover_text(
                                "Ask to open this invitation. Nothing opens until you confirm.",
                            )
                            .clicked()
                        {
                            ui.ctx().data_mut(|data| {
                                data.insert_temp(
                                    egui::Id::new(super::game_activity_ui::JOIN_REQUEST),
                                    (user.to_owned(), activity.game_id.clone(), link.clone()),
                                )
                            });
                        }
                    }
                });
            }
        }
    }
    pub(super) fn paint_presence_badge(
        &self,
        ui: &egui::Ui,
        user: &str,
        response: &egui::Response,
    ) {
        let presence = self
            .bootstrap
            .as_ref()
            .and_then(|b| b.presence.iter().find(|p| p.user_id == user));
        let color = match presence.map(|p| p.status.as_str()).unwrap_or("offline") {
            "online" => c_good(),
            "away" => Color32::from_rgb(240, 178, 50),
            "dnd" => c_bad(),
            _ => c_muted(),
        };
        let pos = egui::Pos2::new(response.rect.left() + 31.0, response.rect.center().y + 10.0);
        ui.painter().circle_filled(pos, 6.0, c_panel());
        ui.painter().circle_filled(pos, 4.0, color);
        if let Some(p) = presence {
            response.clone().on_hover_text(format!(
                "{}{}",
                presence_name(&p.status),
                if p.text.is_empty() {
                    String::new()
                } else {
                    format!(" · {}", p.text)
                }
            ));
        }
    }
    /// A group member's coarse state, from that group's own list. It is not the
    /// friends presence: nothing is drawn when the server sent no row.
    pub(super) fn paint_group_presence_badge(
        &self,
        ui: &egui::Ui,
        channel: &str,
        user: &str,
        response: &egui::Response,
    ) {
        let Some(status) = self
            .bootstrap
            .as_ref()
            .filter(|b| {
                b.capabilities
                    .iter()
                    .any(|c| c == relay_protocol::PEOPLE_GROUP_PRESENCE_CAPABILITY)
            })
            .and_then(|b| {
                b.group_presence
                    .iter()
                    .find(|p| p.channel_id == channel && p.user_id == user)
                    .map(|p| p.status.as_str())
            })
        else {
            return;
        };
        let color = match status {
            "online" => c_good(),
            "idle" => Color32::from_rgb(240, 178, 50),
            "dnd" => c_bad(),
            _ => c_muted(),
        };
        let pos = egui::Pos2::new(response.rect.left() + 31.0, response.rect.center().y + 10.0);
        ui.painter().circle_filled(pos, 6.0, c_panel());
        ui.painter().circle_filled(pos, 4.0, color);
        response
            .clone()
            .on_hover_text(presence_name(if status == "idle" {
                "away"
            } else {
                status
            }));
    }
    pub(super) fn reports_ui(&mut self, ui: &mut egui::Ui) {
        self.ensure_admin_data("reports", "/api/reports");
        let reports = self
            .admin_data
            .get("reports")
            .cloned()
            .and_then(|v| serde_json::from_value::<Vec<UserReport>>(v).ok())
            .unwrap_or_default();
        if ui.button("Refresh Reports").clicked() {
            self.refresh_admin_data("reports", "/api/reports");
        }
        for report in reports {
            ui.collapsing(
                format!(
                    "{} · {}{}",
                    report.created_at,
                    report.reported_id,
                    if report.resolved { " · Resolved" } else { "" }
                ),
                |ui| {
                    ui.label(format!("Reported by {}", report.reporter_id));
                    ui.label(&report.reason);
                    if !report.evidence.is_empty() {
                        ui.collapsing("Evidence explicitly shared by the reporter", |ui| {
                            ui.label(&report.evidence);
                        });
                    }
                    if !report.resolved && ui.button("Mark Resolved").clicked() {
                        self.admin_request(
                            AdminMethod::Put,
                            format!("/api/reports/{}", report.id),
                            Value::Null,
                            "action:reports".into(),
                        );
                    }
                },
            );
        }
    }
}

#[cfg(test)]
mod friends_flow_tests {
    use super::*;

    #[test]
    fn requesting_a_friend_keeps_the_list_filter_and_recipient_draft_separate() {
        let mut state = ServerUi::default();
        state.friend_input(false).push_str(" @Alex ");
        state.friend_input(true).push_str("@Morgan");
        assert_eq!(friend_search_query(state.friend_input(false)), "alex");
        assert_eq!(state.friend_input(true), "@Morgan");
        // Opening Pending/All after a request clears only the list filter;
        // returning to Add Friend retains the name for correction or retry.
        state.friend_search.clear();
        assert!(state.friend_input(false).is_empty());
        assert_eq!(state.friend_input(true), "@Morgan");
    }

    #[test]
    fn disconnected_friends_stay_in_all_without_stale_online_or_blocked_actions() {
        let friend = User {
            id: "friend".into(),
            ..Default::default()
        };
        let mut b = Bootstrap {
            user: User {
                id: "me".into(),
                ..Default::default()
            },
            social: SocialState {
                friends: vec![friend.id.clone()],
                ..Default::default()
            },
            presence: vec![UserPresence {
                user_id: friend.id.clone(),
                status: "online".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(friend_matches_view(&b, &friend, 0, true));
        assert!(!friend_matches_view(&b, &friend, 0, false));
        assert!(friend_matches_view(&b, &friend, 1, false));
        assert!(
            friend_matches_view(&b, &friend, 0, true),
            "reconnection restores online presence"
        );
        b.social.blocked.push(friend.id.clone());
        b.social.incoming.push(friend.id.clone());
        for connected in [false, true] {
            for tab in [0, 1, 2, 4] {
                assert!(!friend_matches_view(&b, &friend, tab, connected));
            }
            assert!(friend_matches_view(&b, &friend, 3, connected));
        }
        assert!(!friend_matches_view(&b, &b.user, 4, true));
    }
}

#[cfg(test)]
mod inbox_context_tests {
    use super::*;

    #[test]
    fn bulk_read_waits_for_each_acknowledgement_and_deduplicates_targets() {
        let a = Target::Channel("general".into());
        let b = Target::Direct("friend".into());
        let mut queue = InboxReadQueue::default();
        queue.start(vec![a.clone(), b.clone(), a.clone()]);
        assert_eq!(queue.pending, Some(a.clone()));
        assert_eq!(queue.remaining.len(), 1);
        assert!(!queue.complete(&b, true));
        assert_eq!(queue.pending, Some(a.clone()));
        queue.start(vec![Target::Channel("unrelated".into())]);
        assert_eq!(queue.pending, Some(a.clone()));
        assert!(queue.complete(&a, true));
        assert_eq!(queue.pending, Some(b.clone()));
        assert!(queue.complete(&b, true));
        assert!(queue.pending.is_none());
        assert!(queue.remaining.is_empty());
    }

    #[test]
    fn bulk_read_failure_stops_the_batch_without_claiming_unsent_targets_read() {
        let a = Target::Channel("general".into());
        let b = Target::Direct("friend".into());
        let mut queue = InboxReadQueue::default();
        queue.start(vec![a.clone(), b.clone()]);
        assert!(queue.complete(&a, false));
        assert!(queue.pending.is_none());
        assert!(queue.remaining.is_empty());
        queue.start(vec![a.clone(), b]);
        assert_eq!(queue.pending, Some(a));
        queue.clear();
        assert!(queue.pending.is_none());
        assert!(queue.remaining.is_empty());
    }

    #[test]
    fn unreads_ignore_blocked_stale_ambiguous_and_unjoined_targets_without_double_counting() {
        let bootstrap = Bootstrap {
            user: User {
                id: "me".into(),
                ..Default::default()
            },
            members: vec![
                User {
                    id: "peer".into(),
                    username: "captain".into(),
                    ..Default::default()
                },
                User {
                    id: "blocked".into(),
                    username: "blocked".into(),
                    ..Default::default()
                },
            ],
            social: SocialState {
                blocked: vec!["blocked".into()],
                ..Default::default()
            },
            channels: vec![
                Channel {
                    id: "general".into(),
                    name: "general".into(),
                    kind: "text".into(),
                    ..Default::default()
                },
                Channel {
                    id: "group".into(),
                    name: "private group".into(),
                    kind: "text".into(),
                    ..Default::default()
                },
            ],
            conversations: vec![ConversationInfo {
                channel_id: "group".into(),
                kind: "group".into(),
                member_ids: vec!["peer".into()],
                ..Default::default()
            }],
            read_states: vec![
                ReadState {
                    channel_id: Some("general".into()),
                    unread_count: 2,
                    mention_count: 1,
                    ..Default::default()
                },
                ReadState {
                    channel_id: Some("general".into()),
                    unread_count: 3,
                    mention_count: 8,
                    ..Default::default()
                },
                ReadState {
                    peer_id: Some("peer".into()),
                    unread_count: 4,
                    ..Default::default()
                },
                ReadState {
                    peer_id: Some("blocked".into()),
                    unread_count: 5,
                    ..Default::default()
                },
                ReadState {
                    peer_id: Some("me".into()),
                    unread_count: 5,
                    ..Default::default()
                },
                ReadState {
                    channel_id: Some("group".into()),
                    unread_count: 5,
                    ..Default::default()
                },
                ReadState {
                    channel_id: Some("missing".into()),
                    unread_count: 5,
                    ..Default::default()
                },
                ReadState {
                    channel_id: Some("general".into()),
                    peer_id: Some("peer".into()),
                    unread_count: 5,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let rows = inbox_unread_rows(&bootstrap, "");
        assert_eq!(rows.len(), 2);
        let general = rows
            .iter()
            .find(|row| row.target == Target::Channel("general".into()))
            .unwrap();
        assert_eq!((general.unread, general.mentions), (3, 3));
        let filtered = inbox_unread_rows(&bootstrap, "  CAPTAIN  ");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].target, Target::Direct("peer".into()));
        assert!(inbox_unread_rows(&bootstrap, "missing").is_empty());
    }

    #[test]
    fn mention_context_only_names_current_accessible_conversations() {
        let mut bootstrap = Bootstrap {
            user: User {
                id: "me".into(),
                ..Default::default()
            },
            members: vec![User {
                id: "peer".into(),
                username: "captain".into(),
                profile: UserProfile {
                    display_name: "Friday Captain".into(),
                    ..Default::default()
                },
                ..Default::default()
            }],
            channels: vec![
                Channel {
                    id: "general".into(),
                    name: "general".into(),
                    kind: "text".into(),
                    ..Default::default()
                },
                Channel {
                    id: "group".into(),
                    name: "Friday group".into(),
                    kind: "text".into(),
                    ..Default::default()
                },
            ],
            conversations: vec![ConversationInfo {
                channel_id: "group".into(),
                kind: "group".into(),
                member_ids: vec!["me".into(), "peer".into()],
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(
            inbox_conversation_label(&bootstrap, &Target::Channel("general".into())).as_deref(),
            Some("#general")
        );
        assert_eq!(
            inbox_conversation_label(&bootstrap, &Target::Channel("group".into())).as_deref(),
            Some("Group · Friday group")
        );
        assert_eq!(
            inbox_conversation_label(&bootstrap, &Target::Direct("peer".into())).as_deref(),
            Some("Friday Captain (@captain)")
        );
        assert!(inbox_conversation_label(&bootstrap, &Target::Direct("me".into())).is_none());
        assert!(inbox_conversation_label(&bootstrap, &Target::Channel("missing".into())).is_none());
        bootstrap.social.blocked.push("peer".into());
        assert!(inbox_conversation_label(&bootstrap, &Target::Direct("peer".into())).is_none());
        bootstrap.conversations[0]
            .member_ids
            .retain(|id| id != "me");
        assert!(inbox_conversation_label(&bootstrap, &Target::Channel("group".into())).is_none());
    }

    #[test]
    fn inbox_sorts_across_conversations_by_actual_time_before_sequence() {
        let older = Message {
            id: "older".into(),
            sequence: 9000,
            created_at: "2026-10-07T14:00:00Z".into(),
            ..Default::default()
        };
        let newer = Message {
            id: "newer".into(),
            sequence: 2,
            created_at: "2026-10-07T16:00:01+02:00".into(),
            ..Default::default()
        };
        assert!(inbox_message_order(&newer) > inbox_message_order(&older));
        let malformed = Message {
            id: "invalid".into(),
            sequence: 9900,
            created_at: "unknown".into(),
            ..Default::default()
        };
        assert!(inbox_message_order(&malformed) < inbox_message_order(&older));
    }
}
fn presence_name(status: &str) -> &str {
    match status {
        "online" => "Online",
        "away" => "Idle",
        "dnd" => "Do Not Disturb",
        "invisible" => "Invisible",
        _ => "Offline",
    }
}
fn unix_time() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
