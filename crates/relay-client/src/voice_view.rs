//! R8.2: the voice room page and its call controls, moved unchanged out of
//! `app.rs`. Only the visibility of `voice_view` changed (`pub(super)`), so
//! the app can still call it.
use super::*;
use crate::voice_ui_state;

impl RelayApp {
    /// Who you hear in the call, one dot per person, and a hint when
    /// someone stays silent.
    fn sound_check(&mut self, ui: &mut egui::Ui, members: &[VoiceMember]) {
        let local_volume_issue = crate::audio_settings::LocalVolumeIssue::current(
            &self.audio_applied,
            self.muted,
            self.deafened,
        );
        if let Some(issue) = local_volume_issue {
            if voice_room_ui::local_volume_help(ui, issue) {
                self.apply_quick_audio_change(|audio| issue.restore(audio));
            }
            ui.add_space(8.0);
        }
        let own = self.session.as_ref().map(|s| s.user.id.clone());
        let now = Instant::now();
        let heard = if self.capture_path.is_some() {
            // Captures run no audio; the panel decides who shows as heard.
            self.capture_heard
                .iter()
                .map(|id| (id.clone(), now))
                .collect()
        } else {
            self.media
                .as_ref()
                .map(|media| media.received_voice())
                .unwrap_or_default()
        };
        let others: Vec<&VoiceMember> = members
            .iter()
            .filter(|m| own.as_ref() != Some(&m.user.id))
            .collect();
        self.sound_check_since
            .retain(|id, _| others.iter().any(|member| member.user.id == *id));
        if others.is_empty() {
            return;
        }
        for member in &others {
            self.sound_check_since
                .entry(member.user.id.clone())
                .or_insert(now);
        }
        let rows: Vec<voice_room_ui::SoundRow> = others
            .iter()
            .map(|member| voice_room_ui::SoundRow {
                name: profile_ui::display_name(&member.user),
                muted: member.muted,
                last_received: heard
                    .get(&member.user.id)
                    .filter(|received| {
                        self.sound_check_since
                            .get(&member.user.id)
                            .is_some_and(|joined| *received >= joined)
                    })
                    .map(|received| now.saturating_duration_since(*received)),
                together: self
                    .sound_check_since
                    .get(&member.user.id)
                    .map(|since| now.saturating_duration_since(*since))
                    .unwrap_or_default(),
            })
            .collect();
        let hint = if local_volume_issue.is_none() {
            voice_room_ui::sound_hint(self.deafened, &rows)
        } else {
            None
        };
        voice_room_ui::sound_check(ui, &rows, hint.as_deref());
        let fixture = ui
            .ctx()
            .data(|d| d.get_temp::<bool>(egui::Id::new("capture-voice-relay-hint")))
            .unwrap_or(false);
        let relayed = fixture
            || crate::connection_path::latest(&self.login.url).is_some_and(|r| {
                matches!(
                    r.kind,
                    crate::connection_path::PathKind::Relay
                        | crate::connection_path::PathKind::PeerRelay
                )
            });
        let tcp = self
            .media
            .as_ref()
            .is_some_and(|m| m.network_stats().tcp_active);
        let private = self
            .media
            .as_ref()
            .is_some_and(MediaEngine::private_network)
            || crate::embedded_network::routed(&self.login.url);
        if (others.len() == 1 || fixture)
            && crate::connection_path::suggest_direct(
                relayed,
                tcp,
                self.audio_applied.direct_voice_enabled,
                private,
            )
        {
            crate::settings_ui::description(ui, "Your voice goes through a relay server. In a two-person call, both of you can enable 'Use a direct connection when faster' in Voice & Video. It may reduce delay and shares your network address with the other person.");
            if ui.small_button("Open setting").clicked() {
                self.settings_open = true;
                self.settings_tab = SettingsTab::VoiceVideo;
                self.voice_video_tab = VoiceVideoTab::Voice;
                ui.ctx()
                    .data_mut(|d| d.insert_temp(egui::Id::new("scroll-direct-voice"), true));
            }
        }
        let quality_id = egui::Id::new(("voice-quality-help", &own, &self.voice_channel));
        let can_smooth = self.capture_path.is_none()
            && self.voice_connected()
            && !self.deafened
            && !self.audio_applied.adaptive_jitter
            && self.audio_applied.jitter_ms <= 10;
        if let Some(stats) = self
            .media
            .as_ref()
            .filter(|_| can_smooth)
            .map(|media| media.network_stats())
        {
            let mut watch = ui
                .ctx()
                .data(|data| data.get_temp::<voice_room_ui::QualityWatch>(quality_id))
                .unwrap_or_default();
            let show = watch.observe(now, stats.received_packets, stats.late_voice_frames);
            ui.ctx()
                .data_mut(|data| data.insert_temp(quality_id, watch));
            if show {
                ui.add_space(8.0);
                if voice_room_ui::steadier_audio_help(ui) {
                    self.apply_quick_audio_change(|audio| {
                        audio.set_latency_mode(crate::audio_settings::LatencyMode::Adaptive);
                    });
                    ui.ctx()
                        .data_mut(|data| data.remove::<voice_room_ui::QualityWatch>(quality_id));
                    ui.ctx().data_mut(|data| {
                        data.remove::<bool>(egui::Id::new("settings-custom-receive-latency"));
                    });
                }
            }
        } else {
            ui.ctx()
                .data_mut(|data| data.remove::<voice_room_ui::QualityWatch>(quality_id));
        }
        ui.add_space(20.0);
        // The hint appears after a while without anything new to draw.
        ui.ctx().request_repaint_after(Duration::from_secs(1));
    }

