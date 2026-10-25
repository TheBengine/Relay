//! Floating desktop settings with grouped navigation and real native controls.
use super::*;
use crate::audio_settings::LatencyMode;
use crate::settings_ui as controls;

fn voice_delay_counter(
    ui: &mut egui::Ui,
    audio: &AudioSettings,
    applied: &AudioSettings,
    period_ms: Option<f32>,
) {
    let budget = audio.voice_delay_budget_for_output(period_ms);
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new("Expected voice delay").strong());
        ui.label(RichText::new(budget.label()).size(font_size(18.0)).color(c_accent()));
        help_tip(ui, "expected voice delay", "A conservative settings budget, assuming the sender and receiver use these settings. It is not a measured end-to-end delay. Network, devices and runtime processing add more time; the receive buffer may start playback sooner than its target.");
        ui.label(
            RichText::new("Settings budget · network and devices add more")
                .size(font_size(crate::appearance::TEXT_CAPTION))
                .color(c_muted()),
        );
    });
    if audio != applied {
        controls::description(
            ui,
            &format!(
                "Preview · applied budget: {}. Save Changes applies your choices.",
                applied.voice_delay_budget_for_output(period_ms).label()
            ),
        );
    }
}

fn voice_delay_details(ui: &mut egui::Ui, audio: &AudioSettings, period_ms: Option<f32>) {
    let budget = audio.voice_delay_budget_for_output(period_ms);
    egui::CollapsingHeader::new("What affects this delay?")
        .id_source("voice-delay-details")
        .show(ui, |ui| {
            ui.label(format!("Packet collection: {} ms · processing batch: {} ms", audio.packet_duration_ms(), budget.capture_ms));
            controls::description(ui, "Packet collection is counted once. Noise suppression or echo cancellation requires at least a 10 ms processing batch, even with 5 ms packets.");
            ui.label(format!("Noise suppression lookahead: {} ms", budget.suppression_ms));
            ui.label(format!("Speech-start history: {} ms", budget.speech_start_ms));
            ui.label(budget.codec_ms.map(|ms| format!("Voice codec lookahead: {ms:.1} ms")).unwrap_or_else(|| "Voice codec lookahead: unavailable".into()));
            ui.label(if audio.adaptive_jitter {
                format!("Receive buffer target: {}–{} ms · automatic", budget.receive_min_ms, budget.receive_max_ms)
            } else {
                format!("Receive buffer target: {} ms · fixed", budget.receive_min_ms)
            });
            ui.label(format!("Playback timing reserve: {:.1} ms", budget.playback_ms));
            controls::description(ui, "This combines your outgoing processing with your incoming buffer to compare settings. A real call uses the sender's processing and the listener's buffer; other people may choose different settings.");
            controls::description(ui, "Bitrate changes quality and bandwidth, not a fixed delay. Gain, volume, sounds, overlay, ducking and priority do not add a configured voice buffer.");
            controls::description(ui, "Voice activity threshold decides when speech starts sending. Speech-start history preserves the preceding 10 ms (20 ms with 20 ms packets), adding up to that much to a new talkspurt. A strictly silent lead-in can be skipped when the next packet is already queued; quiet word beginnings are retained. The full history is included in this conservative budget and never used for push-to-talk. Hold keeps sending after speech stops; it is not added to each packet. Push-to-talk adds key detection time.");
            controls::description(ui, "Echo delay aligns the speaker reference; its slider value is not an extra transmission delay. Device format conversion and buffers, connection quality, TCP fallback, CPU load and video/screen sharing can change real delay and are not predictable from these settings.");
        });
}

fn bluetooth_hint(
    ui: &mut egui::Ui,
    devices: &crate::audio_settings::AudioDevices,
    audio: &AudioSettings,
) {
    if audio
        .input_device
        .as_ref()
        .or(devices.default_input.as_ref())
        .into_iter()
        .chain(
            audio
                .output_device
                .as_ref()
                .or(devices.default_output.as_ref()),
        )
        .any(|name| devices.bluetooth.contains(name))
    {
        controls::description(ui, "Bluetooth can add substantial audio delay. A wired or USB headset usually has lower latency.");
    }
}

fn page_scroll(id: impl std::hash::Hash, capture_bottom: bool) -> egui::ScrollArea {
    egui::ScrollArea::vertical()
        .id_source(id)
        .auto_shrink([false, false])
        .animated(false)
        .stick_to_bottom(capture_bottom)
}

impl SettingsTab {
    fn label(self) -> &'static str {
        match self {
            Self::Account => "My Account",
            Self::Profiles => "Profiles",
            Self::Friends => "Friends & Status",
            Self::Events => "Events",
            Self::Mentions => "Inbox",
            Self::Encryption => "Encryption",
            Self::Privacy => "Privacy & Safety",
            Self::Connections => "Connections",
            Self::VoiceVideo => "Voice & Video",
            Self::Appearance => "Appearance",
            Self::Accessibility => "Accessibility",
            Self::Notifications => "Notifications",
            Self::Keybinds => "Keybinds",
            Self::Advanced => "Advanced",
            Self::Server => "Server Settings",
            Self::Help => "Help",
        }
    }
    fn matches(self, query: &str) -> bool {
        let words = match self {
            Self::Account => "account password sessions devices username logout authenticator two factor security recovery shared global unique browser",
            Self::Profiles => "profiles avatar display name bio about me pronouns accent banner colors identity",
            Self::Friends => "friends requests block blocked status online idle invisible do not disturb",
            Self::Events => "events scheduled interested calendar upcoming",
            Self::Mentions => "inbox mentions unread notifications",
            Self::Encryption => "encryption security devices fingerprint verify keys trust",
            Self::Privacy => "privacy safety remembered login drafts credentials vault",
            Self::Connections => "connections communities servers address",
            Self::VoiceVideo => "voice video microphone input output volume camera screen noise echo buffer bitrate debugging delay latency",
            Self::Appearance => "appearance theme dark light onyx ash font zoom compact cozy messages spacing emoji suggestions convert send chat box composer",
            Self::Accessibility => "accessibility motion animation gifs text readability",
            Self::Notifications => "notifications mentions sounds mute deafen join leave",
            Self::Keybinds => "keybinds keyboard shortcuts push talk hotkey",
            Self::Advanced => "advanced storage preferences reset version connection close quit window tray background hidden icons",
            Self::Server => "server community roles channels permissions members invitations backups hosting",
            Self::Help => "help start invite join friends encryption verify devices keyboard shortcuts",
        };
        query.split_whitespace().all(|word| words.contains(word))
    }
}

impl RelayApp {
    pub(super) fn update_appearance(&mut self, ctx: &egui::Context, system: Option<eframe::Theme>) {
        let changed =
            self.applied_appearance != self.prefs.appearance || self.applied_system_theme != system;
        if changed {
            self.prefs.appearance = self.prefs.appearance.clone().normalized();
            let colors = self.prefs.appearance.apply(ctx, system);
            UI_PALETTE.with(|palette| palette.set(colors));
            UI_FONT_SCALE.with(|scale| scale.set(self.prefs.appearance.font_scale));
            self.applied_appearance = self.prefs.appearance.clone();
            self.applied_system_theme = system;
            self.row_heights = RowHeights::default();
            ctx.request_repaint();
        }
    }

    pub(super) fn close_settings(&mut self) {
        self.settings_open = false;
        self.stop_source_preview();
        self.stop_settings_mic_test();
        self.defer_preferences();
    }

    fn stop_settings_mic_test(&mut self) {
        if self.mic_test {
            if let Some(media) = &mut self.media {
                media.stop_microphone_test();
            }
            self.mic_test = false;
        }
    }

