//! Shared server actions for the header and the saved-server context menu.
use super::*;

fn unix_time() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Open,
    Invite,
    Settings,
    Profile,
    Privacy,
    CreateChannel,
    Categories,
    Events,
    CreateEvent,
    About,
    Search,
}

struct Pending {
    origin: CommandOrigin,
    action: Action,
    at: Instant,
}
impl Pending {
    fn matches(&self, origin: &CommandOrigin, now: Instant) -> bool {
        &self.origin == origin && now.saturating_duration_since(self.at) < Duration::from_secs(15)
    }
}

#[derive(Default)]
pub(super) struct State {
    pending: Option<Pending>,
    remove: Option<SavedCommunity>,
}
impl State {
    pub(super) fn removal_open(&self) -> bool {
        self.remove.is_some()
    }
    pub(super) fn clear(&mut self) {
        *self = Self::default();
    }
    pub(super) fn cancel_if_other(&mut self, url: &str) {
        self.remove = None;
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.origin.url != url)
        {
            self.pending = None;
        }
    }
}

fn item(ui: &mut egui::Ui, label: &str, enabled: bool, accent: bool) -> bool {
    ui.add_enabled_ui(enabled, |ui| {
        if accent {
            desktop_ui::menu_item_accent(ui, label)
        } else {
            desktop_ui::menu_item(ui, label, false)
        }
    })
    .inner
    .clicked()
}

impl RelayApp {
    fn saved_server_origin(&self, url: &str) -> Option<CommandOrigin> {
        if self.community_status(url) != ConnectionStatus::Connected {
            return None;
        }
        if url == self.login.url {
            return self.command_origin();
        }
        self.communities
            .iter()
            .find(|c| c.url == url)
            .map(|c| CommandOrigin {
                url: c.url.clone(),
                server_id: c.server_id.clone(),
                user_id: c.user.id.clone(),
            })
    }

    fn server_notification_key(&self, url: &str) -> Option<String> {
        let origin = self.saved_server_origin(url)?;
        Some(format!(
            "{}|community",
            ConversationMemory::scope(url, &origin.user_id)
        ))
    }

    fn begin_server_action(&mut self, url: &str, action: Action) {
        self.server_menu = false;
        if action == Action::Open {
            self.switch_community(url.into());
            return;
        }
        let Some(origin) = self.saved_server_origin(url) else {
            self.switch_community(url.into());
            return;
        };
        self.server_actions.pending = Some(Pending {
            origin,
            action,
            at: Instant::now(),
        });
        if url == self.login.url {
            self.finish_server_action();
        } else {
            self.switch_community(url.into());
        }
    }

    pub(super) fn finish_server_action(&mut self) {
        let Some(pending) = self.server_actions.pending.take() else {
            return;
        };
        if pending.at.elapsed() >= Duration::from_secs(15) {
            return;
        }
        let Some(origin) = self.command_origin() else {
            return;
        };
        if origin.url != pending.origin.url {
            self.server_actions.pending = Some(pending);
            return;
        }
        if !pending.matches(&origin, Instant::now()) {
            self.error =
                Some("The server or account changed. Choose the server action again.".into());
            return;
        }
        self.selected_nav = true;
        self.home_page = None;
        let permission = |mask| {
            self.bootstrap
                .as_ref()
                .is_some_and(|b| b.has_permission(mask))
        };
        match pending.action {
            Action::Open => {}
            Action::Invite if permission(PERMISSION_INVITES) => {
                if crate::local_host::is_local_url(&self.login.url) {
                    self.open_friend_sharing();
                } else {
                    self.send(Command::Invite {
                        origin,
                        request_id: None,
                    });
                }
            }
            Action::Settings => {
                self.settings_tab = SettingsTab::Server;
                self.settings_open = true;
            }
            Action::Profile => {
                self.settings_tab = SettingsTab::Profiles;
                self.settings_open = true;
            }
            Action::Privacy => {
                self.settings_tab = SettingsTab::Privacy;
                self.settings_open = true;
            }
            Action::CreateChannel if permission(PERMISSION_CHANNELS) => {
                self.create_channel_open = true;
            }
            Action::Categories if permission(PERMISSION_CHANNELS) => {
                self.admin_ui.select_section("channels");
                self.settings_tab = SettingsTab::Server;
                self.settings_open = true;
            }
            Action::Events => {
                self.home_page = Some(SettingsTab::Events);
            }
            Action::CreateEvent if permission(PERMISSION_CHANNELS) => {
                self.home_page = Some(SettingsTab::Events);
                self.start_server_event();
            }
            Action::About if self.has_chat_capability("welcome-v1") => self.open_welcome(),
            Action::Search => self.open_community_search(),
            _ => self.error = Some("You do not have permission for that server action.".into()),
        }
    }

