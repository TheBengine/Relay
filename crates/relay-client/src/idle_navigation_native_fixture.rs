//! Opt-in native regression fixture: synthetic connection updates, no remote
//! service, credentials, microphone, camera or personal profile.
use super::*;
use winit::platform::windows::EventLoopBuilderExtWindows;

const ROOT: &str = r"C:\Important\Projects\2026-10-03\can\work\relay-resource-audit-20261104\native-fixture";

struct Fixture {
    app: RelayApp,
    updates: mpsc::Sender<Update>,
    started: Instant,
    injected: bool,
    checked: bool,
    frames: usize,
    restored_at: Option<usize>,
    restored_and_hidden: bool,
    minimized_at: Option<Instant>,
    minimized_checked: bool,
}

impl eframe::App for Fixture {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        self.frames += 1;
        if self.started.elapsed() > Duration::from_secs(10) {
            assert!(self.checked && self.restored_and_hidden && self.minimized_checked, "Hidden progress and restoration must complete");
            self.app.hide_for_exit();
        }
        if self.started.elapsed() > Duration::from_secs(1) && !self.injected {
            assert!(!self.app.remote_video_window_visible(ctx));
            self.updates.send(Update::Notice("Owned hidden delivery".into())).unwrap();
            self.app.prefs.compact = true;
            self.app.compact = true;
            self.app.defer_preferences();
            self.injected = true;
        }
        if self.restored_and_hidden && self.minimized_at.is_none()
            && ctx.input(|i| i.viewport().minimized == Some(true)) {
            self.minimized_at = Some(Instant::now());
            self.updates.send(Update::Notice("Owned minimized delivery".into())).unwrap();
            self.app.prefs.compact = false;self.app.compact = false;self.app.defer_preferences();
        }
        self.app.update(ctx, frame);
        if self.injected && !self.app.exiting {
            if self.minimized_at.is_none() {
                assert_eq!(self.app.notice.as_ref().map(|n| n.0.as_str()), Some("Owned hidden delivery"));
            } else if self.minimized_at.is_some_and(|at| at.elapsed() > Duration::from_secs(2)) && !self.minimized_checked {
                assert_eq!(self.app.notice.as_ref().map(|n| n.0.as_str()), Some("Owned minimized delivery"));
                assert!(self.app.preferences_dirty_at.is_none());
                self.minimized_checked = true;
            }
            assert!(self.app.current.is_none());
            assert!(self.app.home_page == Some(SettingsTab::Friends));
            if self.started.elapsed() > Duration::from_secs(3) {
                if self.minimized_at.is_none() {
                    assert!(self.app.preferences_dirty_at.is_none(), "Hidden preference writes must finish");
                }
                assert!(self.app.source_preview.is_none() && self.app.preview_release.is_none(), "Hidden local preview must finish releasing its capture worker");
                self.checked = true;
                if self.restored_at.is_none() {
                    self.app.tray.as_ref().unwrap().prepare_show();
                    self.restored_at = Some(self.frames);
                } else if !self.restored_and_hidden && self.restored_at.is_some_and(|start| self.frames > start + 1) {
                    assert!(self.app.tray.as_ref().unwrap().window_visible(), "Restored native window must be visible across rendered frames");
                    self.app.tray.as_ref().unwrap().hide();
                    assert!(!self.app.tray.as_ref().unwrap().window_visible());
                    self.restored_and_hidden = true;
                    self.app.tray.as_ref().unwrap().prepare_show();
                    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                }
            }
        }
    }
    fn on_exit(&mut self, glow: Option<&eframe::glow::Context>) {
        assert!(self.injected && self.checked && self.restored_and_hidden && self.minimized_checked, "Do not report success for an early native exit");
        self.app.on_exit(glow);
        std::fs::write(PathBuf::from(ROOT).join("passed.json"),
            serde_json::to_vec_pretty(&serde_json::json!({"routing":true,"hidden_delivery":true,
                "hidden_preferences":true,"sound_setup_cancelled":true,"normal_exit":true,
                "minimized_service_progress":self.minimized_checked,"hidden_local_preview_released":true,"restored_and_hidden":self.restored_and_hidden,"frames":self.frames})).unwrap()).unwrap();
    }
}

