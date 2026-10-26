//! Test-only native QA for the real server-creation screen. Ignored by default.
//!
//! It runs the actual `RelayApp` through eframe with an owned profile and an
//! owned server-data folder, signed in with a synthetic Relay account that is
//! only a local stand-in (no account service is contacted). It shows the
//! account home with the real "Create a server" button and then leaves the
//! window to an external controller (Codex, through Cua): this fixture clicks
//! nothing. What the controller can prove with it:
//!
//! * the creation screen at the window size in the manifest, including scroll
//!   and the Create / Cancel buttons;
//! * Cancel starts no server, and a second visit shows the recommended defaults
//!   again (see `state.json`: `creation.choosing`, `protection`, `route`,
//!   `new_host`, `busy`);
//! * "Open + Tailscale" travelling through the real path: Create server → the
//!   real `host_local()` → the real relay-server process (staged beside this
//!   executable, data in the owned folder) → the real account form and `Auth`
//!   with `initial_protection` → the server's own answer
//!   (`server.protection_level`) → the saved plan tied to the new server id
//!   (`pending`) → the Tailscale step (`sharing.tailscale_step`). A later run
//!   with the same profile (`run` 2, 3) shows the plan resumed after a restart.
//!
//! Safety: the account-free route would publish to a public directory, so if it
//! is selected the fixture stops before anything is shared and says so in
//! `safety_stop`. A Tailscale sign-in only starts if the controller presses
//! "Enable sharing", which it must not. Nothing is written but to the run folder.
use super::*;
use serde::Deserialize;
use serde_json::{json, Value};
use std::os::windows::fs::MetadataExt;
use winit::platform::windows::EventLoopBuilderExtWindows;

const ROOT: &str = r"C:\Important\Projects\2026-10-03\can\work\friends-call-quality-20261021\native-creation-ui-20261023";
const EXECUTABLE: &str = "relay-creation-qa.exe";
const DEFAULT_SECONDS: u64 = 600;

/// Every path used must be under the known parent and free of redirects.
fn direct(path: &std::path::Path) {
    assert!(path.starts_with(ROOT), "outside the QA folder");
    for ancestor in path.ancestors() {
        if let Ok(metadata) = std::fs::symlink_metadata(ancestor) {
            assert_eq!(metadata.file_attributes() & 0x400, 0, "redirected QA path");
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    x: f32,
    y: f32,
    /// Ordinary window sizes: 600..=1600 wide, 480..=1200 high.
    width: f32,
    height: f32,
    nonce: String,
    /// 1 for a fresh profile, 2 or 3 to reopen the same profile after a restart.
    #[serde(default = "first_run")]
    run: u32,
    #[serde(default = "default_seconds")]
    seconds: u64,
}

fn first_run() -> u32 {
    1
}
fn default_seconds() -> u64 {
    DEFAULT_SECONDS
}

fn validate(manifest: &Manifest) -> Result<(), String> {
    if !manifest.x.is_finite() || !manifest.y.is_finite() {
        return Err("window position must be finite".into());
    }
    if !(600.0..=1600.0).contains(&manifest.width) || !(480.0..=1200.0).contains(&manifest.height) {
        return Err("window size must be 600-1600 wide and 480-1200 high".into());
    }
    if manifest.nonce.len() != 32 || !manifest.nonce.bytes().all(|v| v.is_ascii_hexdigit()) {
        return Err("nonce must be 32 hexadecimal characters".into());
    }
    if !(1..=3).contains(&manifest.run) {
        return Err("run must be 1, 2 or 3".into());
    }
    if !(60..=900).contains(&manifest.seconds) {
        return Err("seconds must be between 60 and 900".into());
    }
    Ok(())
}

/// Free text is published only short and printable.
fn bounded(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).take(160).collect()
}

fn prefix(id: &str) -> String {
    id.chars().take(8).collect()
}

struct OwnedApp {
    app: RelayApp,
    root: PathBuf,
    run: u32,
    seconds: u64,
    frames: u64,
    began: Instant,
    window: [f32; 2],
    safety_stop: Option<&'static str>,
}

impl OwnedApp {
    fn record(&self) -> Value {
        let app = &self.app;
        let setup = &app.hosting_setup;
        let server_id = app.bootstrap.as_ref().map(|b| b.server_id.clone());
        let pending = app.prefs.pending_creation.as_ref();
        let iroh_hosted = server_id
            .as_deref()
            .is_some_and(|id| crate::iroh_route::hosted_status(id).is_some());
        json!({
            "run": self.run,
            "frame": self.frames,
            "elapsed_secs": self.began.elapsed().as_secs(),
            "window": self.window,
            "iroh_available": crate::iroh_route::available(),
            "signed_in": app.session.is_some(),
            "busy": app.busy,
            "error": app.error.as_deref().map(bounded),
            "creation": {
                "choosing": setup.choosing_creation,
                "confirmed": setup.creation_confirmed,
                "protection": format!("{:?}", setup.creation_protection),
                "route": format!("{:?}", setup.creation_route),
                "route_before_account": setup.route_to_apply.map(|r| format!("{r:?}")),
                "new_host": setup.new_host,
                "local_ready": setup.local_ready,
            },
            "server": {
                "id_prefix": server_id.as_deref().map(prefix),
                "protection_level": app.bootstrap.as_ref().map(|b| format!("{:?}", b.protection.level)),
                "is_local": crate::local_host::is_local_url(&app.login.url),
            },
            "pending": {
                "present": pending.is_some(),
                "route": pending.map(|p| format!("{:?}", p.route)),
                "server_id_prefix": pending.map(|p| prefix(&p.server_id)),
                "matches_current_server": pending.zip(server_id.as_deref())
                    .is_some_and(|(p, id)| p.server_id == id),
            },
            "sharing": {
                "label": setup.sharing(iroh_hosted).map(|s| s.label()),
                "dialog_open": setup.open,
                "tailscale_step": setup.tailscale_step,
                "attempt": format!("{:?}", setup.route_attempt),
                "failure": setup.route_failure.map(|f| format!("{f:?}")),
            },
            "safety_stop": self.safety_stop,
        })
    }

    fn publish(&self) {
        let pending = self.root.join("state.pending");
        let published = self.root.join("state.json");
        direct(&pending);
        direct(&published);
        std::fs::write(&pending, serde_json::to_vec(&self.record()).unwrap()).unwrap();
        std::fs::rename(&pending, &published).unwrap();
    }
}

impl eframe::App for OwnedApp {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        // Stop before anything could be published to the public directory.
        if self.safety_stop.is_none() {
            let setup = &self.app.hosting_setup;
            let iroh_chosen = setup.route_to_apply == Some(host_setup_ui::Choice::Iroh)
                || self
                    .app
                    .prefs
                    .pending_creation
                    .as_ref()
                    .is_some_and(|p| p.route == host_setup_ui::Choice::Iroh);
            if iroh_chosen {
                self.safety_stop = Some("account-free route selected; stopped before sharing");
            }
        }
        if self.safety_stop.is_none() {
            self.app.update(ctx, frame);
        }
        self.frames += 1;
        let size = ctx.screen_rect().size();
        self.window = [size.x.round(), size.y.round()];
        self.publish();
        if self.safety_stop.is_some()
            || self.root.join("finish").exists()
            || self.began.elapsed() > Duration::from_secs(self.seconds)
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        ctx.request_repaint_after(Duration::from_millis(100));
    }

    fn on_exit(&mut self, glow: Option<&eframe::glow::Context>) {
        self.app.on_exit(glow);
        let closed = self.root.join(format!("closed-{}.json", self.run));
        direct(&closed);
        std::fs::write(
            closed,
            serde_json::to_vec(&json!({
                "normal_app_exit": true,
                "run": self.run,
                "frames": self.frames,
                "final_state": self.record(),
            }))
            .unwrap(),
        )
        .unwrap();
    }
}

