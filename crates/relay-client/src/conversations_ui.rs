fn group_member_matches(member: &User, query: &str) -> bool {
    member.username.to_lowercase().contains(query)
        || profile_ui::display_name(member)
            .to_lowercase()
            .contains(query)
}

/// Reuse only a joinable room for this exact pair, never a larger private group.
fn private_friend_voice_room(bootstrap: &Bootstrap, peer: &str) -> Option<String> {
    if peer == bootstrap.user.id
        || !bootstrap.social.friends.iter().any(|id| id == peer)
        || bootstrap.social.blocked.iter().any(|id| id == peer)
        || !bootstrap.members.iter().any(|member| member.id == peer)
    {
        return None;
    }
    bootstrap
        .conversations
        .iter()
        .filter(|group| {
            group.kind == "group"
                && !group.archived
                && group.member_ids.len() == 2
                && group.member_ids.contains(&bootstrap.user.id)
                && group.member_ids.iter().any(|id| id == peer)
        })
        .flat_map(|group| {
            bootstrap.conversations.iter().filter(move |room| {
                room.kind == "group_voice"
                    && !room.archived
                    && room.parent_id.as_ref() == Some(&group.channel_id)
            })
        })
        .filter_map(|room| {
            bootstrap.channels.iter().find(|channel| {
                channel.id == room.channel_id && channel.kind == "voice" && channel.can_join
            })
        })
        .map(|channel| channel.id.clone())
        .min()
}

fn private_voice_room_name(peer: &User) -> String {
    let mut name = format!("Call with {}", profile_ui::display_name(peer));
    // The server bounds names in UTF-8 bytes, not characters.
    while name.len() > 64 {
        name.pop();
    }
    name
}

fn group_member_candidates<'a>(
    bootstrap: &'a Bootstrap,
    group: Option<&ConversationInfo>,
) -> Vec<&'a User> {
    bootstrap
        .members
        .iter()
        .filter(|member| {
            if let Some(group) = group {
                group.member_ids.contains(&member.id)
                    || (member.id != bootstrap.user.id
                        && bootstrap.social.friends.contains(&member.id)
                        && !bootstrap.social.blocked.contains(&member.id))
            } else {
                member.id != bootstrap.user.id
                    && bootstrap.social.friends.contains(&member.id)
                    && !bootstrap.social.blocked.contains(&member.id)
            }
        })
        .collect()
}

fn group_members_picker(
    ui: &mut egui::Ui,
    bootstrap: &Bootstrap,
    search: &mut String,
    selected: &mut Vec<String>,
    group: Option<&ConversationInfo>,
) {
    let limit = if group.is_some() { 10 } else { 9 };
    let candidates = group_member_candidates(bootstrap, group);
    ui.horizontal(|ui| {
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let clear = ui
                .add_enabled(!search.is_empty(), secondary("Clear"))
                .clicked();
            if clear {
                search.clear();
            }
            let search_field = ui.add_sized(
                [ui.available_width(), 32.0],
                egui::TextEdit::singleline(search)
                    .id_source("group-member-search")
                    .hint_text("Search friends by name")
                    .char_limit(100)
                    .desired_width(f32::INFINITY),
            );
            if clear {
                search_field.request_focus();
            }
        });
    });
    ui.label(if group.is_some() {
        format!(
            "{} of 10 members · owner stays in the group",
            selected.len()
        )
    } else {
        format!("{} of 9 friends selected", selected.len())
    });
    let query = search.trim().to_lowercase();
    let list_height = (ui.ctx().screen_rect().height() - 380.0).clamp(56.0, 240.0);
    egui::ScrollArea::vertical()
        .id_source("group-member-list")
        .max_height(list_height)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            let mut matches = 0;
            for member in candidates
                .iter()
                .filter(|member| group_member_matches(member, &query))
            {
                matches += 1;
                ui.push_id(&member.id, |ui| {
                    let mut checked = selected.contains(&member.id);
                    let protected = group.is_some_and(|group| member.id == group.owner_id);
                    let enabled = !protected && (checked || selected.len() < limit);
                    let name = profile_ui::display_name(member);
                    let label = if name == member.username {
                        name.to_owned()
                    } else {
                        format!("{name} (@{})", member.username)
                    };
                    let text = crate::settings_ui::single_line(
                        ui,
                        &label,
                        12.0,
                        if enabled { c_text() } else { c_muted() },
                        ui.available_width() - ui.spacing().icon_width - ui.spacing().icon_spacing,
                    );
                    let response = ui
                        .add_enabled(enabled, egui::Checkbox::new(&mut checked, text))
                        .on_hover_text(&label)
                        .on_disabled_hover_text(if protected {
                            "The owner stays in the group."
                        } else {
                            "Remove a selected friend before adding someone else."
                        });
                    // egui 0.27 omits disabled checkbox state from its accessibility node.
                    ui.ctx().accesskit_node_builder(response.id, |builder| {
                        if response.enabled() {
                            builder.clear_disabled();
                        } else {
                            builder.set_disabled();
                        }
                    });
                    if response.changed() {
                        selected.retain(|id| id != &member.id);
                        if checked {
                            selected.push(member.id.clone());
                        }
                    }
                });
            }
            if matches == 0 {
                ui.label(if candidates.is_empty() {
                    "Add friends before creating a group."
                } else {
                    "No friends match this search."
                });
            }
        });
}

