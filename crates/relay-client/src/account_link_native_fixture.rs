//! Explicitly selected native QA only. No production issuer exception or UI input.
use super::*;
use serde_json::{json, Value};
use std::os::windows::fs::MetadataExt;
use std::{fs::OpenOptions, io::Write};
use winit::platform::windows::EventLoopBuilderExtWindows;

struct OwnedUi {
    app: RelayApp,
    root: PathBuf,
    issuer: String,
    start: Instant,
    urls: usize,
    signed_in: bool,
    linked: bool,
    unlinked: bool,
    status: Value,
}

impl eframe::App for OwnedUi {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        self.app.update(ctx, frame);
        // Preserve the app's exact browser request while preventing a default
        // personal browser launch. Codex opens it with Cua in an owned browser.
        let url = ctx.output_mut(|output| output.open_url.take());
        if let Some(url) = url {
            assert!(url.url.starts_with(&(self.issuer.clone() + "/")));
            self.urls += 1;
            assert!(self.urls <= 8);
            let path = self.root.join(format!("browser-{:02}.json", self.urls));
            let mut out = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(path)
                .unwrap();
            out.write_all(json!({"url":url.url}).to_string().as_bytes())
                .unwrap();
        }
        self.signed_in |= self.app.central_account.account.is_some();
        self.linked |= self.app.account_link_ui.linked.is_some();
        self.unlinked |= self.linked
            && self.app.account_link_ui.linked.is_none()
            && !self.app.account_link_ui.editing
            && self.app.account_link_ui.pending.is_none()
            && self.app.account_link_ui.provider.is_some()
            && self.app.settings_open
            && self.app.settings_tab == SettingsTab::Account;
        let status = json!({
            "authenticated":self.app.session.is_some(),
            "signed_in_seen":self.signed_in,"linked_seen":self.linked,"unlinked_seen":self.unlinked,
            "central_handle":self.app.central_account.account.as_ref().map(|a|a.handle.as_str()),
            "linked_handle":self.app.account_link_ui.linked.as_ref().map(|l|l.handle.as_str()),
            "pending":self.app.account_link_ui.pending.is_some(),
            "link_error":self.app.account_link_ui.error,
            "browser_requests":self.urls
        });
        if status != self.status {
            std::fs::write(
                self.root.join("ui-status.json"),
                serde_json::to_vec_pretty(&status).unwrap(),
            )
            .unwrap();
            self.status = status;
        }
        if self.start.elapsed() > Duration::from_secs(840) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        ctx.request_repaint_after(Duration::from_millis(200));
    }
    fn on_exit(&mut self, glow: Option<&eframe::glow::Context>) {
        self.app.on_exit(glow);
        std::fs::write(self.root.join("ui-closed.json"), self.status.to_string()).unwrap();
    }
}

#[test]
#[ignore = "Codex-only native Cua journey with explicit local QA profile and live provider/server"]
fn owned_native_account_link_journey() {
    let root =
        PathBuf::from(std::env::var("RELAY_OWN_NATIVE_UI").expect("explicit own UI fixture root"));
    assert!(
        root.is_absolute()
            && root.starts_with(r"C:\Important\Projects\2026-10-03\can\work\relay-focus-20261020")
    );
    let profile = root.join("profile");
    let args = vec![
        "relay".to_string(),
        "--profile-dir".into(),
        profile.to_str().unwrap().into(),
        "--native-render-probe".into(),
    ];
    let probe = crate::native_render_probe::path_from_args(&args)
        .unwrap()
        .unwrap();
    crate::profile_paths::configure(&args).unwrap();
    let config_path = root.join("ui-config.json");
    assert!(
        std::fs::symlink_metadata(&config_path)
            .unwrap()
            .file_attributes()
            & 0x400
            == 0
    );
    let config: Value = serde_json::from_slice(&std::fs::read(config_path).unwrap()).unwrap();
    let issuer = config["issuer"].as_str().unwrap().to_string();
    let server = url::Url::parse(config["server_url"].as_str().unwrap()).unwrap();
    assert_eq!(server.scheme(), "http");
    assert_eq!(server.host_str(), Some("127.0.0.1"));
    assert!(server.port().is_some());
    let position = egui::pos2(
        config["x"].as_f64().unwrap() as f32,
        config["y"].as_f64().unwrap() as f32,
    );
    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Glow,
        viewport: egui::ViewportBuilder::default()
            .with_title("Relay · Owned account-link QA")
            .with_decorations(false)
            .with_active(false)
            .with_position(position)
            .with_inner_size([1000.0, 1000.0])
            .with_min_inner_size([940.0, 650.0]),
        event_loop_builder: Some(Box::new(|builder| {
            builder.with_any_thread(true);
        })),
        ..Default::default()
    };
    let readback = root.clone();
    eframe::run_native(
        "Relay owned account-link QA",
        options,
        Box::new(move |cc| {
            let mut app = RelayApp::new(cc, crate::invitation_launch::Launch::default());
            app.account_start.bypass = true; // This fixture explicitly exercises a legacy community account.
            app.central_account.provider = Some(
                crate::central_account::Provider::owned_loopback_fixture(&issuer),
            );
            app.central_account.error = None;
            // This older acceptance explicitly drives the browser fallback.
            // The separate account-entry fixture exercises the hosted page.
            app.account_webview = None;
            app.native_render_probe = Some(crate::native_render_probe::Probe::open(probe).unwrap());
            app.login.url = server.to_string().trim_end_matches('/').into();
            app.login.username = "Alice".into();
            app.login.password = "a-long-test-password".into();
            app.authenticate();
            Box::new(OwnedUi {
                app,
                root,
                issuer,
                start: Instant::now(),
                urls: 0,
                signed_in: false,
                linked: false,
                unlinked: false,
                status: Value::Null,
            })
        }),
    )
    .unwrap();
    let status: Value =
        serde_json::from_slice(&std::fs::read(readback.join("ui-closed.json")).unwrap()).unwrap();
    assert_eq!(status["signed_in_seen"], true);
    assert_eq!(status["linked_seen"], true);
    assert_eq!(status["unlinked_seen"], true);
}
