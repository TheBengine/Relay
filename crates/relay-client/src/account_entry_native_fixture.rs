//! Explicit owned Keycloak/Cua acceptance. No production HTTP issuer exception.
use super::*;
use serde_json::{json, Value};
use std::os::windows::fs::MetadataExt;
use winit::platform::windows::EventLoopBuilderExtWindows;

struct OwnedEntry {
    app: RelayApp,
    root: PathBuf,
    handle: String,
    signins: u32,
    signed_in: bool,
    browser_requests: u32,
    status: Value,
    began: Instant,
}
impl eframe::App for OwnedEntry {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        self.app.update(ctx, frame);
        if ctx.output_mut(|output| output.open_url.take()).is_some() {
            self.browser_requests += 1;
        }
        let signed_in = self.app.central_account.account.is_some();
        if signed_in && !self.signed_in {
            self.signins += 1;
            assert_eq!(
                self.app.central_account.account.as_ref().unwrap().handle,
                self.handle
            );
        }
        self.signed_in = signed_in;
        let status = json!({"signed_in":signed_in,"successful_signins":self.signins,"browser_requests":self.browser_requests,
            "community_joined":self.app.session.is_some(),"error":self.app.central_account.error});
        if status != self.status {
            std::fs::write(
                self.root.join("ui-status.json"),
                serde_json::to_vec_pretty(&status).unwrap(),
            )
            .unwrap();
            self.status = status;
        }
        if self.began.elapsed() > Duration::from_secs(1200) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        ctx.request_repaint_after(Duration::from_millis(100));
    }
    fn on_exit(&mut self, glow: Option<&eframe::glow::Context>) {
        self.app.on_exit(glow);
        std::fs::write(self.root.join("ui-closed.json"), self.status.to_string()).unwrap();
    }
}

#[test]
#[ignore = "Codex Cua only: owned local Keycloak, signup/signout/OTP in the real native UI"]
fn owned_native_account_entry_signup_and_otp() {
    let root =
        PathBuf::from(std::env::var("RELAY_OWN_ACCOUNT_ENTRY_UI").expect("explicit QA root"));
    assert!(
        root.is_absolute()
            && root
                .starts_with(r"C:\Important\Projects\2026-10-03\can\work\account-journey-20261020")
    );
    for parent in root.ancestors() {
        assert!(std::fs::symlink_metadata(parent).unwrap().file_attributes() & 0x400 == 0);
    }
    let config: Value =
        serde_json::from_slice(&std::fs::read(root.join("ui-config.json")).unwrap()).unwrap();
    let issuer = config["issuer"].as_str().unwrap().to_owned();
    let handle = config["handle"].as_str().unwrap().to_owned();
    let args = vec![
        "relay".into(),
        "--profile-dir".into(),
        root.join("profile").to_str().unwrap().into(),
    ];
    crate::profile_paths::configure(&args).unwrap();
    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Glow,
        viewport: egui::ViewportBuilder::default()
            .with_title("Relay · Owned account entry QA")
            .with_decorations(false)
            .with_active(false)
            .with_position([
                config["x"].as_f64().unwrap() as f32,
                config["y"].as_f64().unwrap() as f32,
            ])
            .with_inner_size([1000.0, 1100.0])
            .with_min_inner_size([940.0, 650.0]),
        event_loop_builder: Some(Box::new(|builder| {
            builder.with_any_thread(true);
        })),
        ..Default::default()
    };
    let readback = root.clone();
    eframe::run_native(
        "Relay owned account entry QA",
        options,
        Box::new(move |cc| {
            let mut app = RelayApp::new(cc, crate::invitation_launch::Launch::default());
            app.prefs.close_to_tray = false;
            app.central_account.provider = Some(
                crate::central_account::Provider::owned_loopback_fixture(&issuer),
            );
            app.central_account.error = None;
            app.account_webview
                .as_mut()
                .unwrap()
                .owned_loopback_fixture(&issuer);
            Box::new(OwnedEntry {
                app,
                root,
                handle,
                signins: 0,
                signed_in: false,
                browser_requests: 0,
                status: Value::Null,
                began: Instant::now(),
            })
        }),
    )
    .unwrap();
    let result: Value =
        serde_json::from_slice(&std::fs::read(readback.join("ui-closed.json")).unwrap()).unwrap();
    assert!(result["successful_signins"].as_u64().unwrap() >= 2);
    assert_eq!(result["browser_requests"], 0);
    assert_eq!(result["community_joined"], false);
}


/// One visual acceptance surface with synthetic account/server labels. This never
/// calls the normal update loop, signs in, opens a host, or uses media devices.
#[test]
#[ignore = "Cua visual check in an explicit isolated profile"]
fn welcome_home_visual_preview() {
    let root = PathBuf::from(std::env::var("RELAY_UI_PREVIEW_ROOT").expect("explicit preview folder"));
    assert!(root.starts_with(r"C:\Important\Projects\2026-10-03\can\work\login-usability-20261025"));
    for part in root.ancestors() {
        if let Ok(info) = std::fs::symlink_metadata(part) { assert_eq!(info.file_attributes() & 0x400, 0); }
    }
    crate::profile_paths::configure(&["relay".into(), "--profile-dir".into(), root.join("profile").to_str().unwrap().into()]).unwrap();
    struct Preview { app: RelayApp, began: Instant }
    impl eframe::App for Preview {
        fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
            self.app.account_front(ctx);
            if self.began.elapsed() > Duration::from_secs(180) { ctx.send_viewport_cmd(egui::ViewportCommand::Close); }
            ctx.request_repaint_after(Duration::from_millis(250));
        }
        fn on_exit(&mut self, glow: Option<&eframe::glow::Context>) { self.app.on_exit(glow); }
    }
    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Glow,
        viewport: egui::ViewportBuilder::default().with_title("Relay - isolated UI preview")
            .with_active(false).with_position([180.0, 80.0]).with_inner_size([1000.0, 1120.0]),
        event_loop_builder: Some(Box::new(|builder| { builder.with_any_thread(true); })),
        ..Default::default()
    };
    eframe::run_native("Relay - isolated UI preview", options, Box::new(|cc| {
        let mut app = RelayApp::new(cc, crate::invitation_launch::Launch::default());
        app.central_account.provider = None;
        app.central_account.account = Some(crate::central_account::Account {
            issuer: "https://preview.invalid/realms/relay".into(), subject: "preview-only".into(),
            handle: "preview".into(), expires_at: u64::MAX,
        });
        app.account_start.entered = true;
        app.account_start.home = true;
        app.account_start.auto = None;
        app.account_start.hosting_error = Some("The saved server needs a database update. Existing data was kept.".into());
        app.prefs.close_to_tray = false;
        app.prefs.communities = ["Relay House", "Family server"].into_iter().enumerate().map(|(i, name)|
            crate::community::SavedCommunity { url: format!("https://server{i}.invalid"),
                name: name.into(), server_id: uuid::Uuid::new_v4().to_string(), ..Default::default() }
        ).collect();
        app.error = None;
        Box::new(Preview { app, began: Instant::now() })
    })).unwrap();
}
