// Included in admin_ui: actions still run through the selected community.
impl RelayApp {
    fn all_server_friends(&mut self, ui: &mut egui::Ui) {
        let mut owners: Vec<_> = self
            .communities
            .iter()
            .map(|community| CommandOrigin {
                url: community.url.clone(),
                server_id: community.server_id.clone(),
                user_id: community.user.id.clone(),
            })
            .collect();
        if let Some(origin) = self.command_origin() {
            if !owners.contains(&origin) {
                owners.push(origin);
            }
        }
        ui.label(
            RichText::new("Friends on your servers")
                .size(font_size(18.0))
                .strong(),
        );
        ui.label(
            RichText::new("Each friendship belongs to the server shown below.").color(c_muted()),
        );
        ui.add_space(12.0);
        ui.horizontal_wrapped(|ui| {
            for (tab, label) in [(0, "Online"), (1, "All"), (2, "Pending"), (3, "Blocked")] {
                if crate::settings_ui::top_tab(
                    ui,
                    ("all-server-friends", tab),
                    label,
                    self.admin_ui.friend_tab == tab,
                )
                .clicked()
                {
                    self.admin_ui.friend_tab = tab;
                }
            }
        });
        ui.add_sized(
            [ui.available_width(), 34.0],
            egui::TextEdit::singleline(&mut self.admin_ui.friend_search)
                .hint_text("Search friends or servers"),
        );
        let tab = self.admin_ui.friend_tab.min(3);
        let rows: Vec<_> = self
            .community_friends
            .rows(&owners, &self.admin_ui.friend_search, tab)
            .into_iter()
            .filter(|row| {
                tab != 0
                    || (row.online
                        && self.community_status(&row.origin.url) == ConnectionStatus::Connected)
            })
            .collect();
        ui.add_space(12.0);
        if rows.is_empty() {
            ui.label(if self.admin_ui.friend_search.trim().is_empty() {
                "No friends or requests in this view. Open a server to add someone there."
            } else {
                "No friends or servers match your search."
            });
        }
        if rows.len() > 100 {
            ui.label("Showing the first 100 matches. Search to narrow the list.");
        }
        for row in rows.into_iter().take(100) {
            let current = self.command_origin().as_ref() == Some(&row.origin);
            let connected = self.community_status(&row.origin.url) == ConnectionStatus::Connected;
            ui.push_id(
                (
                    &row.origin.url,
                    &row.origin.server_id,
                    &row.origin.user_id,
                    &row.user.id,
                ),
                |ui| {
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.label(RichText::new(profile_ui::display_name(&row.user)).strong());
                            ui.label(RichText::new(&row.server_name).color(c_muted()));
                            ui.label(
                                RichText::new(if !connected {
                                    "Server disconnected · last known list"
                                } else {
                                    match row.relationship {
                                        crate::community_friends::Relationship::Incoming => {
                                            "Incoming friend request"
                                        }
                                        crate::community_friends::Relationship::Outgoing => {
                                            "Outgoing friend request"
                                        }
                                        crate::community_friends::Relationship::Blocked => {
                                            "Blocked on this server"
                                        }
                                        crate::community_friends::Relationship::Friend => {
                                            if row.online {
                                                "Online"
                                            } else {
                                                "Offline"
                                            }
                                        }
                                    }
                                })
                                .color(c_muted()),
                            );
                        });
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if current
                                && row.relationship
                                    == crate::community_friends::Relationship::Friend
                            {
                                if ui.add_enabled(connected, secondary("Message")).clicked() {
                                    self.select(Target::Direct(row.user.id.clone()));
                                }
                                if ui.add_enabled(connected, secondary("Call")).clicked() {
                                    self.start_friend_call(&row.user.id);
                                }
                            } else if ui
                                .button("Open server")
                                .on_hover_text(
                                    "Open this server to message, call or manage this friendship.",
                                )
                                .clicked()
                            {
                                self.switch_community(row.origin.url.clone());
                            }
                        });
                    });
                    ui.add_space(8.0);
                    ui.separator();
                    ui.add_space(8.0);
                },
            );
        }
    }
}