    pub(super) fn saved_server_context_menu(
        &mut self,
        ui: &mut egui::Ui,
        community: &SavedCommunity,
    ) {
        ui.set_min_width(232.0);
        let origin = self.saved_server_origin(&community.url);
        let active = community.url == self.login.url;
        let ready = origin.is_some();
        let permits = |mask| {
            !active
                || self
                    .bootstrap
                    .as_ref()
                    .is_some_and(|b| b.has_permission(mask))
        };
        let invite = permits(PERMISSION_INVITES);
        let manage = active && permits(PERMISSION_CHANNELS);
        let about = !active || self.has_chat_capability("welcome-v1");
        let mut action = None;
        if item(ui, "Open server", true, false) {
            action = Some(Action::Open);
        }
        if invite && item(ui, "Invite friends", ready, true) {
            action = Some(Action::Invite);
        }
        if item(ui, "Mark server as read", ready, false) {
            if let Some(origin) = origin.clone() {
                self.send(Command::MarkCommunityRead { origin });
            }
            ui.close_menu();
            self.server_menu = false;
        }
        ui.separator();
        if let Some(key) = self.server_notification_key(&community.url) {
            ui.menu_button("Notifications", |ui| {
                let before = self
                    .prefs
                    .notification_policies
                    .get(&key)
                    .cloned()
                    .unwrap_or_default();
                for (mode, label) in [
                    ("", "Use app default"),
                    ("all", "All messages"),
                    ("mentions", "Only mentions"),
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
            });
            let muted = self
                .prefs
                .notification_policies
                .get(&key)
                .is_some_and(|p| p.muted_until > unix_time());
            if muted {
                if item(ui, "Unmute server", true, false) {
                    self.prefs
                        .notification_policies
                        .entry(key.clone())
                        .or_default()
                        .muted_until = 0;
                    self.defer_preferences();
                    ui.close_menu();
                    self.server_menu = false;
                }
            } else {
                ui.menu_button("Mute server", |ui| {
                    for (seconds, label) in [
                        (900, "15 minutes"),
                        (3600, "1 hour"),
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
            let mut hide = self
                .prefs
                .notification_policies
                .get(&key)
                .is_some_and(|p| p.hide_muted_channels);
            if ui.checkbox(&mut hide, "Hide muted channels").changed() {
                self.prefs
                    .notification_policies
                    .entry(key)
                    .or_default()
                    .hide_muted_channels = hide;
                self.defer_preferences();
            }
        } else {
            ui.label(
                RichText::new("Open and sign in for server actions")
                    .small()
                    .color(c_muted()),
            );
        }
        ui.separator();
        if item(ui, "Server Settings", ready, false) {
            action = Some(Action::Settings);
        }
        if item(ui, "Edit server profile", ready, false) {
            action = Some(Action::Profile);
        }
        if item(ui, "Privacy & Safety", ready, false) {
            action = Some(Action::Privacy);
        }
        if manage {
            if item(ui, "Create channel…", ready, false) {
                action = Some(Action::CreateChannel);
            }
            if item(ui, "Manage categories…", ready, false) {
                action = Some(Action::Categories);
            }
            if item(ui, "Create event…", ready, false) {
                action = Some(Action::CreateEvent);
            }
        }
        if item(ui, "Events", ready, false) {
            action = Some(Action::Events);
        }
        if about && item(ui, "About / Welcome", ready, false) {
            action = Some(Action::About);
        }
        if item(ui, "Search this server", ready, false) {
            action = Some(Action::Search);
        }
        ui.separator();
        if !community.server_id.is_empty() && item(ui, "Copy server ID", true, false) {
            ui.output_mut(|o| o.copied_text = community.server_id.clone());
            ui.close_menu();
            self.server_menu = false;
        }
        if item(ui, "Copy server address", true, false) {
            ui.output_mut(|o| o.copied_text = community.url.clone());
            ui.close_menu();
            self.server_menu = false;
        }
        if item(ui, "Remove from this device…", true, false) {
            self.server_actions.remove = Some(community.clone());
            ui.close_menu();
            self.server_menu = false;
        }
        if let Some(action) = action {
            self.begin_server_action(&community.url, action);
            ui.close_menu();
        }
    }

    pub(super) fn channel_hidden_by_server_policy(&self, channel: &Channel) -> bool {
        if self.current.as_ref() == Some(&Target::Channel(channel.id.clone()))
            || self.voice_channel.as_ref() == Some(&channel.id)
            || self.joining_voice.as_ref() == Some(&channel.id)
        {
            return false;
        }
        let Some(user) = self.session.as_ref().map(|s| &s.user.id) else {
            return false;
        };
        let scope = ConversationMemory::scope(&self.login.url, user);
        self.prefs
            .notification_policies
            .get(&format!("{scope}|community"))
            .is_some_and(|p| p.hide_muted_channels)
            && self
                .prefs
                .notification_policies
                .get(&format!(
                    "{scope}|{:?}",
                    Target::Channel(channel.id.clone())
                ))
                .is_some_and(|p| p.muted_until > unix_time())
    }

    pub(super) fn server_options_menu(&mut self, ctx: &egui::Context) {
        if self.server_menu {
            let community = self
                .prefs
                .communities
                .iter()
                .find(|c| c.url == self.login.url)
                .cloned()
                .unwrap_or_else(|| SavedCommunity {
                    url: self.login.url.clone(),
                    server_id: self
                        .bootstrap
                        .as_ref()
                        .map(|b| b.server_id.clone())
                        .unwrap_or_default(),
                    name: self
                        .bootstrap
                        .as_ref()
                        .map(|b| b.server_name.clone())
                        .unwrap_or_default(),
                    ..Default::default()
                });
            let menu = egui::Window::new("Server options")
                .title_bar(false)
                .movable(false)
                .fixed_pos(self.server_menu_anchor)
                .constrain_to(ctx.screen_rect())
                .collapsible(false)
                .resizable(false)
                .default_width(248.0)
                .default_height(820.0)
                .min_width(248.0)
                .max_width(248.0)
                .max_height((ctx.screen_rect().height() - 64.0).max(100.0))
                .vscroll(true)
                .frame(
                    egui::Frame::window(&ctx.style())
                        .fill(c_rail())
                        .rounding(8.0)
                        .inner_margin(Margin::same(8.0)),
                )
                .show(ctx, |ui| self.saved_server_context_menu(ui, &community));
            if let Some(menu) = menu {
                let header = Rect::from_min_max(
                    self.server_menu_anchor - Vec2::new(12.0, 50.0),
                    self.server_menu_anchor + Vec2::new(248.0, 0.0),
                );
                if ctx.input(|input| {
                    input.pointer.any_click()
                        && input.pointer.interact_pos().is_some_and(|p| {
                            !menu.response.rect.contains(p)
                                && !header.contains(p)
                                && !ctx
                                    .layer_id_at(p)
                                    .is_some_and(|layer| layer.order >= egui::Order::Foreground)
                        })
                }) {
                    self.server_menu = false;
                }
            }
        }
        if let Some(community) = self.server_actions.remove.clone() {
            let mut open = true;
            let mut confirm = false;
            let mut cancel = false;
            desktop_ui::dialog(ctx, "Remove saved server?")
                .open(&mut open)
                .resizable(false)
                .collapsible(false)
                .default_width(420.0)
                .show(ctx, |ui| {
                    let name = if community.name.is_empty() {
                        &community.url
                    } else {
                        &community.name
                    };
                    ui.label(format!("Remove {name} from this device?"));
                    ui.label("This signs you out here and removes the saved connection. Your server, account, messages and encryption keys are kept. You can add the server again later.");
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        cancel = ui.button("Cancel").clicked();
                        confirm = ui.add(secondary("Remove from this device")).clicked();
                    });
                });
            if confirm {
                if community.url == self.login.url {
                    self.save_preferences();
                    self.stop_media();
                }
                self.server_actions.pending = None;
                self.prefs.communities.retain(|c| c.url != community.url);
                self.community_statuses.remove(&community.url);
                self.send(Command::ForgetCommunity { url: community.url });
                self.save_preferences();
            }
            if confirm
                || cancel
                || !open
                || ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
            {
                self.server_actions.remove = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn delayed_server_action_is_bound_to_server_account_and_short_lifetime() {
        let at = Instant::now();
        let origin = CommandOrigin {
            url: "https://one.invalid".into(),
            server_id: "server-one".into(),
            user_id: "alice".into(),
        };
        let pending = Pending {
            origin: origin.clone(),
            action: Action::Invite,
            at,
        };
        assert!(pending.matches(&origin, at));
        for changed in [
            CommandOrigin {
                url: "https://two.invalid".into(),
                ..origin.clone()
            },
            CommandOrigin {
                server_id: "server-two".into(),
                ..origin.clone()
            },
            CommandOrigin {
                user_id: "bob".into(),
                ..origin.clone()
            },
        ] {
            assert!(!pending.matches(&changed, at));
        }
        assert!(!pending.matches(&origin, at + Duration::from_secs(15)));
        let mut state = State {
            pending: Some(pending),
            remove: None,
        };
        state.cancel_if_other(&origin.url);
        assert!(state.pending.is_some());
        state.cancel_if_other("https://two.invalid");
        assert!(state.pending.is_none());
    }
}
