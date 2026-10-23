impl RelayApp {
    pub(super) fn discard_hidden_discord_token(&mut self) {
        if !self.settings_open
            || self.settings_tab != SettingsTab::Server
            || self.admin_ui.section != Section::Discord
            || !self
                .bootstrap
                .as_ref()
                .is_some_and(|b| b.has_permission(PERMISSION_ADMIN))
            || !self
                .command_origin()
                .is_some_and(|o| crate::local_host::is_local_url(&o.url))
        {
            use zeroize::Zeroize;
            self.admin_ui.discord_token.zeroize();
        }
    }

    fn discord_host_request(&mut self, action: crate::network::DiscordHostAction) {
        let Some(origin) = self.command_origin() else {
            return;
        };
        let key = if matches!(action, crate::network::DiscordHostAction::Status) {
            "discord-host"
        } else {
            "action:discord-host"
        };
        let request_id = self.next_request();
        if let Err(error) = self.network.send(Command::DiscordHost {
            origin,
            action,
            request_id,
        }) {
            self.error = Some(error);
        } else {
            self.admin_ui.discord_host_checked = true;
            self.admin_requests.insert(request_id, key.into());
        }
    }

    fn discord_host_ui(&mut self, ui: &mut egui::Ui, bootstrap: &Bootstrap) {
        use crate::{network::DiscordHostAction as Action, settings_ui as controls};
        controls::nav_group(ui, "BOT TOKEN · HOST COMPUTER ONLY");
        if !bootstrap
            .capabilities
            .iter()
            .any(|c| c == "discord-host-credentials-v1")
        {
            controls::description(
                ui,
                "Update the community host to save the bot token through Relay.",
            );
            return;
        }
        if !self
            .command_origin()
            .is_some_and(|o| crate::local_host::is_local_url(&o.url))
        {
            controls::description(ui, "Open Relay on the computer hosting this community and connect to its local address to configure the token.");
            return;
        }
        let pending = self
            .admin_requests
            .values()
            .any(|k| k == "discord-host" || k == "action:discord-host");
        if !self.admin_ui.discord_host_checked && !pending {
            self.discord_host_request(Action::Status);
        }
        let status = self.admin_data.get("discord-host");
        let ready = status.is_some_and(|s| s["supported"] == true);
        let text = match status {
            Some(s) if s["failed"] == true => {
                "Windows could not read this host's credential. Refresh or paste a replacement."
            }
            Some(s) => match s["source"].as_str() {
                Some("saved") => "Token saved on this Windows host",
                Some("environment") => "Token supplied by the host's environment",
                Some("removed") => "Token removed · old environment credentials remain disabled",
                _ => "No bot token saved",
            },
            None => "Checking local host access…",
        };
        controls::description(ui, text);
        controls::description(ui, "Paste your Discord bot token here. Windows protects it on this host; it is kept out of Relay backups and other clients. Saving does not enable sharing.");
        let mut action = None;
        ui.add_enabled_ui(ready && !pending, |ui| {
            discord_secret_input(ui, &mut self.admin_ui.discord_token);
            ui.horizontal_wrapped(|ui| {
                if ui
                    .add_enabled(
                        !self.admin_ui.discord_token.trim().is_empty(),
                        primary("Save token"),
                    )
                    .clicked()
                {
                    action = Some(Action::Save(std::mem::take(
                        &mut self.admin_ui.discord_token,
                    )));
                }
                if ui.add(secondary("Remove token")).clicked() {
                    use zeroize::Zeroize;
                    self.admin_ui.discord_token.zeroize();
                    action = Some(Action::Remove);
                }
                if ui.add(secondary("Reconnect Discord")).clicked() {
                    action = Some(Action::Retry);
                }
            });
        });
        if ui
            .add_enabled(!pending, secondary("Refresh host access"))
            .clicked()
        {
            action = Some(Action::Status);
        }
        if let Some(action) = action {
            self.discord_host_request(action);
        }
    }

    pub(super) fn refresh_admin_discord_status(&mut self) {
        self.admin_ui.discord = None;
        self.refresh_admin_data("discord", "/api/discord");
    }

    fn discord_channel_status_ui(&mut self, ui: &mut egui::Ui) {
        use crate::settings_ui as controls;
        controls::nav_group(ui, "DISCORD CHANNEL UPDATES");
        let value = self
            .admin_data
            .get("discord")
            .and_then(|v| v["channel_status"].as_str())
            .unwrap_or("off");
        let text=match value {
            "online"=>"One status message is active · updates when the call changes",
            "waiting_for_bot"=>"Waiting for the Discord connection",
            "uncertain"=>"Discord may have received the message. Relay has paused new posts to avoid duplicates. Recover it below.",
            "message_missing"=>"The status message or channel is missing. Turn updates off and remove the old reference before enabling again.",
            "cleanup_needed"=>"The old status may still be visible. Restore the original bot's access and remove it below.",
            "binding_changed"=>"The old status belongs to a different host, app or bot. It is kept untouched; use its original host to remove it.",
            "needs_permission"=>"Check the bot's View Channel, Send Messages and Read Message History permissions.",
            "wrong_channel"=>"Choose a text channel in the configured Discord server.",
            "connection_failed"=>"Discord did not answer · Relay will retry with a longer delay",
            "metadata_failed"=>"Relay could not read the saved status information. Contact the host.",
            _=>"Automatic channel updates are off",
        };
        controls::description(ui, text);
        let pending = self
            .admin_requests
            .values()
            .any(|k| k == "action:discord" || k == "action:discord-status");
        let mut action = None;
        ui.add_enabled_ui(!pending,|ui| {
            if value=="uncertain" {
                controls::description(ui,"In Discord, enable Developer Mode and right-click this bot's status message → Copy Message ID. Relay checks the author and original creation nonce; it never searches other messages.");
                ui.add(egui::TextEdit::singleline(&mut self.admin_ui.discord_message_id).char_limit(20).hint_text("Discord message ID"));
                if ui.add_enabled(relay_protocol::discord::snowflake(&self.admin_ui.discord_message_id).is_some(),secondary("Recover this status message")).clicked() {
                    action=Some(("/api/discord/status/recover",json!({"message_id":self.admin_ui.discord_message_id})));
                }
            }
            if ui.add(secondary("Turn off channel updates & remove status")).clicked() {
                action=Some(("/api/discord/status/cleanup",Value::Null));
            }
        });
        controls::description(ui,"Removal needs the original bot's access. If Discord is unavailable or creation was uncertain, Relay keeps a warning rather than claiming the message was removed.");
        if let Some((path, body)) = action {
            self.admin_request(
                AdminMethod::Post,
                path.into(),
                body,
                "action:discord-status".into(),
            );
        }
    }

    fn discord_settings_ui(&mut self, ui: &mut egui::Ui, bootstrap: &Bootstrap) {
        use crate::settings_ui as controls;
        if !bootstrap
            .capabilities
            .iter()
            .any(|c| c == "discord-command-v1")
        {
            controls::description(
                ui,
                "Update the community host to configure Discord sharing.",
            );
            return;
        }
        self.ensure_admin_data("discord", "/api/discord");
        settings_heading(
            ui,
            "Discord",
            "Let friends use /relay in your Discord server to see the shared call and how to join.",
        );
        controls::description(ui, "Off by default. Only the public label, participant count and instructions below are shared. Names, messages and encryption keys are kept private.");
        controls::description(ui, "Sharing runs on the community host while the server is running. Turn it off here to stop it.");
        let supports_status = bootstrap
            .capabilities
            .iter()
            .any(|c| c == "discord-channel-status-v1");
        let pending = self
            .admin_requests
            .values()
            .any(|k| k == "action:discord" || k == "action:discord-status");
        let status = self
            .admin_data
            .get("discord")
            .and_then(|v| v["connection"].as_str())
            .unwrap_or("off");
        let text = match status {
            "online" => "Connected · /relay is ready",
            "connecting" => "Connecting to Discord…",
            "needs_token" => "Setup needed · add the bot token on the host below",
            "wrong_application" => {
                "Setup needed · the bot token belongs to a different Discord application"
            }
            "setup_failed" => {
                "Setup needed · check the Discord server ID and install the app there"
            }
            "connection_failed" => {
                "Connection failed · check the host's internet connection and bot setup"
            }
            "session_limit" => "Discord's connection limit is reached · wait before reconnecting",
            _ => "Sharing is off",
        };
        ui.label(RichText::new(text).color(if status == "online" {
            Color32::from_rgb(35, 165, 90)
        } else {
            c_muted()
        }));
        if ui.add(secondary("Refresh status & preview")).clicked() {
            self.refresh_admin_data("discord", "/api/discord");
        }
        controls::divider(ui);
        self.discord_host_ui(ui, bootstrap);
        controls::divider(ui);
        let Some((baseline, draft)) = self.admin_ui.discord.as_mut() else {
            ui.spinner();
            return;
        };
        let mut save = false;
        ui.add_enabled_ui(!pending, |ui| {
            ui.checkbox(&mut draft.enabled, "Enable /relay in Discord");
            controls::nav_group(ui, "SHARED VOICE ROOM");
            let selected = draft.channel_id.as_deref().and_then(|id| bootstrap.channels.iter().find(|c| c.id == id)).map(|c| c.name.as_str()).unwrap_or("Choose a room");
            egui::ComboBox::from_id_source("discord-shared-room").selected_text(selected).width(ui.available_width()).show_ui(ui, |ui| {
                ui.selectable_value(&mut draft.channel_id, None, "None");
                for channel in bootstrap.channels.iter().filter(|c| c.kind == "voice" && c.visibility == "public" && c.voice_parent_id.is_none() && !bootstrap.conversations.iter().any(|t| t.channel_id == c.id)) {
                    ui.selectable_value(&mut draft.channel_id, Some(channel.id.clone()), &channel.name);
                }
            });
            controls::description(ui, "Choose a public community room without special channel permissions. A room made private later stops sharing its count.");
            controls::nav_group(ui, "PUBLIC LABEL");
            ui.add_sized([ui.available_width(), 38.0], egui::TextEdit::singleline(&mut draft.public_label).char_limit(80).hint_text("e.g. Let's play"));
            controls::nav_group(ui, "HOW FRIENDS JOIN");
            ui.add_sized([ui.available_width(), 96.0], egui::TextEdit::multiline(&mut draft.join_instructions).char_limit(700).hint_text("Open Relay, connect to … and ask … for an invitation."));
            controls::description(ui, "Write instructions you are happy to share in Discord. No invitation, server address or key is added automatically. Do not include private invitations or encryption keys.");
            controls::nav_group(ui, "DISCORD APPLICATION ID");
            ui.add_sized([ui.available_width(), 38.0], egui::TextEdit::singleline(&mut draft.application_id).char_limit(20).hint_text("Application ID from the Discord Developer Portal"));
            controls::nav_group(ui, "DISCORD SERVER ID");
            ui.add_sized([ui.available_width(), 38.0], egui::TextEdit::singleline(&mut draft.guild_id).char_limit(20).hint_text("The Discord server where /relay will be available"));
            controls::description(ui, "Create a Discord app, allow installation to a server, then copy its Application ID from General Information. Save its bot token on the host above.");
            ui.hyperlink_to("Create or open your Discord app", "https://discord.com/developers/applications");
            controls::description(ui, "For the Server ID, enable Developer Mode in Discord, right-click your Discord server and choose Copy Server ID.");
            if supports_status {
                controls::divider(ui);
                ui.checkbox(&mut draft.status_enabled,"Keep one call status message in a Discord channel");
                let mut channel=draft.status_channel_id.clone().unwrap_or_default();
                ui.add(egui::TextEdit::singleline(&mut channel).char_limit(20).hint_text("Discord text channel ID"));
                draft.status_channel_id=(!channel.trim().is_empty()).then(||channel.trim().to_owned());
                controls::description(ui,"Requires sharing enabled above. Right-click your Discord text channel in Developer Mode → Copy Channel ID. Relay verifies that it belongs to this Discord server.");
                controls::description(ui,"Off by default. Posts the public preview below once, then edits it only when the call changes, at most once every 15 seconds. No names, private rooms, automatic invitations or pings.");
            } else {
                controls::description(ui,"Update the community host to enable automatic channel updates.");
            }
            if let Some(link) = discord_install_url(draft, supports_status) {
                ui.hyperlink_to("Install app in this Discord server", link);
                if supports_status && draft.status_enabled {
                    controls::description(ui, "This requests View Channel, Send Messages and Read Message History. Allow these in the chosen text channel too.");
                } else {
                    controls::description(ui, "This installs /relay without requesting channel-message permissions. If you enable channel updates later, use this link again to grant their permissions.");
                }
            } else {
                controls::description(ui, "Enter a valid Application ID and Server ID to get the installation link.");
            }
        });
        if draft != baseline {
            if let Some(problem) = discord_setup_problem(draft) {
                controls::description(ui, problem);
            }
            ui.horizontal_wrapped(|ui| {
                ui.label("You have unsaved changes.");
                if ui.add_enabled(!pending, secondary("Reset")).clicked() {
                    *draft = baseline.clone();
                }
                if ui
                    .add_enabled(!pending && draft.valid(), primary("Save Changes"))
                    .clicked()
                {
                    save = true;
                }
            });
        }
        let body = save.then(|| discord_policy_body(draft, supports_status));
        if let Some(body) = body {
            self.admin_request(
                AdminMethod::Patch,
                "/api/discord".into(),
                body,
                "action:discord".into(),
            );
        }
        controls::divider(ui);
        controls::nav_group(ui, "CURRENT PUBLIC PREVIEW");
        if let Some(content) = self
            .admin_data
            .get("discord")
            .and_then(|v| v["public_preview"].as_str())
        {
            ui.add(egui::Label::new(content).wrap(true));
        } else {
            controls::description(
                ui,
                "Nothing is shared. Enable a valid public room to see its current preview.",
            );
        }
        controls::description(ui, "The /relay command replies only to the person who uses it. Personal Discord profile activity is still being developed.");
        if supports_status {
            controls::divider(ui);
            self.discord_channel_status_ui(ui);
        }
    }
}

