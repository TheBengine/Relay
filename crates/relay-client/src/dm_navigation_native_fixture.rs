//! Cua-owned routing acceptance with a synthetic verified-account UI state.
//! No OIDC/password flow, real account, personal profile or media is exercised.
//! The real RelayApp update/rail dispatch talks only to this process's loopback
//! service, whose delayed 503 exposes pending/error routing after a human click.
use super::*;
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    io::{Read, Write},
    net::TcpListener,
    os::windows::fs::MetadataExt,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    thread::{self, JoinHandle},
};
use winit::platform::windows::EventLoopBuilderExtWindows;

const ROOT: &str = r"C:\Important\Projects\2026-10-03\can\work\relay-dm-navigation-20261026\native-ui";
const EXECUTABLE: &str = "relay-dm-qa.exe";

fn direct(path: &std::path::Path) {
    assert!(path.starts_with(ROOT), "outside the owned routing fixture");
    for ancestor in path.ancestors() {
        if let Ok(metadata) = std::fs::symlink_metadata(ancestor) {
            assert_eq!(metadata.file_attributes() & 0x400, 0, "redirected fixture path");
        }
    }
}

fn write_record(root: &std::path::Path, name: &str, value: &Value) {
    let path = root.join(name);
    direct(&path);
    std::fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    mode: String,
    x: f32,
    y: f32,
    nonce: String,
    seconds: u64,
}

struct Backend {
    origin: String,
    stop: Arc<AtomicBool>,
    requests: Arc<AtomicUsize>,
    health_requests: Arc<AtomicUsize>,
    worker: Option<JoinHandle<()>>,
}

impl Backend {
    fn start() -> Self {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(AtomicUsize::new(0));
        let health_requests = Arc::new(AtomicUsize::new(0));
        let stopped = stop.clone();
        let count = requests.clone();
        let health_count = health_requests.clone();
        let worker = thread::spawn(move || {
            while !stopped.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut socket, peer)) => {
                        assert!(peer.ip().is_loopback());
                        socket.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
                        socket.set_write_timeout(Some(Duration::from_secs(1))).unwrap();
                        let mut bytes = Vec::new();
                        let mut block = [0u8; 1024];
                        while bytes.len() < 8192 && !bytes.windows(4).any(|part| part == b"\r\n\r\n") {
                            match socket.read(&mut block) {
                                Ok(0) | Err(_) => break,
                                Ok(size) => bytes.extend_from_slice(&block[..size]),
                            }
                        }
                        count.fetch_add(1, Ordering::Relaxed);
                        let line = bytes.split(|byte| *byte == b'\n').next().unwrap_or_default();
                        if line.windows(b"/api/health".len()).any(|part| part == b"/api/health") {
                            health_count.fetch_add(1, Ordering::Relaxed);
                            let until = Instant::now() + Duration::from_secs(8);
                            while Instant::now() < until && !stopped.load(Ordering::Acquire) {
                                thread::sleep(Duration::from_millis(10));
                            }
                        }
                        let body = br#"{"error":"Owned Friends routing service is temporarily unavailable"}"#;
                        let reply = format!("HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                        let _ = socket.write_all(reply.as_bytes());
                        let _ = socket.write_all(body);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });
        Self { origin, stop, requests, health_requests, worker: Some(worker) }
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
    }
}

struct RoutingApp {
    app: RelayApp,
    root: PathBuf,
    mode: String,
    began: Instant,
    seconds: u64,
    requests: Arc<AtomicUsize>,
    health_requests: Arc<AtomicUsize>,
    previous: Value,
}

impl RoutingApp {
    fn snapshot(&self) -> Value {
        json!({
            "mode": self.mode,
            "elapsed_secs": self.began.elapsed().as_secs(),
            "signed_in": self.app.central_account.account.is_some(),
            "session_present": self.app.session.is_some(),
            "bootstrap_present": self.app.bootstrap.is_some(),
            "is_people": self.app.in_people(),
            "account_home": self.app.account_start.home,
            "selected_nav": self.app.selected_nav,
            "friends_page": self.app.home_page == Some(SettingsTab::Friends),
            "signed_in_shell": self.app.signed_in_shell(),
            "friends_shell_selected": self.app.friends_shell_selected(),
            "friends_shell_pending": self.app.friends_shell_pending(),
            "friends_requested": self.app.account_start.friends_requested,
            "pending_attempt": self.app.account_start.pending.is_some(),
            "friends_error_present": self.app.account_start.friends_error.is_some(),
            "error_present": self.app.error.is_some(),
            "current_direct": matches!(self.app.current.as_ref(), Some(Target::Direct(_))),
            "http_requests": self.requests.load(Ordering::Relaxed),
            "health_requests": self.health_requests.load(Ordering::Relaxed),
            "routing_only_synthetic_identity": true
        })
    }
}

impl eframe::App for RoutingApp {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        self.app.update(ctx, frame);
        let state = self.snapshot();
        if state != self.previous {
            write_record(&self.root, "state.json", &state);
            let path = self.root.join("trace.jsonl");
            direct(&path);
            if std::fs::metadata(&path).map_or(0, |info| info.len()) < 2 * 1024 * 1024 {
                let mut output = std::fs::OpenOptions::new().create(true).append(true).open(path).unwrap();
                writeln!(output, "{state}").unwrap();
            }
            self.previous = state;
        }
        direct(&self.root.join("finish"));
        if self.root.join("finish").exists() || self.began.elapsed() > Duration::from_secs(self.seconds) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        ctx.request_repaint_after(Duration::from_millis(50));
    }

    fn on_exit(&mut self, glow: Option<&eframe::glow::Context>) {
        let final_state = self.snapshot();
        self.app.on_exit(glow);
        write_record(&self.root, "closed.json", &json!({
            "normal_app_exit": true, "routing_only": true, "final_state": final_state
        }));
    }
}

