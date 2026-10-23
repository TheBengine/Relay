//! Opt-in native background-alert integration fixture. Synthetic loopback
//! service only; no personal profile, crypto acceptance, microphone or camera.
use super::*;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use serde::Deserialize;
use std::os::windows::fs::MetadataExt;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use winit::platform::windows::EventLoopBuilderExtWindows;

const ROOT: &str = r"C:\Important\Projects\2026-10-03\can\work\friends-call-quality-20261021\native-message-alert-20261023";
const CALL_ROOT: &str = r"C:\Important\Projects\2026-10-03\can\work\friends-call-quality-20261021\native-call-alert-20261023";

fn direct(path: &std::path::Path) {
    assert!(path.starts_with(ROOT) || path.starts_with(CALL_ROOT));
    for ancestor in path.ancestors() {
        if let Ok(metadata) = std::fs::symlink_metadata(ancestor) {
            assert_eq!(
                metadata.file_attributes() & 0x400,
                0,
                "redirected fixture path"
            );
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    server: String,
    username: String,
    password: String,
    x: f32,
    y: f32,
    nonce: String,
    allow_shell_popup: bool,
}

struct OwnedApp {
    app: RelayApp,
    root: PathBuf,
    frames: Arc<AtomicUsize>,
    authenticated: Arc<AtomicBool>,
    closing: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
    monitor: Option<std::thread::JoinHandle<()>>,
}
impl eframe::App for OwnedApp {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        self.frames.fetch_add(1, Ordering::Relaxed);
        if self.closing.load(Ordering::Acquire) {
            // The fixture owns shutdown. This is not a user input, preference
            // change, hidden-app repaint timer or production timeout feature.
            self.app.prefs.close_to_tray = false;
        }
        self.app.update(ctx, frame);
        self.authenticated
            .store(self.app.session.is_some(), Ordering::Release);
    }
    fn on_exit(&mut self, glow: Option<&eframe::glow::Context>) {
        self.app.on_exit(glow);
        self.finished.store(true, Ordering::Release);
        if let Some(monitor) = self.monitor.take() {
            monitor.join().unwrap();
        }
        let path = self.root.join("closed.json");
        direct(&path);
        std::fs::write(
            path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "normal_app_exit": true,
                "shell_api_accepts": self.app.notifications.owned_qa_shell_accepts(),
                "frames": self.frames.load(Ordering::Relaxed),
                "authenticated": self.authenticated.load(Ordering::Acquire),
                "media_joined": self.app.voice_channel.is_some(),
                "active_rings": self.app.call_alerts.active_rings(),
                "call_notice_authorities": self.app.call_alerts.owned_qa_call_notice_authorities(),
            }))
            .unwrap(),
        )
        .unwrap();
    }
}

#[test]
#[ignore = "explicit Cua-owned hidden-app/Shell fixture with synthetic loopback accounts; no media"]
fn owned_native_hidden_message_notification() {
    run_owned_hidden_notification(ROOT);
}

#[test]
#[ignore = "explicit Cua-owned hidden call/Shell fixture; synthetic loopback accounts, physical sound disabled"]
fn owned_native_hidden_call_notification() {
    run_owned_hidden_notification(CALL_ROOT);
}

