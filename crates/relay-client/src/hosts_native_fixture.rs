//! Test-only native QA for the real Hosts page. Ignored by default.
//!
//! It runs the actual `RelayApp` through eframe against an owned loopback HTTP
//! service and an owned profile, signs in through the ordinary network worker
//! with a synthetic account, then opens Server settings > Hosts. The page, its
//! render, dispatch and receive paths are the production ones: nothing here
//! draws a substitute widget or presses a button. A separate controller
//! (Codex, through Cua) supplies the service and does all clicking.
//!
//! Published, bounded and redacted, in the run folder only: `started.json`,
//! `state.json` (phase, request, loading, failure and view data) and, after a
//! normal exit, `closed.json`. No token, cookie or password is ever written.
use super::*;
use serde::Deserialize;
use std::os::windows::fs::MetadataExt;
use winit::platform::windows::EventLoopBuilderExtWindows;

const ROOT: &str = r"C:\Important\Projects\2026-10-03\can\work\friends-call-quality-20261021\native-hosts-ui-20261023";
const EXECUTABLE: &str = "relay-hosts-qa.exe";
const DEFAULT_SECONDS: u64 = 600;

/// Every path written or read must be under the one known parent and free of
/// redirects (junctions, symlinks and other reparse points).
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
    /// Exact loopback service, for example `http://127.0.0.1:4580/`.
    url: String,
    x: f32,
    y: f32,
    /// 32 hexadecimal characters echoed in `started.json`.
    nonce: String,
    /// Synthetic account of the owned service.
    #[serde(default = "default_username")]
    username: String,
    #[serde(default = "default_password")]
    password: String,
    /// Watchdog: the window closes by itself after this many seconds.
    #[serde(default = "default_seconds")]
    seconds: u64,
}

fn default_username() -> String {
    "owned-admin".into()
}
fn default_password() -> String {
    "owned-hosts-password".into()
}
fn default_seconds() -> u64 {
    DEFAULT_SECONDS
}

/// Checks that need no window, kept apart so they can be unit tested.
fn validate(manifest: &Manifest) -> Result<std::net::SocketAddr, String> {
    let url = url::Url::parse(&manifest.url).map_err(|_| "url is not a URL")?;
    if url.scheme() != "http"
        || url.host_str() != Some("127.0.0.1")
        || url.path() != "/"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("url must be exactly http://127.0.0.1:<port>/".into());
    }
    let port = url.port().ok_or("url needs an explicit port")?;
    if !manifest.x.is_finite() || !manifest.y.is_finite() {
        return Err("window position must be finite".into());
    }
    if manifest.nonce.len() != 32 || !manifest.nonce.bytes().all(|v| v.is_ascii_hexdigit()) {
        return Err("nonce must be 32 hexadecimal characters".into());
    }
    if !(60..=900).contains(&manifest.seconds) {
        return Err("seconds must be between 60 and 900".into());
    }
    if manifest.username.is_empty()
        || manifest.username.len() > 64
        || manifest.password.is_empty()
        || manifest.password.len() > 128
    {
        return Err("account fields must be non-empty and short".into());
    }
    Ok(std::net::SocketAddr::from(([127, 0, 0, 1], port)))
}

/// The address as the ordinary sign-in worker stores it (no trailing slash).
/// The manifest still has to be the exact loopback form checked by `validate`;
/// only the comparison with `app.login.url` uses this canonical spelling, and it
/// comes from the same community normalizer the worker uses.
fn canonical_url(manifest: &Manifest) -> Result<String, String> {
    validate(manifest)?;
    crate::community::normalize_url(&manifest.url)
}

/// Free text from the service is shown only in a short, printable form.
fn bounded(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).take(160).collect()
}

fn auth(url: String, manifest: &Manifest) -> Command {
    Command::Auth {
        authority: None,
        url,
        expected_server_id: None,
        register: false,
        request_id: 9001,
        request: AuthRequest {
            username: manifest.username.clone(),
            password: manifest.password.clone(),
            invite: String::new(),
            second_factor: None,
            initial_protection: None,
        },
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    SigningIn,
    WaitingForHostingCapability,
    OnHostsPage,
}

impl Phase {
    fn name(self) -> &'static str {
        match self {
            Phase::SigningIn => "signing_in",
            Phase::WaitingForHostingCapability => "waiting_for_hosting_capability",
            Phase::OnHostsPage => "on_hosts_page",
        }
    }
}

struct OwnedApp {
    app: RelayApp,
    root: PathBuf,
    url: String,
    phase: Phase,
    frames: u64,
    seconds: u64,
    began: Instant,
}

