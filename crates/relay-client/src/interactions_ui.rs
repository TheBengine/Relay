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
            ui.menu_button("Notifications", |ui| {
                let key = format!("{scope}|{:?}", Target::Channel(channel.id.clone()));
                for (mode, label) in [
                    ("", "Use Community Default"),
                    ("all", "All Messages"),
                    ("mentions", "Only Mentions"),
                    ("none", "Nothing"),
                ] {
                    if ui.button(label).clicked() {
                        self.prefs
                            .notification_policies
                            .entry(key.clone())
                            .or_default()
                            .mode = mode.into();
                        self.defer_preferences();
                        ui.close_menu();
                    }
                }
                let muted = self
                    .prefs
                    .notification_policies
                    .get(&key)
                    .is_some_and(|p| p.muted_until > unix_time());
                if ui
                    .button(if muted {
                        "Unmute Channel"
                    } else {
                        "Mute Channel"
                    })
                    .clicked()
                {
                    self.prefs
                        .notification_policies
                        .entry(key)
                        .or_default()
                        .muted_until = if muted { 0 } else { u64::MAX };
                    self.defer_preferences();
                    ui.close_menu();
                }
            });
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
                if ui
                    .add_enabled(!self.is_blocked(&user.id), egui::Button::new("Message"))
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