#[test]
#[ignore = "Explicit Cua-owned native creation-screen fixture; owned profile, loopback server, no devices"]
fn owned_native_creation_screen() {
    let executable = std::env::current_exe().unwrap();
    assert_eq!(
        executable.file_name().and_then(|v| v.to_str()),
        Some(EXECUTABLE),
        "stage the fixture executable under this exact name"
    );
    let root = executable.parent().unwrap().to_path_buf();
    direct(&root);
    let manifest_path = root.join("manifest.json");
    direct(&manifest_path);
    let manifest: Manifest = serde_json::from_slice(&std::fs::read(manifest_path).unwrap())
        .expect("manifest.json: x, y, width, height, nonce");
    validate(&manifest).unwrap();
    let started = root.join(format!("started-{}.json", manifest.run));
    direct(&started);
    assert!(!started.exists(), "one start per run number");
    let profile = root.join("profile");
    direct(&profile);
    if manifest.run == 1 {
        assert!(!profile.exists(), "run 1 needs a fresh profile");
    } else {
        assert!(profile.is_dir(), "later runs reuse the profile of run 1");
    }
    // Server data and the profile stay in the run folder, never in a personal profile.
    let data = root.join("server-data");
    direct(&data);
    std::env::set_var("RELAY_SERVER_DATA_DIR", &data);
    direct(&crate::local_host::data_directory().unwrap());

    crate::profile_paths::configure(&[
        "relay-qa".into(),
        "--profile-dir".into(),
        profile.to_str().unwrap().into(),
    ])
    .unwrap();
    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Glow,
        viewport: egui::ViewportBuilder::default()
            .with_title("Relay - owned creation QA")
            .with_decorations(false)
            .with_active(false)
            .with_position([manifest.x, manifest.y])
            .with_inner_size([manifest.width, manifest.height])
            .with_min_inner_size([600.0, 480.0]),
        event_loop_builder: Some(Box::new(|b| {
            b.with_any_thread(true);
        })),
        ..Default::default()
    };
    eframe::run_native(
        "Relay owned creation QA",
        options,
        Box::new(move |cc| {
            let mut app = RelayApp::new(cc, crate::invitation_launch::Launch::default());
            app.account_start.bypass = true;
            app.account_start.home = true;
            app.central_account.provider = None;
            app.central_account.error = None;
            app.account_webview = None;
            app.central_account.account = Some(crate::central_account::Account {
                issuer: "https://owned-account.invalid/realms/qa".into(),
                subject: "owned-creator".into(),
                handle: "owned-creator".into(),
                expires_at: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs()
                    + manifest.seconds
                    + 60,
            });
            app.prefs.profile_name = "QA".into();
            app.prefs.close_to_tray = false;
            app.prefs.remember_session = false;
            app.prefs.notifications = false;
            app.prefs.audio.sounds_enabled = false;
            app.audio_applied.sounds_enabled = false;
            if let Some(media) = &mut app.media {
                media.configure(app.audio_applied.clone());
            }
            app.sync_call_alert_preferences();
            app.network
                .send(Command::RememberSession { enabled: false })
                .unwrap();
            std::fs::write(
                &started,
                serde_json::to_vec(&json!({
                    "pid": std::process::id(),
                    "nonce": manifest.nonce,
                    "run": manifest.run,
                    "window": [manifest.width, manifest.height],
                    "synthetic_only": true,
                    "server_binary_staged": crate::local_host::server_binary().is_some(),
                    "server_data_in_run_folder": true,
                    "notifications_disabled": true,
                    "sounds_disabled": true,
                    "remember_session_disabled": true,
                    "watchdog_secs": manifest.seconds,
                    "automatic_clicks": false,
                    "stops_if_account_free_route_selected": true,
                }))
                .unwrap(),
            )
            .unwrap();
            Box::new(OwnedApp {
                app,
                root,
                run: manifest.run,
                seconds: manifest.seconds,
                frames: 0,
                began: Instant::now(),
                window: [0.0, 0.0],
                safety_stop: None,
            })
        }),
    )
    .unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> Manifest {
        Manifest {
            x: 2000.0,
            y: 100.0,
            width: 1000.0,
            height: 850.0,
            nonce: "0123456789abcdef0123456789abcdef".into(),
            run: 1,
            seconds: 300,
        }
    }

    #[test]
    fn ordinary_window_sizes_are_accepted_and_odd_ones_are_not() {
        assert!(validate(&manifest()).is_ok());
        for (w, h) in [(600.0, 480.0), (1600.0, 1200.0), (940.0, 650.0)] {
            let mut m = manifest();
            m.width = w;
            m.height = h;
            assert!(validate(&m).is_ok(), "{w}x{h}");
        }
        for (w, h) in [
            (599.0, 800.0),
            (800.0, 479.0),
            (1601.0, 800.0),
            (f32::NAN, 800.0),
        ] {
            let mut m = manifest();
            m.width = w;
            m.height = h;
            assert!(validate(&m).is_err(), "{w}x{h}");
        }
    }

    #[test]
    fn nonce_run_watchdog_and_position_are_bounded() {
        for run in [0, 4, u32::MAX] {
            let mut m = manifest();
            m.run = run;
            assert!(validate(&m).is_err(), "{run}");
        }
        for seconds in [0, 59, 901] {
            let mut m = manifest();
            m.seconds = seconds;
            assert!(validate(&m).is_err(), "{seconds}");
        }
        let mut m = manifest();
        m.nonce = "g".repeat(32);
        assert!(validate(&m).is_err());
        let mut m = manifest();
        m.x = f32::INFINITY;
        assert!(validate(&m).is_err());
    }

    #[test]
    fn the_manifest_rejects_unknown_fields_and_defaults_are_safe() {
        let ok = serde_json::json!({
            "x": 1.0, "y": 2.0, "width": 900.0, "height": 700.0,
            "nonce": "0123456789abcdef0123456789abcdef"
        });
        let m: Manifest = serde_json::from_value(ok.clone()).unwrap();
        assert_eq!((m.run, m.seconds), (1, DEFAULT_SECONDS));
        let mut extra = ok;
        extra["server"] = serde_json::json!("http://127.0.0.1:4580");
        assert!(serde_json::from_value::<Manifest>(extra).is_err());
    }

    #[test]
    fn published_text_is_short_and_ids_are_only_a_prefix() {
        assert_eq!(bounded("a\nb\u{0}c"), "abc");
        assert_eq!(bounded(&"x".repeat(1000)).len(), 160);
        assert_eq!(prefix("4be44aef-8c98-4db6-a9d5-81c2eb311330"), "4be44aef");
    }

    #[test]
    fn qa_paths_stay_under_the_known_parent() {
        direct(&PathBuf::from(ROOT).join("run").join("state.json"));
        let outside = std::panic::catch_unwind(|| direct(std::path::Path::new(r"C:\Windows\x")));
        assert!(outside.is_err());
        let sibling = std::panic::catch_unwind(|| {
            direct(std::path::Path::new(
                r"C:\Important\Projects\2026-10-03\can\work\friends-call-quality-20261021\native-creation-ui-20261023-other\x",
            ))
        });
        assert!(sibling.is_err());
    }
}
