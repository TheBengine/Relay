#[derive(Clone)]
struct ChannelDrag {
    scope: String,
    id: String,
    kind: String,
}
#[derive(Clone)]
struct CategoryDrag {
    scope: String,
    id: String,
}

// A voice room's messages belong to its linked text channel, not to the room.
// Only use channels present in the current account's accessible bootstrap.
fn channel_chat_target(channel: &Channel, channels: &[Channel]) -> Option<Target> {
    let chat = channels.iter().find(|candidate| {
        candidate.kind == "text"
            && if channel.kind == "voice" {
                candidate.voice_parent_id.as_ref() == Some(&channel.id)
            } else {
                channel.kind == "text" && candidate.id == channel.id
            }
    })?;
    Some(Target::Channel(chat.id.clone()))
}

impl RelayApp {
    pub(super) fn channel_interactions(
        &mut self,
        ui: &mut egui::Ui,
        channel: &Channel,
        response: &egui::Response,
    ) {
        let Some(b) = self.bootstrap.clone() else {
            return;
        };
        let manage = b.has_permission(PERMISSION_CHANNELS);
        let Some(scope) = self.conversation_scope() else {
            return;
        };
        let chat_target = channel_chat_target(channel, &b.channels);
        response.context_menu(|ui| {
            if ui.button("Open Channel").clicked() {
                self.select(Target::Channel(channel.id.clone()));
                ui.close_menu();
            }
            if channel.kind == "voice" {
                if ui
                    .add_enabled(channel.can_join, egui::Button::new("Join Voice"))
                    .clicked()
                {
                    self.join_voice(channel.id.clone());
                    ui.close_menu();
                }
                if let Some(chat) = b
                    .channels
                    .iter()
                    .find(|c| c.voice_parent_id.as_ref() == Some(&channel.id))
                {
                    if ui.button("Open Text Chat").clicked() {
                        self.select(Target::Channel(chat.id.clone()));
                        ui.close_menu();
                    }
                }
            }
            if let Some(target) = chat_target.as_ref() {
                if ui.button("Mark as Read").clicked() {
                    // The server bounds this explicit request to the latest message.
                    // Its refreshed read state supplies the actual sequence; never
                    // put this sentinel into the client's automatic-read cache.
                    if let Some(origin) = self.command_origin() {
                        self.send(Command::MarkRead {
                            origin,
                            target: target.clone(),
                            last_read_sequence: u64::MAX,
                        });
                    }
                    ui.close_menu();
                }
                ui.menu_button("Notifications", |ui| {
                    let key = format!("{scope}|{target:?}");
                    let before = self
                        .prefs
                        .notification_policies
                        .get(&key)
                        .cloned()
                        .unwrap_or_default();
                    for (mode, label) in [
                        ("", "Use Community Default"),
                        ("all", "All Messages"),
                        ("mentions", "Only Mentions"),
                        ("none", "Nothing"),
                    ] {
                        if ui.selectable_label(before.mode == mode, label).clicked() {
                            self.prefs
                                .notification_policies
                                .entry(key.clone())
                                .or_default()
                                .mode = mode.into();
                            self.defer_preferences();
                            ui.close_menu();
                        }
                    }
                    if before.muted_until > unix_time() {
                        if ui.button("Unmute Channel").clicked() {
                            self.prefs
                                .notification_policies
                                .entry(key.clone())
                                .or_default()
                                .muted_until = 0;
                            self.defer_preferences();
                            ui.close_menu();
                        }
                    } else {
                        ui.menu_button("Mute Channel", |ui| {
                            for (seconds, label) in [
                                (900, "15 minutes"),
                                (3600, "1 hour"),
                                (10800, "3 hours"),
                                (28800, "8 hours"),
                                (86400, "24 hours"),
                                (u64::MAX, "Until I turn it back on"),
                            ] {
                                if ui.button(label).clicked() {
                                    self.prefs
                                        .notification_policies
                                        .entry(key.clone())
                                        .or_default()
                                        .muted_until = unix_time().saturating_add(seconds);
                                    self.defer_preferences();
                                    ui.close_menu();
                                }
                            }
                        });
                    }
                });
            }
            if channel.kind == "text" && channel.can_send && ui.button("Create Thread").clicked() {
                self.open_conversation_create("thread", Some(channel.id.clone()));
                ui.close_menu();
            }
            if manage {
                if ui.button("Edit Channel / Permissions").clicked() {
                    self.open_channel_settings(channel.clone());
                    ui.close_menu();
                }
                ui.menu_button("Move to Category", |ui| {
                    if ui.button("No Category").clicked() {
                        self.admin_request(
                            AdminMethod::Put,
                            format!("/api/channels/{}/move", channel.id),
                            json!({"category_id":null}),
                            "action:reorder".into(),
                        );
                        ui.close_menu();
                    }
                    for category in self
                        .admin_data
                        .get("categories")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default()
                    {
                        if let (Some(id), Some(name)) =
                            (category["id"].as_str(), category["name"].as_str())
                        {
                            if ui.button(name).clicked() {
                                self.admin_request(
                                    AdminMethod::Put,
                                    format!("/api/channels/{}/move", channel.id),
                                    json!({"category_id":id}),
                                    "action:reorder".into(),
                                );
                                ui.close_menu();
                            }
                        }
                    }
                });
            }
            if ui.button("Copy Channel ID").clicked() {
                ui.output_mut(|o| o.copied_text = channel.id.clone());
                ui.close_menu();
            }
        });
        if manage {
            response.dnd_set_drag_payload(ChannelDrag {
                scope: scope.clone(),
                id: channel.id.clone(),
                kind: channel.kind.clone(),
            });
            if let Some(payload) = response.dnd_hover_payload::<ChannelDrag>() {
                if payload.scope == scope
                    && payload.id != channel.id
                    && payload.kind == channel.kind
                {
                    ui.painter().line_segment(
                        [response.rect.left_top(), response.rect.right_top()],
                        Stroke::new(2.0_f32, c_accent()),
                    );
                }
            }
            if let Some(payload) = response.dnd_release_payload::<ChannelDrag>() {
                if payload.scope == scope
                    && payload.id != channel.id
                    && payload.kind == channel.kind
                {
                    self.admin_request(
                        AdminMethod::Put,
                        format!("/api/channels/{}/move", payload.id),
                        json!({"category_id":channel.category_id,"before_id":channel.id}),
                        "action:reorder".into(),
                    );
                }
            }
        }
    }
    pub(super) fn category_interactions(&mut self, ui: &mut egui::Ui, id: &str, name: &str) {
        let manage = self
            .bootstrap
            .as_ref()
            .is_some_and(|b| b.has_permission(PERMISSION_CHANNELS));
        let Some(scope) = self.conversation_scope() else {
            return;
        };
        let response = ui.add(
            egui::Label::new(
                RichText::new(name.to_uppercase())
                    .size(font_size(TEXT_CAPTION))
                    .color(c_muted()),
            )
            .sense(if manage {
                Sense::click_and_drag()
            } else {
                Sense::hover()
            }),
        );
        if manage {
            response.dnd_set_drag_payload(CategoryDrag {
                scope: scope.clone(),
                id: id.into(),
            });
            if response.dnd_hover_payload::<CategoryDrag>().is_some() {
                if let Some(payload) = response.dnd_release_payload::<CategoryDrag>() {
                    if payload.scope == scope && payload.id != id {
                        self.admin_request(
                            AdminMethod::Put,
                            format!("/api/categories/{}/move", payload.id),
                            json!({"before_id":id}),
                            "action:reorder".into(),
                        );
                    }
                }
            }
            if response.dnd_hover_payload::<ChannelDrag>().is_some() {
                if let Some(payload) = response.dnd_release_payload::<ChannelDrag>() {
                    if payload.scope == scope {
                        self.admin_request(
                            AdminMethod::Put,
                            format!("/api/channels/{}/move", payload.id),
                            json!({"category_id":id}),
                            "action:reorder".into(),
                        );
                    }
                }
            }
            response.context_menu(|ui| {
                if ui.button("Move Category to Bottom").clicked() {
                    self.admin_request(
                        AdminMethod::Put,
                        format!("/api/categories/{id}/move"),
                        json!({}),
                        "action:reorder".into(),
                    );
                    ui.close_menu();
                }
                if ui.button("Edit Category").clicked() {
                    self.admin_ui.section = Section::Channels;
                    self.settings_tab = SettingsTab::Server;
                    self.settings_open = true;
                    ui.close_menu();
                }
            });
        }
    }
    pub(super) fn user_interactions(
        &mut self,
        _ui: &mut egui::Ui,
        user: &User,
        response: &egui::Response,
    ) {
        response.context_menu(|ui| {
            if ui.button("View Profile").clicked() {
                self.open_profile(user.clone());
                ui.close_menu();
            }
            if self.session.as_ref().is_some_and(|s| s.user.id != user.id) {
                let can_message = !self.is_blocked(&user.id) && self.can_message(&user.id);
                if ui
                    .add_enabled(can_message, egui::Button::new("Message"))
                    .on_disabled_hover_text("You can message friends you have not blocked.")
                    .clicked()
                {
                    self.select(Target::Direct(user.id.clone()));
                    ui.close_menu();
                }
                self.social_buttons(ui, user);
            }
            if self
                .bootstrap
                .as_ref()
                .is_some_and(|b| b.has_permission(PERMISSION_MEMBERS))
                && ui.button("Manage Member").clicked()
            {
                self.admin_ui.section = Section::Members;
                self.settings_tab = SettingsTab::Server;
                self.settings_open = true;
                ui.close_menu();
            }
        });
    }
    pub(super) fn stream_context_menu(
        &mut self,
        _ui: &mut egui::Ui,
        key: &str,
        response: &egui::Response,
    ) {
        response.context_menu(|ui| {
            if ui.button("View Larger").clicked() {
                self.stream_preview = Some(key.into());
                ui.close_menu();
            }
            if ui.button("Full Screen").clicked() {
                self.stream_restore_fullscreen =
                    ui.ctx().input(|i| i.viewport().fullscreen.unwrap_or(false));
                self.stream_preview = Some(key.into());
                self.stream_fullscreen = true;
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::Fullscreen(true));
                ui.close_menu();
            }
            if ui
                .button(if self.focused_stream.as_deref() == Some(key) {
                    "Back to Stream Gallery"
                } else {
                    "Focus Stream"
                })
                .clicked()
            {
                self.focused_stream = if self.focused_stream.as_deref() == Some(key) {
                    None
                } else {
                    Some(key.into())
                };
                ui.close_menu();
            }
        });
    }
}

