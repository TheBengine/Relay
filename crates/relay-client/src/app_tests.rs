//! R8.2: `app.rs`'s own unit tests, moved unchanged into their own file.
use super::*;

#[test]
fn failed_device_selection_keeps_newer_editor_choices_and_audio_controls() {
    let attempted = AudioSettings {
        input_device: Some("missing".into()),
        output_device: Some("new speakers".into()),
        ..Default::default()
    };
    let restored = AudioSettings::default();
    let mut edited = attempted.clone();
    edited.input_device = Some("newer unsaved microphone".into());
    edited.microphone_gain = 1.9;
    edited.transmission = crate::audio_settings::TransmissionMode::PushToTalk;
    crate::audio_settings::restore_device_selection(&mut edited, &attempted, &restored);
    assert_eq!(
        edited.input_device.as_deref(),
        Some("newer unsaved microphone")
    );
    assert_eq!(edited.output_device, None);
    assert_eq!(edited.microphone_gain, 1.9);
    assert_eq!(
        edited.transmission,
        crate::audio_settings::TransmissionMode::PushToTalk
    );
}

#[test]
fn existing_preferences_hide_on_close_and_explicit_exit_choice_survives_restart() {
    let mut legacy = serde_json::to_value(Preferences::default()).unwrap();
    legacy.as_object_mut().unwrap().remove("close_to_tray");
    let mut prefs: Preferences = serde_json::from_value(legacy).unwrap();
    assert!(prefs.close_to_tray);
    prefs.close_to_tray = false;
    let restarted: Preferences =
        serde_json::from_slice(&preferences_snapshot(&prefs, &AudioSettings::default()).unwrap())
            .unwrap();
    assert!(!restarted.close_to_tray);
}

#[test]
fn recovered_active_connection_clears_only_resolved_transport_errors() {
    use ConnectionStatus::{Connected, Offline, Reconnecting, SignInRequired};
    for message in [
        "Cannot reach this server. Ask the host to check it is running and confirm the address.",
        crate::connection_errors::WINDOWS_CONNECTION_BLOCKED,
        "The server took too long to respond.",
        "The request failed. Check your connection and server certificate.",
    ] {
        for previous in [Offline, Reconnecting] {
            let mut error = Some(message.to_owned());
            app_update::clear_recovered_connection_error(&previous, &Connected, &mut error);
            assert_eq!(error, None, "successful recovery resolves {message}");
        }
        for (previous, next) in [
            (Reconnecting, Reconnecting),
            (Reconnecting, SignInRequired),
            (SignInRequired, Connected),
            (Connected, Connected),
        ] {
            let mut error = Some(message.to_owned());
            app_update::clear_recovered_connection_error(&previous, &next, &mut error);
            assert_eq!(error.as_deref(), Some(message));
        }
    }
    for message in [
        "Sign in again",
        "Server identity changed",
        "Cannot decrypt this message",
    ] {
        let mut error = Some(message.to_owned());
        app_update::clear_recovered_connection_error(&Reconnecting, &Connected, &mut error);
        assert_eq!(error.as_deref(), Some(message));
    }
}

#[test]
fn toolbar_of_scrolled_message_stays_inside_chat_viewport() {
    for (clip, row, width) in [
        (
            Rect::from_min_max(Pos2::new(330.0, 96.0), Pos2::new(900.0, 590.0)),
            Rect::from_min_max(Pos2::new(340.0, -90.0), Pos2::new(890.0, 330.0)),
            224.0,
        ),
        (
            Rect::from_min_max(Pos2::new(325.0, 126.0), Pos2::new(661.0, 400.0)),
            Rect::from_min_max(Pos2::new(330.0, 30.0), Pos2::new(651.0, 450.0)),
            196.0,
        ),
    ] {
        let toolbar = Rect::from_min_size(
            message_toolbar_position(row, clip, width),
            Vec2::new(width + 4.0, 40.0),
        );
        assert!(
            clip.contains_rect(toolbar),
            "toolbar must not cover the channel header or composer: {toolbar:?}"
        );
    }
}

#[test]
fn unrelated_preference_saves_do_not_commit_pending_audio_edits() {
    let applied = AudioSettings::default();
    let mut prefs = Preferences::default();
    prefs.audio.microphone_gain = 2.0;
    prefs.audio.input_device = Some("A different input".into());
    prefs.audio.event_sounds.get_mut(AudioCue::Message).source =
        crate::sound_customization::SoundSource::BuiltIn { cue: AudioCue::Join };
    prefs.audio.event_sounds.get_mut(AudioCue::Leave).enabled = false;
    prefs.audio.event_sounds.get_mut(AudioCue::Ring).volume = 0.4;
    prefs.appearance.theme = Theme::Onyx;
    let stored: Preferences =
        serde_json::from_slice(&preferences_snapshot(&prefs, &applied).unwrap()).unwrap();
    assert_eq!(stored.audio, applied);
    assert_eq!(stored.appearance.theme, Theme::Onyx);
    assert_eq!(prefs.audio.microphone_gain, 2.0);
    assert_eq!(
        prefs.audio.input_device.as_deref(),
        Some("A different input")
    );
    let committed: Preferences =
        serde_json::from_slice(&preferences_snapshot(&prefs, &prefs.audio).unwrap()).unwrap();
    assert_eq!(committed.audio, prefs.audio);
}