#[cfg(test)]
mod group_picker_tests {
    use super::*;

    fn bootstrap() -> Bootstrap {
        let members: Vec<_> = (0..=13)
            .map(|index| User {
                id: format!("id{index}"),
                username: format!("friend{index}"),
                ..Default::default()
            })
            .collect();
        Bootstrap {
            user: members[0].clone(),
            members,
            social: SocialState {
                friends: (1..=12).map(|index| format!("id{index}")).collect(),
                blocked: vec!["id12".into()],
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn generated_private_room_name_fits_the_server_limit_with_unicode() {
        let mut peer = bootstrap().members[1].clone();
        peer.profile.display_name = "🎧朋友".repeat(16);
        let name = private_voice_room_name(&peer);
        assert!(name.starts_with("Call with ") && name.len() <= 64);
        assert!(name.contains('友'));
    }
    #[test]
    fn private_voice_selection_requires_the_exact_friend_pair_and_current_permission() {
        let mut b = bootstrap();
        b.channels = vec![Channel {
            id: "pair-voice".into(),
            kind: "voice".into(),
            can_join: true,
            ..Default::default()
        }];
        b.conversations = vec![
            ConversationInfo {
                channel_id: "pair".into(),
                kind: "group".into(),
                member_ids: vec!["id0".into(), "id1".into()],
                ..Default::default()
            },
            ConversationInfo {
                channel_id: "pair-voice".into(),
                kind: "group_voice".into(),
                parent_id: Some("pair".into()),
                ..Default::default()
            },
        ];
        assert_eq!(
            private_friend_voice_room(&b, "id1").as_deref(),
            Some("pair-voice")
        );
        b.conversations[0].member_ids.push("id2".into());
        assert!(
            private_friend_voice_room(&b, "id1").is_none(),
            "a larger group is never a one-to-one room"
        );
        b.conversations[0].member_ids.pop();
        b.channels[0].can_join = false;
        assert!(private_friend_voice_room(&b, "id1").is_none());
        b.channels[0].can_join = true;
        b.social.blocked.push("id1".into());
        assert!(private_friend_voice_room(&b, "id1").is_none());
        b.social.blocked.retain(|id| id != "id1");
        b.social.friends.retain(|id| id != "id1");
        b.social.incoming.push("id1".into());
        assert!(
            private_friend_voice_room(&b, "id1").is_none(),
            "a pending request grants no private voice action"
        );
        b.social.friends.push("id1".into());
        b.conversations[0].archived = true;
        assert!(private_friend_voice_room(&b, "id1").is_none());
        assert!(private_friend_voice_room(&b, "id0").is_none());
    }
    #[test]
    fn member_search_preserves_existing_members_and_excludes_blocked_new_recipients() {
        let mut bootstrap = bootstrap();
        bootstrap.members[1].profile.display_name = "Friday Captain".into();
        assert!(group_member_matches(&bootstrap.members[1], "captain"));
        assert!(group_member_matches(&bootstrap.members[1], "friend1"));
        let ids: Vec<_> = group_member_candidates(&bootstrap, None)
            .iter()
            .map(|member| member.id.as_str())
            .collect();
        assert_eq!(ids.len(), 11);
        assert!(!ids.contains(&"id0"));
        assert!(!ids.contains(&"id12"));
        assert!(!ids.contains(&"id13"));
        let group = ConversationInfo {
            owner_id: "id0".into(),
            member_ids: vec!["id0".into(), "id12".into(), "id13".into()],
            ..Default::default()
        };
        let ids: Vec<_> = group_member_candidates(&bootstrap, Some(&group))
            .iter()
            .map(|member| member.id.as_str())
            .collect();
        assert!(ids.contains(&"id0"));
        assert!(ids.contains(&"id12"));
        assert!(ids.contains(&"id13"));
    }

    fn frame(
        ctx: &egui::Context,
        bootstrap: &Bootstrap,
        search: &mut String,
        selected: &mut Vec<String>,
        group: Option<&ConversationInfo>,
        events: Vec<egui::Event>,
    ) -> egui::FullOutput {
        ctx.run(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(940.0, 650.0))),
                events,
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.set_max_width(420.0);
                    group_members_picker(ui, bootstrap, search, selected, group);
                });
            },
        )
    }

    fn click_checkbox(output: egui::FullOutput, name: &str, enabled: bool) -> Vec<egui::Event> {
        let update = output.platform_output.accesskit_update.unwrap();
        let (_, node) = update
            .nodes
            .iter()
            .find(|(_, node)| {
                node.role() == egui::accesskit::Role::CheckBox && node.name() == Some(name)
            })
            .unwrap();
        assert_eq!(!node.is_disabled(), enabled);
        let bounds = node.bounds().unwrap();
        let pos = Pos2::new(
            ((bounds.x0 + bounds.x1) * 0.5) as f32,
            ((bounds.y0 + bounds.y1) * 0.5) as f32,
        );
        vec![
            egui::Event::PointerMoved(pos),
            egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::default(),
            },
            egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::default(),
            },
        ]
    }

    #[test]
    fn clearing_a_focused_recipient_query_keeps_selection_and_accessibility_focus_valid() {
        let bootstrap = bootstrap();
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut search = "friend1".to_owned();
        let mut selected = vec!["id1".to_owned()];
        frame(&ctx, &bootstrap, &mut search, &mut selected, None, vec![]);
        let output = frame(&ctx, &bootstrap, &mut search, &mut selected, None, vec![]);
        let update = output.platform_output.accesskit_update.unwrap();
        let clear_id = update
            .nodes
            .iter()
            .find(|(_, node)| {
                node.role() == egui::accesskit::Role::Button && node.name() == Some("Clear")
            })
            .unwrap()
            .0;
        let output = frame(
            &ctx,
            &bootstrap,
            &mut search,
            &mut selected,
            None,
            vec![egui::Event::AccessKitActionRequest(
                egui::accesskit::ActionRequest {
                    action: egui::accesskit::Action::Default,
                    target: clear_id,
                    data: None,
                },
            )],
        );
        assert!(search.is_empty());
        assert_eq!(selected, ["id1"]);
        let update = output.platform_output.accesskit_update.unwrap();
        assert!(update.nodes.iter().any(|(id, _)| *id == update.focus));
        for _ in 0..3 {
            let output = frame(&ctx, &bootstrap, &mut search, &mut selected, None, vec![]);
            let update = output.platform_output.accesskit_update.unwrap();
            assert!(update.nodes.iter().any(|(id, _)| *id == update.focus));
        }
    }

    #[test]
    fn filtered_checkbox_input_enforces_limit_and_keeps_hidden_selections() {
        let bootstrap = bootstrap();
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut search = "  FRIEND10  ".to_owned();
        let mut selected: Vec<_> = (1..=9).map(|index| format!("id{index}")).collect();
        frame(&ctx, &bootstrap, &mut search, &mut selected, None, vec![]);
        let output = frame(&ctx, &bootstrap, &mut search, &mut selected, None, vec![]);
        let events = click_checkbox(output, "friend10", false);
        frame(&ctx, &bootstrap, &mut search, &mut selected, None, events);
        assert_eq!(selected.len(), 9);
        assert!(!selected.contains(&"id10".into()));
        search = "friend1".into();
        frame(&ctx, &bootstrap, &mut search, &mut selected, None, vec![]);
        let output = frame(&ctx, &bootstrap, &mut search, &mut selected, None, vec![]);
        let events = click_checkbox(output, "friend1", true);
        frame(&ctx, &bootstrap, &mut search, &mut selected, None, events);
        assert_eq!(selected.len(), 8);
        assert!(!selected.contains(&"id1".into()));
        search = "friend10".into();
        frame(&ctx, &bootstrap, &mut search, &mut selected, None, vec![]);
        let output = frame(&ctx, &bootstrap, &mut search, &mut selected, None, vec![]);
        let events = click_checkbox(output, "friend10", true);
        frame(&ctx, &bootstrap, &mut search, &mut selected, None, events);
        assert_eq!(selected.len(), 9);
        assert!(selected.contains(&"id10".into()));
        assert!((2..=9).all(|index| selected.contains(&format!("id{index}"))));
    }

    #[test]
    fn editing_cannot_remove_the_owner_or_add_an_eleventh_member() {
        let bootstrap = bootstrap();
        let group = ConversationInfo {
            owner_id: "id0".into(),
            member_ids: (0..=9).map(|index| format!("id{index}")).collect(),
            ..Default::default()
        };
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut selected = group.member_ids.clone();
        let mut search = "friend0".to_owned();
        frame(
            &ctx,
            &bootstrap,
            &mut search,
            &mut selected,
            Some(&group),
            vec![],
        );
        let output = frame(
            &ctx,
            &bootstrap,
            &mut search,
            &mut selected,
            Some(&group),
            vec![],
        );
        let events = click_checkbox(output, "friend0", false);
        frame(
            &ctx,
            &bootstrap,
            &mut search,
            &mut selected,
            Some(&group),
            events,
        );
        assert_eq!(selected, group.member_ids);
        search = "friend10".into();
        let output = frame(
            &ctx,
            &bootstrap,
            &mut search,
            &mut selected,
            Some(&group),
            vec![],
        );
        let events = click_checkbox(output, "friend10", false);
        frame(
            &ctx,
            &bootstrap,
            &mut search,
            &mut selected,
            Some(&group),
            events,
        );
        assert_eq!(selected, group.member_ids);
    }

    #[test]
    fn group_search_clear_button_stays_inside_dialog_at_large_text() {
        for (width, height, font_scale) in [
            (1320.0, 850.0, 1.0),
            (940.0, 650.0, 1.0),
            (940.0 / 1.4, 650.0 / 1.4, 1.4),
        ] {
            let mut bootstrap = bootstrap();
            for member in &mut bootstrap.members {
                member.profile.display_name = "W".repeat(32);
            }
            let ctx = egui::Context::default();
            configure(&ctx);
            ctx.enable_accesskit();
            let mut style = (*ctx.style()).clone();
            for font in style.text_styles.values_mut() {
                font.size *= font_scale;
            }
            ctx.set_style(style);
            let mut search = "friend".to_owned();
            let mut selected = vec![];
            let mut name = "Friday group".to_owned();
            let mut previous_width = None;
            for index in 0..12 {
                let mut body_right = 0.0;
                let output = ctx.run(
                    egui::RawInput {
                        screen_rect: Some(Rect::from_min_size(
                            Pos2::ZERO,
                            Vec2::new(width, height),
                        )),
                        ..Default::default()
                    },
                    |ctx| {
                        desktop_ui::dialog(ctx, "Create Conversation")
                            .id(egui::Id::new("group-layout"))
                            .default_width(420.0)
                            .show(ctx, |ui| {
                                body_right = ui.available_rect_before_wrap().right();
                                ui.label("New Group DM");
                                ui.add(
                                    egui::TextEdit::singleline(&mut name)
                                        .desired_width(f32::INFINITY),
                                );
                                group_members_picker(
                                    ui,
                                    &bootstrap,
                                    &mut search,
                                    &mut selected,
                                    None,
                                );
                                ui.label(
                                    "Each conversation uses its own end-to-end encrypted group.",
                                );
                                ui.horizontal(|ui| {
                                    ui.add(secondary("Cancel"));
                                    ui.add(primary("Create"));
                                });
                            });
                    },
                );
                let update = output.platform_output.accesskit_update.unwrap();
                let clear = update
                    .nodes
                    .iter()
                    .find(|(_, node)| {
                        node.role() == egui::accesskit::Role::Button && node.name() == Some("Clear")
                    })
                    .unwrap()
                    .1
                    .bounds()
                    .unwrap();
                assert!(
                    clear.x1 <= f64::from(body_right) + 0.5,
                    "Clear {clear:?} exceeds body right{body_right} at font{font_scale}"
                );
                for (_, node) in update
                    .nodes
                    .iter()
                    .filter(|(_, node)| node.role() == egui::accesskit::Role::CheckBox)
                {
                    assert!(
                        node.bounds().unwrap().x1 <= f64::from(body_right) + 0.5,
                        "A long recipient row exceeds the dialog body."
                    );
                }
                let window =
                    ctx.memory(|memory| memory.area_rect(egui::Id::new("group-layout")).unwrap());
                if index > 8 {
                    assert!(
                        (window.width() - previous_width.unwrap_or(window.width())).abs() < 0.5,
                        "Dialog keeps growing: {window:?}"
                    );
                }
                previous_width = Some(window.width());
            }
        }
    }
}