impl OwnedApp {
    fn record(&self) -> Value {
        let hosting = &self.app.admin_ui.hosting;
        let view = hosting.view.as_ref().map(|(_, view)| view);
        let failure = hosting.failure.as_ref().map(|failure| {
            json!({
                "kind": if failure.loading { "refresh_or_load" } else { "change" },
                "message": bounded(&failure.message),
            })
        });
        json!({
            "phase": self.phase.name(),
            "frame": self.frames,
            "elapsed_secs": self.began.elapsed().as_secs(),
            "signed_in": self.app.session.is_some() && self.app.login.url == self.url,
            "bootstrap_received": self.app.bootstrap.is_some(),
            "hosting_capability": self.app.bootstrap.as_ref()
                .is_some_and(|b| b.capabilities.iter().any(|c| c == "hosting-v1")),
            "settings_open": self.app.settings_open,
            "hosts_page_selected": self.app.admin_ui.hosts_selected(),
            "hosting": {
                "requests_dispatched": hosting.dispatched,
                "request_pending": self.app.hosting_pending(),
                "view_present": view.is_some(),
                "view_revision": view.map(|v| v.revision),
                "view_candidates": view.map(|v| v.candidates.len()),
                "view_can_manage": view.map(|v| v.can_manage),
                "stale": is_stale(hosting.failure.as_ref()),
                "loaded_secs_ago": hosting.loaded.map(|at| at.elapsed().as_secs()),
                "failure": failure,
            },
            "notice": self.app.notice.as_ref().map(|(text, _)| bounded(text)),
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
        self.app.update(ctx, frame);
        self.frames += 1;
        if self.phase == Phase::SigningIn
            && self.app.session.is_some()
            && self.app.login.url == self.url
            && self.app.bootstrap.is_some()
        {
            self.phase = Phase::WaitingForHostingCapability;
        }
        if self.phase == Phase::WaitingForHostingCapability
            && self
                .app
                .bootstrap
                .as_ref()
                .is_some_and(|b| b.capabilities.iter().any(|c| c == "hosting-v1"))
        {
            // Open the page once, the way a member would: Settings > Server >
            // Hosts. After that the controller is in charge of the window.
            self.app.settings_open = true;
            self.app.settings_tab = SettingsTab::Server;
            self.app.admin_ui.select_section("hosts");
            self.phase = Phase::OnHostsPage;
        }
        self.publish();
        if self.root.join("finish").exists()
            || self.began.elapsed() > Duration::from_secs(self.seconds)
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        ctx.request_repaint_after(Duration::from_millis(100));
    }

    fn on_exit(&mut self, glow: Option<&eframe::glow::Context>) {
        self.app.on_exit(glow);
        let closed = self.root.join("closed.json");
        direct(&closed);
        std::fs::write(
            closed,
            serde_json::to_vec(&json!({
                "normal_app_exit": true,
                "frames": self.frames,
                "final_state": self.record(),
            }))
            .unwrap(),
        )
        .unwrap();
    }
}

#[test]
#[ignore = "Explicit Cua-owned native Hosts page fixture; synthetic account on an owned loopback service"]
fn owned_native_hosts_page() {
    let executable = std::env::current_exe().unwrap();
    assert_eq!(
        executable.file_name().and_then(|v| v.to_str()),
        Some(EXECUTABLE),
        "stage the fixture executable under this exact name"
    );
    let root = executable.parent().unwrap().to_path_buf();
    direct(&root);
    assert!(
        !root.join("started.json").exists() && !root.join("profile").exists(),
        "one run per folder"
    );
    let manifest_path = root.join("manifest.json");
    direct(&manifest_path);
    let manifest: Manifest = serde_json::from_slice(&std::fs::read(manifest_path).unwrap())
        .expect("manifest.json: url, x, y, nonce");
    let address = validate(&manifest).unwrap();
    let canonical = canonical_url(&manifest).unwrap();
    // The owned service must already be listening on exactly this address.
    std::net::TcpStream::connect_timeout(&address, Duration::from_secs(2))
        .expect("nothing is listening on the manifest address");

    crate::profile_paths::configure(&[
        "relay-qa".into(),
        "--profile-dir".into(),
        root.join("profile").to_str().unwrap().into(),
    ])
    .unwrap();
    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Glow,
        viewport: egui::ViewportBuilder::default()
            .with_title("Relay - owned Hosts QA")
            .with_decorations(false)
            .with_active(false)
            .with_position([manifest.x, manifest.y])
            .with_inner_size([1000.0, 850.0])
            .with_min_inner_size([940.0, 650.0]),
        event_loop_builder: Some(Box::new(|b| {
            b.with_any_thread(true);
        })),
        ..Default::default()
    };
    eframe::run_native(
        "Relay owned Hosts QA",
        options,
        Box::new(move |cc| {
            let mut app = RelayApp::new(cc, crate::invitation_launch::Launch::default());
            app.account_start.bypass = true;
            app.account_start.home = false;
            app.central_account.provider = None;
            app.central_account.error = None;
            app.account_webview = None;
            app.central_account.account = Some(crate::central_account::Account {
                issuer: "https://owned-account.invalid/realms/qa".into(),
                subject: manifest.username.clone(),
                handle: manifest.username.clone(),
                expires_at: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs()
                    + manifest.seconds
                    + 60,
            });
            // Close really closes; nothing is remembered; no sound, no toast.
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
            app.network
                .send(auth(canonical.clone(), &manifest))
                .unwrap();
            std::fs::write(
                root.join("started.json"),
                serde_json::to_vec(&json!({
                    "pid": std::process::id(),
                    "nonce": manifest.nonce,
                    "url": manifest.url,
                    "canonical_url": canonical,
                    "synthetic_only": true,
                    "notifications_disabled": true,
                    "sounds_disabled": true,
                    "remember_session_disabled": true,
                    "watchdog_secs": manifest.seconds,
                    "automatic_clicks": false,
                }))
                .unwrap(),
            )
            .unwrap();
            Box::new(OwnedApp {
                app,
                root,
                url: canonical,
                phase: Phase::SigningIn,
                frames: 0,
                seconds: manifest.seconds,
                began: Instant::now(),
            })
        }),
    )
    .unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(url: &str) -> Manifest {
        Manifest {
            url: url.into(),
            x: 2000.0,
            y: 100.0,
            nonce: "0123456789abcdef0123456789abcdef".into(),
            username: default_username(),
            password: default_password(),
            seconds: 300,
        }
    }

    #[test]
    fn only_an_exact_loopback_address_with_a_port_is_accepted() {
        let ok = validate(&manifest("http://127.0.0.1:4580/")).unwrap();
        assert_eq!(ok.to_string(), "127.0.0.1:4580");
        for bad in [
            "https://127.0.0.1:4580/",
            "http://localhost:4580/",
            "http://127.0.0.2:4580/",
            "http://127.0.0.1/",
            "http://127.0.0.1:4580/source",
            "http://user@127.0.0.1:4580/",
            "http://127.0.0.1:4580/?x=1",
            "http://127.0.0.1:4580/#f",
            "http://example.com:4580/",
            "not a url",
        ] {
            assert!(validate(&manifest(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_stored_address_is_the_one_the_sign_in_worker_uses() {
        let m = manifest("http://127.0.0.1:4580/");
        let canonical = canonical_url(&m).unwrap();
        // The defect: comparing the manifest spelling with the worker's spelling
        // never matched, so the fixture stayed in "signing_in" for ever.
        assert_ne!(m.url, canonical);
        assert_eq!(canonical, "http://127.0.0.1:4580");
        assert!(!canonical.ends_with('/'));
        // It is exactly what the shared community normalizer produces, and
        // normalizing again changes nothing.
        assert_eq!(canonical, crate::community::normalize_url(&m.url).unwrap());
        assert_eq!(
            crate::community::normalize_url(&canonical).unwrap(),
            canonical
        );
        // The exact-loopback validation still applies before normalizing.
        for bad in [
            "http://localhost:4580/",
            "https://127.0.0.1:4580/",
            "http://127.0.0.1:4580/x",
        ] {
            assert!(canonical_url(&manifest(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn nonce_position_watchdog_and_account_are_bounded() {
        let url = "http://127.0.0.1:4580/";
        let mut m = manifest(url);
        m.nonce = "short".into();
        assert!(validate(&m).is_err());
        let mut m = manifest(url);
        m.nonce = "g".repeat(32);
        assert!(validate(&m).is_err());
        for seconds in [0, 59, 901, u64::MAX] {
            let mut m = manifest(url);
            m.seconds = seconds;
            assert!(validate(&m).is_err(), "{seconds}");
        }
        let mut m = manifest(url);
        m.x = f32::NAN;
        assert!(validate(&m).is_err());
        let mut m = manifest(url);
        m.password = String::new();
        assert!(validate(&m).is_err());
    }

    #[test]
    fn published_text_is_short_and_printable() {
        assert_eq!(bounded("a\nb\u{0}c"), "abc");
        assert_eq!(bounded(&"x".repeat(1000)).len(), 160);
    }

    #[test]
    fn the_manifest_rejects_unknown_fields() {
        let ok = serde_json::json!({
            "url": "http://127.0.0.1:4580/", "x": 1.0, "y": 2.0,
            "nonce": "0123456789abcdef0123456789abcdef"
        });
        assert!(serde_json::from_value::<Manifest>(ok.clone()).is_ok());
        let mut extra = ok;
        extra["token"] = serde_json::json!("secret");
        assert!(serde_json::from_value::<Manifest>(extra).is_err());
    }

    #[test]
    fn qa_paths_stay_under_the_known_parent() {
        direct(&PathBuf::from(ROOT).join("run").join("state.json"));
        let outside = std::panic::catch_unwind(|| direct(std::path::Path::new(r"C:\Windows\x")));
        assert!(outside.is_err());
        let sibling = std::panic::catch_unwind(|| {
            direct(std::path::Path::new(
                r"C:\Important\Projects\2026-10-03\can\work\friends-call-quality-20261021\native-hosts-ui-20261023-other\x",
            ))
        });
        assert!(sibling.is_err());
    }
}