#[test]
fn audio_shortcuts_keep_pending_capture_and_device_edits_out_of_runtime_and_storage() {
    let applied = AudioSettings {
        transmission: TransmissionMode::PushToTalk,
        input_device: Some("Active headset microphone".into()),
        output_device: Some("Active headphones".into()),
        ..Default::default()
    };
    let mut prefs = Preferences {
        audio: AudioSettings {
            transmission: TransmissionMode::Continuous,
            input_device: Some("Pending different microphone".into()),
            microphone_gain: 3.0,
            playback_volume: 0.25,
            packet_ms: 20,
            echo_cancellation: true,
            ..applied.clone()
        },
        ..Default::default()
    };
    let draft = prefs.audio.clone();
    let runtime = crate::audio_settings::quick_audio_change(&applied, &mut prefs.audio, |audio| {
        audio.set_latency_mode(crate::audio_settings::LatencyMode::Adaptive);
    });
    let mut expected = applied.clone();
    expected.set_latency_mode(crate::audio_settings::LatencyMode::Adaptive);
    assert_eq!(
        runtime, expected,
        "smoothing cannot change capture or devices"
    );
    let mut expected_draft = draft;
    expected_draft.set_latency_mode(crate::audio_settings::LatencyMode::Adaptive);
    assert_eq!(prefs.audio, expected_draft, "other edits remain pending");
    let stored: Preferences =
        serde_json::from_slice(&preferences_snapshot(&prefs, &runtime).unwrap()).unwrap();
    assert_eq!(stored.audio, expected);
    assert_eq!(stored.audio.transmission, TransmissionMode::PushToTalk);
    assert_ne!(stored.audio, prefs.audio);

    let quieter = crate::audio_settings::quick_audio_change(&runtime, &mut prefs.audio, |audio| {
        audio.playback_volume = 0.5;
    });
    expected.playback_volume = 0.5;
    expected_draft.playback_volume = 0.5;
    assert_eq!(
        quieter, expected,
        "output slider cannot apply pending input changes"
    );
    assert_eq!(prefs.audio, expected_draft);

    let chosen = crate::audio_settings::quick_audio_change(&quieter, &mut prefs.audio, |audio| {
        audio.input_device = None;
    });
    expected.input_device = None;
    expected_draft.input_device = None;
    assert_eq!(
        chosen, expected,
        "an explicit default-device choice wins only its own field"
    );
    assert_eq!(prefs.audio, expected_draft);
    assert_eq!(chosen.transmission, TransmissionMode::PushToTalk);
    assert_eq!(chosen.packet_ms, 10);
}

#[test]
fn pre_theme_preferences_keep_real_devices_communities_and_privacy_choices() {
    let prefs = Preferences {
        server: "http://127.0.0.1:8080".into(),
        username: "Fixture".into(),
        compact: true,
        ui_scale: 1.2,
        remember_drafts: false,
        remember_session: true,
        communities: vec![SavedCommunity {
            url: "http://127.0.0.1:8080".into(),
            name: "Fixture community".into(),
            ..Default::default()
        }],
        audio: AudioSettings {
            input_device: Some("Fixture input".into()),
            transmission: TransmissionMode::PushToTalk,
            ..Default::default()
        },
        screen_options: CaptureOptions {
            fps: 60,
            application_audio: true,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut legacy = serde_json::to_value(&prefs).unwrap();
    legacy.as_object_mut().unwrap().remove("appearance");
    let migrated: Preferences = serde_json::from_value(legacy).unwrap();
    assert_eq!(migrated.audio, prefs.audio);
    assert_eq!(migrated.screen_options, prefs.screen_options);
    assert_eq!(migrated.communities[0].url, prefs.communities[0].url);
    assert_eq!(migrated.appearance, AppearanceSettings::default());
    assert!(migrated.compact && migrated.remember_session);
    assert!(!migrated.remember_drafts);
    assert_eq!(migrated.ui_scale, 1.2);
}

#[test]
fn sign_in_preferences_default_to_staying_signed_in_and_preserve_explicit_opt_out() {
    let new = Preferences::default();
    assert!(new.stay_signed_in && new.remember_session);
    let old: Preferences = serde_json::from_str(r#"{"server":"","username":"","remember_session":false}"#).unwrap();
    // The old password-only choice is preserved; the new combined account
    // choice is shown before the next interactive account sign-in.
    assert!(old.stay_signed_in);
    assert!(!old.remember_session);
    let chosen = Preferences { stay_signed_in: false, remember_session: false, ..new };
    let restarted: Preferences = serde_json::from_slice(&serde_json::to_vec(&chosen).unwrap()).unwrap();
    assert!(!restarted.stay_signed_in && !restarted.remember_session);
}

#[test]
fn older_preferences_prefer_iroh_and_keep_a_chosen_mode() {
    let mut legacy = serde_json::to_value(Preferences::default()).unwrap();
    legacy.as_object_mut().unwrap().remove("connection");
    let migrated: Preferences = serde_json::from_value(legacy).unwrap();
    assert_eq!(
        migrated.connection.mode,
        crate::route_choice::ConnectionMode::PreferIroh
    );
    assert!(!migrated.connection.allow_plain_public_login);
    let chosen = Preferences {
        connection: crate::route_choice::ConnectionPrefs {
            mode: crate::route_choice::ConnectionMode::Direct,
            allow_plain_public_login: false,
            offer: vec!["https://chat.example.org".into()],
        },
        ..Default::default()
    };
    let saved = serde_json::to_string(&chosen).unwrap();
    let loaded: Preferences = serde_json::from_str(&saved).unwrap();
    assert_eq!(loaded.connection, chosen.connection);
}

#[test]
fn source_picker_bounds_long_names_without_changing_selected_source_identity() {
    let context = egui::Context::default();
    let source = CaptureSource {
        id: "fixture-window-identity".into(),
        name: "A very long application window title with enough detail to exceed the settings column width".into(),
        kind: SourceKind::Window,
        width: 1280,
        height: 720,
    };
    let mut selected = Some(source.id.clone());
    let mut content_right = 0.0_f32;
    let _ = context.run(
        egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(350.0, 600.0))),
            ..Default::default()
        },
        |context| {
            egui::CentralPanel::default().show(context, |ui| {
                source_combo(
                    ui,
                    "Camera",
                    "test-camera-source",
                    &mut selected,
                    std::slice::from_ref(&source),
                );
                content_right = ui.min_rect().right();
            });
        },
    );
    assert!(
        content_right <= 350.0,
        "source picker overflowed: {content_right}"
    );
    assert_eq!(selected.as_deref(), Some(source.id.as_str()));
}
use chrono::TimeZone;