#[test]
#[ignore = "Owned native hidden-window and connection-adoption regression; explicit isolated path"]
fn owned_idle_and_friends_navigation() {
    let root = PathBuf::from(ROOT);
    for ancestor in root.ancestors() {
        if let Ok(metadata) = std::fs::symlink_metadata(ancestor) {
            use std::os::windows::fs::MetadataExt;
            assert_eq!(metadata.file_attributes() & 0x400, 0);
        }
    }
    assert_eq!(std::env::current_exe().unwrap(), root.join("relay-idle-fixture.exe"));
    assert!(!root.join("profile").exists() && !root.join("passed.json").exists());
    assert_eq!(std::env::var("RELAY_ACCOUNT_ISSUER").unwrap().trim(), "");
    assert!(std::env::var("RELAY_TEST_PASSWORD").is_err());
    crate::profile_paths::configure(&["owned-idle".into(), "--profile-dir".into(), root.join("profile").to_str().unwrap().into()]).unwrap();
    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Glow,
        viewport: egui::ViewportBuilder::default().with_visible(false).with_active(false)
            .with_title("Relay owned idle navigation test").with_inner_size([1000.0, 800.0]),
        event_loop_builder: Some(Box::new(|builder| { builder.with_any_thread(true); })),
        ..Default::default()
    };
    eframe::run_native("Relay owned idle navigation test", options, Box::new(|cc| {
        let mut app = RelayApp::new(cc, crate::invitation_launch::Launch::default());
        drop(app.tray.take());
        app.tray = Some(crate::tray::Tray::bind(cc, true).unwrap());
        app.prefs.automatic_update_checks = false;
        app.prefs.stay_signed_in = false;
        app.prefs.remember_session = false;
        app.prefs.audio_setup_offered = true;
        app.prefs.close_to_tray = false;
        app.account_start.entered = true;
        app.account_start.home = false;
        let issuer = "http://127.0.0.1:9/realms/relay-accounts";
        app.central_account = crate::central_account::Controller::owned(
            crate::central_account::Provider::owned_loopback_fixture(issuer));
        app.central_account.account = Some(crate::central_account::Account {
            issuer: issuer.into(), subject: "owned".into(), handle: "Owned".into(), expires_at: u64::MAX,
        });
        app.prefs.last_relay_identity = Some((issuer.into(), "owned".into()));
        let url = app.people_url().unwrap();
        let user = User { id: "owned-user".into(), username: "Owned".into(), ..Default::default() };
        let session = Session { user: user.clone(), token: "synthetic-no-service-token".into() };
        let mut bootstrap = Bootstrap { user, server_id: "9e2bf898-d40b-45b8-a81e-2c9cf3720fb1".into(), server_name: "Owned Friends".into(), ..Default::default() };
        bootstrap.capabilities.push("people-v1".into());
        bootstrap.channels.push(relay_protocol::Channel { id: "old-group".into(), name: "Owned group".into(), kind: "text".into(), ..Default::default() });
        let scope = crate::community::conversation_scope(&app.prefs.communities, &url, &bootstrap.server_id, &session.user.id);
        app.prefs.conversations.remember(&scope, &Target::Channel("old-group".into()));
        let (tx, rx) = mpsc::channel();
        app.network.rx = rx;
        let connect = |app: &mut RelayApp| {
            tx.send(Update::Connected { url: url.clone(), session: session.clone(), bootstrap: Box::new(bootstrap.clone()) }).unwrap();
            app.updates(&cc.egui_ctx);
        };
        // This is the path missed by the old shell-only fix: Connected arrives
        // before the user has selected Friends, with a saved group in preferences.
        app.selected_nav = true;
        app.account_start.friends_requested = false;
        connect(&mut app);
        assert!(app.current.is_none());
        assert!(app.home_page == Some(SettingsTab::Friends));
        app.open_people();
        assert!(app.current.is_none());
        assert!(app.home_page == Some(SettingsTab::Friends));
        // A conversation deliberately opened now survives reconnection.
        app.select(Target::Channel("old-group".into()));
        connect(&mut app);
        assert_eq!(app.current, Some(Target::Channel("old-group".into())));
        // Returning to Friends before reconnecting must not reopen that chat.
        app.home_page = Some(SettingsTab::Friends);
        connect(&mut app);
        assert!(app.current.is_none());
        // Explicit launch/navigation still wins over the overview.
        app.initial_channel = Some("Owned group".into());
        connect(&mut app);
        assert_eq!(app.current, Some(Target::Channel("old-group".into())));
        app.home_page = Some(SettingsTab::Friends);
        connect(&mut app);
        // Starting and ending a call while hidden cancels a stale setup and
        // restores its unsaved draft without ever opening a microphone here.
        let mut draft = app.audio_applied.clone();
        draft.microphone_gain = 0.65;
        app.audio_setup = Some(crate::audio_setup::AudioSetup { draft_before: Some(draft.clone()), ..Default::default() });
        app.joining_voice = Some("owned-call".into());
        app.audio_setup_tick();
        app.joining_voice = None;
        app.audio_setup_tick();
        assert!(app.audio_setup.is_none() && !app.mic_test);
        assert_eq!(app.prefs.audio, draft);
        app.prefs.audio = app.audio_applied.clone();
        app.source_preview = Some(source_preview::SourcePreview::owned_generated());
        Box::new(Fixture { app, updates: tx, started: Instant::now(), injected: false, checked: false, frames: 0, restored_at: None, restored_and_hidden: false, minimized_at: None, minimized_checked: false })
    })).unwrap();
    assert!(root.join("passed.json").is_file());
}