// Bot-only installation has no callback or access token. IDs are parsed before
// constructing the fixed official URL; credentials and Relay invitations never enter it.
fn discord_install_url(
    settings: &relay_protocol::discord::DiscordBridgeSettings,
    supports_status: bool,
) -> Option<String> {
    let app = relay_protocol::discord::snowflake(&settings.application_id)?;
    let guild = relay_protocol::discord::snowflake(&settings.guild_id)?;
    let permissions = if supports_status && settings.status_enabled {
        (1_u64 << 10) | (1_u64 << 11) | (1_u64 << 16)
    } else {
        0
    };
    let mut link = url::Url::parse("https://discord.com/oauth2/authorize").ok()?;
    link.query_pairs_mut()
        .append_pair("client_id", &app.to_string())
        .append_pair("guild_id", &guild.to_string())
        .append_pair("disable_guild_select", "true")
        .append_pair("integration_type", "0")
        .append_pair("scope", "bot applications.commands")
        .append_pair("permissions", &permissions.to_string());
    Some(link.into())
}

fn discord_setup_problem(
    settings: &relay_protocol::discord::DiscordBridgeSettings,
) -> Option<&'static str> {
    if settings.valid() {
        return None;
    }
    if settings.enabled {
        if relay_protocol::discord::snowflake(&settings.application_id).is_none() {
            return Some("Enter the Discord Application ID: digits only, with no spaces. Copy it from your app's General Information page.");
        }
        if relay_protocol::discord::snowflake(&settings.guild_id).is_none() {
            return Some("Enter the Discord Server ID: digits only, with no spaces. Enable Developer Mode, then right-click your Discord server and choose Copy Server ID.");
        }
        if settings.channel_id.as_ref().is_none_or(|id| id.is_empty()) {
            return Some("Choose the public Relay voice room you want to share.");
        }
        if settings.public_label.trim().is_empty() {
            return Some("Add a public label so your friends can recognise the call.");
        }
        if settings.join_instructions.trim().is_empty() {
            return Some("Add instructions explaining how your friends can join Relay.");
        }
        if settings.status_enabled
            && settings
                .status_channel_id
                .as_deref()
                .and_then(relay_protocol::discord::snowflake)
                .is_none()
        {
            return Some("For channel updates, enter a Discord text Channel ID. Enable Developer Mode, then right-click the text channel and choose Copy Channel ID.");
        }
    }
    Some("Check the field lengths and remove unsupported control characters before saving.")
}