fn run_owned_hidden_notification(fixture_root: &str) {
    let root = PathBuf::from(fixture_root);
    let manifest_path = root.join("manifest.json");
    direct(&manifest_path);
    assert!(
        !root.join("started.json").exists(),
        "never replay an owned native run"
    );
    let manifest: Manifest =
        serde_json::from_slice(&std::fs::read(manifest_path).unwrap()).unwrap();
    assert!(manifest.allow_shell_popup);
    assert!(manifest.nonce.len() == 32 && manifest.nonce.bytes().all(|b| b.is_ascii_hexdigit()));
    assert!(manifest.x.is_finite() && manifest.y.is_finite());
    let server = url::Url::parse(&manifest.server).unwrap();
    assert_eq!(server.scheme(), "http");
    assert_eq!(server.host_str(), Some("127.0.0.1"));
    assert!(server.port().is_some());
    assert_eq!(server.path(), "/people");
    assert!(server.username().is_empty() && server.password().is_none());
    assert!(server.query().is_none() && server.fragment().is_none());
    let profile = root.join("profile");
    direct(&profile);
    assert!(!profile.exists(), "fresh owned profile only");
    crate::profile_paths::configure(&[
        "relay-qa".into(),
        "--profile-dir".into(),
        profile.to_str().unwrap().into(),
    ])
    .unwrap();
    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Glow,
        viewport: egui::ViewportBuilder::default()
            .with_title("Relay - owned background-alert QA")
            .with_decorations(false)
            .with_active(false)
            .with_position([manifest.x, manifest.y])
            .with_inner_size([1000.0, 950.0])
            .with_min_inner_size([940.0, 650.0]),
        event_loop_builder: Some(Box::new(|builder| {
            builder.with_any_thread(true);
        })),
        ..Default::default()
    };
    eframe::run_native(
        "Relay owned background-message QA",
        options,
        Box::new(move |cc| {
            let RawWindowHandle::Win32(handle) = cc.window_handle().unwrap().as_raw() else {
                panic!("owned Windows window")
            };
            let hwnd = handle.hwnd.get();
            let mut app = RelayApp::new(cc, crate::invitation_launch::Launch::default());
            app.account_start.bypass = true; // Only this synthetic local transport fixture.
            app.central_account.provider = None;
            app.central_account.error = None;
            app.account_webview = None;
            app.prefs.close_to_tray = true;
            app.prefs.notifications = true;
            app.prefs.notification_previews = false;
            app.prefs.audio.sounds_enabled = false;
            app.audio_applied.sounds_enabled = false;
            if let Some(media) = &mut app.media {
                media.configure(app.audio_applied.clone());
            }
            app.sync_call_alert_preferences();
            app.notifications.enable_owned_qa_shell().unwrap();
            app.login.url = manifest.server;
            app.login.username = manifest.username;
            app.login.password = manifest.password;
            app.authenticate();
            let frames = Arc::new(AtomicUsize::new(0));
            let authenticated = Arc::new(AtomicBool::new(false));
            let closing = Arc::new(AtomicBool::new(false));
            let finished = Arc::new(AtomicBool::new(false));
            let monitor_frames = frames.clone();
            let monitor_auth = authenticated.clone();
            let monitor_closing = closing.clone();
            let monitor_finished = finished.clone();
            let notifications = app.notifications.clone();
            let call_alerts = app.call_alerts.clone();
            let monitor_root = root.clone();
            let ctx = cc.egui_ctx.clone();
            std::fs::write(
                root.join("started.json"),
                serde_json::to_vec_pretty(&serde_json::json!({
                    "pid": std::process::id(), "window_id": hwnd, "nonce": manifest.nonce,
                    "synthetic_loopback_only": true, "audio_disabled": true,
                }))
                .unwrap(),
            )
            .unwrap();
            let monitor = std::thread::spawn(move || {
                use windows::Win32::{
                    Foundation::{HWND, LPARAM, WPARAM},
                    UI::WindowsAndMessaging::{IsWindowVisible, PostMessageW, WM_CLOSE, WM_PAINT},
                };
                let start = Instant::now();
                let mut previous = serde_json::Value::Null;
                while !monitor_finished.load(Ordering::Acquire) {
                    let state = serde_json::json!({
                        "elapsed_ms": start.elapsed().as_millis(),
                        "frames": monitor_frames.load(Ordering::Relaxed),
                        "authenticated": monitor_auth.load(Ordering::Acquire),
                        "visible": unsafe { IsWindowVisible(HWND(hwnd as *mut _)).as_bool() },
                        "shell_api_accepts": notifications.owned_qa_shell_accepts(),
                        "active_rings": call_alerts.active_rings(),
                        "call_notice_authorities": call_alerts.owned_qa_call_notice_authorities(),
                    });
                    // A small metadata receipt. No message content, credentials,
                    // account IDs, private keys or UI screenshots are exported.
                    if state != previous {
                        std::fs::write(
                            monitor_root.join("state.pending"),
                            serde_json::to_vec_pretty(&state).unwrap(),
                        )
                        .unwrap();
                        std::fs::rename(
                            monitor_root.join("state.pending"),
                            monitor_root.join("state.json"),
                        )
                        .unwrap();
                        previous = state;
                    }
                    if monitor_root.join("finish").exists()
                        || start.elapsed() > Duration::from_secs(240)
                    {
                        monitor_closing.store(true, Ordering::Release);
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        // The owned test host wakes one final hidden frame solely
                        // for normal app shutdown; no external GUI input is used.
                        unsafe {
                            let _ =
                                PostMessageW(HWND(hwnd as *mut _), WM_CLOSE, WPARAM(0), LPARAM(0));
                            let _ =
                                PostMessageW(HWND(hwnd as *mut _), WM_PAINT, WPARAM(0), LPARAM(0));
                        }
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(200));
                }
            });
            Box::new(OwnedApp {
                app,
                root,
                frames,
                authenticated,
                closing,
                finished,
                monitor: Some(monitor),
            })
        }),
    )
    .unwrap();
}