#[test]
fn capture_mode_ignores_interaction_but_preserves_screenshot_and_frame_geometry() {
    let context = egui::Context::default();
    let screen = Rect::from_min_size(Pos2::ZERO, Vec2::new(400.0, 240.0));
    let screenshot = std::sync::Arc::new(egui::ColorImage::new([2, 2], egui::Color32::LIGHT_BLUE));
    let mut raw = egui::RawInput {
        screen_rect: Some(screen),
        time: Some(12.0),
        focused: true,
        modifiers: egui::Modifiers::SHIFT,
        hovered_files: vec![Default::default()],
        dropped_files: vec![Default::default()],
        events: vec![
            egui::Event::PointerMoved(Pos2::new(30.0, 20.0)),
            egui::Event::PointerButton {
                pos: Pos2::new(30.0, 20.0),
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            },
            egui::Event::PointerButton {
                pos: Pos2::new(30.0, 20.0),
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            },
            enter(egui::Modifiers::NONE, true),
            egui::Event::Text("do not send".into()),
            egui::Event::Paste("do not upload".into()),
            egui::Event::Scroll(Vec2::new(0.0, 120.0)),
            egui::Event::Screenshot {
                viewport_id: egui::ViewportId::ROOT,
                image: screenshot.clone(),
            },
        ],
        ..Default::default()
    };
    capture_only_input(&mut raw);
    assert_eq!(raw.screen_rect, Some(screen));
    assert_eq!(raw.time, Some(12.0));
    assert!(!raw.focused);
    assert_eq!(raw.modifiers, egui::Modifiers::NONE);
    assert!(raw.hovered_files.is_empty() && raw.dropped_files.is_empty());
    assert_eq!(raw.events.len(), 1);
    let egui::Event::Screenshot { image, .. } = &raw.events[0] else {
        panic!("screenshot event was removed");
    };
    assert!(std::sync::Arc::ptr_eq(image, &screenshot));
    let mut clicked = false;
    let _ = context.run(raw, |context| {
        egui::CentralPanel::default().show(context, |ui| {
            clicked = ui.button("Join voice").clicked();
        });
    });
    assert!(!clicked);
    assert!(!context.input(|input| input.key_pressed(egui::Key::Enter)));
    assert!(context.input(|input| input.pointer.hover_pos().is_none()));
}

#[test]
fn bottom_follow_cancels_for_scrolling_but_not_ordinary_content_clicks() {
    let rect = Rect::from_min_size(Pos2::ZERO, Vec2::new(200.0, 160.0));
    let press = |pos| egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed: true,
        modifiers: egui::Modifiers::NONE,
    };
    for (events, expected) in [
        (
            vec![
                egui::Event::PointerMoved(Pos2::new(50.0, 50.0)),
                press(Pos2::new(50.0, 50.0)),
            ],
            false,
        ),
        (
            vec![
                egui::Event::PointerMoved(Pos2::new(195.0, 50.0)),
                press(Pos2::new(195.0, 50.0)),
            ],
            true,
        ),
        (
            vec![
                egui::Event::PointerMoved(Pos2::new(50.0, 50.0)),
                egui::Event::Scroll(Vec2::new(0.0, 30.0)),
            ],
            true,
        ),
    ] {
        let context = egui::Context::default();
        let _ = context.run(
            egui::RawInput {
                events,
                ..Default::default()
            },
            |_| {},
        );
        assert_eq!(
            context.input(|input| chat_scroll_intent(input, rect)),
            expected
        );
    }
}

