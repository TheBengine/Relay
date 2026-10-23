//! Explicit visual-only native QA; no real credentials, network session or media.
use super::*;
use std::os::windows::fs::MetadataExt;
use winit::platform::windows::EventLoopBuilderExtWindows;

struct OwnedPeople {
    app: RelayApp,
    root: PathBuf,
    began: Instant,
}
impl eframe::App for OwnedPeople {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        self.app.update(ctx, frame);
        std::fs::write(
            self.root.join("ui-running"),
            b"owned fixture; no account or media session",
        )
        .unwrap();
        if self.began.elapsed() > Duration::from_secs(180) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
    fn on_exit(&mut self, glow: Option<&eframe::glow::Context>) {
        self.app.on_exit(glow);
        std::fs::write(self.root.join("ui-closed"), b"closed normally").unwrap();
    }
}

#[test]
#[ignore = "explicit owned Cua native visual fixture, no real accounts or media"]
fn people_owned_native_friends_layout() {
    let root = PathBuf::from(std::env::var("RELAY_OWN_PEOPLE_UI").unwrap_or_else(|_| {
        r"C:\Important\Projects\2026-10-03\can\work\people-qa-20261021\native".into()
    }));
    assert!(root.starts_with(r"C:\Important\Projects\2026-10-03\can\work\people-qa-20261021"));
    for p in root.ancestors() {
        assert_eq!(
            std::fs::symlink_metadata(p).unwrap().file_attributes() & 0x400,
            0
        );
    }
    crate::profile_paths::configure(&[
        "relay".into(),
        "--profile-dir".into(),
        root.join("profile").to_str().unwrap().into(),
    ])
    .unwrap();
    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Glow,
        viewport: egui::ViewportBuilder::default()
            .with_title("Relay Friends - owned visual QA")
            .with_active(false)
            .with_inner_size([940.0, 650.0])
            .with_min_inner_size([940.0, 650.0]),
        event_loop_builder: Some(Box::new(|b| {
            b.with_any_thread(true);
        })),
        ..Default::default()
    };
    eframe::run_native(
        "Relay Friends owned QA",
        options,
        Box::new(move |cc| {
            let mut app = RelayApp::new(cc, crate::invitation_launch::Launch::default());
            app.prefs.close_to_tray = false;
            app.prefs.notifications = false;
            app.prefs.audio.sounds_enabled = false;
            app.account_start.home = false;
            app.account_start.bypass = false;
            app.central_account.provider = Some(
                crate::central_account::Provider::new(
                    "https://accounts.example.test/realms/relay-accounts",
                    "relay-desktop",
                )
                .unwrap(),
            );
            app.central_account.account = Some(crate::central_account::Account {
                issuer: "https://accounts.example.test/realms/relay-accounts".into(),
                subject: "owned-alice".into(),
                handle: "alice".into(),
                expires_at: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs()
                    .saturating_add(3600),
            });
            let me = User {
                id: "alice-owned".into(),
                username: "alice".into(),
                role: "member".into(),
                ..Default::default()
            };
            let peer = User {
                id: "bob-owned".into(),
                username: "bob".into(),
                role: "member".into(),
                ..Default::default()
            };
            let mut b = Bootstrap {
                user: me.clone(),
                server_id: "owned-people-service".into(),
                server_name: "Relay Friends".into(),
                members: vec![me.clone(), peer.clone()],
                capabilities: vec![
                    "people-v1".into(),
                    "social-v1".into(),
                    "private-calls-v1".into(),
                ],
                ..Default::default()
            };
            b.social.incoming.push(peer.id);
            app.session = Some(Session {
                user: me,
                token: "owned-visual-fixture-not-a-real-credential".into(),
            });
            app.bootstrap = Some(Arc::new(b));
            app.login.url = "https://accounts.example.test/people".into();
            app.selected_nav = false;
            app.home_page = Some(SettingsTab::Friends);
            app.connection_status = ConnectionStatus::Connected;
            Box::new(OwnedPeople {
                app,
                root,
                began: Instant::now(),
            })
        }),
    )
    .unwrap();
}