    pub(super) fn settings_view(&mut self, ctx: &egui::Context) {
        let audio_before = self.prefs.audio.clone();
        if self.settings_tab == SettingsTab::VoiceVideo
            && !self.inventory_loaded
            && self.inventory_rx.is_none()
        {
            self.refresh_devices();
        }
        let before = (
            self.compact,
            self.ui_scale,
            self.prefs.notifications,
            self.prefs.notification_previews,
            self.prefs.mentions_only,
            self.prefs.remember_drafts,
            self.prefs.remember_session,
            self.prefs.reduced_motion,
            self.prefs.ptt_key,
            self.prefs.appearance.clone(),
            self.prefs.screen_options,
            self.prefs.camera_options,
        );
        let screen = ctx.screen_rect();
        let rect = Rect::from_min_max(
            Pos2::new(screen.left(), (screen.top() + 32.0).min(screen.bottom())),
            screen.max,
        );
        let size = rect.size();
        let nav_width = (size.x * 0.23).clamp(174.0, 232.0);
        egui::Window::new("Settings")
            .id(egui::Id::new("desktop-settings-window"))
            .title_bar(false)
            .collapsible(false)
            .resizable(false)
            .movable(false)
            .constrain(false)
            .fixed_rect(rect)
            .frame(egui::Frame::none().fill(c_bg()))
            .show(ctx, |ui| {
                ui.set_min_size(size);
                ui.set_max_size(size);
                egui::SidePanel::left("settings-navigation")
                    .exact_width(nav_width)
                    .resizable(false)
                    .frame(egui::Frame::none().fill(c_panel()))
                    .show_inside(ui, |ui| {
                        ui.style_mut().spacing.scroll = egui::style::ScrollStyle::solid();
                        egui::Frame::none()
                            .inner_margin(Margin {
                                left: 16.0,
                                right: 12.0,
                                top: 20.0,
                                bottom: 20.0,
                            })
                            .show(ui, |ui| {
                                let search = if self.settings_tab == SettingsTab::Server {
                                    &mut self.admin_ui.search
                                } else {
                                    &mut self.settings_search
                                };
                                ui.add_sized(
                                    [ui.available_width(), 32.0],
                                    egui::TextEdit::singleline(&mut *search)
                                        .hint_text("Search settings")
                                        .margin(Vec2::new(8.0, 7.0)),
                                );
                                ui.add_space(16.0);
                                let query = search.to_lowercase();
                                egui::ScrollArea::vertical()
                                    .id_source("settings-navigation-scroll")
                                    .auto_shrink([false, false])
                                    .show(ui, |ui| {
                                        if self.settings_tab == SettingsTab::Server {
                                            if let Some(bootstrap) = self.bootstrap.as_ref() {
                                                self.admin_ui.navigation(ui, bootstrap, &query);
                                            }
                                            controls::divider(ui);
                                            if controls::nav_item(
                                                ui,
                                                "return-user-settings",
                                                "User Settings",
                                                false,
                                            )
                                            .clicked()
                                            {
                                                self.settings_tab = SettingsTab::Account;
                                            }
                                        } else {
                                            for (heading, pages) in [
                                                (
                                                    "USER SETTINGS",
                                                    &[
                                                        SettingsTab::Account,
                                                        SettingsTab::Profiles,
                                                        SettingsTab::Encryption,
                                                        SettingsTab::Privacy,
                                                        SettingsTab::Connections,
                                                    ][..],
                                                ),
                                                (
                                                    "APP SETTINGS",
                                                    &[
                                                        SettingsTab::Accessibility,
                                                        SettingsTab::VoiceVideo,
                                                        SettingsTab::Notifications,
                                                        SettingsTab::Keybinds,
                                                        SettingsTab::Advanced,
                                                        SettingsTab::Help,
                                                    ][..],
                                                ),
                                                ("DISPLAY", &[SettingsTab::Appearance][..]),
                                                ("COMMUNITY", &[SettingsTab::Server][..]),
                                            ] {
                                                let visible: Vec<_> = pages
                                                    .iter()
                                                    .copied()
                                                    .filter(|page| page.matches(&query))
                                                    .collect();
                                                if visible.is_empty() {
                                                    continue;
                                                }
                                                controls::nav_group(ui, heading);
                                                for page in visible {
                                                    if controls::nav_item(
                                                        ui,
                                                        ("settings-nav", page as u8),
                                                        page.label(),
                                                        self.settings_tab == page,
                                                    )
                                                    .clicked()
                                                    {
                                                        if self.settings_tab != page {
                                                            self.stop_source_preview();
                                                            self.stop_settings_mic_test();
                                                        }
                                                        self.settings_tab = page;
                                                        if page == SettingsTab::Encryption
                                                            && self.session.is_some()
                                                        {
                                                            self.send(Command::CryptoRefresh);
                                                        }
                                                    }
                                                }
                                                ui.add_space(12.0);
                                                controls::divider(ui);
                                            }
                                        }
                                        if self.session.is_some()
                                            && controls::nav_item(
                                                ui,
                                                "settings-log-out",
                                                "Log Out",
                                                false,
                                            )
                                            .clicked()
                                        {
                                            self.close_settings();
                                            self.stop_media();
                                            self.send(Command::Logout);
                                        }
                                        ui.add_space(16.0);
                                        controls::description(
                                            ui,
                                            &crate::build_metadata::current().label(),
                                        );
                                        controls::description(ui, "Free · Self-hosted · Native");
                                    });
                            });
                    });
                egui::SidePanel::right("settings-close-gutter")
                    .exact_width(44.0)
                    .resizable(false)
                    .frame(
                        egui::Frame::none()
                            .fill(c_bg())
                            .inner_margin(Margin::symmetric(7.0, 14.0)),
                    )
                    .show_inside(ui, |ui| {
                        if icon_button(ui, Icon::Close, false, "Close settings · Esc").clicked() {
                            self.close_settings();
                        }
                        // Caption size; one line, so large text cannot wrap
                        // "ESC" in the 44 px close column.
                        ui.add(
                            egui::Label::new(
                                RichText::new("ESC")
                                    .size(font_size(crate::appearance::TEXT_CAPTION))
                                    .color(c_muted()),
                            )
                            .wrap(false),
                        );
                    });
                egui::CentralPanel::default()
                    .frame(
                        egui::Frame::none()
                            .fill(c_bg())
                            .inner_margin(Margin::symmetric(24.0, 22.0)),
                    )
                    .show_inside(ui, |ui| {
                        ui.spacing_mut().scroll = egui::style::ScrollStyle::solid();
                        if self.prefs.audio != self.audio_applied {
                            egui::TopBottomPanel::bottom("settings-save-bar")
                                .frame(
                                    egui::Frame::none()
                                        .fill(c_rail())
                                        .rounding(6.0)
                                        .inner_margin(Margin::same(12.0)),
                                )
                                .show_inside(ui, |ui| {
                                    ui.horizontal_wrapped(|ui| {
                                        ui.label("Your voice settings have changed.");
                                        if ui.add(secondary("Reset")).clicked() {
                                            self.stop_settings_mic_test();
                                            self.prefs.audio = self.audio_applied.clone();
                                            self.save_preferences();
                                        }
                                        if ui.add(primary("Save Changes")).clicked() {
                                            self.apply_audio_settings();
                                        }
                                    });
                                    controls::description(
                                        ui,
                                        "Changing a device reconnects your current voice call.",
                                    );
                                });
                        }
                        if let Some(error) = &self.error {
                            egui::Frame::none()
                                .fill(c_panel())
                                .rounding(6.0)
                                .inner_margin(Margin::same(12.0))
                                .show(ui, |ui| {
                                    ui.add(
                                        egui::Label::new(RichText::new(error).color(c_bad()))
                                            .wrap(true),
                                    );
                                });
                            ui.add_space(12.0);
                        }
                        if let Some((notice, _)) = &self.notice {
                            ui.add(
                                egui::Label::new(RichText::new(notice).color(c_good())).wrap(true),
                            );
                            ui.add_space(12.0);
                        }
                        let capture_bottom =
                            self.settings_scroll_bottom && self.capture_path.is_some();
                        if self.settings_tab == SettingsTab::VoiceVideo {
                            // Pinned under the page (above Save Changes) so it stays in
                            // view while settings scroll, without pushing the title down.
                            egui::TopBottomPanel::bottom("settings-voice-delay-counter")
                                .show_separator_line(false)
                                .frame(
                                    egui::Frame::none()
                                        .fill(c_rail())
                                        .rounding(6.0)
                                        .inner_margin(Margin::symmetric(12.0, 8.0))
                                        .outer_margin(Margin { top: 10.0, ..Default::default() }),
                                )
                                .show_inside(ui, |ui| {
                                    voice_delay_counter(ui, &self.prefs.audio, &self.audio_applied, self.media.as_ref().and_then(|m| m.network_stats().output_timing).map(|t| t.period_ms));
                                    if let Some(stats) = self.media.as_ref().filter(|m| m.is_active()).map(|m| m.network_stats()) {
                                        if let Some(rtt) = stats.udp_round_trip_ms {
                                            controls::description(ui, &format!("Measured relay round trip: {rtt:.1} ms · shown separately, not added to the one-way budget"));
                                        }
                                    }
                                });
                        }
                        let scroll = page_scroll(
                            (
                                "settings-page",
                                self.settings_tab as u8,
                                self.voice_video_tab as u8,
                            ),
                            capture_bottom,
                        );
                        scroll.show(ui, |ui| {
                            match self.settings_tab {
                                SettingsTab::Account => self.account_page(ui),
                                SettingsTab::Profiles => self.profiles_page(ui),
                                SettingsTab::Friends => self.friends_page(ui),
                                SettingsTab::Events => self.events_page(ui),
                                SettingsTab::Mentions => self.mention_inbox_page(ui),
                                SettingsTab::Encryption => self.encryption_page(ui),
                                SettingsTab::Privacy => self.privacy_page(ui),
                                SettingsTab::Connections => self.connections_page(ui),
                                SettingsTab::VoiceVideo => self.voice_video_page(ui),
                                SettingsTab::Appearance => self.appearance_page(ui),
                                SettingsTab::Accessibility => self.accessibility_page(ui),
                                SettingsTab::Notifications => self.notifications_page(ui),
                                SettingsTab::Keybinds => self.keybinds_page(ui),
                                SettingsTab::Advanced => self.advanced_page(ui),
                                SettingsTab::Help => self.help_page(ui),
                                SettingsTab::Server => {
                                    self.server_settings_ui(ui);
                                }
                            }
                            ui.add_space(32.0);
                            if self.settings_scroll_bottom {
                                ui.scroll_to_cursor(Some(Align::BOTTOM));
                                if self.capture_path.is_none() {
                                    self.settings_scroll_bottom = false;
                                }
                            }
                        });
                    });
            });
        let after = (
            self.compact,
            self.ui_scale,
            self.prefs.notifications,
            self.prefs.notification_previews,
            self.prefs.mentions_only,
            self.prefs.remember_drafts,
            self.prefs.remember_session,
            self.prefs.reduced_motion,
            self.prefs.ptt_key,
            self.prefs.appearance.clone(),
            self.prefs.screen_options,
            self.prefs.camera_options,
        );
        if before != after {
            if before.0 != after.0 || before.1 != after.1 {
                self.row_heights = RowHeights::default();
            }
            if before.1 != after.1 {
                ctx.set_zoom_factor(self.ui_scale);
            }
            if before.6 != after.6 {
                self.send(Command::RememberSession {
                    enabled: self.prefs.remember_session,
                });
            }
            if before.8 != after.8 {
                if let Some(media) = &mut self.media {
                    media.set_push_to_talk_key(self.prefs.ptt_key);
                }
            }
            self.defer_preferences();
        }
        if audio_before != self.prefs.audio {
            // The pinned counter is drawn before controls; refresh it immediately.
            ctx.request_repaint();
        }
    }