fn discord_policy_body(
    settings: &relay_protocol::discord::DiscordBridgeSettings,
    supports_status: bool,
) -> Value {
    let mut body = json!(settings);
    if !supports_status {
        if let Some(fields) = body.as_object_mut() {
            fields.remove("status_enabled");
            fields.remove("status_channel_id");
        }
    }
    body
}

fn discord_secret_input(
    ui: &mut egui::Ui,
    token: &mut zeroize::Zeroizing<String>,
) -> egui::Response {
    let mut output = egui::TextEdit::singleline(&mut **token)
        .id_source("discord-host-token")
        .password(true)
        .char_limit(512)
        .desired_width(ui.available_width())
        .hint_text("Paste bot token")
        .show(ui);
    // Password masking does not disable egui's plaintext undo snapshots.
    // Keep cursor/selection, but never retain token history between frames.
    output.state.clear_undoer();
    output.state.store(ui.ctx(), output.response.id);
    output.response
}

#[cfg(test)]
mod discord_secret_tests {
    use super::*;
    #[test]
    fn installation_link_is_bound_to_the_selected_server_and_minimal_permissions() {
        let mut settings = relay_protocol::discord::DiscordBridgeSettings {
            application_id: "123456789012345678".into(),
            guild_id: "223456789012345678".into(),
            public_label: "Not exported in the link".into(),
            join_instructions: "Private draft must not enter this URL".into(),
            ..Default::default()
        };
        let query = |settings: &relay_protocol::discord::DiscordBridgeSettings, supported| {
            let link = url::Url::parse(&discord_install_url(settings, supported).unwrap()).unwrap();
            assert_eq!(link.scheme(), "https");
            assert_eq!(link.host_str(), Some("discord.com"));
            assert_eq!(link.path(), "/oauth2/authorize");
            assert!(link.fragment().is_none());
            link.query_pairs()
                .into_owned()
                .collect::<std::collections::BTreeMap<_, _>>()
        };
        let fields = query(&settings, true);
        assert_eq!(fields.len(), 6);
        assert_eq!(fields["client_id"], settings.application_id);
        assert_eq!(fields["guild_id"], settings.guild_id);
        assert_eq!(fields["disable_guild_select"], "true");
        assert_eq!(fields["integration_type"], "0");
        assert_eq!(fields["scope"], "bot applications.commands");
        assert_eq!(fields["permissions"], "0");
        settings.status_enabled = true;
        assert_eq!(query(&settings, true)["permissions"], "68608");
        assert_eq!(query(&settings, false)["permissions"], "0");
        for invalid in ["0", " 123", "1&scope=identify", "18446744073709551616"] {
            settings.application_id = invalid.into();
            assert!(discord_install_url(&settings, true).is_none());
        }
        settings.application_id = "123456789012345678".into();
        settings.guild_id = "1&permissions=8".into();
        assert!(discord_install_url(&settings, true).is_none());
    }