#[test]
fn disabled_restart_persistence_preserves_community_drafts_only_until_process_exit() {
    let mut prefs = Preferences {
        remember_drafts: false,
        ..Default::default()
    };
    let mut ram = ScopedDrafts::default();
    let target = Target::Direct("same-peer-id".into());
    for scope in ["server-one/user", "server-two/user"] {
        prefs.conversations.remember(scope, &target);
        save_draft_scope(
            &mut prefs,
            &mut ram,
            scope,
            vec![(
                target.clone(),
                Draft {
                    content: format!("do-not-save:{scope}"),
                    ..Default::default()
                },
            )],
        );
    }
    assert_eq!(
        ram.load("server-one/user").unwrap()[0].1.content,
        "do-not-save:server-one/user"
    );
    assert_eq!(
        ram.load("server-two/user").unwrap()[0].1.content,
        "do-not-save:server-two/user"
    );
    let bytes = serde_json::to_vec(&prefs).unwrap();
    assert!(!String::from_utf8_lossy(&bytes).contains("do-not-save"));
    let restarted: Preferences = serde_json::from_slice(&bytes).unwrap();
    assert!(restarted
        .conversations
        .drafts("server-one/user")
        .iter()
        .all(|(_, draft)| draft.content.is_empty()));
    assert!(ScopedDrafts::default().load("server-one/user").is_none());
    prefs.remember_drafts = true;
    let drafts = ram.load("server-one/user").unwrap();
    save_draft_scope(&mut prefs, &mut ram, "server-one/user", drafts);
    let restarted: Preferences =
        serde_json::from_slice(&serde_json::to_vec(&prefs).unwrap()).unwrap();
    assert_eq!(
        restarted.conversations.drafts("server-one/user")[0]
            .1
            .content,
        "do-not-save:server-one/user"
    );
}

fn queued_owner_updates(origin: &CommandOrigin) -> [Update; 3] {
    [
        Update::Changes {
            origin: origin.clone(),
            changes: Vec::new(),
        },
        Update::MessageProtections {
            origin: origin.clone(),
            protections: Vec::new(),
        },
        Update::CommunityBootstrap {
            url: origin.url.clone(),
            bootstrap: Bootstrap {
                server_id: origin.server_id.clone(),
                user: User {
                    id: origin.user_id.clone(),
                    ..Default::default()
                },
                ..Default::default()
            },
        },
    ]
}

fn queued_community(origin: &CommandOrigin, current: bool) -> CommunityInfo {
    CommunityInfo {
        url: origin.url.clone(),
        server_id: origin.server_id.clone(),
        name: origin.url.clone(),
        user: User {
            id: origin.user_id.clone(),
            ..Default::default()
        },
        current,
    }
}

#[test]
fn queued_owner_updates_use_current_session_before_community_list_refresh() {
    let old = CommandOrigin {
        url: "east".into(),
        server_id: "server-east".into(),
        user_id: "alex".into(),
    };
    let current = CommandOrigin {
        user_id: "mira".into(),
        ..old.clone()
    };
    let stale_list = [queued_community(&old, true)];
    for update in queued_owner_updates(&old) {
        assert!(!community_update_allowed(
            &update,
            "east",
            Some(&current),
            &stale_list
        ));
    }
    for update in queued_owner_updates(&current) {
        assert!(community_update_allowed(
            &update,
            "east",
            Some(&current),
            &stale_list
        ));
    }
    let replaced_server = CommandOrigin {
        server_id: "replacement-server".into(),
        ..current.clone()
    };
    for update in queued_owner_updates(&current) {
        assert!(!community_update_allowed(
            &update,
            "east",
            Some(&replaced_server),
            &stale_list
        ));
    }
    for update in queued_owner_updates(&replaced_server) {
        assert!(community_update_allowed(
            &update,
            "east",
            Some(&replaced_server),
            &stale_list
        ));
    }
}

#[test]
fn queued_owner_updates_reject_logout_and_unknown_actors_but_keep_background() {
    let east = CommandOrigin {
        url: "east".into(),
        server_id: "server-east".into(),
        user_id: "alex".into(),
    };
    let west = CommandOrigin {
        url: "west".into(),
        server_id: "server-west".into(),
        user_id: "jonas".into(),
    };
    let communities = [
        queued_community(&east, true),
        queued_community(&west, false),
    ];
    for update in queued_owner_updates(&east) {
        assert!(!community_update_allowed(
            &update,
            "east",
            None,
            &communities
        ));
    }
    for update in queued_owner_updates(&west) {
        assert!(community_update_allowed(
            &update,
            "east",
            None,
            &communities
        ));
    }
    for rejected in [
        CommandOrigin {
            url: "unknown".into(),
            ..west.clone()
        },
        CommandOrigin {
            server_id: "old-server".into(),
            ..west.clone()
        },
        CommandOrigin {
            user_id: "old-account".into(),
            ..west.clone()
        },
        CommandOrigin {
            url: String::new(),
            ..west.clone()
        },
        CommandOrigin {
            server_id: String::new(),
            ..west.clone()
        },
        CommandOrigin {
            user_id: String::new(),
            ..west.clone()
        },
    ] {
        for update in queued_owner_updates(&rejected) {
            assert!(!community_update_allowed(
                &update,
                "east",
                Some(&east),
                &communities
            ));
        }
    }
}

