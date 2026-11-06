//! R8.2: the voice room page and its call controls, moved unchanged out of
//! `app.rs`. Only the visibility of `voice_view` changed (`pub(super)`), so
//! the app can still call it.
use super::*;
use crate::voice_ui_state;

impl RelayApp {
    /// Route and microphone causes in a call. Deafen and zero volumes already
    /// have their own help with a one-click fix.
    fn call_voice_problem(&mut self) -> Option<crate::voice_problem::VoiceProblem> {
        let media = self.media.as_ref()?;
        let stats = media.network_stats();
        let route_down = matches!(
            crate::voice_connection::Status::from_stats(&stats).route,
            crate::voice_connection::Route::Reconnecting
        );
        let sending_allowed = !self.muted
            && !self.deafened
            && (self.audio_applied.transmission != TransmissionMode::PushToTalk || media.is_transmitting());
        let silent_for = self.mic_silence.observe(media.capture_level(), sending_allowed, Instant::now());
        // Following Windows: a missing chosen output plays on the default meanwhile
        // (with a notice naming it); only without following is it a problem.
        let output_missing = self.device_watch.is_none()
            && self.inventory_loaded
            && self
                .audio_applied
                .output_device
                .as_ref()
                .is_some_and(|name| !self.inventory.audio.outputs.contains(name));
        crate::voice_problem::diagnose(&crate::voice_problem::Inputs {
            route_down,
            microphone_silent_for: silent_for,
            input_volume: self.audio_applied.microphone_gain.max(0.01),
            output_volume: 1.0,
            output_device_missing: output_missing,
            ..Default::default()
        })
    }

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
        } else if let Some(problem) = self.call_voice_problem() {
            voice_room_ui::voice_problem_help(ui, problem);
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

    /// Call messages under the bar, or under Join Voice before joining.
    fn call_messages(
        &mut self,
        ui: &mut egui::Ui,
        joined: bool,
        selected_call: bool,
        ready: bool,
        can_join: bool,
        can_stream: bool,
    ) {
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
        #[cfg(not(feature = "native-rtc"))]
        let _ = ready;
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
    }

    /// What the call surfaces show about your own call: the real state, or a
    /// capture fixture's fixed state while capturing screenshots.
    pub(super) fn call_state(&self) -> voice_room_ui::CallState {
        if let Some(fixture) = self.capture_call.filter(|_| self.capture_path.is_some()) {
            return fixture.state();
        }
        voice_room_ui::CallState {
            ready: self.voice_ready(),
            connected: self.voice_connected(),
            reconnecting: self.voice_reconnecting(),
            screen_live: self.source_active(false),
            camera_live: self.source_active(true),
            video_connected: self.rtc_active(),
            video_connecting: self.rtc_starting(),
        }
    }

    /// The name of the screen or window you share, for the stop-sharing bar.
    fn shared_source_name(&self) -> Option<String> {
        let id = self.screen_source.as_ref()?;
        self.inventory
            .screens
            .iter()
            .find(|source| &source.id == id)
            .map(|source| source.name.clone())
    }

    /// The first received stream of someone in `room`, pinned first: what
    /// the bar's Focus button puts large.
    fn first_room_stream(&self, room: &str) -> Option<String> {
        let members: Vec<_> = self
            .bootstrap
            .as_ref()?
            .voice_members
            .iter()
            .filter(|member| member.channel_id == room)
            .map(|member| member.user.id.clone())
            .collect();
        let mut keys: Vec<_> = self
            .video
            .iter()
            .filter(|(_, video)| members.contains(&video.metadata.user.id))
            .map(|(key, _)| key.clone())
            .collect();
        keys.sort_by_key(|key| (self.pinned_stream.as_ref() != Some(key), key.clone()));
        keys.into_iter().next()
    }

    /// Discord's call bar: camera, screen, microphone and deafen (each audio
    /// button with its device menu), the red Disconnect, and at the right end
    /// the layout, full screen and Voice & Video settings.
    fn call_bar(
        &mut self,
        ui: &mut egui::Ui,
        room: &str,
        ready: bool,
        can_stream: bool,
        call: voice_room_ui::CallState,
    ) {
        use voice_room_ui::{BarTool, BAR_TOOL, CALL_GAP, CALL_LEAVE_GAP};
        const MENU: f32 = 16.0;
        const MENU_GAP: f32 = 2.0;
        let first_stream = self.first_room_stream(room);
        let width = voice_room_ui::call_bar_width(5) + 2.0 * (MENU + MENU_GAP);
        let tools = if first_stream.is_some() { 3 } else { 2 };
        let tools_width = tools as f32 * BAR_TOOL + (tools - 1) as f32 * 4.0;
        let side = ((ui.available_width() - width) * 0.5).max(0.0);
        let show_tools = side >= tools_width + 8.0;
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = CALL_GAP;
            ui.add_space(side);
            let camera_on = call.camera_live;
            if ui
                .add_enabled_ui(
                    ready && (can_stream || camera_on || self.source_starting(true)),
                    |ui| {
                        call_control(
                            ui,
                            Icon::Camera,
                            camera_on,
                            false,
                            if self.source_starting(true) {
                                "Cancel camera start"
                            } else if camera_on {
                                "Turn Off Camera"
                            } else {
                                "Turn On Camera"
                            },
                        )
                    },
                )
                .inner
                .clicked()
            {
                self.camera_toggle();
            }
            let screen_on = call.screen_live;
            if ui
                .add_enabled_ui(
                    ready && (can_stream || screen_on || self.source_starting(false)),
                    |ui| {
                        call_control(
                            ui,
                            Icon::Screen,
                            screen_on,
                            false,
                            if self.source_starting(false) {
                                "Cancel screen share start"
                            } else if screen_on {
                                "Stop Sharing"
                            } else {
                                "Share Your Screen"
                            },
                        )
                    },
                )
                .inner
                .clicked()
            {
                self.screen_toggle();
            }
            if call_control(
                ui,
                Icon::Mic,
                self.muted,
                false,
                if self.muted { "Unmute" } else { "Mute" },
            )
            .clicked()
            {
                self.toggle_mute();
            }
            ui.add_space(MENU_GAP - CALL_GAP);
            self.audio_device_menu(ui, true);
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
            ui.add_space(MENU_GAP - CALL_GAP);
            self.audio_device_menu(ui, false);
            ui.add_space(CALL_LEAVE_GAP - CALL_GAP);
            if call_control(ui, Icon::HangUp, false, true, "Disconnect").clicked() {
                self.leave_voice();
            }
            if show_tools {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    if voice_room_ui::bar_tool(ui, BarTool::Settings, "Voice & Video Settings")
                        .clicked()
                    {
                        self.settings_open = true;
                        self.settings_tab = SettingsTab::VoiceVideo;
                        self.voice_video_tab = VoiceVideoTab::Voice;
                    }
                    let fullscreen = ui
                        .ctx()
                        .input(|input| input.viewport().fullscreen.unwrap_or(false));
                    let (tool, label) = if fullscreen {
                        (BarTool::ExitFullScreen, "Exit Full Screen")
                    } else {
                        (BarTool::FullScreen, "Full Screen")
                    };
                    if voice_room_ui::bar_tool(ui, tool, label).clicked() {
                        ui.ctx()
                            .send_viewport_cmd(egui::ViewportCommand::Fullscreen(!fullscreen));
                    }
                    if let Some(first) = first_stream {
                        let focused = self.focused_stream.is_some();
                        let (tool, label) = if focused {
                            (BarTool::Grid, "Grid View")
                        } else {
                            (BarTool::Focus, "Focus View")
                        };
                        if voice_room_ui::bar_tool(ui, tool, label).clicked() {
                            self.focused_stream = if focused { None } else { Some(first) };
                        }
                    }
                });
            }
        });
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
        let call = self.call_state();
        let ready = joined && call.ready;
        let needs_consent = self.call_needs_open_consent();
        // Discord's call view sits on a black stage; the bar shares it.
        ui.painter()
            .rect_filled(ui.max_rect(), 0.0, voice_room_ui::stage_fill());
        if joined {
            egui::TopBottomPanel::bottom("voice-controls")
                .frame(
                    egui::Frame::none()
                        .fill(voice_room_ui::stage_fill())
                        .inner_margin(Margin::symmetric(16.0, 14.0)),
                )
                .show_inside(ui, |ui| {
                    self.call_bar(ui, &selected, ready, can_stream, call);
                    ui.vertical_centered(|ui| {
                        self.call_messages(ui, joined, selected_call, ready, can_join, can_stream);
                    });
                });
        }
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
        let connection = voice_room_ui::RoomConnection {
            joined,
            encryption_paused: selected_call && self.voice_encryption_error.is_some(),
            reconnecting: selected_call && call.reconnecting,
            connected: call.connected,
            media_ready: ready,
        };
        let status = voice_room_ui::room_status(connection);
        if self.voice_channel.is_none() {
            self.sound_check_since.clear();
        }
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.add_space(16.0);
            if let Some(status) = status {
                ui.vertical_centered(|ui| {
                    voice_room_ui::status_pill(ui, status, voice_room_ui::status_tone(connection));
                });
                ui.add_space(12.0);
            }
            self.call_protection_notice(ui);
            if joined && call.screen_live {
                let source = self.shared_source_name();
                let stop = egui::Frame::none()
                    .inner_margin(Margin::symmetric(24.0, 0.0))
                    .show(ui, |ui| voice_room_ui::sharing_bar(ui, source.as_deref()))
                    .inner;
                if stop {
                    self.stop_capture(false);
                }
                ui.add_space(12.0);
            }
            if joined && (ready || self.capture_path.is_some()) {
                self.sound_check(ui, &members);
                if let Some(stats) = self.media.as_ref().filter(|_| self.capture_path.is_none()).map(|media| media.network_stats()) {
                    let friends_call = self.bootstrap.as_ref().is_some_and(|b| b.capabilities.iter().any(|cap| cap == "people-v1"));
                    // An unavailable optional route should not look like a
                    // broken call or prevent someone from sharing their server.
                    // Keep Stop accessible if the person already opted in.
                    if stats.gateway_available || self.audio_applied.local_voice_gateways {
                        let choice = self.media.as_ref().and_then(|media| media.gateway_choice()).map(|choice| {
                            voice_room_ui::gateway_choice_text(&choice, stats.gateway_active, |id| {
                                members
                                    .iter()
                                    .find(|m| m.user.id == id)
                                    .map(|m| format!("{}'s PC", profile_ui::display_name(&m.user)))
                                    .unwrap_or_else(|| "An approved PC".into())
                            })
                        });
                        if let Some(enabled) = voice_room_ui::local_gateway_controls(
                            ui,
                            self.audio_applied.local_voice_gateways,
                            stats.gateway_available,
                            stats.gateway_active,
                            friends_call,
                            choice,
                        ) {
                            self.apply_quick_audio_change(|audio| audio.local_voice_gateways = enabled);
                        }
                        ui.add_space(16.0);
                    }
                }
            }
            // Streams of people in this room, the pinned one first.
            let mut streams: Vec<_> = self
                .video
                .iter()
                .filter(|(_, v)| members.iter().any(|m| m.user.id == v.metadata.user.id))
                .map(|(key, tile)| (key.clone(), tile.clone()))
                .collect();
            streams.sort_by(|(a_key, a), (b_key, b)| {
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
            if members.is_empty() && streams.is_empty() {
                ui.add_space(30.0);
                ui.vertical_centered(|ui| {
                    icon(ui, Icon::Headphones, 60.0, c_muted());
                    ui.add_space(14.0);
                    ui.label(
                        RichText::new("No one is in voice. Be the first to join.")
                            .size(font_size(13.0))
                            .color(c_muted()),
                    );
                });
            } else {
                let mut tiles: Vec<_> = members
                    .iter()
                    .map(|member| {
                        voice_room_ui::Tile::Person(voice_room_ui::ParticipantTile {
                            user: &member.user,
                            avatar: self.avatar_texture(&member.user),
                            speaking: self.member_speaking(member),
                            muted: member.muted,
                            deafened: member.deafened,
                            sharing: member.sharing,
                            camera: member.camera,
                        })
                    })
                    .collect();
                let mut ids: Vec<_> = members
                    .iter()
                    .map(|member| egui::Id::new(("voice-member-tile", &member.user.id)))
                    .collect();
                for (key, video) in &streams {
                    tiles.push(voice_room_ui::Tile::Stream(voice_room_ui::StreamTile {
                        user: &video.metadata.user,
                        texture: video.texture.clone(),
                        screen: video.metadata.kind == "screen",
                        preview: !video.metadata.url.is_empty(),
                        pinned: self.pinned_stream.as_ref() == Some(key),
                    }));
                    ids.push(egui::Id::new(("voice-stream-tile", key)));
                }
                // Someone shares but no picture has arrived: Discord's Watch Stream tile.
                let own = self.session.as_ref().map(|session| session.user.id.clone());
                let watch = voice_room_ui::watch_offer(joined, call);
                let waiting: Vec<&VoiceMember> = members
                    .iter()
                    .filter(|member| {
                        member.sharing
                            && own.as_ref() != Some(&member.user.id)
                            && !streams.iter().any(|(_, video)| {
                                video.metadata.user.id == member.user.id
                                    && video.metadata.kind == "screen"
                            })
                    })
                    .collect();
                for member in &waiting {
                    tiles.push(voice_room_ui::Tile::Waiting(voice_room_ui::WaitingTile {
                        user: &member.user,
                        avatar: self.avatar_texture(&member.user),
                        watch,
                    }));
                    ids.push(egui::Id::new(("voice-waiting-tile", &member.user.id)));
                }
                let focus = self
                    .focused_stream
                    .as_ref()
                    .and_then(|focused| streams.iter().position(|(key, _)| key == focused))
                    .map(|index| members.len() + index);
                // The whisper line under the grid is centred with it.
                let whisper = joined
                    && self
                        .media
                        .as_ref()
                        .is_some_and(|media| media.active_settings().whisper_enabled);
                let below = if whisper {
                    72.0
                } else if joined {
                    0.0
                } else {
                    voice_room_ui::join_block_height()
                };
                let space = ui.available_height() - 16.0;
                let mut responses =
                    voice_room_ui::participant_grid(ui, &tiles, &ids, focus, space, below, !joined)
                        .into_iter();
                drop(tiles);
                let people: Vec<_> = responses.by_ref().take(members.len()).collect();
                for ((key, video), response) in streams.iter().zip(responses.by_ref()) {
                    let focused = self.focused_stream.as_ref() == Some(key);
                    let mut hover = format!(
                        "{} · {}",
                        profile_ui::display_name(&video.metadata.user),
                        voice_room_ui::stream_kind(&voice_room_ui::StreamTile {
                            user: &video.metadata.user,
                            texture: video.texture.clone(),
                            screen: video.metadata.kind == "screen",
                            preview: !video.metadata.url.is_empty(),
                            pinned: false,
                        })
                    );
                    if let Some(line) = self.delivered_caption(key, video) {
                        hover.push('\n');
                        hover.push_str(&line);
                        hover.push('\n');
                        hover.push_str(crate::video_quality::DELIVERED_HINT);
                    }
                    hover.push_str(if focused {
                        "\nClick to return to the grid. Right-click for more."
                    } else {
                        "\nClick to focus this stream. Right-click for more."
                    });
                    if response.double_clicked() {
                        self.stream_preview = Some(key.clone());
                    } else if response.clicked() {
                        self.focused_stream = if focused { None } else { Some(key.clone()) };
                    }
                    let response = response.on_hover_text(hover);
                    self.stream_context_menu(ui, key, &response);
                }
                for (member, response) in waiting.iter().zip(responses.by_ref()) {
                    let name = profile_ui::display_name(&member.user).to_owned();
                    let response = response.on_hover_text(match watch {
                        voice_room_ui::Watch::Offer => {
                            format!("Connect video to watch {name}'s screen")
                        }
                        other => other.caption().to_owned(),
                    });
                    if watch == voice_room_ui::Watch::Offer && response.clicked() {
                        self.request_video_connection();
                    }
                }
                for (member, response) in members.iter().zip(people) {
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
                                self.squads.active = None;
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
                                self.squads.active = None;
                                ui.close_menu();
                            }
                            if self.session.as_ref().is_some_and(|s| s.user.id != member.user.id) {
                                self.squad_menu(ui, &member.user.id);
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
            if !joined {
                ui.add_space(voice_room_ui::JOIN_GAP);
                ui.vertical_centered(|ui| {
                    let joining = self.joining_voice.as_ref() == Some(&selected);
                    if ui
                        .add_enabled(
                            can_join && !needs_consent && !joining,
                            primary(if joining { "Joining…" } else { "Join Voice" })
                                .min_size(Vec2::new(130.0, 38.0)),
                        )
                        .on_hover_text(format!("Join {name}"))
                        .clicked()
                    {
                        self.join_voice(selected.clone());
                    }
                    ui.add_space(6.0);
                    self.call_messages(ui, joined, selected_call, ready, can_join, can_stream);
                });
            }
            if joined {
                // Same side margin as the grid above.
                egui::Frame::none()
                    .inner_margin(Margin::symmetric(24.0, 8.0))
                    .show(ui, |ui| {
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
                let whisper_on = self
                    .media
                    .as_ref()
                    .is_some_and(|media| media.active_settings().whisper_enabled);
                if whisper_on {
                    self.squad_bar(ui);
                }
                    });
            }
            ui.add_space(16.0);
        });
    }
}