    fn account_page(&mut self, ui: &mut egui::Ui) {
        controls::page_heading(
            ui,
            "My Account",
            "Your account belongs to the community you are connected to.",
        );
        self.central_account_settings(ui);
        self.account_link_settings(ui);
        let Some(user) = self.session.as_ref().map(|session| session.user.clone()) else {
            controls::description(ui, "Connect to a community to manage your account.");
            return;
        };
        egui::Frame::none()
            .fill(c_panel())
            .rounding(8.0)
            .show(ui, |ui| {
                let (cover, _) =
                    ui.allocate_exact_size(Vec2::new(ui.available_width(), 70.0), Sense::hover());
                ui.painter().rect_filled(
                    cover,
                    Rounding {
                        nw: 8.0,
                        ne: 8.0,
                        sw: 0.0,
                        se: 0.0,
                    },
                    c_accent(),
                );
                egui::Frame::none()
                    .inner_margin(Margin::same(20.0))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            self.user_avatar(ui, &user, 64.0, false);
                            ui.vertical(|ui| {
                                ui.label(
                                    RichText::new(profile_ui::display_name(&user))
                                        .size(font_size(22.0))
                                        .strong(),
                                );
                                controls::description(
                                    ui,
                                    &format!(
                                        "{} · {}",
                                        self.bootstrap
                                            .as_ref()
                                            .map(|b| b.server_name.as_str())
                                            .unwrap_or("Community"),
                                        user.role
                                    ),
                                );
                            });
                        });
                        ui.add_space(20.0);
                        controls::nav_group(ui, "USERNAME");
                        ui.label(&user.username);
                        ui.add_space(12.0);
                        egui::CollapsingHeader::new("Advanced")
                            .id_source(("account-advanced", &self.login.url, &user.id))
                            .default_open(false)
                            .show(ui, |ui| {
                                controls::nav_group(ui, "ACCOUNT ID");
                                controls::description(ui, &user.id);
                                if ui.small_button("Copy account ID").clicked() {
                                    ui.output_mut(|output| output.copied_text = user.id.clone());
                                }
                            });
                    });
            });
        ui.add_space(20.0);
        if ui.add(secondary("Edit Profile")).clicked() {
            self.settings_tab = SettingsTab::Profiles;
        }
        self.account_settings_ui(ui);
    }

    fn privacy_page(&mut self, ui: &mut egui::Ui) {
        controls::page_heading(ui, "Privacy & Safety", "Control what this PC remembers.");
        if let Some(error) = &self.preferences_storage_error {
            ui.add(egui::Label::new(RichText::new(error).color(c_bad())).wrap(true));
            controls::description(ui, "Your existing preferences file was preserved. If it cannot be unlocked, settings stay in memory for this session; restore the original file and restart Relay.");
            controls::divider(ui);
        }
        controls::switch_row(
            ui,
            "remember-login",
            "Remember my login",
            "Keep this community's session in the Windows credential vault.",
            &mut self.prefs.remember_session,
        );
        controls::divider(ui);
        controls::switch_row(
            ui,
            "remember-drafts",
            "Remember message drafts",
            "Restore unfinished messages after restarting Relay. Preferences and drafts are protected by Windows for this PC's user account.",
            &mut self.prefs.remember_drafts,
        );
        if ui.add(secondary("Clear Saved Drafts")).clicked() {
            self.prefs.conversations.clear_drafts();
            self.drafts = DraftStore::default();
            self.scoped_drafts.clear();
            self.draft.clear();
            self.attachments.clear();
            self.reply = None;
            self.save_preferences();
            self.notice = Some(("Saved drafts cleared".into(), Instant::now()));
        }
        controls::divider(ui);
        controls::section_heading(ui,"Community privacy","The server's owner controls access and retention. Private channels and direct messages follow server permissions.");
        controls::description(ui,"Passwords are never stored in your local preferences. Signing out revokes the current server session.");
        controls::section_heading(
            ui,
            "Encryption privacy",
            "Verify devices in Encryption settings before sharing protected messages or calls.",
        );
        controls::description(ui,"The server cannot recover your encryption keys. Account, channel, membership, timing and traffic metadata remain visible to the server.");
        controls::divider(ui);
        self.game_activity_settings(ui);
    }

    /// How Relay reaches a community. Automatic is right for almost everyone;
    /// the other choices are for a friend who needs a particular route.
    fn connection_route_settings(&mut self, ui: &mut egui::Ui) {
        let capabilities = crate::route_choice::Capabilities::current();
        controls::nav_group(ui, "HOW TO CONNECT");
        controls::description(
            ui,
            "Used when you join a community with an invitation. Communities you already joined keep the way they connect.",
        );
        let mut changed = false;
        for mode in crate::route_choice::ConnectionMode::ALL {
            let unavailable =
                mode == crate::route_choice::ConnectionMode::Iroh && !capabilities.iroh;
            ui.add_enabled_ui(!unavailable, |ui| {
                changed |= ui
                    .radio_value(&mut self.prefs.connection.mode, mode, mode.label())
                    .changed();
            });
            let explanation = if unavailable {
                "Not part of this version of Relay yet."
            } else {
                mode.explanation()
            };
            ui.horizontal(|ui| {
                ui.add_space(24.0);
                ui.add(egui::Label::new(RichText::new(explanation).color(c_muted())).wrap(true));
            });
        }
        if unavailable_choice_saved(&self.prefs.connection, capabilities) {
            self.prefs.connection.mode = crate::route_choice::ConnectionMode::Automatic;
            changed = true;
        }
        ui.collapsing("Advanced", |ui| {
            changed |= ui
                .checkbox(
                    &mut self.prefs.connection.allow_plain_public_login,
                    "Allow signing in without encryption over the internet",
                )
                .changed();
            controls::description(
                ui,
                "Only for an address that starts with http:// and is not on your home network. Anyone on the way can read your password. Automatic never uses such an address unless this is on. If you need it only once, Relay asks when you join.",
            );
        });
        if changed {
            self.save_preferences();
        }
        controls::divider(ui);
    }

    fn connections_page(&mut self, ui: &mut egui::Ui) {
        controls::page_heading(
            ui,
            "Connections",
            "How Relay connects, and your saved self-hosted communities.",
        );
        self.connection_route_settings(ui);
        for community in self.prefs.communities.clone() {
            egui::Frame::none()
                .fill(c_card())
                .rounding(6.0)
                .inner_margin(Margin::same(16.0))
                .show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.vertical(|ui| {
                            ui.label(RichText::new(&community.name).strong());
                            controls::description(ui, &community.url);
                        });
                        if community.url == self.login.url {
                            ui.label(RichText::new("Connected").color(c_good()));
                        } else if ui.add(secondary("Switch")).clicked() {
                            self.switch_community(community.url.clone());
                            self.close_settings();
                        }
                    });
                });
            ui.add_space(12.0);
        }
        if ui.add(primary("Add a Community")).clicked() {
            self.open_community_dialog();
        }
    }

    fn voice_video_page(&mut self, ui: &mut egui::Ui) {
        controls::page_heading(
            ui,
            "Voice & Video",
            "Make sure you sound and look your best.",
        );
        ui.horizontal(|ui| {
            for (page, label) in [
                (VoiceVideoTab::Voice, "Voice"),
                (VoiceVideoTab::Video, "Video"),
                (VoiceVideoTab::Debugging, "Debugging"),
            ] {
                if controls::top_tab(
                    ui,
                    ("voice-video", page as u8),
                    label,
                    self.voice_video_tab == page,
                )
                .clicked()
                {
                    if self.voice_video_tab != page {
                        self.stop_source_preview();
                        self.stop_settings_mic_test();
                    }
                    self.voice_video_tab = page;
                }
            }
        });
        controls::divider(ui);
        match self.voice_video_tab {
            VoiceVideoTab::Voice => {
                self.voice_page(ui);
                // Next to the pinned delay budget it explains.
                ui.add_space(16.0);
                voice_delay_details(
                    ui,
                    &self.prefs.audio,
                    self.media
                        .as_ref()
                        .and_then(|m| m.network_stats().output_timing)
                        .map(|t| t.period_ms),
                );
            }
            VoiceVideoTab::Video => self.video_page(ui),
            VoiceVideoTab::Debugging => self.debug_page(ui),
        }
    }

    fn voice_page(&mut self, ui: &mut egui::Ui) {
        let width = ui.available_width();
        if width >= 480.0 {
            ui.columns(2, |columns| {
                controls::device_combo(
                    &mut columns[0],
                    "settings-input",
                    "INPUT DEVICE",
                    &mut self.prefs.audio.input_device,
                    &self.inventory.audio.inputs,
                );
                controls::device_combo(
                    &mut columns[1],
                    "settings-output",
                    "OUTPUT DEVICE",
                    &mut self.prefs.audio.output_device,
                    &self.inventory.audio.outputs,
                );
                percent_slider(
                    &mut columns[0],
                    "INPUT VOLUME",
                    &mut self.prefs.audio.microphone_gain,
                    0.0..=4.0,
                );
                percent_slider(
                    &mut columns[1],
                    "OUTPUT VOLUME",
                    &mut self.prefs.audio.playback_volume,
                    0.0..=2.0,
                );
            });
        } else {
            controls::device_combo(
                ui,
                "settings-input",
                "INPUT DEVICE",
                &mut self.prefs.audio.input_device,
                &self.inventory.audio.inputs,
            );
            percent_slider(
                ui,
                "INPUT VOLUME",
                &mut self.prefs.audio.microphone_gain,
                0.0..=4.0,
            );
            controls::device_combo(
                ui,
                "settings-output",
                "OUTPUT DEVICE",
                &mut self.prefs.audio.output_device,
                &self.inventory.audio.outputs,
            );
            percent_slider(
                ui,
                "OUTPUT VOLUME",
                &mut self.prefs.audio.playback_volume,
                0.0..=2.0,
            );
        }
        if ui
            .small_button(if self.inventory_rx.is_some() {
                "Finding devices…"
            } else {
                "Refresh Devices"
            })
            .clicked()
        {
            self.refresh_devices();
        }
        bluetooth_hint(ui, &self.inventory.audio, &self.prefs.audio);
        controls::section_heading(
            ui,
            "Receive Latency",
            "Choose how much incoming voice Relay buffers before playback.",
        );
        let custom_mode_id = egui::Id::new("settings-custom-receive-latency");
        let mut latency_mode = if ui
            .ctx()
            .data(|data| data.get_temp::<bool>(custom_mode_id))
            .unwrap_or(false)
        {
            LatencyMode::Custom
        } else {
            self.prefs.audio.latency_mode()
        };
        let previous_mode = latency_mode;
        egui::ComboBox::from_id_source("settings-receive-latency")
            .width(ui.available_width().min(320.0))
            .selected_text(match latency_mode {
                LatencyMode::Adaptive => "Automatic · 10–80 ms · Recommended",
                LatencyMode::LowLatency => "Low latency · 10 ms · Stable networks",
                LatencyMode::Balanced => "Balanced · 40 ms",
                LatencyMode::Custom => "Custom receive buffer",
            })
            .show_ui(ui, |ui| {
                for (mode, label) in [
                    (LatencyMode::Adaptive, "Automatic · 10–80 ms · Recommended"),
                    (
                        LatencyMode::LowLatency,
                        "Low latency · 10 ms · Stable networks",
                    ),
                    (LatencyMode::Balanced, "Balanced · 40 ms"),
                    (LatencyMode::Custom, "Custom receive buffer"),
                ] {
                    let response = ui.selectable_value(&mut latency_mode, mode, label);
                    if response.clicked() || response.changed() {
                        ui.memory_mut(|memory| memory.close_popup());
                    }
                }
            });
        if latency_mode != previous_mode {
            self.prefs.audio.set_latency_mode(latency_mode);
            ui.ctx().data_mut(|data| {
                data.insert_temp(custom_mode_id, latency_mode == LatencyMode::Custom);
            });
        }
        if latency_mode == LatencyMode::Custom {
            ui.spacing_mut().slider_width = (ui.available_width() - 140.0).max(40.0);
            ui.add(
                egui::Slider::new(&mut self.prefs.audio.jitter_ms, 5..=120)
                    .step_by(5.0)
                    .suffix(" ms"),
            );
        }
        controls::description(ui,"Automatic is the default for new profiles. It starts at 10 ms and adds buffering separately for each speaker only when their audio arrives unevenly. Low latency uses a fixed 10 ms target with less room for uneven arrival, so use it on a stable wired or local connection. If audio breaks up, choose Automatic or Balanced. These are receive targets, not total call delay. Save Changes applies your choice.");
        controls::nav_group(ui, "AUDIO PER PACKET");
        let mut packet_samples = self.prefs.audio.packet_samples();
        egui::ComboBox::from_id_source("settings-voice-packet-duration")
            .selected_text(format!("{} ms", self.prefs.audio.packet_duration_ms()))
            .show_ui(ui, |ui| {
                for (duration, label) in [
                    (120, "2.5 ms · 400 packets/s"),
                    (240, "5 ms · 200 packets/s"),
                    (480, "10 ms · 100 packets/s"),
                    (960, "20 ms · 50 packets/s"),
                ] {
                    let response = ui.selectable_value(&mut packet_samples, duration, label);
                    if response.clicked() || response.changed() {
                        ui.memory_mut(|memory| memory.close_popup());
                    }
                }
            });
        self.prefs.audio.set_packet_samples(packet_samples);
        controls::description(ui, "Shorter packets send audio sooner and use more network overhead. Everyone needs an updated Relay client to receive 2.5 ms packets. Noise suppression and echo cancellation process at least 10 ms at a time. Save Changes applies your choice.");
        controls::switch_row(
            ui,
            "recover-voice-packets",
            "Help others hear me through brief network losses",
            "Send extra speech recovery data in 10 or 20 ms packets. Uses a little more codec delay; the displayed delay budget includes it. Recovery needs the next packet to arrive on time and cannot repair every dropout. Off keeps the shortest codec delay.",
            &mut self.prefs.audio.recover_packet_loss,
        );
        if self.prefs.audio.recover_packet_loss && self.prefs.audio.packet_samples() < 480 {
            controls::description(ui, "Recovery is inactive with 2.5 or 5 ms packets. Choose 10 or 20 ms packets to use it.");
        }
        controls::section_heading(
            ui,
            "Mic Test",
            "Check your microphone level before joining a call.",
        );
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(
                    self.voice_channel.is_none(),
                    primary(if self.mic_test {
                        "Stop Testing"
                    } else {
                        "Let's Check"
                    }),
                )
                .clicked()
            {
                if self.mic_test {
                    if let Some(media) = &mut self.media {
                        media.stop_microphone_test();
                    }
                    self.mic_test = false;
                } else {
                    if let Some(media) = &mut self.media {
                        match media.preview_microphone(self.prefs.audio.clone()) {
                            Ok(()) => self.mic_test = true,
                            Err(error) => self.error = Some(error),
                        }
                    }
                }
            }
            let level = self
                .media
                .as_ref()
                .map(MediaEngine::input_level)
                .unwrap_or_default();
            let meter_width = ui.available_width().max(120.0);
            ui.add_sized(
                [meter_width, 24.0],
                egui::ProgressBar::new((level * 4.0).clamp(0.0, 1.0))
                    .fill(c_good())
                    .text(if self.mic_test {
                        "Listening locally…"
                    } else {
                        "Microphone level"
                    }),
            );
        });
        controls::description(
            ui,
            if self.voice_channel.is_some() {
                "Your microphone is already being used by the current call."
            } else {
                "Tests your current selections locally without saving or sending your voice. Stop and test again after making changes."
            },
        );
        controls::divider(ui);
        controls::section_heading(
            ui,
            "Input Mode",
            "Choose how your microphone is transmitted.",
        );
        for mode in [
            TransmissionMode::VoiceActivation,
            TransmissionMode::PushToTalk,
            TransmissionMode::Continuous,
        ] {
            controls::radio_value(
                ui,
                ("input-mode", mode as u32),
                &mut self.prefs.audio.transmission,
                mode,
                transmission_label(mode),
                match mode {
                    TransmissionMode::VoiceActivation => {
                        "Automatically transmit while you are speaking."
                    }
                    TransmissionMode::PushToTalk => "Only transmit while your shortcut is held.",
                    TransmissionMode::Continuous => {
                        "Keep transmitting until you mute your microphone."
                    }
                },
            );
        }
        if self.prefs.audio.transmission == TransmissionMode::PushToTalk {
            self.ptt_control(ui);
        }
        if self.prefs.audio.transmission == TransmissionMode::VoiceActivation {
            let mut automatic = self.prefs.audio.automatic_sensitivity();
            if ui
                .checkbox(&mut automatic, "Automatic input sensitivity (recommended)")
                .changed()
            {
                self.prefs.audio.automatic_sensitivity = Some(automatic);
            }
            if automatic {
                controls::description(
                    ui,
                    "Picks up quieter voices and adjusts to the quiet parts between words.",
                );
            } else {
                controls::section_heading(
                    ui,
                    "Input Sensitivity",
                    "Lower the threshold to pick up a quieter voice.",
                );
                ui.spacing_mut().slider_width = (ui.available_width() - 140.0).max(40.0);
                ui.add(
                    egui::Slider::new(&mut self.prefs.audio.activation_threshold, 0.001..=0.2)
                        .logarithmic(true)
                        .suffix(" RMS"),
                );
            }
        }
        controls::divider(ui);
        controls::section_heading(
            ui,
            "Voice Processing",
            "Clean up background noise and speaker echo.",
        );
        controls::switch_row(
            ui,
            "preserve-speech-start",
            "Keep the beginning of words",
            "Voice Activity includes the preceding 10 ms so quiet word beginnings are not cut off (20 ms with 20 ms packets). Adds that much speech delay; never records a push-to-talk lead-in.",
            &mut self.prefs.audio.preserve_speech_start,
        );
        controls::switch_row(
            ui,
            "automatic-microphone-volume",
            "Keep microphone volume even",
            "Boost quieter speech automatically and limit loud peaks.",
            &mut self.prefs.audio.automatic_microphone_volume,
        );
        controls::switch_row(
            ui,
            "noise-suppression",
            "Noise Suppression",
            "Reduce steady background noise with RNNoise.",
            &mut self.prefs.audio.noise_suppression,
        );
        #[cfg(feature = "native-rtc")]
        controls::switch_row(
            ui,
            "echo-cancellation",
            "Echo Cancellation",
            "Reduce speaker audio reaching your microphone. Experimental; off by default.",
            &mut self.prefs.audio.echo_cancellation,
        );
        controls::divider(ui);
        controls::nav_group(ui, "GAMING FEATURES");
        ui.checkbox(
            &mut self.prefs.audio.overlay_enabled,
            "Show speaking overlay",
        );
        if self.prefs.audio.overlay_enabled {
            ui.horizontal(|ui| {
                ui.label("Overlay position");
                ui.add(egui::DragValue::new(&mut self.prefs.audio.overlay_x).prefix("X "));
                ui.add(egui::DragValue::new(&mut self.prefs.audio.overlay_y).prefix("Y "));
            });
        }
        ui.checkbox(
            &mut self.prefs.audio.ducking_enabled,
            "Lower other applications while someone speaks",
        );
        if self.prefs.audio.ducking_enabled {
            ui.add(
                egui::Slider::new(&mut self.prefs.audio.ducking_volume, 0.0..=1.0)
                    .text("Other app volume"),
            );
        }
        ui.checkbox(
            &mut self.prefs.audio.whisper_enabled,
            "Enable private whispers - hold F8",
        );
        controls::description(ui, "Choose a person or room from its voice member menu. Cross-room voice requires permission to join the destination room. Whisper uses a separately verified private conversation; if its keys are unavailable, no whisper audio is sent.");
        #[cfg(windows)]
        {
            ui.checkbox(&mut self.prefs.audio.low_latency_device, "Use the shortest supported Windows audio period").on_hover_text("Relay asks the device for its smallest supported shared audio period. If the device cannot use it, Relay automatically uses the normal audio driver. Other apps can keep playing audio.");
            controls::description(ui, "The actual device period appears in Debugging while connected. Device timing is separate from the settings estimate.");
        }
        let direct_response = ui.checkbox(&mut self.prefs.audio.direct_voice_enabled, "Use a direct connection when faster").on_hover_text("Both participants must enable this. Your network address is shared with the other participant. Encrypted voice returns to the server if the direct route fails. If a server permission update is lost, it can take up to half a second to apply.");
        if ui
            .ctx()
            .data(|d| d.get_temp::<bool>(egui::Id::new("scroll-direct-voice")))
            .unwrap_or(false)
        {
            direct_response.scroll_to_me(Some(Align::Center));
            ui.ctx()
                .data_mut(|d| d.remove::<bool>(egui::Id::new("scroll-direct-voice")));
        }
        controls::description(ui, "Direct voice currently supports rooms with two people. Relay checks both participants and compares connection times; larger rooms, whispers and unavailable direct routes use the server.");
        ui.checkbox(&mut self.prefs.audio.local_voice_gateways, "Allow approved local voice gateways").on_hover_text("You can change this during a call. Save Changes applies it immediately; the call also has an Enable/Stop control. Nothing is used until Relay has measured a complete working voice path through the gateway.");
        controls::description(ui, "Advanced. In a community, helpers need the accepted Voice gateway role under Hosts. In a private Friends call, either joined participant can volunteer by enabling this option; no server role is granted. Only people in that exact live call can use its gateway. Relay measures the full voice path before using it and falls back to the normal route if it fails. Relay's built-in private connection uses a separate gateway route, keeping your own call untouched. Other per-person tunnels keep their normal route. Voice stays encrypted as usual. Video uses a separate path.");
        ui.checkbox(
            &mut self.prefs.audio.voice_qos,
            "Mark voice packets as high priority",
        );
        controls::description(ui, "Turn off if your router has trouble with it. Applies when you next join a call; the network may ignore this hint.");
        ui.checkbox(
            &mut self.prefs.audio.tcp_fallback_enabled,
            "Use TCP voice if UDP is blocked",
        );
        controls::description(ui, "Account-free connections use this fallback automatically when voice packets cannot get through.");
        ui.checkbox(
            &mut self.prefs.audio.priority_enabled,
            "Enable priority speakers",
        );
        if self.prefs.audio.priority_enabled {
            ui.add(
                egui::Slider::new(&mut self.prefs.audio.priority_volume, 0.0..=1.0)
                    .text("Other speakers volume"),
            );
        }
        controls::description(ui, "Mark a priority speaker from their voice menu. The overlay is a click-through floating window for windowed/borderless games. Each option is independent and applies with Save Changes.");
        controls::description(
            ui,
            "Save Changes applies your voice settings. Extra timing controls are under Debugging.",
        );
    }

    fn ptt_control(&mut self, ui: &mut egui::Ui) {
        controls::section_heading(
            ui,
            "Shortcut",
            "Hold this key to talk, even when Relay is in the background.",
        );
        egui::ComboBox::from_id_source("settings-ptt-key")
            .width(ui.available_width().min(300.0))
            .selected_text(ptt_key_name(self.prefs.ptt_key))
            .show_ui(ui, |ui| {
                for (key, label) in [
                    (0xA4, "Left Alt"),
                    (0xA2, "Left Ctrl"),
                    (0x56, "V"),
                    (0x20, "Space"),
                    (0x05, "Mouse button 4"),
                    (0x06, "Mouse button 5"),
                ] {
                    ui.selectable_value(&mut self.prefs.ptt_key, key, label);
                }
            });
    }

    fn video_page(&mut self, ui: &mut egui::Ui) {
        controls::section_heading(
            ui,
            "Camera",
            "Choose a camera and check it before publishing video.",
        );
        let previous_source = self.camera_source.clone();
        source_combo(
            ui,
            "CAMERA",
            "settings-camera",
            &mut self.camera_source,
            &self.inventory.cameras,
        );
        if previous_source != self.camera_source {
            self.stop_source_preview();
        }
        egui::Frame::none()
            .fill(c_rail())
            .rounding(8.0)
            .inner_margin(Margin::same(20.0))
            .show(ui, |ui| {
                #[cfg(feature = "native-rtc")]
                if let Some(preview) = self
                    .source_preview
                    .as_ref()
                    .filter(|preview| preview.camera)
                {
                    preview.show(ui);
                    return;
                }
                ui.vertical_centered(|ui| {
                    ui.add_space(24.0);
                    ui.label(
                        RichText::new("Camera Preview")
                            .size(font_size(18.0))
                            .color(c_muted()),
                    );
                    controls::description(ui, "Your camera opens only when you choose Test Video.");
                    ui.add_space(24.0);
                });
            });
        #[cfg(feature = "native-rtc")]
        {
            let active = self
                .source_preview
                .as_ref()
                .is_some_and(|preview| preview.camera);
            if ui
                .add_enabled(
                    active || self.camera_source.is_some(),
                    primary(if active { "Stop Preview" } else { "Test Video" }),
                )
                .clicked()
            {
                if active {
                    self.stop_source_preview();
                } else if let Some(source) = self
                    .inventory
                    .cameras
                    .iter()
                    .find(|source| Some(&source.id) == self.camera_source.as_ref())
                    .cloned()
                {
                    self.stop_source_preview();
                    match source_preview::SourcePreview::new(source, true) {
                        Ok(preview) => self.source_preview = Some(preview),
                        Err(error) => self.error = Some(error),
                    }
                }
            }
        }
        controls::section_heading(
            ui,
            "Camera Quality",
            "Resolution and frame rate for your next camera session.",
        );
        capture_options_ui(
            ui,
            "settings-camera-quality",
            &mut self.prefs.camera_options,
            cfg!(feature = "native-rtc"),
        );
        controls::divider(ui);
        controls::section_heading(
            ui,
            "Screen Share",
            "Choose a display or application window each time you go live.",
        );
        capture_options_ui(
            ui,
            "settings-screen-quality",
            &mut self.prefs.screen_options,
            cfg!(feature = "native-rtc"),
        );
        controls::switch_row(ui,"default-app-audio","Share Application Audio","Include the selected window's process audio when screen sharing. You can change this in the share picker.",&mut self.prefs.screen_options.application_audio);
        controls::divider(ui);
        controls::section_heading(
            ui,
            "Video Encoding",
            "Select automatic hardware acceleration or a software encoder.",
        );
        codec_options_ui(
            ui,
            "settings-screen-codec",
            "SCREEN SHARE ENCODER",
            &mut self.prefs.screen_options.codec,
        );
        codec_options_ui(
            ui,
            "settings-camera-codec",
            "CAMERA ENCODER",
            &mut self.prefs.camera_options.codec,
        );
        controls::description(ui,"Automatic tries hardware H.264 and can fall back to software VP8. Target frame rates depend on your device and connection. Restart sharing to apply changes.");
        if ui.small_button("Refresh Cameras and Screens").clicked() {
            self.refresh_devices();
        }
        for warning in &self.inventory.warnings {
            ui.label(RichText::new(warning).color(c_bad()));
        }
    }

    fn debug_page(&mut self, ui: &mut egui::Ui) {
        controls::section_heading(
            ui,
            "Advanced Voice Settings",
            "Tune quality and buffering for your connection.",
        );
        ui.spacing_mut().slider_width = (ui.available_width() - 140.0).max(40.0);
        controls::nav_group(ui, "BITRATE");
        ui.add(
            egui::Slider::new(&mut self.prefs.audio.bitrate_kbps, 16..=128)
                .step_by(16.0)
                .suffix(" kbps"),
        );
        controls::nav_group(ui, "RECEIVE BUFFER");
        ui.add_enabled(
            !self.prefs.audio.adaptive_jitter,
            egui::Slider::new(&mut self.prefs.audio.jitter_ms, 5..=120)
                .step_by(5.0)
                .suffix(" ms"),
        );
        controls::description(
            ui,
            if self.prefs.audio.adaptive_jitter {
                "Automatic receive buffering is enabled. Choose a manual profile on the Voice tab to set a fixed target."
            } else {
                "A smaller receive buffer reduces delay; a larger buffer tolerates uneven packet arrival."
            },
        );
        controls::nav_group(ui, "VOICE ACTIVITY HOLD");
        ui.add(
            egui::Slider::new(&mut self.prefs.audio.activation_hold_ms, 0..=1000)
                .step_by(50.0)
                .suffix(" ms"),
        );
        #[cfg(feature = "native-rtc")]
        controls::nav_group(ui, "ECHO DELAY");
        #[cfg(feature = "native-rtc")]
        ui.add_enabled(
            self.prefs.audio.echo_cancellation,
            egui::Slider::new(&mut self.prefs.audio.echo_delay_ms, 0..=250).suffix(" ms"),
        );
        controls::divider(ui);
        controls::section_heading(
            ui,
            "Connection Diagnostics",
            "Live counters for the current call.",
        );
        if let Some(media) = self.media.as_ref().filter(|media| media.is_active()) {
            let stats = media.network_stats();
            self.connection_path_details(ui);
            ui.label(format!(
                "Packets sent: {}   Received: {}",
                stats.sent_packets, stats.received_packets
            ));
            ui.label(format!(
                "Rejected: {}   Input drops: {}   Playback drops: {}",
                stats.rejected_packets, stats.dropped_capture_frames, stats.dropped_playback_frames
            ));
            ui.label(format!(
                "Late frames: {}   Reordered: {}   Concealed: {}",
                stats.late_voice_frames, stats.reordered_voice_frames, stats.concealed_voice_frames
            ));
            ui.label(
                stats
                    .receive_target_ms
                    .map(|ms| format!("Highest current receive target: {ms} ms"))
                    .unwrap_or_else(|| "Receive target: waiting for a speaker".into()),
            );
            ui.label(format!(
                "Automatic adjustment: {} held frames · {} quiet intervals omitted",
                stats.adaptive_held_frames, stats.adaptive_quiet_skips
            ));
            for (label, timing) in [
                ("Microphone", stats.input_timing),
                ("Speakers", stats.output_timing),
            ] {
                if let Some(timing) = timing {
                    ui.label(format!("{label}: {:.2} ms shared audio period (default {:.2} ms); device buffer {:.2} ms", timing.period_ms, timing.default_period_ms, timing.buffer_ms));
                } else {
                    ui.label(format!(
                        "{label}: compatibility audio driver; device timing unavailable"
                    ));
                }
            }
            controls::description(ui, "An audio period is the driver's processing interval. Buffer capacity and the settings estimate do not measure complete mouth-to-ear delay.");
            controls::description(ui, "Direct routing uses fresh server permission checks and switches back after missed probes. Connection RTT is separate from the settings delay estimate.");
            controls::description(
                ui,
                if stats.tcp_active {
                    "Voice connected through TCP fallback. UDP recovery probes continue."
                } else if stats.udp_reachable {
                    "UDP reachable"
                } else if stats.udp_recovery_remaining_ms.is_some() {
                    "Voice is reconnecting. Disconnecting cancels recovery. If the connection does not return, join the room again."
                } else if stats.udp_probe_timed_out {
                    "UDP probe timed out. Check the host and firewall."
                } else {
                    "UDP status pending."
                },
            );
        } else {
            controls::description(
                ui,
                "Join a voice channel to see live connection diagnostics.",
            );
        }
        controls::description(
            ui,
            "Relay RTT excludes microphone capture, encoding and playback delay.",
        );
        #[cfg(feature = "native-rtc")]
        if let Some(rtc) = &self.rtc {
            ui.label(format!("Decoded video frames: {}", rtc.decoded_frames()));
            ui.label(if rtc.is_application_audio_active() {
                "Application audio: publishing"
            } else {
                "Application audio: off"
            });
            for (label, camera) in [("Screen", false), ("Camera", true)] {
                if let Some(d) = rtc.source_diagnostics(camera) {
                    ui.label(format!(
                        "{label}: {} × {} · {:.1} / {} fps · {}",
                        d.actual_width,
                        d.actual_height,
                        d.delivered_fps,
                        d.target_fps,
                        d.video_codec
                    ));
                    controls::description(ui, &d.source);
                    if let Some(adaptation) = &d.adaptive_output {
                        controls::description(ui, adaptation);
                        ui.label(format!("RTC quality samples: {}", d.adaptive_samples));
                    }
                }
            }
        }
        for (label, capture) in [("Screen", &self.screen), ("Camera", &self.camera)] {
            if let Some(capture) = capture {
                let d = capture.diagnostics();
                ui.label(format!(
                    "{label}: {} × {} · {:.1} / {} fps · {}",
                    d.actual_width, d.actual_height, d.delivered_fps, d.target_fps, d.video_codec
                ));
                controls::description(ui, &d.source);
            }
        }
        controls::divider(ui);
        if ui
            .add(secondary("Reset Voice and Video Settings"))
            .clicked()
        {
            self.prefs.audio = AudioSettings::default();
            self.prefs.screen_options = CaptureOptions::default();
            self.prefs.camera_options = CaptureOptions::default();
            self.defer_preferences();
        }
    }

    fn appearance_page(&mut self, ui: &mut egui::Ui) {
        controls::page_heading(ui, "Appearance", "Make Relay feel like home.");
        controls::section_heading(
            ui,
            "Theme",
            "Choose the background and contrast you prefer.",
        );
        let columns = ((ui.available_width() + 8.0) / 106.0)
            .floor()
            .clamp(1.0, 5.0) as usize;
        egui::Grid::new("settings-theme-grid")
            .num_columns(columns)
            .min_col_width(98.0)
            .max_col_width(98.0)
            .spacing(Vec2::new(8.0, 8.0))
            .show(ui, |ui| {
                for (index, (theme, label)) in [
                    (Theme::Dark, "Dark"),
                    (Theme::Onyx, "Onyx"),
                    (Theme::Ash, "Ash"),
                    (Theme::Light, "Light"),
                    (Theme::System, "System"),
                ]
                .into_iter()
                .enumerate()
                {
                    let colors = crate::appearance::palette(theme);
                    let selected = self.prefs.appearance.theme == theme;
                    egui::Frame::none()
                        .fill(c_panel())
                        .rounding(6.0)
                        .inner_margin(Margin::same(8.0))
                        .show(ui, |ui| {
                            ui.vertical(|ui| {
                                let (rect, response) =
                                    ui.allocate_exact_size(Vec2::new(82.0, 64.0), Sense::click());
                                ui.painter().rect_filled(rect, 4.0, colors.bg);
                                ui.painter().rect_filled(
                                    Rect::from_min_size(rect.min, Vec2::new(24.0, 64.0)),
                                    4.0,
                                    colors.panel,
                                );
                                ui.painter().rect_filled(
                                    Rect::from_min_size(
                                        rect.min + Vec2::new(30.0, 12.0),
                                        Vec2::new(42.0, 6.0),
                                    ),
                                    3.0,
                                    colors.text,
                                );
                                ui.painter().rect_filled(
                                    Rect::from_min_size(
                                        rect.min + Vec2::new(30.0, 26.0),
                                        Vec2::new(34.0, 4.0),
                                    ),
                                    2.0,
                                    colors.muted,
                                );
                                ui.painter().rect_stroke(
                                    rect,
                                    4.0,
                                    Stroke::new(
                                        if selected { 3.0_f32 } else { 1.0_f32 },
                                        if selected { c_accent() } else { c_line() },
                                    ),
                                );
                                if response.clicked() {
                                    self.prefs.appearance.theme = theme;
                                }
                                response.widget_info(|| {
                                    egui::WidgetInfo::labeled(
                                        egui::WidgetType::SelectableLabel,
                                        label,
                                    )
                                });
                                if ui
                                    .selectable_label(selected, label)
                                    .on_hover_text(if theme == Theme::System {
                                        "Sync with your computer's light or dark theme"
                                    } else {
                                        label
                                    })
                                    .clicked()
                                {
                                    self.prefs.appearance.theme = theme;
                                }
                            });
                        });
                    if (index + 1) % columns == 0 {
                        ui.end_row();
                    }
                }
            });
        controls::divider(ui);
        controls::section_heading(
            ui,
            "Message Display",
            "Choose how much room conversations use.",
        );
        controls::radio_value(
            ui,
            "density-cozy",
            &mut self.compact,
            false,
            "Cozy",
            "Show avatars and comfortable message groups.",
        );
        controls::radio_value(
            ui,
            "density-compact",
            &mut self.compact,
            true,
            "Compact",
            "Fit more messages into less space.",
        );
        controls::slider_f32(
            ui,
            "font-scale",
            "Chat Font Scaling",
            "Change text size independently of interface zoom.",
            &mut self.prefs.appearance.font_scale,
            0.85..=1.4,
            "×",
        );
        controls::slider_f32(
            ui,
            "message-spacing",
            "Space Between Message Groups",
            "Leave more or less room between groups.",
            &mut self.prefs.appearance.message_spacing,
            0.0..=24.0,
            " px",
        );
        controls::slider_f32(
            ui,
            "interface-zoom",
            "Zoom Level",
            "Scale the whole interface, including sidebars and controls.",
            &mut self.ui_scale,
            0.85..=1.4,
            "×",
        );
        controls::divider(ui);
        controls::section_heading(
            ui,
            "Preview",
            "These preferences also apply to your conversations.",
        );
        egui::Frame::none()
            .fill(c_panel())
            .rounding(6.0)
            .inner_margin(Margin::same(16.0))
            .show(ui, |ui| {
                ui.horizontal_top(|ui| {
                    if !self.compact {
                        avatar(ui, "Mira", 36.0, false);
                    }
                    ui.vertical(|ui| {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("Mira").strong());
                            ui.label(
                                RichText::new("Today at 8:00 PM")
                                    .size(font_size(11.0))
                                    .color(c_muted()),
                            );
                        });
                        ui.label(
                            RichText::new("Hey! Ready to jump into voice?").size(font_size(15.0)),
                        );
                        ui.add_space(self.prefs.appearance.message_spacing);
                        ui.label(RichText::new("Alex").strong());
                        ui.label(
                            RichText::new("Absolutely. See you in the Lounge.")
                                .size(font_size(15.0)),
                        );
                    });
                });
            });
        controls::divider(ui);
        controls::section_heading(ui, "Chat Box", "Choose the help you see while writing.");
        controls::switch_row(
            ui,
            "convert-emoji-shortcodes",
            "Convert :names: to emoji",
            "Turn names such as :sob: and server emoji names into emoji when sending or saving an edit.",
            &mut self.prefs.appearance.convert_emoji_shortcodes,
        );
        controls::switch_row(
            ui,
            "emoji-suggestions",
            "Show emoji suggestions",
            "Show choices while typing :names. Tab or Enter inserts the selected choice.",
            &mut self.prefs.appearance.emoji_suggestions,
        );
        controls::switch_row(
            ui,
            "mention-suggestions",
            "Show mention suggestions",
            "Show people while typing @names. Tab or Enter inserts the selected person.",
            &mut self.prefs.appearance.mention_suggestions,
        );
        controls::switch_row(
            ui,
            "show-send-button",
            "Show send button",
            "Enter still sends a message. Shift+Enter starts a new line.",
            &mut self.prefs.appearance.show_send_button,
        );
    }

    fn accessibility_page(&mut self, ui: &mut egui::Ui) {
        controls::page_heading(
            ui,
            "Accessibility",
            "Make text and motion comfortable for you.",
        );
        controls::switch_row(
            ui,
            "reduced-motion",
            "Enable Reduced Motion",
            "Pause GIF animations throughout Relay.",
            &mut self.prefs.reduced_motion,
        );
        controls::switch_row(
            ui,
            "gif-autoplay",
            "Automatically Play GIFs",
            "Animate GIFs in messages and expanded previews. Reduced Motion takes priority.",
            &mut self.prefs.appearance.gif_autoplay,
        );
        controls::divider(ui);
        controls::slider_f32(
            ui,
            "accessible-font-scale",
            "Text Size",
            "Increase text size without enlarging every control.",
            &mut self.prefs.appearance.font_scale,
            0.85..=1.4,
            "×",
        );
        controls::section_heading(ui,"Keyboard Navigation","Use Tab and Shift+Tab to move between controls. Press Space or Enter to activate focused controls.");
        if ui.add(secondary("View Keybinds")).clicked() {
            self.settings_tab = SettingsTab::Keybinds;
        }
    }

    fn notifications_page(&mut self, ui: &mut egui::Ui) {
        self.notification_scopes_ui(ui);
        controls::page_heading(ui, "Notifications", "Choose what gets your attention.");
        controls::switch_row(
            ui,
            "desktop-notifications",
            "Enable Desktop Notifications",
            "Show notifications for messages arriving while Relay is in the background.",
            &mut self.prefs.notifications,
        );
        ui.add_enabled_ui(self.prefs.notifications, |ui| {
            controls::switch_row(
                ui,
                "mention-notifications",
                "Only Mentions and Direct Messages",
                "Keep notifications focused on messages for you.",
                &mut self.prefs.mentions_only,
            );
            controls::switch_row(
                ui,
                "notification-previews",
                "Show Message Previews",
                "Include message text in Windows notifications. Anyone who can see those notifications can read the preview; leave this off for private notifications.",
                &mut self.prefs.notification_previews,
            );
        });
        controls::divider(ui);
        controls::section_heading(
            ui,
            "Sounds",
            "Audio feedback for microphone and call controls.",
        );
        controls::switch_row(
            ui,
            "notification-sounds",
            "Enable Sounds",
            "Play short sounds for mute, unmute, deafen, joining and leaving.",
            &mut self.prefs.audio.sounds_enabled,
        );
        ui.add_enabled_ui(self.prefs.audio.sounds_enabled, |ui| {
            percent_slider(
                ui,
                "SOUND VOLUME",
                &mut self.prefs.audio.sound_volume,
                0.0..=1.0,
            );
        });
        if ui
            .add_enabled(self.prefs.audio.sounds_enabled, secondary("Test Sound"))
            .clicked()
        {
            if let Some(media) = &mut self.media {
                if let Err(error) = media.preview_sound(self.prefs.audio.clone()) {
                    self.error = Some(error);
                }
            }
        }
        controls::description(
            ui,
            "Test Sound previews your choices without saving. Save Changes applies them.",
        );
    }

    fn keybinds_page(&mut self, ui: &mut egui::Ui) {
        controls::page_heading(
            ui,
            "Keybinds",
            "Keep the controls you use most close at hand.",
        );
        self.ptt_control(ui);
        controls::divider(ui);
        controls::section_heading(
            ui,
            "Keyboard Shortcuts",
            "Built-in shortcuts for conversations and settings.",
        );
        for (action, key) in [
            ("Find a conversation", "Ctrl + K"),
            ("Search message history", "Ctrl + F"),
            ("Send a message", "Enter"),
            ("Add a new line", "Shift + Enter"),
            ("Close settings or a dialog", "Esc"),
        ] {
            egui::Frame::none()
                .fill(c_panel())
                .rounding(4.0)
                .inner_margin(Margin::same(12.0))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(action);
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.label(RichText::new(key).monospace().color(c_muted()));
                        });
                    });
                });
            ui.add_space(8.0);
        }
    }

    fn advanced_page(&mut self, ui: &mut egui::Ui) {
        controls::page_heading(
            ui,
            "Advanced",
            "Local preferences and connection information.",
        );
        #[cfg(windows)]
        {
            controls::section_heading(
                ui,
                "Window behavior",
                "Choose what happens when you close the window.",
            );
            let before = self.prefs.close_to_tray;
            controls::switch_row(ui, "close-to-tray", "Keep Relay running in the background",
                "The close button hides Relay in the notification area (hidden icons). Calls and hosted servers keep running. Right-click its icon and choose Quit Relay to stop everything.",
                &mut self.prefs.close_to_tray);
            if before != self.prefs.close_to_tray {
                self.defer_preferences();
            }
            if !self.tray.as_ref().is_some_and(|tray| tray.available()) {
                controls::description(ui, "The notification-area icon is unavailable. Closing the window will quit Relay.");
            }
            controls::description(ui, "When you quit, the window disappears immediately while your servers finish shutting down in the background.");
            controls::divider(ui);
        }
        controls::section_heading(
            ui,
            "About Relay",
            "A native Rust desktop app with self-hosted communities.",
        );
        ui.label(crate::build_metadata::current().label());
        ui.collapsing("Build details", |ui| {
            ui.add(egui::Label::new(crate::build_metadata::current().details()).wrap(true));
            if ui.add(secondary("Copy build information")).clicked() {
                let text = crate::build_metadata::current().json().to_string();
                ui.output_mut(|output| output.copied_text = text);
            }
        });
        ui.hyperlink_to(
            "Emoji artwork: Twemoji (CC BY 4.0)",
            "https://github.com/jdecked/twemoji",
        );
        controls::description(ui, "Twemoji by Twitter and contributors / jdecked. Artwork resized and packed for Relay. Unicode 17 data; emoji shortcodes by iamcal/emoji-data (MIT).");
        controls::description(ui, &self.login.url);
        controls::divider(ui);
        let in_call = self.voice_channel.is_some()
            || self.audio_connecting
            || self.joining_voice.is_some()
            || self.rtc_request.is_some();
        self.update_ui.show(
            ui,
            in_call,
            self.own_local_server
                || self.busy
                || !self.pending_sends.is_empty()
                || self.network.update_busy(),
        );
        controls::divider(ui);
        controls::section_heading(
            ui,
            "Online GIFs",
            "Search uses GIPHY. GIFs from your PC work without an online provider.",
        );
        if ui
            .checkbox(
                &mut self.prefs.online_gif_previews,
                "Show online GIF previews",
            )
            .changed()
        {
            self.defer_preferences();
        }
        self.online_gif_setup(ui);
        controls::divider(ui);
        controls::section_heading(
            ui,
            "Local Preferences",
            "Appearance, saved community addresses and optional drafts stay on this PC.",
        );
        if ui.add(secondary("Reset Appearance Settings")).clicked() {
            self.prefs.appearance = AppearanceSettings::default();
            self.ui_scale = 1.0;
            self.compact = false;
            self.prefs.reduced_motion = false;
        }
        controls::description(
            ui,
            "This resets themes, text sizes, message layout and motion preferences.",
        );
        controls::divider(ui);
        if ui.add(secondary("Open Server Settings")).clicked() {
            self.settings_tab = SettingsTab::Server;
        }
    }
}

