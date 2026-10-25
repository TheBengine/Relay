//! Synthetic transport UI proof only. Voice admission always fails before media.
use super::*;
use serde::Deserialize;
use std::os::windows::fs::MetadataExt;
use winit::platform::windows::EventLoopBuilderExtWindows;

const ROOT: &str = r"C:\Important\Projects\2026-10-03\can\work\friends-call-quality-20261021\native-people-answer-20261023";

fn direct(path: &std::path::Path) {
    assert!(path.starts_with(ROOT));
    for ancestor in path.ancestors() {
        if let Ok(metadata) = std::fs::symlink_metadata(ancestor) {
            assert_eq!(metadata.file_attributes() & 0x400, 0);
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    source: String,
    people: String,
    x: f32,
    y: f32,
    nonce: String,
}

struct OwnedApp {
    app: RelayApp,
    root: PathBuf,
    source: String,
    people: String,
    incoming: Option<Incoming>,
    phase: u8,
    began: Instant,
}

fn auth(url: String, request_id: u64) -> Command {
    Command::Auth {
        authority: None,
        url,
        expected_server_id: None,
        register: false,
        request_id,
        request: AuthRequest {
            username: "owned-alice".into(),
            password: "owned-answer-password".into(),
            invite: String::new(),
            second_factor: None,
            initial_protection: None,
        },
    }
}

impl eframe::App for OwnedApp {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        self.app.update(ctx, frame);
        if self.phase == 0 && self.app.session.is_some() && self.app.login.url == self.people {
            let b = self.app.bootstrap.as_ref().unwrap();
            self.incoming = incoming(&self.people, b, b.social.server_time);
            assert!(self.incoming.is_some());
            self.app
                .network
                .send(auth(self.source.clone(), 9002))
                .unwrap();
            self.phase = 1;
        }
        if self.phase == 1
            && self.app.session.is_some()
            && self.app.login.url == self.source
            && self.app.communities.iter().any(|c| c.url == self.people)
        {
            // The synthetic account service is not OIDC. Deliver only its
            // already authenticated bootstrap's notice; the production clock,
            // generation, account switch and actual Answer button remain real.
            self.app.people_ui.incoming = self.incoming.take();
            self.phase = 2;
        }
        let record = serde_json::json!({"phase":self.phase,"source_selected":self.app.login.url == self.source,
            "people_selected":self.app.login.url == self.people,"answer_waiting":self.app.people_ui.answer.is_some(),
            "voice_joining":self.app.joining_voice.is_some(),"voice_active":self.app.voice_channel.is_some(),
            "private_reply_pending":self.app.private_calls.intent.is_some(),
            "notice_present":self.app.people_ui.incoming.is_some(),"media_connected":self.app.voice_connected()});
        let pending = self.root.join("state.pending");
        let final_path = self.root.join("state.json");
        direct(&pending);
        direct(&final_path);
        std::fs::write(&pending, serde_json::to_vec(&record).unwrap()).unwrap();
        std::fs::rename(&pending, &final_path).unwrap();
        if self.root.join("finish").exists() || self.began.elapsed() > Duration::from_secs(180) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        ctx.request_repaint_after(Duration::from_millis(100));
    }
    fn on_exit(&mut self, glow: Option<&eframe::glow::Context>) {
        self.app.on_exit(glow);
        std::fs::write(
            self.root.join("closed.json"),
            br#"{"normal_app_exit":true}"#,
        )
        .unwrap();
    }
}

#[test]
#[ignore = "Explicit Cua-owned synthetic Answer button fixture; no real accounts or media"]
fn owned_native_background_answer_button() {
    let executable = std::env::current_exe().unwrap();
    assert_eq!(
        executable.file_name().and_then(|v| v.to_str()),
        Some("relay-answer-qa.exe")
    );
    let root = executable.parent().unwrap().to_path_buf();
    direct(&root);
    assert!(!root.join("started.json").exists() && !root.join("profile").exists());
    let manifest: Manifest =
        serde_json::from_slice(&std::fs::read(root.join("manifest.json")).unwrap()).unwrap();
    for (value, path) in [(&manifest.source, "/source"), (&manifest.people, "/people")] {
        let url = url::Url::parse(value).unwrap();
        assert_eq!(url.scheme(), "http");
        assert_eq!(url.host_str(), Some("127.0.0.1"));
        assert_eq!(url.path(), path);
        assert!(url.port().is_some());
        assert!(
            url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none()
        );
    }
    assert!(manifest.x.is_finite() && manifest.y.is_finite());
    assert!(manifest.nonce.len() == 32 && manifest.nonce.bytes().all(|v| v.is_ascii_hexdigit()));
    crate::profile_paths::configure(&[
        "relay-qa".into(),
        "--profile-dir".into(),
        root.join("profile").to_str().unwrap().into(),
    ])
    .unwrap();
    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Glow,
        viewport: egui::ViewportBuilder::default()
            .with_title("Relay - owned Answer QA")
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
    eframe::run_native("Relay owned Answer QA",options,Box::new(move|cc|{
        let mut app=RelayApp::new(cc,crate::invitation_launch::Launch::default());
        app.account_start.bypass=true; app.account_start.home=false;
        app.central_account.provider=None; app.central_account.error=None; app.account_webview=None;
        app.central_account.account=Some(crate::central_account::Account {
            issuer:"https://owned-account.invalid/realms/qa".into(), subject:"owned-alice".into(),
            handle:"owned-alice".into(), expires_at:std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()+600,
        });
        app.prefs.close_to_tray=false; app.prefs.remember_session=false;
        app.prefs.notifications=false; app.prefs.audio.sounds_enabled=false;
        app.audio_applied.sounds_enabled=false;
        if let Some(media)=&mut app.media {media.configure(app.audio_applied.clone());}
        app.sync_call_alert_preferences();
        app.network.send(Command::RememberSession {enabled:false}).unwrap();
        app.network.send(auth(manifest.people.clone(),9001)).unwrap();
        std::fs::write(root.join("started.json"),serde_json::to_vec(&serde_json::json!({
            "pid":std::process::id(),"nonce":manifest.nonce,"synthetic_only":true,"media_admission_disabled":true
        })).unwrap()).unwrap();
        Box::new(OwnedApp {app,root,source:manifest.source,people:manifest.people,
            incoming:None,phase:0,began:Instant::now()})
    })).unwrap();
}