    pub(super) fn voice_view(&mut self, ui: &mut egui::Ui, name: &str, selected: &str) {
        self.stage_controls(ui, selected);
        let selected = selected.to_owned();
        let joined = self.voice_channel.as_ref() == Some(&selected);
        let selected_call = joined || self.joining_voice.as_ref() == Some(&selected);
        let channel = self.bootstrap.as_ref().and_then(|bootstrap| {
            bootstrap
                .channels
                .iter()
                .find(|channel| channel.id == selected)
        });
        let can_join = channel.is_some_and(|channel| channel.can_join);
        let can_stream = channel.is_some_and(|channel| channel.can_stream);
        let ready = joined && self.voice_ready();
        let needs_consent = self.call_needs_open_consent();
        egui::TopBottomPanel::bottom("voice-controls")
            .frame(egui::Frame::none().inner_margin(Margin::symmetric(16.0, 12.0)))
            .show_inside(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    let controls_width = if joined { 6.0 * 54.0 } else { 220.0 };
                    ui.add_space(((ui.available_width() - controls_width) * 0.5).max(0.0));
                    if self.current_voice_text_parent().is_none() {
                        if let Some(chat) = self
                            .bootstrap
                            .as_ref()
                            .and_then(|b| {
                                b.channels
                                    .iter()
                                    .find(|c| c.voice_parent_id.as_ref() == Some(&selected))
                            })
                            .cloned()
                        {
                            if call_control(ui, Icon::Chat, false, false, "Open voice channel chat")
                                .clicked()
                            {
                                self.select(Target::Channel(chat.id));
                            }
                        }
                    }
                    if joined {
                        if call_control(
                            ui,
                            Icon::Mic,
                            self.muted,
                            false,
                            if self.muted {
                                "Unmute microphone"
                            } else {
                                "Mute microphone"
                            },
                        )
                        .clicked()
                        {
                            self.toggle_mute();
                        }
                        if call_control(
                            ui,
                            Icon::Headphones,
                            self.deafened,
                            false,
                            if self.deafened { "Undeafen" } else { "Deafen" },
                        )
                        .clicked()
                        {
                            self.toggle_deafen();
                        }
                        if ui
                            .add_enabled_ui(
                                ready
                                    && (can_stream
                                        || self.source_active(false)
                                        || self.source_starting(false)),
                                |ui| {
                                    call_control(
                                        ui,
                                        Icon::Screen,
                                        self.source_active(false),
                                        false,
                                        if self.source_starting(false) {
                                            "Cancel screen share start"
                                        } else if self.source_active(false) {
                                            "Stop screen share"
                                        } else {
                                            "Start screen share"
                                        },
                                    )
                                },
                            )
                            .inner
                            .clicked()
                        {
                            self.screen_toggle();
                        }
                        if ui
                            .add_enabled_ui(
                                ready
                                    && (can_stream
                                        || self.source_active(true)
                                        || self.source_starting(true)),
                                |ui| {
                                    call_control(
                                        ui,
                                        Icon::Camera,
                                        self.source_active(true),
                                        false,
                                        if self.source_starting(true) {
                                            "Cancel camera start"
                                        } else if self.source_active(true) {
                                            "Stop camera"
                                        } else {
                                            "Start camera"
                                        },
                                    )
                                },
                            )
                            .inner
                            .clicked()
                        {
                            self.camera_toggle();
                        }
                        if call_control(ui, Icon::HangUp, false, true, "Disconnect").clicked() {
                            self.leave_voice();
                        }
                    } else if ui
                        .add_enabled(
                            can_join
                                && !needs_consent
                                && self.joining_voice.as_ref() != Some(&selected),
                            primary(if self.joining_voice.as_ref() == Some(&selected) {
                                "Joining…"
                            } else {
                                "Join voice"
                            })
                            .min_size(Vec2::new(130.0, 38.0)),
                        )
                        .clicked()
                    {
                        self.join_voice(selected.clone());
                    }
                });
                if let Some(reason) = self
                    .voice_encryption_error
                    .clone()
                    .filter(|_| selected_call)
                {
                    ui.add(egui::Label::new(RichText::new(reason).color(c_bad())).wrap(true));
                    let open = self.bootstrap.as_ref().is_some_and(|b| {
                        b.protection.level == relay_protocol::ProtectionLevel::Open
                    });
                    match voice_ui_state::pause_action(self.voice_reconnecting(), open) {
                        Some(voice_ui_state::PauseAction::RetryCall)
                            if ui.add(secondary("Try again")).clicked() =>
                        {
                            self.send(Command::CryptoRefresh);
                        }
                        Some(voice_ui_state::PauseAction::EncryptionDetails)
                            if ui.add(secondary("Encryption details")).clicked() =>
                        {
                            self.settings_tab = SettingsTab::Encryption;
                            self.settings_open = true;
                            self.send(Command::CryptoRefresh);
                        }
                        _ => {}
                    }
                }
                let video_restart_hint = match self.video_restart_needed {
                    [true, true] => Some("Camera and screen sharing stopped when the call changed. Start them again when ready."),
                    [true, false] => Some("Screen sharing stopped when the call changed. Start sharing again when ready."),
                    [false, true] => Some("Your camera stopped when the call changed. Start it again when ready."),
                    [false, false] => None,
                };
                if let Some(hint) = video_restart_hint.filter(|_| joined) {
                    ui.add(egui::Label::new(RichText::new(hint).size(font_size(TEXT_CAPTION)).color(c_muted())).wrap(true));
                }
                if self.rtc_reconnecting() {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Reconnecting video…");
                    });
                }
                #[cfg(feature = "native-rtc")]
                if joined
                    && ready
                    && !self.rtc_active()
                    && !self.rtc_starting()
                    && self
                        .bootstrap
                        .as_ref()
                        .is_some_and(|b| b.capabilities.iter().any(|cap| cap == "native-rtc-v1"))
                    && ui.add(secondary("Connect Video")).clicked()
                {
                    self.request_video_connection();
                }
                if !joined
                    && self.joining_voice.is_some()
                    && ui.add(secondary("Cancel join")).clicked()
                {
                    self.leave_voice();
                }
                if !can_join {
                    ui.label(
                        RichText::new("You do not have permission to join this room.")
                            .size(font_size(12.0))
                            .color(c_muted()),
                    );
                } else if !can_stream && joined {
                    ui.label(
                        RichText::new("Sharing is disabled by this room's permissions.")
                            .size(font_size(12.0))
                            .color(c_muted()),
                    );
                }
                if !joined {
                    ui.label(
                        RichText::new("Your microphone opens when you join.")
                            .size(font_size(TEXT_CAPTION))
                            .color(c_muted()),
                    );
                }
            });
        let members = self
            .bootstrap
            .as_ref()
            .map(|b| {
                b.voice_members
                    .iter()
                    .filter(|m| m.channel_id == selected)
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let status = voice_room_ui::room_status(voice_room_ui::RoomConnection {
            joined,
            encryption_paused: selected_call && self.voice_encryption_error.is_some(),
            reconnecting: selected_call && self.voice_reconnecting(),
            connected: self.voice_connected(),
            media_ready: ready,
        });
        if self.voice_channel.is_none() {
            self.sound_check_since.clear();
        }
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.add_space(24.0);
            voice_room_ui::room_header(ui, name, members.len(), status);
            ui.add_space(28.0);
            self.call_protection_notice(ui);
            if joined && (ready || self.capture_path.is_some()) {
                self.sound_check(ui, &members);
                if let Some(stats) = self.media.as_ref().filter(|_| self.capture_path.is_none()).map(|media| media.network_stats()) {
                    let friends_call = self.bootstrap.as_ref().is_some_and(|b| b.capabilities.iter().any(|cap| cap == "people-v1"));
                    if let Some(enabled) = voice_room_ui::local_gateway_controls(
                        ui,
                        self.audio_applied.local_voice_gateways,
                        stats.gateway_available,
                        stats.gateway_active,
                        friends_call,
                    ) {
                        self.apply_quick_audio_change(|audio| audio.local_voice_gateways = enabled);
                    }
                    ui.add_space(16.0);
                }
            }
            if members.is_empty() {
                ui.add_space(30.0);
                ui.vertical_centered(|ui| {
                    // The header already says "No one in voice".
                    icon(ui, Icon::Headphones, 60.0, c_muted());
                    ui.add_space(14.0);
                    ui.label(
                        RichText::new("Be the first to join.")
                            .size(font_size(13.0))
                            .color(c_muted()),
                    );
                });
            } else {
                let own = self.session.as_ref().map(|session| session.user.id.clone());
                let level =
                    self.media
                        .as_ref()
                        .map(|media| if self.muted { 0.0 } else { media.input_level() });
                let tiles: Vec<_> = members
                    .iter()
                    .map(|member| voice_room_ui::ParticipantTile {
                        user: &member.user,
                        avatar: self.avatar_texture(&member.user),
                        speaking: self.member_speaking(member),
                        muted: member.muted,
                        deafened: member.deafened,
                        sharing: member.sharing,
                        camera: member.camera,
                        own_level: level
                            .filter(|_| joined && own.as_ref() == Some(&member.user.id)),
                    })
                    .collect();
                // Centre the grid in the page below the header, unless a
                // whisper line or video tiles follow it; the closing 28-point
                // gap of the page stays reserved.
                let follows = self.focused_stream.is_some()
                    || self
                        .video
                        .values()
                        .any(|v| members.iter().any(|m| m.user.id == v.metadata.user.id))
                    || (joined
                        && self
                            .media
                            .as_ref()
                            .is_some_and(|media| media.active_settings().whisper_enabled));
                let space = if follows {
                    0.0
                } else {
                    ui.available_height() - 28.0
                };
                let responses = voice_room_ui::participant_grid(ui, &tiles, space);
                for (member, response) in members.iter().zip(responses) {
                    let display = profile_ui::display_name(&member.user);
                    if response.clicked() {
                        self.open_profile(member.user.clone());
                    }
                    response.on_hover_text(display).context_menu(|ui| {
                        if ui.button("View Profile").clicked() {
                            self.open_profile(member.user.clone());
                            ui.close_menu();
                        }
                        if self.voice_channel.is_some()
                            && self
                                .session
                                .as_ref()
                                .is_some_and(|s| s.user.id != member.user.id)
                        {
                            let key = self.peer_volume_key(&member.user.id);
                            let mut volume = self
                                .prefs
                                .peer_volumes
                                .get(&key)
                                .copied()
                                .unwrap_or_else(|| {
                                    self.media
                                        .as_ref()
                                        .map(|media| media.user_volume(&member.user.id))
                                        .unwrap_or(1.0)
                                });
                            let mut priority = self.prefs.priority_speakers.contains(&key);
                            if ui.checkbox(&mut priority, "Priority speaker").changed() {
                                if priority {
                                    self.prefs.priority_speakers.insert(key.clone());
                                } else {
                                    self.prefs.priority_speakers.remove(&key);
                                }
                                self.defer_preferences();
                            }
                            if self
                                .media
                                .as_ref()
                                .is_some_and(|media| media.active_settings().whisper_enabled)
                                && self.voice_channel.is_some()
                                && ui.button("Whisper to this person - hold F8").clicked()
                            {
                                if let Some(media) = &self.media {
                                    media.select_whisper(&member.user.id);
                                }
                                ui.close_menu();
                            }
                            if self
                                .media
                                .as_ref()
                                .is_some_and(|media| media.active_settings().whisper_enabled)
                                && self.voice_channel.is_some()
                                && ui.button("Shout to this room - hold F8").clicked()
                            {
                                let peers =
                                    self.bootstrap
                                        .as_ref()
                                        .map(|b| {
                                            b.voice_members
                                                .iter()
                                                .filter(|peer| {
                                                    peer.channel_id == member.channel_id
                                                        && self.session.as_ref().is_some_and(|s| {
                                                            s.user.id != peer.user.id
                                                        })
                                                })
                                                .map(|peer| peer.user.id.clone())
                                                .collect()
                                        })
                                        .unwrap_or_default();
                                if let Some(media) = &self.media {
                                    media.select_whisper_group(peers);
                                }
                                ui.close_menu();
                            }
                            ui.label("User Volume").on_hover_text(
                                "Changes this person's voice and shared application audio only for you.",
                            );
                            if ui
                                .add(
                                    egui::Slider::new(&mut volume, 0.0..=2.0)
                                        .custom_formatter(|v, _| format!("{:.0}%", v * 100.0)),
                                )
                                .changed()
                            {
                                if let Some(media) = &self.media {
                                    media.set_user_volume(&member.user.id, volume);
                                }
                                self.prefs.peer_volumes.insert(key, volume);
                                self.defer_preferences();
                            }
                        }
                    });
                }
            }
            if joined {
                if let Some(media) = &self.media {
                    if media.active_settings().whisper_enabled {
                        let targets = media.whisper_targets();
                        if !targets.is_empty() {
                            let names: Vec<_> = targets
                                .iter()
                                .map(|target| {
                                    self.bootstrap
                                        .as_ref()
                                        .and_then(|b| {
                                            b.voice_members.iter().find(|m| &m.user.id == target)
                                        })
                                        .map(|m| profile_ui::display_name(&m.user).to_owned())
                                        .unwrap_or_else(|| "Unavailable recipient".into())
                                })
                                .collect();
                            ui.label(format!(
                                "Private voice to {} - F8 - {}",
                                names.join(", "),
                                if targets.iter().all(|target| media.whisper_ready(target)) {
                                    "ready"
                                } else {
                                    "paused / preparing"
                                }
                            ));
                        } else {
                            ui.label("Choose recipients from a voice menu - hold F8");
                        }
                    }
                }
            }
            ui.add_space(28.0);
            if self.focused_stream.is_some()
                && ui.add(secondary("Back to Stream Gallery")).clicked()
            {
                self.focused_stream = None;
            }
            let mut tiles: Vec<_> = self
                .video
                .iter()
                .filter(|(_, v)| members.iter().any(|m| m.user.id == v.metadata.user.id))
                .filter(|(key, _)| {
                    self.focused_stream
                        .as_ref()
                        .is_none_or(|focused| focused == *key)
                })
                .map(|(key, tile)| (key.clone(), tile.clone()))
                .collect();
            tiles.sort_by(|(a_key, a), (b_key, b)| {
                let pinned = self.pinned_stream.as_ref();
                (
                    pinned != Some(a_key),
                    profile_ui::display_name(&a.metadata.user),
                )
                    .cmp(&(
                        pinned != Some(b_key),
                        profile_ui::display_name(&b.metadata.user),
                    ))
            });
            for (key, tile) in &tiles {
                egui::Frame::none()
                    .inner_margin(Margin::symmetric(24.0, 10.0))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            dot(ui, c_bad(), 4.0);
                            ui.label(
                                RichText::new(format!(
                                    "{} · {}",
                                    profile_ui::display_name(&tile.metadata.user),
                                    if tile.metadata.url.is_empty()
                                        && tile.metadata.kind == "screen"
                                    {
                                        "Screen share"
                                    } else if tile.metadata.url.is_empty() {
                                        "Camera"
                                    } else if tile.metadata.kind == "screen" {
                                        "Screen preview"
                                    } else {
                                        "Camera preview"
                                    }
                                ))
                                .size(font_size(12.0))
                                .family(egui::FontFamily::Name("Relay Semibold".into()))
                                .strong(),
                            );
                        });
                        ui.horizontal(|ui| {
                            if ui.small_button("View larger").clicked() {
                                self.stream_preview = Some(key.clone());
                            }
                            let pinned = self.pinned_stream.as_ref() == Some(key);
                            if ui
                                .small_button(if pinned { "Unpin" } else { "Pin stream" })
                                .clicked()
                            {
                                self.pinned_stream = if pinned { None } else { Some(key.clone()) };
                            }
                        });
                        ui.add_space(8.0);
                        let size = tile.texture.size_vec2();
                        let max = Vec2::new(
                            ui.available_width(),
                            if self.pinned_stream.as_ref() == Some(key) {
                                600.0
                            } else {
                                380.0
                            },
                        );
                        let scale = (max.x / size.x).min(max.y / size.y);
                        let response = ui
                            .add(
                                egui::Image::new((tile.texture.id(), size * scale))
                                    .rounding(12.0)
                                    .sense(Sense::click()),
                            )
                            .on_hover_text("Click to enlarge this stream");
                        self.stream_context_menu(ui, key, &response);
                        if response.clicked() {
                            self.stream_preview = Some(key.clone());
                        }
                    });
            }
        });
    }
}