    #[test]
    fn setup_help_allows_opt_out_and_explains_missing_enabled_fields() {
        let mut settings = relay_protocol::discord::DiscordBridgeSettings::default();
        assert!(discord_setup_problem(&settings).is_none());
        settings.enabled = true;
        assert!(discord_setup_problem(&settings)
            .unwrap()
            .contains("Application ID"));
        settings.application_id = "123456789012345678".into();
        assert!(discord_setup_problem(&settings)
            .unwrap()
            .contains("Server ID"));
        settings.guild_id = "223456789012345678".into();
        assert!(discord_setup_problem(&settings)
            .unwrap()
            .contains("voice room"));
        settings.channel_id = Some("room".into());
        assert!(discord_setup_problem(&settings)
            .unwrap()
            .contains("public label"));
        settings.public_label = "Games".into();
        assert!(discord_setup_problem(&settings)
            .unwrap()
            .contains("instructions"));
        settings.join_instructions = "Ask the host how to join".into();
        assert!(discord_setup_problem(&settings).is_none());
        settings.status_enabled = true;
        assert!(discord_setup_problem(&settings)
            .unwrap()
            .contains("Channel ID"));
        settings.enabled = false;
        settings.application_id.clear();
        settings.guild_id.clear();
        settings.channel_id = None;
        assert!(
            discord_setup_problem(&settings).is_none(),
            "Opt-out never requires Discord IDs"
        );
    }