/// A generated picture for capture fixtures: a dark app window with a title
/// bar, a side list and content blocks in `tint`. Never a real screen.
fn fixture_picture(width: usize, height: usize, tint: [u8; 3]) -> egui::ColorImage {
    let mut image = egui::ColorImage::new([width, height], Color32::from_rgb(30, 31, 36));
    let mut fill = |x0: usize, y0: usize, x1: usize, y1: usize, color: Color32| {
        for y in y0.min(height)..y1.min(height) {
            for x in x0.min(width)..x1.min(width) {
                image.pixels[y * width + x] = color;
            }
        }
    };
    let [r, g, b] = tint;
    let bar = height / 12;
    fill(0, 0, width, bar, Color32::from_rgb(18, 19, 22));
    fill(0, bar, width / 5, height, Color32::from_rgb(24, 25, 29));
    for row in 0..5 {
        let y = bar + 12 + row * (height / 9);
        fill(10, y, width / 5 - 10, y + height / 30, Color32::from_rgb(52, 54, 60));
    }
    let left = width / 5 + 16;
    fill(left, bar + 16, width - 16, bar + 16 + height / 3, Color32::from_rgb(r, g, b));
    for column in 0..3 {
        let x = left + column * ((width - left - 16) / 3);
        fill(
            x,
            bar + 28 + height / 3,
            x + (width - left - 16) / 3 - 10,
            height - 16,
            Color32::from_rgb(r / 3 + 30, g / 3 + 30, b / 3 + 34),
        );
    }
    image
}