#[test]
#[ignore = "Cua-owned routing-only native UI with synthetic identity and delayed loopback 503"]
fn owned_native_dm_navigation() {
    let executable = std::env::current_exe().unwrap();
    assert_eq!(executable.file_name().and_then(|name| name.to_str()), Some(EXECUTABLE));
    let root = executable.parent().unwrap().to_path_buf();
    direct(&root);
    direct(&executable);
    assert_eq!(root.parent(), Some(std::path::Path::new(ROOT)));
    let name = root.file_name().unwrap().to_str().unwrap();
    assert!(!name.is_empty() && name.len() <= 64
        && name.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'));
    for name in ["started.json", "state.json", "trace.jsonl", "closed.json", "profile"] {
        direct(&root.join(name));
        assert!(!root.join(name).exists(), "one run per folder");
    }
    let manifest_path = root.join("manifest.json");
    direct(&manifest_path);
    assert!(std::fs::metadata(&manifest_path).unwrap().len() <= 4096);
    let manifest: Manifest = serde_json::from_slice(&std::fs::read(manifest_path).unwrap()).unwrap();
    assert!(matches!(manifest.mode.as_str(), "startup" | "community"));
    assert!(manifest.x.is_finite() && manifest.y.is_finite());
    assert!((60..=900).contains(&manifest.seconds));
    assert!(manifest.nonce.len() == 32 && manifest.nonce.bytes().all(|byte| byte.is_ascii_hexdigit()));
    crate::profile_paths::configure(&[
        "relay-dm-qa".into(), "--profile-dir".into(), root.join("profile").to_str().unwrap().into()
    ]).unwrap();
    let backend = Backend::start();
    let issuer = format!("{}/realms/relay-accounts", backend.origin);
    let origin = backend.origin.clone();
    let requests = backend.requests.clone();
    let health_requests = backend.health_requests.clone();
    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Glow,
        viewport: egui::ViewportBuilder::default()
            .with_title("Relay - owned DM routing QA")
            .with_active(false)
            .with_position([manifest.x, manifest.y])
            .with_inner_size([1000.0, 850.0])
            .with_min_inner_size([940.0, 650.0]),
        event_loop_builder: Some(Box::new(|builder| {
            builder.with_any_thread(true);
        })),
        ..Default::default()
    };
    eframe::run_native("Relay owned DM routing QA", options, Box::new(move |cc| {
        let mut app = RelayApp::new(cc, crate::invitation_launch::Launch::default());
        app.central_account = crate::central_account::Controller::owned(
            crate::central_account::Provider::owned_loopback_fixture(&issuer));
        app.central_account.account = Some(crate::central_account::Account {
            issuer: issuer.clone(), subject: "owned-routing-user".into(), handle: "RoutingAlex".into(),
            expires_at: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap()
                .as_secs().saturating_add(3600),
        });
        app.prefs.last_relay_identity = Some((issuer.clone(), "owned-routing-user".into()));
        app.prefs.stay_signed_in = false;
        app.prefs.remember_session = false;
        app.prefs.notifications = false;
        app.prefs.automatic_update_checks = false;
        app.prefs.audio.sounds_enabled = false;
        app.audio_applied.sounds_enabled = false;
        if let Some(media) = &mut app.media {
            media.configure(app.audio_applied.clone());
        }
        app.sync_call_alert_preferences();
        app.prefs.close_to_tray = false;
        app.account_webview = None;
        app.account_start.home = false;
        app.account_start.bypass = false;
        // Startup exercises normal automatic entry; community mode waits for
        // the controller's actual Direct messages rail click.
        app.account_start.entered = manifest.mode == "community";
        app.account_start.auto = None;
        if manifest.mode == "community" {
            let user = User { id: "owned-routing-member".into(), username: "RoutingAlex".into(), ..Default::default() };
            app.session = Some(Session { user: user.clone(), token: "synthetic-routing-no-server-credential".into() });
            app.bootstrap = Some(Arc::new(Bootstrap {
                user, server_id: "18cb3a02-0e22-471f-b2e0-78bab6ed901b".into(),
                server_name: "Owned routing community".into(), protocol_version: relay_protocol::API_PROTOCOL_VERSION,
                ..Default::default()
            }));
            app.login.url = format!("{origin}/community");
            app.connection_status = ConnectionStatus::Connected;
            app.selected_nav = true;
            app.home_page = None;
        }
        write_record(&root, "started.json", &json!({
            "pid": std::process::id(), "nonce": manifest.nonce, "mode": manifest.mode,
            "owned_origin": origin, "seconds": manifest.seconds,
            "routing_only": true, "synthetic_identity": true, "oidc_or_password_test": false,
            "automatic_clicks": false, "profile": "adjacent fresh profile", "media_started": false
        }));
        Box::new(RoutingApp {
            app, root, mode: manifest.mode, began: Instant::now(), seconds: manifest.seconds,
            requests, health_requests, previous: Value::Null,
        })
    })).unwrap();
    drop(backend);
}