    #[test]
    fn older_host_receives_only_the_supported_policy_fields() {
        let p = relay_protocol::discord::DiscordBridgeSettings::default();
        let old = discord_policy_body(&p, false);
        assert!(old.get("status_enabled").is_none());
        assert!(old.get("status_channel_id").is_none());
        assert_eq!(old["enabled"], false);
        let new = discord_policy_body(&p, true);
        assert_eq!(new["status_enabled"], false);
        assert!(new["status_channel_id"].is_null());
    }
    #[test]
    fn pasted_secret_is_masked_and_cannot_return_through_undo_after_save() {
        let ctx = egui::Context::default();
        let mut token = zeroize::Zeroizing::new(String::new());
        let mut id = egui::Id::NULL;
        let frame = |events: Vec<egui::Event>| egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(600.0, 200.0))),
            events,
            ..Default::default()
        };
        let _ = ctx.run(frame(vec![]), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let response = discord_secret_input(ui, &mut token);
                id = response.id;
                response.request_focus();
            });
        });
        let secret = "fixture-token-should-never-be-visible";
        let output = ctx.run(frame(vec![egui::Event::Paste(secret.into())]), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                discord_secret_input(ui, &mut token);
            });
        });
        assert_eq!(token.as_str(), secret);
        assert!(!format!("{:?}", output.platform_output.events).contains(secret));
        for shape in &output.shapes {
            if let egui::epaint::Shape::Text(text) = &shape.shape {
                assert!(!text.galley.job.text.contains(secret));
            }
        }
        let state = egui::TextEdit::load_state(&ctx, id).unwrap();
        assert!(!state
            .undoer()
            .has_undo(&(egui::text::CCursorRange::default(), String::new())));
        let saved = std::mem::take(&mut token);
        assert_eq!(saved.as_str(), secret);
        let _ = ctx.run(
            frame(vec![egui::Event::Key {
                key: egui::Key::Z,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::CTRL,
            }]),
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    discord_secret_input(ui, &mut token);
                });
            },
        );
        assert!(token.is_empty(), "Ctrl+Z must not revive a saved token");
    }
}