impl RelayApp {
    /// Capture fixtures for the call surfaces. Display state only: no voice
    /// transport, device, camera or capture starts, and nothing is sent.
    /// Returns true when `panel` was one of them.
    pub(super) fn call_capture_panel(&mut self, panel: &str, b: &Bootstrap) -> bool {
        const PANELS: [&str; 9] = [
            "stream-popout",
            "call-connected",
            "call-reconnecting",
            "call-sharing",
            "stream-waiting",
            "share-picker-live",
            "share-picker-screens",
            "camera-preview",
            "call-focus",
        ];
        if self.capture_path.is_none() || !PANELS.contains(&panel) {
            return false;
        }
        self.prefs.audio_setup_offered = true;
        let Some(room) = b.channels.iter().find(|channel| channel.kind == "voice") else {
            return true;
        };
        let own = b.user.id.clone();
        self.select(Target::Channel(room.id.clone()));
        self.voice_channel = Some(room.id.clone());
        self.muted = false;
        self.deafened = false;
        self.capture_call = Some(CaptureCall {
            connected: panel != "call-reconnecting",
            reconnecting: panel == "call-reconnecting",
            sharing_screen: panel == "call-sharing",
            video: panel != "stream-waiting",
        });
        // The person who shares (as the fixture server says) first.
        let mut others: Vec<&VoiceMember> = b
            .voice_members
            .iter()
            .filter(|member| member.channel_id == room.id && member.user.id != own)
            .collect();
        others.sort_by_key(|member| !member.sharing);
        let others: Vec<User> = others.into_iter().map(|member| member.user.clone()).collect();
        let add_stream = |app: &mut Self, user: &User, kind: &str, tint: [u8; 3]| {
            let key = format!("{}:{kind}", user.id);
            let texture = app.capture_ctx.load_texture(
                &key,
                fixture_picture(640, 360, tint),
                egui::TextureOptions::LINEAR,
            );
            app.video.insert(
                key.clone(),
                VideoTile {
                    metadata: MediaFrame {
                        user: user.clone(),
                        kind: kind.into(),
                        // Kept by the capture-only rule in `updates`.
                        url: "owned-capture://synthetic".into(),
                        frame_id: "generated-fixture".into(),
                        width: 640,
                        height: 360,
                    },
                    texture,
                    updated: Instant::now(),
                },
            );
            key
        };
        match panel {
            "call-sharing" | "call-focus" | "stream-popout" => {
                if let Some(user) = others.first() {
                    let key = add_stream(self, user, "screen", [88, 101, 242]);
                    if panel == "call-focus" {
                        self.focused_stream = Some(key);
                    } else if panel == "stream-popout" {
                        // Opens the second window; the capture shows the main one.
                        self.stream_popout = Some(key);
                    }
                }
                self.inventory.screens = vec![CaptureSource {
                    id: "window:fixture-game".into(),
                    name: "Game Night - Map Planner".into(),
                    kind: SourceKind::Window,
                    width: 1920,
                    height: 1080,
                }];
                self.screen_source = Some("window:fixture-game".into());
            }
            // "stream-waiting": the fixture server marks someone as sharing and
            // no picture is added, so the Watch Stream tile shows.
            "share-picker-live" | "share-picker-screens" => {
                self.inventory_loaded = true;
                self.share_kind = if panel == "share-picker-live" {
                    SourceKind::Window
                } else {
                    SourceKind::Monitor
                };
                let fixtures = [
                    ("window:fixture-1", "Game Night - Map Planner", SourceKind::Window, [88, 101, 242]),
                    ("window:fixture-2", "Notes - Friday co-op plans", SourceKind::Window, [35, 165, 90]),
                    ("window:fixture-3", "Music Player", SourceKind::Window, [235, 69, 158]),
                    ("window:fixture-4", "Browser - Patch notes", SourceKind::Window, [240, 178, 50]),
                    ("window:fixture-5", "A window that blocks capture", SourceKind::Window, [0, 0, 0]),
                    ("monitor:fixture-1", "Display 1 (primary)", SourceKind::Monitor, [88, 101, 242]),
                    ("monitor:fixture-2", "Display 2", SourceKind::Monitor, [69, 140, 200]),
                ];
                let mut thumbnails = share_thumbnails::ShareThumbnails::default();
                self.inventory.screens = fixtures
                    .iter()
                    .map(|(id, name, kind, tint)| {
                        if *tint != [0, 0, 0] {
                            thumbnails.insert_fixture(
                                &self.capture_ctx,
                                id,
                                fixture_picture(320, 180, *tint),
                            );
                        } else {
                            thumbnails.mark_unavailable(id);
                        }
                        CaptureSource {
                            id: (*id).into(),
                            name: (*name).into(),
                            kind: *kind,
                            width: 1920,
                            height: 1080,
                        }
                    })
                    .collect();
                self.screen_source = Some(if panel == "share-picker-live" {
                    "window:fixture-1".into()
                } else {
                    "monitor:fixture-1".into()
                });
                self.share_thumbnails = Some(thumbnails);
                self.share_picker = Some(false);
            }
            "camera-preview" => {
                self.inventory_loaded = true;
                self.inventory.cameras = vec![CaptureSource {
                    id: "camera:fixture".into(),
                    name: "Generated test camera".into(),
                    kind: SourceKind::Camera,
                    width: 1280,
                    height: 720,
                }];
                self.camera_source = Some("camera:fixture".into());
                let mut thumbnails = share_thumbnails::ShareThumbnails::default();
                thumbnails.insert_fixture(
                    &self.capture_ctx,
                    "camera:fixture",
                    fixture_picture(640, 360, [69, 140, 200]),
                );
                self.share_thumbnails = Some(thumbnails);
                self.share_picker = Some(true);
            }
            _ => {}
        }
        true
    }
}