#[test]
fn queued_old_bootstrap_cannot_replace_read_social_or_notification_context() {
    let east = CommandOrigin {
        url: "east".into(),
        server_id: "server-east".into(),
        user_id: "mira".into(),
    };
    let west = CommandOrigin {
        url: "west".into(),
        server_id: "server-west".into(),
        user_id: "jonas".into(),
    };
    let communities = [
        queued_community(&east, true),
        queued_community(&west, false),
    ];
    let mut reads = HashMap::new();
    let mut social = HashMap::new();
    let snapshot =
        |owner: &CommandOrigin, unread_count, blocked: &[&str], status: &str| Bootstrap {
            server_id: owner.server_id.clone(),
            user: User {
                id: owner.user_id.clone(),
                ..Default::default()
            },
            read_states: vec![ReadState {
                unread_count,
                mention_count: unread_count,
                ..Default::default()
            }],
            social: SocialState {
                blocked: blocked.iter().map(|peer| (*peer).to_owned()).collect(),
                ..Default::default()
            },
            presence: vec![UserPresence {
                user_id: owner.user_id.clone(),
                status: status.into(),
                ..Default::default()
            }],
            ..Default::default()
        };
    cache_community_snapshot(
        &mut reads,
        &mut social,
        "east",
        &snapshot(&east, 2, &[], "away"),
    );
    cache_community_snapshot(
        &mut reads,
        &mut social,
        "west",
        &snapshot(&west, 4, &["west-peer"], "online"),
    );
    let old = CommandOrigin {
        user_id: "alex".into(),
        ..east.clone()
    };
    for (owner, count, blocked, status, accepted) in [
        (&old, 7, vec!["old-peer"], "dnd", false),
        (&east, 3, Vec::new(), "away", true),
        (&west, 5, vec!["west-peer"], "online", true),
    ] {
        let update = Update::CommunityBootstrap {
            url: owner.url.clone(),
            bootstrap: snapshot(owner, count, &blocked, status),
        };
        let allowed = community_update_allowed(&update, "east", Some(&east), &communities);
        assert_eq!(allowed, accepted);
        if allowed {
            if let Update::CommunityBootstrap { url, bootstrap } = update {
                cache_community_snapshot(&mut reads, &mut social, &url, &bootstrap);
            }
        }
        assert_eq!(social["east"].0, "mira");
        assert!(social["east"].1.blocked.is_empty());
        assert_eq!(social["east"].2, "away");
        assert_eq!(
            reads["east"][0].unread_count,
            if owner == &old { 2 } else { 3 }
        );
        assert_eq!(
            reads["east"][0].mention_count,
            reads["east"][0].unread_count
        );
    }
    assert_eq!(reads["west"][0].unread_count, 5);
    assert_eq!(social["west"].1.blocked, ["west-peer"]);
}