impl RelayApp {
    pub(super) fn conversation_info(&self, channel: &str) -> Option<&ConversationInfo> {
        self.bootstrap
            .as_ref()?
            .conversations
            .iter()
            .find(|c| c.channel_id == channel)
    }
    pub(super) fn community_channel(&self, channel: &Channel) -> bool {
        channel.voice_parent_id.is_none()
            && self
                .conversation_info(&channel.id)
                .is_none_or(|c| !matches!(c.kind.as_str(), "group" | "group_voice" | "thread"))
    }
    pub(super) fn open_conversation_create(&mut self, kind: &str, parent: Option<String>) {
        self.admin_ui.conversation_create = true;
        self.admin_ui.conversation_kind = kind.into();
        self.admin_ui.conversation_name.clear();
        self.admin_ui.conversation_members.clear();
        self.admin_ui.conversation_member_search.clear();
        self.admin_ui.conversation_parent = parent;
    }
    pub(super) fn open_friend_voice_room(&mut self, peer: &str) {
        let Some(bootstrap) = self.bootstrap.clone() else {
            return;
        };
        if !bootstrap
            .capabilities
            .iter()
            .any(|cap| cap == "conversations-v1")
        {
            self.notice = Some((
                "Update this server to open private voice rooms.".into(),
                Instant::now(),
            ));
            return;
        }
        if bootstrap.social.blocked.iter().any(|id| id == peer)
            || !bootstrap.social.friends.iter().any(|id| id == peer)
        {
            self.notice = Some((
                "Accept the friend request before opening a private voice room.".into(),
                Instant::now(),
            ));
            return;
        }
        let Some(member) = bootstrap
            .members
            .iter()
            .find(|member| member.id == peer && member.id != bootstrap.user.id)
        else {
            return;
        };
        self.profile_ui.card = None;
        if let Some(room) = private_friend_voice_room(&bootstrap, peer) {
            // Opening a room never starts physical capture or leaves another call.
            self.select(Target::Channel(room));
        } else {
            let name = private_voice_room_name(member);
            self.open_conversation_create("group", None);
            self.admin_ui.conversation_name = name;
            self.admin_ui.conversation_members = vec![peer.into()];
        }
    }
    pub(super) fn conversation_dialogs(&mut self, ctx: &egui::Context) {
        let Some(b) = self.bootstrap.clone() else {
            return;
        };
        if self.admin_ui.conversation_create {
            let mut open = true;
            let mut create = false;
            let mut cancel = false;
            let window = desktop_ui::dialog(ctx, "Create Conversation")
                .id(egui::Id::new("create-conversation"))
                .open(&mut open)
                .default_width(420.0)
                .collapsible(false);
            // The shared header (title, Close, Escape) stays above the scrolling body.
            if host_setup_ui::show_with_header(ctx, window, "Create Conversation", |ui| {
                ui.label(match self.admin_ui.conversation_kind.as_str() {
                    "group" => "New private group",
                    "thread" => "New Thread / Forum Post",
                    "forum" => "New Forum Channel",
                    "announcement" => "New Announcement Channel",
                    "stage" => "New Stage Channel",
                    _ => "New Conversation",
                });
                ui.add(
                    egui::TextEdit::singleline(&mut self.admin_ui.conversation_name)
                        .char_limit(64)
                        .hint_text("Name")
                        .desired_width(f32::INFINITY),
                );
                if self.admin_ui.conversation_kind == "group" {
                    group_members_picker(
                        ui,
                        &b,
                        &mut self.admin_ui.conversation_member_search,
                        &mut self.admin_ui.conversation_members,
                        None,
                    );
                }
                ui.label(RichText::new(if b.protection.level == relay_protocol::ProtectionLevel::Open {
                    "End-to-end encryption is off for this server. Members accept its choice before joining calls."
                } else {
                    "Messages and calls use this conversation's own end-to-end encrypted group."
                }).color(c_muted()));
                let valid = !self.admin_ui.conversation_name.trim().is_empty()
                    && (self.admin_ui.conversation_kind != "group"
                        || (1..=9).contains(&self.admin_ui.conversation_members.len()));
                let pending = self
                    .admin_requests
                    .values()
                    .any(|v| v == "action:conversation:create");
                ui.horizontal(|ui| {
                    cancel = ui.add_enabled(!pending, secondary("Cancel")).clicked();
                    create = ui
                        .add_enabled(valid && !pending, primary("Create"))
                        .clicked();
                });
            }) {
                open = false;
                // Closing never creates the conversation.
                create = false;
            }
            self.admin_ui.conversation_create = open && !cancel;
            if create {
                self.admin_request(AdminMethod::Post,"/api/conversations".into(),json!({"name":self.admin_ui.conversation_name,"kind":self.admin_ui.conversation_kind,"parent_id":self.admin_ui.conversation_parent,"member_ids":self.admin_ui.conversation_members}),"action:conversation:create".into());
            }
        }
        if let Some(channel) = self.admin_ui.conversation_edit.clone() {
            let Some(info) = b
                .conversations
                .iter()
                .find(|c| c.channel_id == channel)
                .cloned()
            else {
                self.admin_ui.conversation_edit = None;
                return;
            };
            let mut open = true;
            let window = desktop_ui::dialog(ctx, "Conversation Settings")
                .id(egui::Id::new("conversation-settings"))
                .open(&mut open)
                .default_width(420.0)
                .collapsible(false);
            // The shared header (title, Close, Escape) stays above the scrolling body.
            if host_setup_ui::show_with_header(ctx, window, "Conversation Settings", |ui| {
                let can_edit = info.owner_id == b.user.id
                    || (info.kind != "group" && b.has_permission(PERMISSION_CHANNELS));
                ui.add_enabled_ui(can_edit, |ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.admin_ui.conversation_edit_name)
                            .char_limit(64)
                            .desired_width(f32::INFINITY),
                    );
                    if info.kind == "group" {
                        group_members_picker(
                            ui,
                            &b,
                            &mut self.admin_ui.conversation_member_search,
                            &mut self.admin_ui.conversation_edit_members,
                            Some(&info),
                        );
                    }
                    if info.kind == "thread"
                        && ui
                            .button(if info.archived {
                                "Unarchive Thread"
                            } else {
                                "Archive Thread"
                            })
                            .clicked()
                    {
                        self.admin_request(
                            AdminMethod::Patch,
                            format!("/api/conversations/{channel}"),
                            json!({"archived":!info.archived}),
                            "action:conversation:edit".into(),
                        );
                    }
                    let valid = !self.admin_ui.conversation_edit_name.trim().is_empty()
                        && (info.kind != "group"
                            || (2..=10).contains(&self.admin_ui.conversation_edit_members.len()));
                    ui.horizontal(|ui| {
                        if ui.add_enabled(valid, primary("Save Changes")).clicked() {
                            let mut body = json!({"name":self.admin_ui.conversation_edit_name});
                            if info.kind == "group" {
                                body["member_ids"] = json!(self.admin_ui.conversation_edit_members);
                            }
                            self.admin_request(
                                AdminMethod::Patch,
                                format!("/api/conversations/{channel}"),
                                body,
                                "action:conversation:edit".into(),
                            );
                        }
                        if ui.button("Reset").clicked() {
                            self.admin_ui.conversation_edit_name = b
                                .channels
                                .iter()
                                .find(|c| c.id == channel)
                                .map(|c| c.name.clone())
                                .unwrap_or_default();
                            self.admin_ui.conversation_edit_members = info.member_ids.clone();
                        }
                    });
                });
                if info.kind == "group" && ui.button("Leave Group").clicked() {
                    self.admin_request(
                        AdminMethod::Delete,
                        format!("/api/conversations/{channel}"),
                        Value::Null,
                        "action:conversation:edit".into(),
                    );
                }
            }) {
                open = false;
            }
            if !open {
                self.admin_ui.conversation_edit = None;
            }
        }
    }
    pub(super) fn open_conversation_settings(&mut self, channel: &str) {
        let Some(b) = self.bootstrap.as_ref() else {
            return;
        };
        let Some(info) = b.conversations.iter().find(|c| c.channel_id == channel) else {
            return;
        };
        self.admin_ui.conversation_member_search.clear();
        self.admin_ui.conversation_edit = Some(channel.into());
        self.admin_ui.conversation_edit_name = b
            .channels
            .iter()
            .find(|c| c.id == channel)
            .map(|c| c.name.clone())
            .unwrap_or_default();
        self.admin_ui.conversation_edit_members = info.member_ids.clone();
    }
    pub(super) fn conversation_toolbar(&mut self, ui: &mut egui::Ui) {
        let Some(Target::Channel(channel)) = self.current.clone() else {
            return;
        };
        let Some(b) = self.bootstrap.clone() else {
            return;
        };
        let info = self.conversation_info(&channel).cloned();
        ui.horizontal_wrapped(|ui| {
            if let Some(info) = &info {
                if info.kind == "group" {
                    ui.label(format!("{} members", info.member_ids.len()));
                    if let Some(call) = b.conversations.iter().find(|c| {
                        c.kind == "group_voice" && c.parent_id.as_deref() == Some(&channel)
                    }) {
                        if ui.button("Join private voice").clicked() {
                            self.select(Target::Channel(call.channel_id.clone()));
                            self.join_voice(call.channel_id.clone());
                        }
                    }
                }
                if matches!(info.kind.as_str(), "group" | "thread")
                    && ui.button("Conversation Settings").clicked()
                {
                    self.open_conversation_settings(&channel);
                }
                if info.kind == "thread" {
                    if info.archived {
                        ui.label("Archived · Read Only");
                    }
                    if let Some(parent) = &info.parent_id {
                        if ui.button("Back to Channel").clicked() {
                            self.select(Target::Channel(parent.clone()));
                        }
                    }
                }
            }
        });
    }
    pub(super) fn threads_navigation(&mut self, ui: &mut egui::Ui) {
        let Some(b) = self.bootstrap.clone() else {
            return;
        };
        let threads: Vec<_> = b
            .conversations
            .iter()
            .filter(|c| c.kind == "thread")
            .collect();
        if threads.is_empty() {
            return;
        }
        ui.collapsing("Threads & Forum Posts", |ui| {
            for thread in threads {
                if let Some(channel) = b.channels.iter().find(|c| c.id == thread.channel_id) {
                    if ui
                        .selectable_label(
                            self.current == Some(Target::Channel(channel.id.clone())),
                            format!(
                                "{}{}",
                                channel.name,
                                if thread.archived { " · Archived" } else { "" }
                            ),
                        )
                        .clicked()
                    {
                        self.select(Target::Channel(channel.id.clone()));
                    }
                }
            }
        });
    }
    pub(super) fn groups_navigation(&mut self, ui: &mut egui::Ui) {
        let Some(b) = self.bootstrap.clone() else {
            return;
        };
        for (label, page) in [
            ("Friends", SettingsTab::Friends),
            ("Inbox", SettingsTab::Mentions),
        ] {
            if crate::settings_ui::nav_item(
                ui,
                ("home-nav", page as u8),
                label,
                self.home_page == Some(page),
            )
            .clicked()
            {
                self.home_page = Some(page);
            }
        }
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("DIRECT MESSAGES")
                    .size(font_size(TEXT_CAPTION))
                    .strong()
                    .color(c_muted()),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if !self.in_people()
                    && icon_button(ui, Icon::Plus, false, "New group message").clicked()
                {
                    self.open_conversation_create("group", None);
                }
            });
        });
        let people = self.in_people();
        for group in b
            .conversations
            .iter()
            .filter(|c| c.kind == "group" && !people)
        {
            if let Some(channel) = b.channels.iter().find(|c| c.id == group.channel_id) {
                let clicked = ui
                    .horizontal(|ui| {
                        ui.add_space(6.0);
                        icon(ui, Icon::Users, 28.0, c_muted());
                        ui.selectable_label(
                            self.current == Some(Target::Channel(channel.id.clone())),
                            RichText::new(format!(
                                "{}  ·  {} members",
                                channel.name,
                                group.member_ids.len()
                            ))
                            .size(font_size(14.0)),
                        )
                        .clicked()
                    })
                    .inner;
                if clicked {
                    self.select(Target::Channel(channel.id.clone()));
                }
            }
        }
        ui.separator();
    }
    pub(super) fn forum_view(&mut self, ui: &mut egui::Ui, channel: &str) {
        let Some(b) = self.bootstrap.clone() else {
            return;
        };
        crate::settings_ui::section_heading(ui, "Posts", "Open a discussion or create a new post.");
        egui::ScrollArea::vertical()
            .id_source("forum-posts")
            .show(ui, |ui| {
                for thread in b
                    .conversations
                    .iter()
                    .filter(|c| c.kind == "thread" && c.parent_id.as_deref() == Some(channel))
                {
                    if let Some(child) = b.channels.iter().find(|c| c.id == thread.channel_id) {
                        egui::Frame::none()
                            .fill(c_card())
                            .rounding(8.0)
                            .inner_margin(Margin::same(16.0))
                            .show(ui, |ui| {
                                ui.label(RichText::new(&child.name).strong());
                                if thread.archived {
                                    ui.label("Archived");
                                }
                                if ui.button("Open Discussion").clicked() {
                                    self.select(Target::Channel(child.id.clone()));
                                }
                            });
                        ui.add_space(8.0);
                    }
                }
            });
    }
    pub(super) fn stage_controls(&mut self, ui: &mut egui::Ui, channel: &str) {
        let Some(b) = self.bootstrap.clone() else {
            return;
        };
        if self
            .conversation_info(channel)
            .is_none_or(|c| c.kind != "stage")
        {
            return;
        }
        let own = b
            .stage_members
            .iter()
            .find(|s| s.channel_id == channel && s.user_id == b.user.id);
        let speaker = b.has_permission(PERMISSION_MEMBERS) || own.is_some_and(|s| s.speaker);
        ui.label(if speaker {
            "Stage · Speaker"
        } else {
            "Stage · Audience"
        });
        if !speaker
            && ui
                .button(if own.is_some_and(|s| s.raised) {
                    "Lower Hand"
                } else {
                    "Request to Speak"
                })
                .clicked()
        {
            self.admin_request(
                AdminMethod::Put,
                format!("/api/stages/{channel}"),
                json!({"user_id":b.user.id,"raised":!own.is_some_and(|s|s.raised)}),
                "action:stage".into(),
            );
        }
        if b.has_permission(PERMISSION_MEMBERS) {
            ui.collapsing("Manage Stage Speakers",|ui|{for member in b.voice_members.iter().filter(|v|v.channel_id==channel&&v.user.id!=b.user.id){let state=b.stage_members.iter().find(|s|s.channel_id==channel&&s.user_id==member.user.id);ui.horizontal(|ui|{ui.label(format!("{}{}",profile_ui::display_name(&member.user),if state.is_some_and(|s|s.raised){" · Hand raised"}else{""}));if ui.button(if state.is_some_and(|s|s.speaker){"Move to Audience"}else{"Invite to Speak"}).clicked(){self.admin_request(AdminMethod::Put,format!("/api/stages/{channel}"),json!({"user_id":member.user.id,"speaker":!state.is_some_and(|s|s.speaker)}),"action:stage".into());}});}});
        }
    }
    pub(super) fn stage_listener(&self) -> bool {
        let Some(b) = self.bootstrap.as_ref() else {
            return false;
        };
        let Some(channel) = &self.voice_channel else {
            return false;
        };
        self.conversation_info(channel)
            .is_some_and(|c| c.kind == "stage")
            && !b.has_permission(PERMISSION_MEMBERS)
            && !b
                .stage_members
                .iter()
                .any(|s| s.channel_id == *channel && s.user_id == b.user.id && s.speaker)
    }
    pub(super) fn start_server_event(&mut self) {
        if let Some(channel) = self
            .bootstrap
            .as_ref()
            .filter(|b| b.has_permission(PERMISSION_CHANNELS))
            .and_then(|b| b.channels.iter().find(|c| self.community_channel(c)))
        {
            self.admin_ui.event_draft = Some(ScheduledEvent {
                name: String::new(),
                channel_id: channel.id.clone(),
                starts_at: unix_time() + 3600,
                ends_at: unix_time() + 7200,
                ..Default::default()
            });
            self.admin_ui.event_start.clear();
            self.admin_ui.event_end.clear();
        }
    }
    pub(super) fn events_page(&mut self, ui: &mut egui::Ui) {
        crate::settings_ui::page_heading(ui, "Events", "Upcoming events in this community.");
        let Some(b) = self.bootstrap.clone() else {
            return;
        };
        if b.has_permission(PERMISSION_CHANNELS) && ui.button("Create Event").clicked() {
            self.start_server_event();
        }
        for event in &b.scheduled_events {
            ui.push_id(&event.id, |ui| {
                egui::Frame::none()
                    .fill(c_card())
                    .rounding(8.0)
                    .inner_margin(Margin::same(16.0))
                    .show(ui, |ui| {
                        ui.label(RichText::new(&event.name).size(font_size(18.0)).strong());
                        ui.label(&event.description);
                        ui.label(
                            chrono::DateTime::from_timestamp(event.starts_at as i64, 0)
                                .map(|d| {
                                    d.with_timezone(&chrono::Local)
                                        .format("%d %b %Y · %H:%M")
                                        .to_string()
                                })
                                .unwrap_or_default(),
                        );
                        ui.label(format!("{} interested", event.interested.len()));
                        ui.horizontal_wrapped(|ui| {
                            if ui
                                .button(if event.interested.contains(&b.user.id) {
                                    "Not Interested"
                                } else {
                                    "Interested"
                                })
                                .clicked()
                            {
                                self.admin_request(
                                    AdminMethod::Post,
                                    format!("/api/scheduled-events/{}/interest", event.id),
                                    Value::Null,
                                    "action:event:interest".into(),
                                );
                            }
                            if ui.button("Open Channel").clicked() {
                                self.select(Target::Channel(event.channel_id.clone()));
                                self.settings_open = false;
                            }
                            if b.has_permission(PERMISSION_CHANNELS) {
                                if ui.button("Edit").clicked() {
                                    self.admin_ui.event_draft = Some(event.clone());
                                    self.admin_ui.event_start.clear();
                                    self.admin_ui.event_end.clear();
                                }
                                if ui.button("Cancel Event").clicked() {
                                    self.admin_request(
                                        AdminMethod::Delete,
                                        format!("/api/scheduled-events/{}", event.id),
                                        Value::Null,
                                        "action:event".into(),
                                    );
                                }
                            }
                        });
                    });
                ui.add_space(8.0);
            });
        }
        if let Some(draft) = self.admin_ui.event_draft.as_mut() {
            ui.separator();
            ui.label("Event Details");
            ui.add(
                egui::TextEdit::singleline(&mut draft.name)
                    .char_limit(100)
                    .hint_text("Event name")
                    .desired_width(f32::INFINITY),
            );
            ui.add(
                egui::TextEdit::multiline(&mut draft.description)
                    .char_limit(1000)
                    .hint_text("Description")
                    .desired_width(f32::INFINITY),
            );
            egui::ComboBox::from_id_source("event-channel")
                .selected_text(
                    b.channels
                        .iter()
                        .find(|c| c.id == draft.channel_id)
                        .map(|c| c.name.as_str())
                        .unwrap_or("Choose channel"),
                )
                .show_ui(ui, |ui| {
                    for c in b.channels.iter().filter(|c| {
                        c.voice_parent_id.is_none()
                            && b.conversations
                                .iter()
                                .find(|i| i.channel_id == c.id)
                                .is_none_or(|i| {
                                    !matches!(i.kind.as_str(), "group" | "group_voice" | "thread")
                                })
                    }) {
                        ui.selectable_value(&mut draft.channel_id, c.id.clone(), &c.name);
                    }
                });
            let mut dates_valid = true;
            for (label, time, date) in [
                (
                    "Start",
                    &mut draft.starts_at,
                    &mut self.admin_ui.event_start,
                ),
                ("End", &mut draft.ends_at, &mut self.admin_ui.event_end),
            ] {
                if date.is_empty() {
                    *date = chrono::DateTime::from_timestamp(*time as i64, 0)
                        .map(|d| {
                            d.with_timezone(&chrono::Local)
                                .format("%Y-%m-%d %H:%M")
                                .to_string()
                        })
                        .unwrap_or_default();
                }
                ui.horizontal(|ui| {
                    ui.label(label);
                    ui.text_edit_singleline(date);
                });
                use chrono::TimeZone;
                if let Some(at) = chrono::NaiveDateTime::parse_from_str(date, "%Y-%m-%d %H:%M")
                    .ok()
                    .and_then(|local| chrono::Local.from_local_datetime(&local).single())
                {
                    *time = at.timestamp().max(0) as u64;
                } else {
                    dates_valid = false;
                }
            }
            ui.label(
                RichText::new(
                    "Event details are visible to members who can access the selected channel.",
                )
                .color(c_muted()),
            );
            let submitted = draft.clone();
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(
                        !submitted.name.trim().is_empty()
                            && dates_valid
                            && submitted.starts_at > unix_time()
                            && submitted.ends_at > submitted.starts_at,
                        primary("Save Event"),
                    )
                    .clicked()
                {
                    self.admin_request(
                        AdminMethod::Post,
                        "/api/scheduled-events".into(),
                        serde_json::to_value(submitted).unwrap(),
                        "action:event".into(),
                    );
                }
                if ui.button("Discard").clicked() {
                    self.admin_ui.event_draft = None;
                }
            });
        }
    }
}