fn percent_slider(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut f32,
    range: std::ops::RangeInclusive<f32>,
) {
    ui.add_space(12.0);
    ui.horizontal(|ui| {
        controls::nav_group(ui, label);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(RichText::new(format!("{:.0}%", *value * 100.0)).color(c_muted()));
        });
    });
    ui.spacing_mut().slider_width = ui.available_width().max(40.0);
    ui.add(egui::Slider::new(value, range).show_value(false));
    ui.add_space(8.0);
}

/// A saved choice this build cannot honour falls back to Automatic.
fn unavailable_choice_saved(
    prefs: &crate::route_choice::ConnectionPrefs,
    capabilities: crate::route_choice::Capabilities,
) -> bool {
    prefs.mode == crate::route_choice::ConnectionMode::Iroh && !capabilities.iroh
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bluetooth_hint_renders_for_a_selected_or_default_wireless_device() {
        let context = egui::Context::default();
        configure(&context);
        let devices = crate::audio_settings::AudioDevices {
            bluetooth: ["Wireless headset".to_owned()].into_iter().collect(),
            default_output: Some("Wireless headset".into()),
            ..Default::default()
        };
        let result = context.run(egui::RawInput::default(), |context| {
            egui::CentralPanel::default().show(context, |ui| {
                bluetooth_hint(ui, &devices, &AudioSettings::default())
            });
        });
        assert!(result.shapes.iter().any(|s| matches!(&s.shape, egui::epaint::Shape::Text(t) if t.galley.text().contains("Bluetooth can add"))));
    }

    #[test]
    fn pinned_delay_counter_wraps_and_stays_below_the_scrolling_settings() {
        let context = egui::Context::default();
        configure(&context);
        let applied = AudioSettings::default();
        let draft = AudioSettings {
            adaptive_jitter: true,
            packet_ms: 5,
            ..applied.clone()
        };
        for width in [700.0, 360.0, 280.0] {
            // A bottom panel takes its height from the previous frame, so the
            // first frame after a width change may overflow; check settled frames.
            for frame in 0..4 {
                let settled = frame > 0;
                let _ = context.run(
                    egui::RawInput {
                        screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(width, 420.0))),
                        ..Default::default()
                    },
                    |context| {
                        egui::CentralPanel::default().show(context, |ui| {
                            let pinned = egui::TopBottomPanel::bottom("delay-test-pinned")
                                .show_inside(ui, |ui| {
                                    voice_delay_counter(ui, &draft, &applied, None)
                                });
                            if !settled {
                                return;
                            }
                            assert!(context.screen_rect().contains_rect(pinned.response.rect));
                            assert!(
                                pinned.response.rect.height() < 160.0,
                                "counter leaves no room for controls"
                            );
                            let page = page_scroll("delay-test-page", true).show(ui, |ui| {
                                for _ in 0..60 {
                                    ui.label("A voice setting");
                                }
                            });
                            assert!(page.inner_rect.bottom() <= pinned.response.rect.top());
                        });
                    },
                );
            }
        }
        assert!(SettingsTab::VoiceVideo.matches("delay"));
        assert!(SettingsTab::VoiceVideo.matches("latency"));
    }

    struct RenderedPage {
        id: egui::Id,
        offset: f32,
        inner: Rect,
        action: Rect,
    }

    fn render_page(
        context: &egui::Context,
        capture_bottom: bool,
        width: f32,
        height: f32,
        rows: usize,
    ) -> RenderedPage {
        let mut page = None;
        let _ = context.run(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(width, height))),
                ..Default::default()
            },
            |context| {
                egui::CentralPanel::default().show(context, |ui| {
                    let rendered = page_scroll("settings-scroll-regression", capture_bottom)
                        .show(ui, |ui| {
                            for index in 0..rows {
                                ui.add(
                                    egui::Label::new(format!(
                                        "Setting {index}: a description that wraps when the settings column becomes narrow."
                                    ))
                                    .wrap(true),
                                );
                            }
                            let action = ui.button("Refresh Cameras and Screens").rect;
                            ui.add_space(32.0);
                            action
                        });
                    page = Some(RenderedPage {
                        id: rendered.id,
                        offset: rendered.state.offset.y,
                        inner: rendered.inner_rect,
                        action: rendered.inner,
                    });
                });
            },
        );
        page.unwrap()
    }

    #[test]
    fn capture_bottom_keeps_final_action_visible_after_content_and_window_resize() {
        let context = egui::Context::default();
        for (width, height, rows) in [
            (520.0, 300.0, 24),
            (330.0, 240.0, 24),
            (280.0, 190.0, 36),
            (420.0, 270.0, 16),
        ] {
            // Settle scrollbar geometry and wrapped text, as real capture frames do.
            for _ in 0..20 {
                render_page(&context, true, width, height, rows);
            }
            let page = render_page(&context, true, width, height, rows);
            assert!(
                page.offset > 0.0,
                "the page never reached its lower content"
            );
            assert!(
                page.inner.contains(page.action.center()),
                "final action {:?} is outside the visible page {:?}",
                page.action,
                page.inner,
            );
        }
    }

    #[test]
    fn ordinary_settings_page_starts_at_top_and_preserves_its_saved_offset() {
        let context = egui::Context::default();
        for _ in 0..20 {
            render_page(&context, false, 330.0, 240.0, 24);
        }
        let page = render_page(&context, false, 330.0, 240.0, 24);
        assert_eq!(page.offset, 0.0);
        assert!(!page.inner.contains(page.action.center()));

        let mut state = egui::scroll_area::State::load(&context, page.id).unwrap();
        state.offset.y = 120.0;
        state.store(&context, page.id);
        for _ in 0..3 {
            render_page(&context, false, 330.0, 240.0, 24);
        }
        let page = render_page(&context, false, 330.0, 240.0, 24);
        assert!((page.offset - 120.0).abs() < 0.01);
        assert!(!page.inner.contains(page.action.center()));
    }
}