#[test]
fn parked_community_snapshot_keeps_latest_reads_and_replaces_same_url_account() {
    let mut east = Bootstrap {
        user: User {
            id: "alex-east".into(),
            ..Default::default()
        },
        read_states: vec![ReadState {
            channel_id: Some("general-east".into()),
            last_read_sequence: 8,
            unread_count: 7,
            mention_count: 1,
            ..Default::default()
        }],
        presence: vec![UserPresence {
            user_id: "alex-east".into(),
            status: "dnd".into(),
            ..Default::default()
        }],
        social: SocialState {
            blocked: vec!["peer-east".into()],
            ..Default::default()
        },
        ..Default::default()
    };
    let west = Bootstrap {
        user: User {
            id: "alex-west".into(),
            ..Default::default()
        },
        read_states: vec![ReadState {
            channel_id: Some("general-west".into()),
            last_read_sequence: 4,
            unread_count: 3,
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut reads = HashMap::new();
    let mut social = HashMap::new();
    cache_community_snapshot(&mut reads, &mut social, "east", &east);
    // Active read/arrival changes must be captured before parking, even
    // when no background bootstrap event has ever arrived.
    east.read_states[0].last_read_sequence = 10;
    east.read_states[0].unread_count = 5;
    cache_community_snapshot(&mut reads, &mut social, "east", &east);
    cache_community_snapshot(&mut reads, &mut social, "west", &west);
    assert_eq!(reads["east"][0].last_read_sequence, 10);
    assert_eq!(reads["east"][0].unread_count, 5);
    assert_eq!(reads["west"][0].unread_count, 3);
    assert_eq!(social["east"].0, "alex-east");
    assert_eq!(social["east"].1.blocked, ["peer-east"]);
    assert_eq!(social["east"].2, "dnd");
    let mira = Bootstrap {
        user: User {
            id: "mira-east".into(),
            ..Default::default()
        },
        read_states: vec![ReadState {
            channel_id: Some("general-east".into()),
            unread_count: 2,
            ..Default::default()
        }],
        ..Default::default()
    };
    cache_community_snapshot(&mut reads, &mut social, "east", &mira);
    assert_eq!(reads["east"][0].unread_count, 2);
    assert_eq!(reads["east"][0].last_read_sequence, 0);
    assert_eq!(social["east"].0, "mira-east");
    assert!(social["east"].1.blocked.is_empty());
    assert!(social["east"].2.is_empty());
    assert_eq!(reads["west"][0].unread_count, 3);
    assert_eq!(social["west"].0, "alex-west");
}

#[test]
fn notification_navigation_rejects_revoked_channels_and_self_direct_messages() {
    let bootstrap = Bootstrap {
        user: User {
            id: "self".into(),
            ..Default::default()
        },
        channels: vec![Channel {
            id: "visible".into(),
            ..Default::default()
        }],
        members: vec![
            User {
                id: "friend".into(),
                ..Default::default()
            },
            User {
                id: "self".into(),
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    assert!(target_available(
        &Target::Channel("visible".into()),
        &bootstrap
    ));
    assert!(target_available(
        &Target::Direct("friend".into()),
        &bootstrap
    ));
    assert!(!target_available(
        &Target::Channel("revoked".into()),
        &bootstrap
    ));
    assert!(!target_available(
        &Target::Direct("self".into()),
        &bootstrap
    ));
    assert!(!target_available(
        &Target::Direct("removed".into()),
        &bootstrap
    ));
}

#[test]
fn notification_accounts_use_current_session_and_keep_other_authenticated_communities() {
    let old = CommandOrigin {
        url: "east".into(),
        server_id: "east-server".into(),
        user_id: "alex".into(),
    };
    let current = CommandOrigin {
        user_id: "mira".into(),
        ..old.clone()
    };
    let west = CommandOrigin {
        url: "west".into(),
        server_id: "west-server".into(),
        user_id: "jonas".into(),
    };
    let stale = [queued_community(&old, true), queued_community(&west, false)];
    let mut statuses = HashMap::new();
    assert_eq!(
        notification_accounts("east", Some(current.clone()), &stale, &statuses),
        [current.clone(), west.clone()]
    );
    assert_eq!(
        notification_accounts("east", None, &stale, &statuses),
        std::slice::from_ref(&west),
        "logout never falls back to a stale active list entry"
    );
    statuses.insert("west".into(), ConnectionStatus::SignInRequired);
    assert_eq!(
        notification_accounts("east", Some(current), &stale, &statuses).len(),
        1
    );
    statuses.insert("east".into(), ConnectionStatus::SignInRequired);
    assert!(notification_accounts("east", Some(old), &stale, &statuses).is_empty());
}

#[test]
fn pending_notification_navigation_revalidates_owner_and_access_after_connected() {
    let origin = CommandOrigin {
        url: "east".into(),
        server_id: "server".into(),
        user_id: "alex".into(),
    };
    let target = Target::Channel("shared".into());
    let click = NotificationClick {
        origin: origin.clone(),
        target: target.clone(),
        guard: None,
    };
    let mut bootstrap = Bootstrap {
        server_id: origin.server_id.clone(),
        user: User {
            id: origin.user_id.clone(),
            ..Default::default()
        },
        channels: vec![Channel {
            id: "shared".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    assert_eq!(
        pending_notification_target(Some(click.clone()), "east", &bootstrap),
        Some(target)
    );
    assert!(pending_notification_target(Some(click.clone()), "west", &bootstrap).is_none());
    bootstrap.user.id = "mira".into();
    assert!(
        pending_notification_target(Some(click.clone()), "east", &bootstrap).is_none(),
        "same URL/channel does not authorize a prior account's target"
    );
    bootstrap.user.id = "alex".into();
    bootstrap.server_id = "replacement".into();
    assert!(pending_notification_target(Some(click.clone()), "east", &bootstrap).is_none());
    bootstrap.server_id = "server".into();
    bootstrap.channels.clear();
    assert!(
        pending_notification_target(Some(click), "east", &bootstrap).is_none(),
        "a valid owner does not preserve revoked channel access"
    );
}

#[test]
fn mentions_use_complete_case_insensitive_names_and_never_substrings() {
    let members = vec![
        User {
            id: "a".into(),
            username: "Alice".into(),
            ..Default::default()
        },
        User {
            id: "b".into(),
            username: "bob_1".into(),
            ..Default::default()
        },
        User {
            id: "c".into(),
            username: "Al".into(),
            ..Default::default()
        },
    ];
    assert_eq!(
        mentioned_users("Hi @ALICE, (@bob_1)! @Alice @Alfred email@Alice", &members),
        ["a", "b"]
    );
    assert!(mentioned_users("Alice without @ and @unknown", &members).is_empty());
}

fn local_time() -> chrono::DateTime<chrono::Local> {
    chrono::Local
        .with_ymd_and_hms(2025, 1, 15, 12, 0, 0)
        .single()
        .expect("unambiguous local midday")
}

fn message(author_id: &str, at: chrono::DateTime<chrono::Local>) -> Message {
    Message {
        author: User {
            id: author_id.into(),
            username: "Same display name".into(),
            role: "member".into(),
            ..Default::default()
        },
        created_at: at.to_rfc3339(),
        ..Message::default()
    }
}

#[test]
fn consecutive_messages_group_only_inside_the_five_minute_interval() {
    let at = local_time();
    let previous = message("author", at);
    for seconds in [0, 1, 299, 300] {
        assert!(
            group_messages(
                &previous,
                &message("author", at + chrono::Duration::seconds(seconds))
            ),
            "messages {seconds} seconds apart should group"
        );
    }
    assert!(!group_messages(
        &previous,
        &message("author", at + chrono::Duration::seconds(301))
    ));
    assert!(!group_messages(
        &previous,
        &message("author", at + chrono::Duration::milliseconds(300_001))
    ));
}

#[test]
fn different_author_ids_never_group_even_with_the_same_display_name() {
    let at = local_time();
    assert!(!group_messages(
        &message("first", at),
        &message("second", at + chrono::Duration::seconds(1))
    ));
}

#[test]
fn messages_with_backward_timestamps_start_a_new_group() {
    let at = local_time();
    let previous = message("author", at);
    for milliseconds in [-1, -500, -1000] {
        assert!(
            !group_messages(
                &previous,
                &message("author", at + chrono::Duration::milliseconds(milliseconds))
            ),
            "backward time difference {milliseconds} ms must not group"
        );
    }
}

#[test]
fn malformed_timestamps_do_not_group() {
    let valid = message("author", local_time());
    let mut invalid = valid.clone();
    invalid.created_at = "not-a-timestamp".into();
    assert!(!group_messages(&invalid, &valid));
    assert!(!group_messages(&valid, &invalid));
    assert!(!group_messages(&invalid, &invalid));
}

#[test]
fn local_date_boundaries_start_a_new_group_even_seconds_apart() {
    let midnight = chrono::Local
        .with_ymd_and_hms(2025, 1, 16, 0, 0, 0)
        .single()
        .expect("unambiguous local midnight");
    let previous = message("author", midnight - chrono::Duration::seconds(1));
    let current = message("author", midnight + chrono::Duration::seconds(1));
    assert_ne!(
        message_day(&previous.created_at),
        message_day(&current.created_at)
    );
    assert!(!group_messages(&previous, &current));
}

fn video_texture_fixture(
    context: &egui::Context,
    key: &str,
    size: [usize; 2],
    updated: Instant,
) -> VideoTile {
    let texture = context.load_texture(
        key,
        egui::ColorImage::new(size, Color32::WHITE),
        egui::TextureOptions::LINEAR,
    );
    // Simulate the backend consuming each upload, without a window or GPU.
    drop(context.tex_manager().write().take_delta());
    VideoTile {
        metadata: MediaFrame {
            user: User {
                id: key.into(),
                ..Default::default()
            },
            kind: "screen".into(),
            frame_id: "fixture".into(),
            url: String::new(),
            width: size[0] as u32,
            height: size[1] as u32,
        },
        texture,
        updated,
    }
}

#[test]
fn four_k_video_textures_replace_without_double_counting_and_evict_the_oldest() {
    let context = egui::Context::default();
    let _ = context.run(
        egui::RawInput {
            max_texture_side: Some(4096),
            ..Default::default()
        },
        |_| {},
    );
    let baseline = context.tex_manager().read().num_allocated();
    let now = Instant::now();
    let mut tiles = HashMap::new();
    let size = [3840, 2160];
    for (key, seconds) in [("first", 0), ("second", 1)] {
        assert!(reserve_video_tile(&mut tiles, key, size));
        tiles.insert(
            key.into(),
            video_texture_fixture(&context, key, size, now + Duration::from_secs(seconds)),
        );
    }
    let first_id = tiles["first"].texture.id();
    let second_id = tiles["second"].texture.id();
    assert!(reserve_video_tile(&mut tiles, "first", size));
    let first = tiles.get_mut("first").unwrap();
    first.texture.set(
        egui::ColorImage::new(size, Color32::BLACK),
        egui::TextureOptions::LINEAR,
    );
    first.updated = now + Duration::from_secs(2);
    drop(context.tex_manager().write().take_delta());
    assert_eq!(tiles.len(), 2);
    assert_eq!(tiles["first"].texture.id(), first_id);
    assert_eq!(context.tex_manager().read().num_allocated(), baseline + 2);

    assert!(reserve_video_tile(&mut tiles, "third", size));
    assert!(tiles.contains_key("first"));
    assert!(!tiles.contains_key("second"));
    assert!(context.tex_manager().read().meta(second_id).is_none());
    tiles.insert(
        "third".into(),
        video_texture_fixture(&context, "third", size, now + Duration::from_secs(3)),
    );
    assert_eq!(context.tex_manager().read().num_allocated(), baseline + 2);
    assert_eq!(
        tiles
            .values()
            .map(|tile| {
                let size = tile.texture.size();
                size[0] * size[1] * 4
            })
            .sum::<usize>(),
        66_355_200
    );
    let third_id = tiles["third"].texture.id();
    for invalid in [[0, 2160], [3841, 2160], [3840, 2161], [usize::MAX, 1]] {
        assert!(!reserve_video_tile(&mut tiles, "first", invalid));
        assert!(!reserve_video_tile(&mut tiles, "invalid", invalid));
        assert_eq!(tiles.len(), 2);
        assert_eq!(tiles["first"].texture.id(), first_id);
        assert_eq!(tiles["third"].texture.id(), third_id);
    }
}

#[test]
fn video_texture_stream_limit_preserves_replacement_and_releases_evicted_handles() {
    let context = egui::Context::default();
    let baseline = context.tex_manager().read().num_allocated();
    let now = Instant::now();
    let mut tiles = HashMap::new();
    for index in 0..16 {
        let key = format!("stream-{index}");
        assert!(reserve_video_tile(&mut tiles, &key, [16, 16]));
        tiles.insert(
            key.clone(),
            video_texture_fixture(&context, &key, [16, 16], now + Duration::from_secs(index)),
        );
    }
    let retained_id = tiles["stream-0"].texture.id();
    let evicted_id = tiles["stream-1"].texture.id();
    assert!(reserve_video_tile(&mut tiles, "stream-0", [32, 32]));
    let retained = tiles.get_mut("stream-0").unwrap();
    retained.texture.set(
        egui::ColorImage::new([32, 32], Color32::BLACK),
        egui::TextureOptions::LINEAR,
    );
    retained.updated = now + Duration::from_secs(100);
    drop(context.tex_manager().write().take_delta());
    assert_eq!(tiles.len(), 16);
    assert_eq!(tiles["stream-0"].texture.id(), retained_id);
    assert_eq!(tiles["stream-0"].texture.size(), [32, 32]);
    assert!(reserve_video_tile(&mut tiles, "stream-16", [16, 16]));
    assert!(!tiles.contains_key("stream-1"));
    assert!(context.tex_manager().read().meta(evicted_id).is_none());
    tiles.insert(
        "stream-16".into(),
        video_texture_fixture(
            &context,
            "stream-16",
            [16, 16],
            now + Duration::from_secs(101),
        ),
    );
    assert_eq!(tiles.len(), 16);
    assert_eq!(context.tex_manager().read().num_allocated(), baseline + 16);
}

#[test]
fn video_display_respects_the_graphics_limit_and_preserves_small_frames() {
    let context = egui::Context::default();
    context.begin_frame(egui::RawInput {
        max_texture_side: Some(1024),
        ..Default::default()
    });
    let color = Color32::from_rgba_premultiplied(60, 40, 20, 128);
    let large = Arc::new(egui::ColorImage::new([1280, 720], color));
    let resized = display_video_frame(large.clone(), 1024).unwrap();
    assert_eq!(large.size, [1280, 720]);
    assert_eq!(resized.size, [1024, 576]);
    assert!(resized.pixels.iter().all(|pixel| *pixel == color));
    let texture = context.load_texture("bounded-video", resized, egui::TextureOptions::LINEAR);
    assert_eq!(texture.size(), [1024, 576]);
    let small = Arc::new(egui::ColorImage::new([320, 180], color));
    assert!(Arc::ptr_eq(
        &small,
        &display_video_frame(small.clone(), 1024).unwrap()
    ));
    assert!(display_video_frame(small, 0).is_none());
    assert!(display_video_frame(
        Arc::new(egui::ColorImage {
            size: [3840, 2160],
            pixels: vec![],
        }),
        4096
    )
    .is_none());
    let _ = context.end_frame();
}

fn enter(modifiers: egui::Modifiers, pressed: bool) -> egui::Event {
    egui::Event::Key {
        key: egui::Key::Enter,
        physical_key: Some(egui::Key::Enter),
        pressed,
        repeat: false,
        modifiers,
    }
}

fn input(events: Vec<egui::Event>, modifiers: egui::Modifiers) -> egui::InputState {
    egui::InputState::default().begin_frame(
        egui::RawInput {
            events,
            modifiers,
            ..egui::RawInput::default()
        },
        false,
        1.0,
    )
}

#[test]
fn plain_enter_is_consumed_once_and_its_release_is_preserved() {
    let mut input = input(
        vec![
            enter(egui::Modifiers::NONE, true),
            enter(egui::Modifiers::NONE, false),
        ],
        egui::Modifiers::NONE,
    );
    assert!(consume_send_key(&mut input));
    assert_eq!(input.events.len(), 1);
    assert!(matches!(
        input.events[0],
        egui::Event::Key { pressed: false, .. }
    ));
    assert!(!consume_send_key(&mut input));
    assert_eq!(input.events.len(), 1);
}

#[test]
fn modified_enter_is_preserved_for_newlines_and_other_shortcuts() {
    for modifiers in [
        egui::Modifiers::SHIFT,
        egui::Modifiers::ALT,
        egui::Modifiers::CTRL,
        egui::Modifiers::COMMAND,
        egui::Modifiers::SHIFT | egui::Modifiers::ALT,
        egui::Modifiers::SHIFT | egui::Modifiers::CTRL,
    ] {
        let mut input = input(vec![enter(modifiers, true)], modifiers);
        assert!(!consume_send_key(&mut input));
        assert_eq!(input.events.len(), 1);
        assert!(
            matches!(input.events[0], egui::Event::Key { modifiers: actual, pressed: true, .. } if actual == modifiers)
        );
    }
}

#[test]
fn mixed_input_consumes_only_the_plain_enter_event() {
    let mut input = input(
        vec![
            egui::Event::Text("hello".into()),
            enter(egui::Modifiers::NONE, true),
            enter(egui::Modifiers::SHIFT, true),
            enter(egui::Modifiers::CTRL, true),
        ],
        egui::Modifiers::SHIFT,
    );
    assert!(consume_send_key(&mut input));
    assert_eq!(input.events.len(), 3);
    assert!(matches!(&input.events[0], egui::Event::Text(text) if text == "hello"));
    assert!(
        matches!(input.events[1], egui::Event::Key { modifiers, .. } if modifiers == egui::Modifiers::SHIFT)
    );
    assert!(
        matches!(input.events[2], egui::Event::Key { modifiers, .. } if modifiers == egui::Modifiers::CTRL)
    );
}

#[test]
fn older_profile_without_audio_keeps_manual_timing_while_new_profiles_are_automatic() {
    let old: Preferences = serde_json::from_str(r#"{"server":"","username":""}"#).unwrap();
    assert!(!old.audio.adaptive_jitter);
    assert!(Preferences::default().audio.adaptive_jitter);
}