#[cfg(test)]
mod channel_context_tests {
    use super::*;

    fn channel(id: &str, kind: &str, voice_parent: Option<&str>) -> Channel {
        Channel {
            id: id.into(),
            kind: kind.into(),
            voice_parent_id: voice_parent.map(str::to_owned),
            ..Default::default()
        }
    }

    #[test]
    fn voice_read_and_notification_actions_use_the_linked_text_conversation() {
        let room = channel("voice", "voice", None);
        let channels = vec![
            channel("unrelated", "text", None),
            channel("other-room-chat", "text", Some("another-room")),
            room.clone(),
            channel("voice-chat", "text", Some("voice")),
        ];
        assert_eq!(
            channel_chat_target(&room, &channels),
            Some(Target::Channel("voice-chat".into()))
        );
        assert_eq!(
            channel_chat_target(&channels[0], &channels),
            Some(Target::Channel("unrelated".into()))
        );
    }

    #[test]
    fn inaccessible_or_non_text_channels_cannot_be_used_for_chat_actions() {
        let room = channel("voice", "voice", None);
        let invalid_child = channel("not-text", "voice", Some("voice"));
        let channels = vec![room.clone(), invalid_child];
        assert_eq!(channel_chat_target(&room, &channels), None);
        assert_eq!(
            channel_chat_target(&channel("revoked", "text", None), &channels),
            None
        );
        assert_eq!(
            channel_chat_target(&channel("category", "category", None), &channels),
            None
        );
    }
}
