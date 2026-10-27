//! Regression tests for the native client's command worker. No device APIs run.
#[path = "network_call_switch_tests.rs"]
mod call_switch_tests;
#[path = "network_message_alert_tests.rs"]
mod message_alert_tests;
#[path = "network_read_receipt_tests.rs"]
mod read_receipt_tests;
#[path = "network_send_binding_tests.rs"]
mod send_binding_tests;
#[cfg(windows)]
#[path = "network_status_problem_tests.rs"]
mod status_problem_tests;

#[path = "network_upload_policy_tests.rs"]
mod upload_policy_tests;
#[path = "network_navigation_cancel_tests.rs"]
mod navigation_cancel_tests;

use super::*;
use base64::Engine as _;
use serde_json::{json, Value};
use std::{
    fs,
    io::{Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    thread::{self, JoinHandle},
    time::SystemTime,
};

#[test]
fn queued_discord_secret_change_rejects_new_account_or_community_before_host_access() {
    let mut test = TestWorker::new();
    test.connection("http://127.0.0.1:4580".into());
    let origin = test.command_origin();
    test.worker.connection.as_mut().unwrap().session.user.id = "new-account".into();
    let result = test.worker.handle(Command::DiscordHost {
        origin: origin.clone(),
        action: DiscordHostAction::Save(zeroize::Zeroizing::new("never-sent-fixture-key".into())),
        request_id: 99,
    });
    assert!(result.unwrap_err().contains("another account or community"));
    test.connection("http://127.0.0.1:4580".into());
    test.worker.connection.as_mut().unwrap().server_id = "new-community".into();
    let result = test.worker.handle(Command::DiscordHost {
        origin,
        action: DiscordHostAction::Remove,
        request_id: 100,
    });
    assert!(result.unwrap_err().contains("another account or community"));
    assert!(test.drain().is_empty());
}

#[test]
fn local_setup_hint_uses_the_validated_anonymous_health_check_and_is_optional() {
    for required in [Some(true), Some(false), None] {
        let server = MockHttp::new(move |_| {
            let mut value = json!({"ok":true,"server_name":"Owned local setup",
                "server_id":"4be44aef-8c98-4db6-a9d5-81c2eb311330",
                "protocol_version":relay_protocol::API_PROTOCOL_VERSION,"rtc_enabled":true});
            if let Some(required) = required {
                value["setup_required"] = json!(required);
            }
            Reply::json(200, value)
        });
        let test = TestWorker::new();
        let summary = check_server(&test.worker.http, &server.url).unwrap();
        assert_eq!(summary.setup_required, required);
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "GET");
        assert_eq!(requests[0].path, "/api/health");
        assert!(requests[0].authorization.is_none());
    }
    let invalid = MockHttp::new(|_| {
        Reply::json(
            200,
            json!({
                "ok":true,"server_name":"Invalid setup hint","server_id":"not-a-server-id",
                "protocol_version":relay_protocol::API_PROTOCOL_VERSION,"rtc_enabled":true,"setup_required":true
            }),
        )
    });
    let test = TestWorker::new();
    assert!(check_server(&test.worker.http, &invalid.url).is_err());
}

struct TestWorker {
    worker: Worker,
    updates: mpsc::Receiver<Update>,
}

#[test]
fn handover_restore_survives_account_changes_and_prevents_the_old_default_host_start() {
    let mut test = TestWorker::new();
    test.connection("https://owned.example".into());
    let origin = test.command_origin();
    test.worker.handover_job = Some((
        crate::handover_client::Job::held_for_test(origin.clone(), true),
        0,
    ));
    test.worker.command(Command::StopHandover {
        origin: origin.clone(),
        account_change: true,
    });
    assert!(!test
        .worker
        .handover_job
        .as_ref()
        .unwrap()
        .0
        .cancelled_for_test());
    let mut other = origin.clone();
    other.user_id = "other-account".into();
    test.worker.command(Command::StopHandover {
        origin: other,
        account_change: false,
    });
    assert!(!test
        .worker
        .handover_job
        .as_ref()
        .unwrap()
        .0
        .cancelled_for_test());
    // Internal remembered-login restores bypass command(), so guard them too.
    let folder = tempfile::tempdir().unwrap();
    let error = test
        .worker
        .restore_app_host(
            folder.path().join("never-start.exe"),
            folder.path().join("never-open"),
            origin.server_id.clone(),
        )
        .unwrap_err();
    assert!(error.contains("transferred community"));
    assert!(!folder.path().join("never-open").exists());
    assert!(test.worker.local_server.is_none());
    test.worker.command(Command::StopHandover {
        origin: origin.clone(),
        account_change: false,
    });
    assert!(test
        .worker
        .handover_job
        .as_ref()
        .unwrap()
        .0
        .cancelled_for_test());
    test.worker.handover_job = Some((
        crate::handover_client::Job::held_for_test(origin.clone(), false),
        0,
    ));
    test.worker.command(Command::StopHandover {
        origin,
        account_change: true,
    });
    assert!(test
        .worker
        .handover_job
        .as_ref()
        .unwrap()
        .0
        .cancelled_for_test());
}

#[test]
fn reconnecting_poll_does_not_latch_an_error_but_auth_and_command_failures_remain_visible() {
    let mut test = TestWorker::new();
    test.connection("https://fixture.invalid".into());
    test.worker.set_status(ConnectionStatus::Reconnecting);
    for error in [SERVER_TIMEOUT, SERVER_UNREACHABLE] {
        test.worker.report_refresh_error(error.into());
    }
    let updates = test.drain();
    assert!(updates.iter().any(|u| matches!(
        u,
        Update::ConnectionState {
            status: ConnectionStatus::Reconnecting,
            ..
        }
    )));
    assert!(!updates.iter().any(|u| matches!(u, Update::Error(_))));
    // Authentication, identity, certificate and unrelated failures are actionable.
    for error in [
        "Sign in again",
        crate::connection_errors::WINDOWS_CONNECTION_BLOCKED,
        IDENTITY_CHANGED,
        "The request failed. Check your connection and server certificate.",
        "Cannot read encryption keys",
    ] {
        test.worker.report_refresh_error(error.into());
        assert!(test
            .drain()
            .iter()
            .any(|u| matches!(u, Update::Error(e) if e == error)));
    }
    test.worker.set_status(ConnectionStatus::Connected);
    test.worker.report_refresh_error(SERVER_TIMEOUT.into());
    let updates = test.drain();
    assert!(updates.iter().any(|u| matches!(
        u,
        Update::ConnectionState {
            status: ConnectionStatus::Connected,
            ..
        }
    )));
    assert!(updates
        .iter()
        .any(|u| matches!(u, Update::Error(e) if e == SERVER_TIMEOUT)));
    let server = MockHttp::new(|_| Reply::json(503, json!({"error": SERVER_TIMEOUT})));
    test.worker.set_status(ConnectionStatus::Reconnecting);
    test.drain();
    test.worker.command(Command::Auth {
        authority: None,
        url: server.url.clone(),
        expected_server_id: None,
        request: AuthRequest {
            second_factor: None,
            initial_protection: None,
            username: "synthetic".into(),
            password: "synthetic password".into(),
            invite: String::new(),
        },
        register: false,
        request_id: 18,
    });
    assert!(test.drain().iter().any(
        |u| matches!(u, Update::RequestFailed { request_id: 18, error } if error == SERVER_TIMEOUT)
    ));
}

fn test_work_root() -> PathBuf {
    std::env::var_os("RELAY_TEST_WORK_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("relay-tests"))
}

#[test]
#[ignore = "Needs an isolated verified Windows package and unused local hosting ports."]
fn actual_app_host_starts_stops_and_restores_the_same_community() {
    let package =
        PathBuf::from(std::env::var_os("RELAY_LIFECYCLE_QA_INSTALL").expect("isolated package"));
    let data =
        PathBuf::from(std::env::var_os("RELAY_SERVER_DATA_DIR").expect("isolated host storage"));
    assert!(
        !data.join("relay.sqlite3").exists(),
        "fresh synthetic data only"
    );
    let server = package.join("dist/relay-server.exe");
    let mut test = TestWorker::new();
    assert!(
        !test.worker.local_server_ready(),
        "another local server is running"
    );
    test.worker
        .handle(Command::HostLocal {
            server: server.clone(),
            data: data.clone(),
        })
        .unwrap();
    let before = test.worker.local_identity().unwrap();
    assert!(test.worker.owned_local.is_some());
    assert_eq!(
        crate::local_host::AppHost::load(&data).unwrap().server_id,
        before.server_id
    );
    test.worker.close_app_host().unwrap();
    assert!(!test.worker.local_server_ready());
    assert!(data.join("relay.sqlite3").is_file());
    test.worker
        .restore_app_host(server, data.clone(), before.server_id.clone())
        .unwrap();
    let after = test.worker.local_identity().unwrap();
    assert_eq!(before.server_id, after.server_id);
    assert_ne!(before.instance_id, after.instance_id);
    // The same normal-close path used by RelayApp::drop, without device APIs.
    assert_eq!(
        test.worker
            .finish_normal(Instant::now() + Duration::from_secs(2))
            .failed_requests,
        0
    );
    assert!(!test.worker.local_server_ready());
    assert!(!data.join("stack-instance.json").exists());
    assert!(data.join("relay.sqlite3").is_file());
}

impl TestWorker {
    fn new() -> Self {
        // Tests invoke the worker synchronously; allow its bounded encryption,
        // bootstrap and request acknowledgements before the fixture drains.
        let (tx, updates) = mpsc::sync_channel(32);
        let worker = Worker::new(
            tx,
            eframe::egui::Context::default(),
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        Self { worker, updates }
    }

    fn drain(&self) -> Vec<Update> {
        self.updates.try_iter().collect()
    }

    fn handle(&mut self, command: Command) -> Vec<Update> {
        self.worker.handle(command).unwrap();
        self.drain()
    }

    fn command_origin(&self) -> CommandOrigin {
        self.worker
            .connection
            .as_ref()
            .map(CommandOrigin::from)
            .unwrap_or_else(|| CommandOrigin {
                url: String::new(),
                server_id: String::new(),
                user_id: String::new(),
            })
    }

    fn connection(&mut self, url: String) {
        self.worker.connection = Some(Connection {
            url,
            session: Session {
                token: "fixture-token".into(),
                user: User {
                    id: "fixture-user".into(),
                    username: "fixture".into(),
                    role: "member".into(),
                    ..Default::default()
                },
            },
            current: None,
            voice_channel: Some("voice-room".into()),
            voice_attempt: None,
            voice_admission: None,
            voice_recovery: None,
            frame_ids: HashMap::new(),
            sharing: false,
            camera: false,
            server_id: String::new(),
            name: "Fixture".into(),
            change_cursor: 0,
            protocol_version: 0,
            remembered: false,
            chat: extended::ChatSession::default(),
            crypto: None,
            crypto_registered: false,
            crypto_call_epoch: None,
            open_call_context: None,
            crypto_pending_join: None,
        });
    }
}

fn connect_shutdown_network(server: &MockHttp) -> Network {
    let mut network = Network::new(eframe::egui::Context::default());
    network
        .send(Command::Auth {
            authority: None,
            url: server.url.clone(),
            expected_server_id: None,
            request: AuthRequest {
                second_factor: None,
                initial_protection: None,
                username: "shutdown-fixture".into(),
                password: "owned-synthetic-password".into(),
                invite: String::new(),
            },
            register: false,
            request_id: 0,
        })
        .unwrap();
    let until = Instant::now() + Duration::from_secs(4);
    let mut connected = false;
    while Instant::now() < until {
        while let Ok(update) = network.try_update() {
            if matches!(update, Update::Connected { .. }) {
                connected = true;
            }
        }
        if connected && !network.update_busy() {
            return network;
        }
        thread::sleep(Duration::from_millis(5));
    }
    panic!("owned loopback network did not authenticate and become idle");
}

fn shutdown_fixture_reply(request: &RecordedRequest) -> Reply {
    let user = User {
        id: "fixture-shutdown-user".into(),
        username: "shutdown-fixture".into(),
        ..Default::default()
    };
    match request.path.as_str() {
        "/api/login" => Reply::json(
            200,
            serde_json::to_value(Session {
                token: "synthetic-shutdown-token".into(),
                user,
            })
            .unwrap(),
        ),
        "/api/bootstrap" => Reply::json(
            200,
            serde_json::to_value(Bootstrap {
                user,
                server_id: "own-synthetic-shutdown-community".into(),
                ..Default::default()
            })
            .unwrap(),
        ),
        "/api/voice/leave" | "/api/logout" => {
            assert_eq!(
                request.authorization.as_deref(),
                Some("Bearer synthetic-shutdown-token")
            );
            Reply::json(200, json!({}))
        }
        _ => Reply::json(404, json!({"error":"not part of owned shutdown fixture"})),
    }
}

fn wait_shutdown(network: &mut Network) -> Result<(), String> {
    let until = Instant::now() + Duration::from_secs(4);
    loop {
        while network.try_update().is_ok() {}
        match network.poll_shutdown() {
            ShutdownPoll::Finished(result) => return result,
            ShutdownPoll::Pending(_) => {}
        }
        assert!(
            Instant::now() < until,
            "owned shutdown failed to return within its test deadline"
        );
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn native_network_shutdown_waits_for_actual_logout_acknowledgement_and_join() {
    let requested = Arc::new(AtomicBool::new(false));
    let accepted = Arc::new(AtomicBool::new(false));
    let received = requested.clone();
    let ack = accepted.clone();
    let server = MockHttp::with_request(move |request| {
        if request.path == "/api/logout" {
            received.store(true, Ordering::Release);
            thread::sleep(Duration::from_millis(150));
            ack.store(true, Ordering::Release);
        }
        shutdown_fixture_reply(request)
    });
    let mut network = connect_shutdown_network(&server);
    assert!(matches!(network.poll_shutdown(), ShutdownPoll::Pending(_)));
    let until = Instant::now() + Duration::from_secs(2);
    while !requested.load(Ordering::Acquire) {
        assert!(Instant::now() < until);
        while network.try_update().is_ok() {}
        thread::sleep(Duration::from_millis(5));
    }
    assert!(!accepted.load(Ordering::Acquire));
    assert!(matches!(network.poll_shutdown(), ShutdownPoll::Pending(_)));
    let old_worker = network.worker.as_ref().unwrap().thread().id();
    network.reopen_if_stopped(&eframe::egui::Context::default());
    assert_eq!(
        network.worker.as_ref().unwrap().thread().id(),
        old_worker,
        "cancellation cannot create an overlapping account worker"
    );
    wait_shutdown(&mut network).unwrap();
    assert!(accepted.load(Ordering::Acquire));
    assert!(network.worker.is_none(), "the actual worker was joined");
    let report = network.shutdown_result.as_ref().unwrap().as_ref().unwrap();
    assert_eq!(report.revoked_confirmed, 1);
    assert_eq!(report.voice_confirmed, 1);
    assert_eq!(report.failed_requests, 0);
}

#[test]
fn native_network_stalled_logout_blocks_update_and_allows_reconnection_after_join() {
    let server = MockHttp::with_request(|request| {
        if request.path == "/api/logout" {
            thread::sleep(Duration::from_millis(1200));
        }
        shutdown_fixture_reply(request)
    });
    let mut network = connect_shutdown_network(&server);
    let started = Instant::now();
    assert!(wait_shutdown(&mut network)
        .unwrap_err()
        .contains("did not confirm"));
    assert!(started.elapsed() < Duration::from_secs(3));
    assert!(network.worker.is_none());
    network.reopen_if_stopped(&eframe::egui::Context::default());
    assert!(matches!(
        network.try_update().unwrap(),
        Update::Disconnected
    ));
    assert!(
        matches!(network.try_update().unwrap(), Update::Communities(communities) if communities.is_empty())
    );
    assert!(!network.shutdown.load(Ordering::Acquire));
    assert!(network.send(Command::ClearSelection).is_ok());
}

#[test]
fn saturated_command_and_ui_queues_cannot_prevent_acknowledged_normal_close() {
    let logout = Arc::new(AtomicUsize::new(0));
    let revoked = logout.clone();
    let server = MockHttp::with_request(move |request| {
        if request.path == "/api/logout" {
            revoked.fetch_add(1, Ordering::AcqRel);
        }
        shutdown_fixture_reply(request)
    });
    let mut network = connect_shutdown_network(&server);
    // Commands publish multiple acknowledgements; stop consuming the eight-item
    // UI queue until the control worker is blocked and the 64 commands fill up.
    let mut rejected = false;
    for _ in 0..1000 {
        if network.send(Command::LeaveVoice).is_err() {
            rejected = true;
            break;
        }
    }
    assert!(rejected);
    assert!(
        network.update_busy(),
        "queued work blocks installer consent"
    );
    let started = Instant::now();
    network.finish_drop();
    assert!(started.elapsed() < Duration::from_secs(4));
    assert!(network.worker.is_none());
    assert_eq!(logout.load(Ordering::Acquire), 1);
    assert!(network.shutdown_result.as_ref().unwrap().is_ok());
}

#[test]
fn updater_hold_prevents_new_account_commands_and_releases_without_logout() {
    let server = MockHttp::with_request(shutdown_fixture_reply);
    let mut network = connect_shutdown_network(&server);
    network.set_update_hold(true);
    let until = Instant::now() + Duration::from_secs(2);
    while network.update_busy() {
        while network.try_update().is_ok() {}
        assert!(Instant::now() < until);
        thread::sleep(Duration::from_millis(5));
    }
    let control_requests = || {
        server
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter_map(|request| {
                // The already-started event subscriber may perform its read-only
                // WebSocket handshake after control work has become idle.
                if request.path == "/api/events" {
                    assert_eq!(request.method, "GET");
                    assert_eq!(
                        request.authorization.as_deref(),
                        Some("Bearer synthetic-shutdown-token")
                    );
                    None
                } else {
                    Some((request.method.clone(), request.path.clone()))
                }
            })
            .collect::<Vec<_>>()
    };
    let before = control_requests();
    assert!(network.send(Command::ClearSelection).is_err());
    thread::sleep(Duration::from_millis(80));
    assert_eq!(control_requests(), before);
    network.set_update_hold(false);
    assert!(network.send(Command::ClearSelection).is_ok());
    wait_shutdown(&mut network).unwrap();
}

#[test]
fn remembered_normal_close_keeps_session_and_protected_actor_continuity() {
    let server = MockHttp::new(|path| match path {
        "/api/voice/leave" => Reply::json(200, json!({})),
        "/api/logout" => panic!("normal close must not revoke a remembered session"),
        _ => Reply::json(404, json!({})),
    });
    let fixture = AttachmentFixture::new();
    let crypto = fixture.session();
    let identity = crypto.device.identity().clone();
    let mut test = TestWorker::new();
    test.connection(server.url.clone());
    let c = test.worker.connection.as_mut().unwrap();
    c.remembered = true;
    c.server_id = identity.server_id.clone();
    c.crypto = Some(crypto);
    let token = c.session.token.clone();
    let report = test
        .worker
        .finish_normal(Instant::now() + Duration::from_secs(2));
    assert_eq!(report.failed_requests, 0);
    assert_eq!(report.revoked_confirmed, 0);
    assert_eq!(report.voice_confirmed, 1);
    assert_eq!(
        test.worker.connection.as_ref().unwrap().session.token,
        token
    );
    drop(test.worker.connection.take());
    let reopened = fixture.session();
    assert_eq!(reopened.device.identity(), &identity);
    assert!(!server
        .paths
        .lock()
        .unwrap()
        .iter()
        .any(|path| path == "/api/logout"));
}

#[cfg(windows)]
struct OwnedSignInVault(Vec<String>);

#[cfg(windows)]
impl OwnedSignInVault {
    fn new(urls: Vec<String>) -> Self {
        use std::os::windows::fs::MetadataExt;
        let profile = PathBuf::from(std::env::var_os("RELAY_PROFILE_DIR")
            .expect("the sign-in vault test requires its isolated profile"));
        let root = PathBuf::from(r"C:\Important\Projects\2026-10-03\can\work\relay-signin-persistence-20261026");
        assert_eq!(profile.parent(), Some(root.as_path()));
        assert_eq!(test_work_root().parent(), Some(root.as_path()));
        for path in profile.ancestors() {
            assert_eq!(fs::symlink_metadata(path).unwrap().file_attributes() & 0x400, 0);
        }
        for url in &urls {
            assert!(crate::credential_vault::load(url).unwrap().is_none(),
                "fresh synthetic URL must not overwrite any existing credential");
        }
        Self(urls)
    }
}

#[cfg(windows)]
impl Drop for OwnedSignInVault {
    fn drop(&mut self) {
        for url in &self.0 {
            let _ = crate::credential_vault::forget(url);
        }
    }
}

#[test]
#[cfg(windows)]
fn remember_account_sessions_updates_active_parked_and_disconnected_without_touching_other_urls() {
    const SERVER: &str = "46ec6ef9-9abc-47f0-8076-3b6149c37843";
    let nonce = uuid::Uuid::new_v4();
    let urls: Vec<_> = ["active", "parked", "disconnected", "unrelated"].into_iter()
        .map(|name| format!("https://{name}-{nonce}.owned-signin.invalid")).collect();
    let _vault = OwnedSignInVault::new(urls.clone());
    let mut client = TestWorker::new();
    client.connection(urls[1].clone());
    client.worker.connection.as_mut().unwrap().server_id = SERVER.into();
    let parked = client.worker.connection.take().unwrap();
    client.worker.saved_connections.insert(urls[1].clone(), SavedConnection {
        connection: parked,
        stop: Arc::new(AtomicBool::new(false)),
        dirty: Arc::new(AtomicBool::new(false)),
    });
    client.connection(urls[0].clone());
    client.worker.connection.as_mut().unwrap().server_id = SERVER.into();
    let session = client.worker.connection.as_ref().unwrap().session.clone();
    let mut unrelated = session.clone();
    unrelated.token = "owned-unrelated-token".into();
    crate::credential_vault::save(&urls[2], SERVER, &session).unwrap();
    crate::credential_vault::save(&urls[3], SERVER, &unrelated).unwrap();
    client.handle(Command::RememberAccountSessions { enabled: true, urls: urls[..3].to_vec() });
    assert!(client.worker.remember_session);
    assert!(client.worker.connection.as_ref().unwrap().remembered);
    assert!(client.worker.saved_connections[&urls[1]].connection.remembered);
    for url in &urls[..3] {
        let saved = crate::credential_vault::load(url).unwrap().unwrap();
        assert_eq!(saved.server_id.as_deref(), Some(SERVER));
        assert_eq!(saved.session.token, session.token);
        assert_eq!(saved.session.user.id, session.user.id);
    }
    client.handle(Command::RememberAccountSessions { enabled: false, urls: urls[..3].to_vec() });
    assert!(!client.worker.remember_session);
    assert!(!client.worker.connection.as_ref().unwrap().remembered);
    assert!(!client.worker.saved_connections[&urls[1]].connection.remembered);
    assert_eq!(client.worker.connection.as_ref().unwrap().session.token, session.token,
        "opting out must keep the current in-memory account connected");
    for url in &urls[..3] {
        assert!(crate::credential_vault::load(url).unwrap().is_none());
    }
    assert_eq!(crate::credential_vault::load(&urls[3]).unwrap().unwrap().session.token,
        unrelated.token, "another saved account must not be removed");
}

#[test]
#[cfg(windows)]
fn remembered_account_resumes_in_a_fresh_worker_and_opt_out_revokes_on_normal_close() {
    const SERVER: &str = "e967e2ae-3716-4e69-b8c2-c58edcffda50";
    let server = MockHttp::with_request(|request| match request.path.as_str() {
        "/api/health" => {
            assert!(request.authorization.is_none());
            Reply::json(200, invitation_health(SERVER))
        }
        "/api/bootstrap" => {
            assert_eq!(request.authorization.as_deref(), Some("Bearer fixture-token"));
            Reply::json(200, serde_json::to_value(Bootstrap {
                server_id: SERVER.into(),
                user: User { id: "fixture-user".into(), username: "owned-restarted".into(), ..Default::default() },
                protocol_version: API_PROTOCOL_VERSION,
                ..Default::default()
            }).unwrap())
        }
        "/api/voice/leave" | "/api/logout" => Reply::json(200, json!({})),
        _ => Reply::json(404, json!({})),
    });
    let _vault = OwnedSignInVault::new(vec![server.url.clone()]);
    let mut first = TestWorker::new();
    first.connection(server.url.clone());
    first.worker.connection.as_mut().unwrap().server_id = SERVER.into();
    first.handle(Command::RememberAccountSessions { enabled: true, urls: vec![server.url.clone()] });
    let report = first.worker.finish_normal(Instant::now() + Duration::from_secs(2));
    assert_eq!(report.failed_requests, 0);
    assert_eq!(report.revoked_confirmed, 0);
    assert!(crate::credential_vault::load(&server.url).unwrap().is_some());
    drop(first);
    let mut resumed = TestWorker::new();
    let updates = resumed.handle(Command::Resume {
        url: server.url.clone(), expected_server_id: Some(SERVER.into()),
    });
    assert!(updates.iter().any(|update| matches!(update,
        Update::Connected { session, .. } if session.user.username == "owned-restarted")));
    let connection = resumed.worker.connection.as_ref().unwrap();
    assert!(connection.remembered);
    assert_eq!(connection.session.token, "fixture-token");
    assert_eq!(connection.server_id, SERVER);
    assert!(!server.paths.lock().unwrap().iter().any(|path| path == "/api/logout"));
    resumed.handle(Command::RememberAccountSessions { enabled: false, urls: vec![server.url.clone()] });
    assert!(crate::credential_vault::load(&server.url).unwrap().is_none());
    assert!(!resumed.worker.connection.as_ref().unwrap().remembered);
    let report = resumed.worker.finish_normal(Instant::now() + Duration::from_secs(2));
    assert_eq!(report.failed_requests, 0);
    assert_eq!(report.revoked_confirmed, 1);
    assert_eq!(server.paths.lock().unwrap().iter().filter(|path| path.as_str() == "/api/logout").count(), 1);
    assert!(crate::credential_vault::load(&server.url).unwrap().is_none());
}

#[test]
#[cfg(windows)]
fn bulk_forget_accounts_acknowledges_completed_cleanup_and_reports_invalid_urls() {
    const SERVER: &str = "624070bc-0d9d-40f5-b5b1-3b3bd7b6fbf2";
    let (entered_tx, entered_rx) = mpsc::channel();
    let release = Arc::new(AtomicBool::new(false));
    let handler = || {
        let entered_tx = entered_tx.clone();
        let release = release.clone();
        move |path: &str| {
            if path == "/api/logout" {
                let _ = entered_tx.send(());
                let deadline = Instant::now() + Duration::from_secs(3);
                while !release.load(Ordering::Acquire) && Instant::now() < deadline {
                    thread::sleep(Duration::from_millis(2));
                }
            }
            Reply::json(200, json!({}))
        }
    };
    let active = MockHttp::new(handler());
    let parked = MockHttp::new(handler());
    let saved_only = MockHttp::new(|_| panic!("saved-only cleanup must not make an HTTP request"));
    let unrelated = MockHttp::new(|_| panic!("an unrelated account must not be contacted"));
    let urls = vec![active.url.clone(), parked.url.clone(), saved_only.url.clone(), unrelated.url.clone()];
    let _vault = OwnedSignInVault::new(urls.clone());
    let mut setup = TestWorker::new();
    setup.connection(active.url.clone());
    let session = setup.worker.connection.as_ref().unwrap().session.clone();
    drop(setup);
    for url in &urls {
        crate::credential_vault::save(url, SERVER, &session).unwrap();
    }
    let selected = urls[..3].to_vec();
    let (ready_tx, ready_rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        let mut test = TestWorker::new();
        test.connection(selected[1].clone());
        let mut parked = test.worker.connection.take().unwrap();
        parked.server_id = SERVER.into();
        parked.remembered = true;
        test.worker.saved_connections.insert(selected[1].clone(), SavedConnection {
            connection: parked,
            stop: Arc::new(AtomicBool::new(false)),
            dirty: Arc::new(AtomicBool::new(false)),
        });
        test.connection(selected[0].clone());
        test.worker.connection.as_mut().unwrap().server_id = SERVER.into();
        test.worker.connection.as_mut().unwrap().remembered = true;
        let updates = std::mem::replace(&mut test.updates, mpsc::sync_channel(1).1);
        ready_tx.send(updates).unwrap();
        test.worker.handle(Command::ForgetAccountSessions { urls: selected, request_id: 81 }).unwrap();
        assert!(test.worker.connection.is_none());
        assert!(test.worker.saved_connections.is_empty());
    });
    let updates = ready_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    let early_ack = updates.try_iter().any(|update|
        matches!(update, Update::AccountSessionsForgotten { request_id: 81, .. }));
    // Release before asserting so a failed ordering check cannot strand the worker.
    release.store(true, Ordering::Release);
    worker.join().unwrap();
    assert!(!early_ack, "success must not precede the completed known-account cleanup");
    let completed: Vec<_> = updates.try_iter().filter_map(|update| match update {
        Update::AccountSessionsForgotten { request_id: 81, result } => Some(result),
        _ => None,
    }).collect();
    assert_eq!(completed.len(), 1);
    assert!(completed[0].is_ok());
    for url in &urls[..3] {
        assert!(crate::credential_vault::load(url).unwrap().is_none(),
            "a success acknowledgement must leave no selected stored session");
    }
    assert!(crate::credential_vault::load(&urls[3]).unwrap().is_some());
    let mut invalid = TestWorker::new();
    invalid.connection(unrelated.url.clone());
    let updates = invalid.handle(Command::ForgetAccountSessions {
        urls: vec![unrelated.url.clone(), "not a server URL".into()], request_id: 82,
    });
    let acknowledgements: Vec<_> = updates.iter().filter_map(|update| match update {
        Update::AccountSessionsForgotten { request_id: 82, result } => Some(result),
        _ => None,
    }).collect();
    assert_eq!(acknowledgements.len(), 1);
    assert!(acknowledgements[0].is_err());
    assert!(invalid.worker.connection.is_some());
    assert_eq!(crate::credential_vault::load(&unrelated.url).unwrap().unwrap().session.token, session.token);
    assert!(unrelated.requests.lock().unwrap().is_empty());
}

#[test]
fn actual_encrypted_history_open_blocks_update_until_its_archive_lease_finishes() {
    let folder = test_work_root().join(format!("native-shutdown-history-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&folder).unwrap();
    let path = folder.join("own-synthetic-history.relayhistory");
    let archive = crate::recovery::HistoryArchive::new(
        relay_crypto::Identity {
            server_id: "own-archive-community".into(),
            user_id: "own-archive-user".into(),
            device_id: "own-archive-device".into(),
        },
        "a".repeat(64),
    );
    let password = "synthetic history passphrase";
    crate::recovery::save(&path, &archive, password).unwrap();
    let original = fs::read(&path).unwrap();
    let mut network = Network::new(eframe::egui::Context::default());
    network
        .send(Command::HistoryBackup {
            origin: None,
            path: path.clone(),
            password: zeroize::Zeroizing::new(password.into()),
            export: false,
            request_id: 921,
        })
        .unwrap();
    let until = Instant::now() + Duration::from_secs(20);
    let mut saw_archive_lease = false;
    let mut result = false;
    while Instant::now() < until {
        if network.work.archive.load(Ordering::Acquire) {
            saw_archive_lease = true;
            assert!(
                network.update_busy() || !network.work.archive.load(Ordering::Acquire),
                "history bypasses UI Busy but must still block installation"
            );
        }
        while let Ok(update) = network.try_update() {
            if let Update::HistoryBackup {
                request_id: 921,
                result: opened,
                ..
            } = update
            {
                assert!(matches!(
                    opened.unwrap(),
                    crate::recovery::BackupResult::Opened(_)
                ));
                result = true;
            }
        }
        if result && !network.update_busy() {
            break;
        }
        thread::sleep(Duration::from_millis(2));
    }
    assert!(result && saw_archive_lease);
    assert!(!network.update_busy());
    assert_eq!(
        fs::read(&path).unwrap(),
        original,
        "opening never mutates the history archive"
    );
    wait_shutdown(&mut network).unwrap();
    fs::remove_file(path).unwrap();
    fs::remove_dir(folder).unwrap();
}

impl Drop for TestWorker {
    fn drop(&mut self) {
        self.worker.socket_stop.store(true, Ordering::Release);
        #[cfg(windows)]
        if let Some(crypto) = self
            .worker
            .connection
            .as_ref()
            .and_then(|c| c.crypto.as_ref())
        {
            crypto.remove_test_checkpoint();
        }
    }
}

struct AttachmentFixture(PathBuf);
impl AttachmentFixture {
    fn new() -> Self {
        let root = test_work_root().join(format!("encrypted-attachments-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        Self(root)
    }
    fn session(&self) -> crate::e2ee::CryptoSession {
        crate::e2ee::CryptoSession::open_for_test(
            self.0.join("keys"),
            &format!("fixture-{}", self.0.file_name().unwrap().to_string_lossy()),
            "fixture-user",
        )
        .unwrap()
    }
}
impl Drop for AttachmentFixture {
    fn drop(&mut self) {
        assert_eq!(
            self.0.canonicalize().unwrap().parent().unwrap(),
            test_work_root().canonicalize().unwrap()
        );
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn install_file_crypto(client: &mut TestWorker, crypto: crate::e2ee::CryptoSession) {
    let connection = client.worker.connection.as_mut().unwrap();
    connection.server_id = crypto.device.identity().server_id.clone();
    connection.crypto = Some(crypto);
    connection.crypto_registered = true;
    connection.chat = extended::ChatSession::new(&Bootstrap {
        capabilities: vec!["e2ee_mls_v1".into()],
        ..Default::default()
    });
}

fn encrypted_attachment(
    bytes: &[u8],
    id: &str,
) -> (Vec<u8>, Attachment, crate::e2ee::EncryptedAttachment) {
    let (wire, file) =
        relay_crypto::files::encrypt(bytes, "fixture/server/conversation/random-operation")
            .unwrap();
    let attachment = Attachment {
        id: id.into(),
        name: "fixture.gif".into(),
        mime: "image/gif".into(),
        size: bytes.len() as u64,
        url: format!("/api/uploads/{id}"),
    };
    let encrypted = crate::e2ee::EncryptedAttachment {
        id: id.into(),
        name: attachment.name.clone(),
        mime: attachment.mime.clone(),
        file,
    };
    (wire, attachment, encrypted)
}

fn existing_upload_group(
    crypto: &mut crate::e2ee::CryptoSession,
    target: &Target,
) -> CryptoContext {
    let server = crypto.device.identity().server_id.clone();
    let scope = match target {
        Target::Channel(id) => format!("{server}/channel/{id}"),
        Target::Direct(peer) => {
            let mut pair = ["fixture-user", peer.as_str()];
            pair.sort_unstable();
            format!("{server}/dm/{}/{}", pair[0], pair[1])
        }
    };
    let group_id = format!("fixture-file-group-{}", uuid::Uuid::new_v4());
    crypto
        .with_device(|d| d.create_group(&scope, &group_id))
        .unwrap();
    let own = crypto.advertised();
    let mut devices = vec![CryptoDevice {
        user_id: own.identity.user_id,
        device_id: own.identity.device_id,
        identity: own.fingerprint,
        key_package: base64::engine::general_purpose::STANDARD.encode(own.key_package),
        invite_attestation: None,
    }];
    let mut roster = vec![User {
        id: "fixture-user".into(),
        username: "fixture".into(),
        role: "member".into(),
        ..Default::default()
    }];
    if let Target::Direct(peer) = target {
        let mut recipient = relay_crypto::Device::new(relay_crypto::Identity {
            server_id: server.clone(),
            user_id: peer.clone(),
            device_id: "verified-fixture-peer".into(),
        })
        .unwrap();
        let public = recipient.key_package().unwrap();
        crypto
            .with_device(|device| {
                device.approve(&public, &public.fingerprint)?;
                device.add_members(&scope, std::slice::from_ref(&public))?;
                Ok(())
            })
            .unwrap();
        devices.push(CryptoDevice {
            user_id: public.identity.user_id.clone(),
            device_id: public.identity.device_id.clone(),
            identity: public.fingerprint.clone(),
            key_package: base64::engine::general_purpose::STANDARD.encode(&public.key_package),
            invite_attestation: None,
        });
        roster.push(User {
            id: peer.clone(),
            username: "peer".into(),
            role: "member".into(),
            ..Default::default()
        });
    }
    CryptoContext {
        server_id: server,
        target: match target {
            Target::Channel(id) => CryptoTarget {
                channel_id: Some(id.clone()),
                peer_id: None,
            },
            Target::Direct(id) => CryptoTarget {
                channel_id: None,
                peer_id: Some(id.clone()),
            },
        },
        roster,
        devices,
        group_id: Some(group_id),
        ..Default::default()
    }
}

#[derive(Clone)]
pub(super) struct Reply {
    pub(super) status: u16,
    pub(super) body: Vec<u8>,
    pub(super) headers: Vec<(String, String)>,
}

impl Reply {
    pub(super) fn json(status: u16, value: Value) -> Self {
        Self {
            status,
            body: serde_json::to_vec(&value).unwrap(),
            headers: Vec::new(),
        }
    }

    fn header(mut self, name: &str, value: &str) -> Self {
        assert!(!name.contains(['\r', '\n']) && !value.contains(['\r', '\n']));
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }
}

pub(super) struct MockHttp {
    pub(super) url: String,
    http: Client,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    paths: Arc<std::sync::Mutex<Vec<String>>>,
    pub(super) requests: Arc<std::sync::Mutex<Vec<RecordedRequest>>>,
    crypto_contexts: Arc<std::sync::Mutex<HashMap<Target, CryptoContext>>>,
    call_refresh_ack: Arc<AtomicBool>,
}

fn fixture_call_refresh_ack(
    contexts: &std::sync::Mutex<HashMap<Target, CryptoContext>>,
    request: &RecordedRequest,
) -> Reply {
    let batch: AppendCryptoEvents = serde_json::from_slice(&request.body).unwrap();
    let target = match (&batch.target.channel_id, &batch.target.peer_id) {
        (Some(id), None) => Target::Channel(id.clone()),
        _ => return Reply::json(400, json!({"error":"fixture expects a voice channel"})),
    };
    let mut contexts = contexts.lock().unwrap();
    let context = contexts.get_mut(&target).unwrap();
    if batch.expected_sequence != context.sequence
        || batch.epoch != context.sequence + 1
        || context.group_id.as_ref() != Some(&batch.group_id)
        || batch.events.len() != 1
        || batch.events[0].kind != "commit"
        || batch.events[0].recipient_device_id.is_some()
        || !context
            .devices
            .iter()
            .any(|d| d.user_id == "fixture-user" && d.device_id == batch.device_id)
    {
        return Reply::json(409, json!({"error":"fixture key context changed"}));
    }
    context.sequence += 1;
    context.events.push(CryptoEvent {
        sequence: context.sequence,
        epoch: batch.epoch,
        group_id: batch.group_id,
        operation_id: batch.operation_id,
        kind: "commit".into(),
        author_user_id: "fixture-user".into(),
        author_device_id: batch.device_id,
        recipient_device_id: None,
        body: batch.events[0].body.clone(),
    });
    Reply::json(200, serde_json::to_value(context).unwrap())
}

pub(super) struct RecordedRequest {
    pub(super) method: String,
    pub(super) path: String,
    pub(super) body: Vec<u8>,
    pub(super) authorization: Option<String>,
}

impl MockHttp {
    fn new(handler: impl Fn(&str) -> Reply + Send + Sync + 'static) -> Self {
        Self::with_request(move |request| handler(&request.path))
    }

    pub(super) fn with_request(
        handler: impl Fn(&RecordedRequest) -> Reply + Send + Sync + 'static,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let paths = Arc::new(std::sync::Mutex::new(Vec::new()));
        let thread_paths = paths.clone();
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let thread_requests = requests.clone();
        let crypto_contexts = Arc::new(std::sync::Mutex::new(
            HashMap::<Target, CryptoContext>::new(),
        ));
        let thread_contexts = crypto_contexts.clone();
        let call_refresh_ack = Arc::new(AtomicBool::new(false));
        let thread_call_refresh_ack = call_refresh_ack.clone();
        let handler = Arc::new(handler);
        let worker = thread::spawn(move || {
            let mut connections: Vec<JoinHandle<()>> = Vec::new();
            while !thread_stop.load(Ordering::Acquire) {
                let mut index = 0;
                while index < connections.len() {
                    if connections[index].is_finished() {
                        connections.swap_remove(index).join().unwrap();
                    } else {
                        index += 1;
                    }
                }
                // Bound the fixture's socket workers without serializing graceful closes.
                if connections.len() >= 32 {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                }
                let (mut socket, _) = match listener.accept() {
                    Ok(client) => client,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::Interrupted
                                | std::io::ErrorKind::ConnectionAborted
                                | std::io::ErrorKind::ConnectionReset
                        ) =>
                    {
                        continue;
                    }
                    Err(_) => return,
                };
                let paths = thread_paths.clone();
                let requests = thread_requests.clone();
                let handler = handler.clone();
                let contexts = thread_contexts.clone();
                let call_refresh_ack = thread_call_refresh_ack.clone();
                connections.push(thread::spawn(move || {
                    // Winsock accept inherits the listener's nonblocking mode. The
                    // blocking parser must wait for request headers and every body chunk.
                    socket.set_nonblocking(false).unwrap();
                    socket.set_nodelay(true).unwrap();
                    socket
                        .set_read_timeout(Some(Duration::from_secs(10)))
                        .unwrap();
                    socket
                        .set_write_timeout(Some(Duration::from_secs(10)))
                        .unwrap();
                    let Some(request) = read_request(&mut socket) else {
                        return;
                    };
                    paths.lock().unwrap().push(request.path.clone());
                    let mut reply = if request.path == "/api/e2ee/events" && call_refresh_ack.load(Ordering::Acquire) {
                        fixture_call_refresh_ack(&contexts, &request)
                    } else if request.path.starts_with("/api/e2ee/context?") {
                        let query = request.path.split_once('?').unwrap().1;
                        let target = url::form_urlencoded::parse(query.as_bytes()).find_map(|(key,value)|
                            match key.as_ref() { "channel_id" => Some(Target::Channel(value.into_owned())),
                                "peer_id" => Some(Target::Direct(value.into_owned())), _ => None });
                        let context = target.and_then(|target| contexts.lock().unwrap().get(&target).cloned());
                        let after = url::form_urlencoded::parse(query.as_bytes())
                            .find_map(|(key,value)| (key == "after").then(|| value.parse::<u64>().unwrap())).unwrap_or(0);
                        context.map(|mut context| {
                            context.events.retain(|event| event.sequence > after);
                            Reply::json(200,serde_json::to_value(context).unwrap())
                        })
                            .unwrap_or_else(|| handler(&request))
                    } else if request.path.starts_with("/api/history?after=") && !contexts.lock().unwrap().is_empty() {
                        Reply::json(200,serde_json::to_value(HistoryPage::default()).unwrap())
                    } else { handler(&request) };
                    if request.path == "/api/bootstrap" && reply.status==200 {
                        if let Some(context) = contexts.lock().unwrap().values().next() {
                            if let Ok(mut bootstrap) = serde_json::from_slice::<Bootstrap>(&reply.body) {
                                bootstrap.server_id=context.server_id.clone();
                                bootstrap.user=context.roster.iter().find(|u|u.id=="fixture-user").unwrap().clone();
                                bootstrap.members=context.roster.clone();
                                bootstrap.capabilities.push("e2ee_mls_v1".into());
                                reply.body=serde_json::to_vec(&bootstrap).unwrap();
                            }
                        }
                    }
                    requests.lock().unwrap().push(request);
                    if reply.status == 0 { return; } // Simulate a lost response after committing.
                    let extra_headers: String = reply.headers.iter().map(|(name, value)| format!("{name}: {value}\r\n")).collect();
                    let header = format!(
                        "HTTP/1.1 {} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{extra_headers}Connection: close\r\n\r\n",
                        reply.status,
                        reply.body.len()
                    );
                    if socket.write_all(header.as_bytes()).is_ok() {
                        // A bounded reader may close before an oversized response ends.
                        let _ = socket.write_all(&reply.body);
                    }
                    // Send FIN, then drain on this connection's worker. Keeping the socket
                    // alive avoids Windows resets; a slow close cannot block later requests.
                    let _ = socket.shutdown(Shutdown::Write);
                    let mut remainder = [0u8; 1024];
                    while matches!(socket.read(&mut remainder), Ok(n) if n > 0) {}
                }));
            }
            for connection in connections {
                connection.join().unwrap();
            }
        });
        Self {
            url,
            http: Client::builder()
                .timeout(Duration::from_secs(8))
                .no_proxy()
                .build()
                .unwrap(),
            stop,
            worker: Some(worker),
            paths,
            requests,
            crypto_contexts,
            call_refresh_ack,
        }
    }

    #[cfg(windows)]
    fn enable_encrypted_target(&self, client: &mut TestWorker, target: Target) {
        let connection = client.worker.connection.as_mut().unwrap();
        let mut crypto = connection.crypto.take().unwrap_or_else(|| {
            let root = test_work_root().join(format!("worker-crypto-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&root).unwrap();
            crate::e2ee::CryptoSession::open_for_test(
                root.join("keys"),
                &format!("fixture-{}", uuid::Uuid::new_v4()),
                "fixture-user",
            )
            .unwrap()
        });
        let context = existing_upload_group(&mut crypto, &target);
        for id in ["fixture-upload", "kept-attachment"] {
            let (_, _, file) = encrypted_attachment(b"synthetic attachment", id);
            crypto.add_file(file).unwrap();
        }
        connection.server_id = crypto.device.identity().server_id.clone();
        connection.crypto = Some(crypto);
        connection.crypto_registered = true;
        let mut bootstrap = Bootstrap {
            capabilities: vec!["e2ee_mls_v1".into()],
            ..Default::default()
        };
        bootstrap.channels.push(Channel {
            id: match &target {
                Target::Channel(id) => id.clone(),
                _ => String::new(),
            },
            ..Default::default()
        });
        connection.chat.update_bootstrap(&bootstrap);
        self.crypto_contexts.lock().unwrap().insert(target, context);
    }

    fn message_requests(&self) -> Vec<Vec<u8>> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.method == "POST" && r.path == "/api/messages")
            .map(|r| r.body.clone())
            .collect()
    }

    #[cfg(windows)]
    fn encrypted_message(
        &self,
        client: &mut TestWorker,
        target: &Target,
        id: &str,
        text: &str,
    ) -> Message {
        let scope = client.worker.crypto_scope(target).unwrap();
        let crypto = client
            .worker
            .connection
            .as_mut()
            .unwrap()
            .crypto
            .as_mut()
            .unwrap();
        let client_id = format!("client-{id}");
        let wire = crypto
            .encrypted_content(crate::e2ee::Envelope {
                version: 1,
                scope,
                client_id: client_id.clone(),
                content: text.into(),
                reply_to_id: None,
                mentions: Vec::new(),
                attachments: Vec::new(),
                action: None,
            })
            .unwrap();
        crypto.bind_delivery(&wire, id, 1).unwrap();
        crypto.committed(&client_id).unwrap();
        Message {
            id: id.into(),
            content: wire,
            client_id: Some(client_id),
            sequence: 1,
            author: client
                .worker
                .connection
                .as_ref()
                .unwrap()
                .session
                .user
                .clone(),
            channel_id: match target {
                Target::Channel(id) => Some(id.clone()),
                _ => None,
            },
            recipient_id: match target {
                Target::Direct(id) => Some(id.clone()),
                _ => None,
            },
            ..Default::default()
        }
    }

    fn response(&self, path: &str) -> reqwest::blocking::Response {
        self.http
            .get(format!("{}{path}", self.url))
            .send()
            .unwrap_or_else(|error| panic!("fixture request {path} failed: {error:#}"))
    }
}

impl Drop for MockHttp {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            if let Err(panic) = worker.join() {
                if thread::panicking() {
                    let message = panic
                        .downcast_ref::<String>()
                        .map(String::as_str)
                        .or_else(|| panic.downcast_ref::<&str>().copied())
                        .unwrap_or("non-string panic payload");
                    eprintln!("Owned mock HTTP handler also panicked during failed-test cleanup: {message}");
                } else {
                    std::panic::resume_unwind(panic);
                }
            }
        }
    }
}

fn read_request(socket: &mut TcpStream) -> Option<RecordedRequest> {
    let mut request = Vec::new();
    let mut buffer = [0u8; 4096];
    let header_end = loop {
        let n = socket.read(&mut buffer).ok()?;
        if n == 0 || request.len() + n > 64 * 1024 {
            return None;
        }
        request.extend_from_slice(&buffer[..n]);
        if let Some(index) = request.windows(4).position(|v| v == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let header = String::from_utf8_lossy(&request[..header_end]);
    let mut first_line = header.lines().next()?.split_whitespace();
    let method = first_line.next()?.to_owned();
    let path = first_line.next()?.to_owned();
    let length = header
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    let authorization = header.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("authorization")
            .then(|| value.trim().to_owned())
    });
    // Bound fixture bodies by the supported wire ceiling; headers stay 64 KiB.
    if length > MAX_SUPPORTED_UPLOAD_BYTES {
        return None;
    }
    while request.len() < header_end + length {
        let n = socket.read(&mut buffer).ok()?;
        if n == 0 {
            return None;
        }
        request.extend_from_slice(&buffer[..n]);
    }
    Some(RecordedRequest {
        method,
        path,
        body: request[header_end..header_end + length].to_vec(),
        authorization,
    })
}

#[test]
fn mock_http_drains_request_bodies_and_slow_closes_without_blocking_next_request() {
    let server = MockHttp::new(|path| Reply::json(200, json!({"path":path})));
    let mut held = TcpStream::connect(server.url.trim_start_matches("http://")).unwrap();
    held.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    // Accept may precede the first bytes, and a body may arrive in separate packets.
    thread::sleep(Duration::from_millis(50));
    held.write_all(b"POST /held HTTP/1.1\r\nHost: localhost\r\nContent-Length: 7\r\n\r\n")
        .unwrap();
    held.write_all(b"pay").unwrap();
    thread::sleep(Duration::from_millis(10));
    held.write_all(b"load").unwrap();
    let mut response = Vec::new();
    held.read_to_end(&mut response).unwrap();
    assert!(response.ends_with(b"{\"path\":\"/held\"}"));
    // The write side intentionally remains open while the server drains this socket.
    let next: Value = server
        .http
        .get(format!("{}/next", server.url))
        .timeout(Duration::from_secs(1))
        .send()
        .expect("an open prior socket must not block a new request")
        .json()
        .unwrap();
    assert_eq!(next, json!({"path":"/next"}));
    assert_eq!(server.requests.lock().unwrap()[0].body, b"payload");
    held.shutdown(Shutdown::Write).unwrap();
}

fn preview_attachment(id: &str) -> Attachment {
    Attachment {
        id: id.into(),
        name: "animation.gif".into(),
        mime: "image/gif".into(),
        size: 64,
        url: format!("/api/uploads/{id}"),
    }
}

#[cfg(windows)]
#[test]
fn idle_chat_and_rejected_media_requests_create_no_media_workers_or_http_runtimes() {
    let server = MockHttp::new(|path| match path {
        "/api/messages?limit=100&channel_id=text-room" => Reply::json(200, json!([])),
        "/api/voice/join" => Reply::json(403, json!({"error":"join denied"})),
        _ => Reply::json(404, json!({"error":"missing"})),
    });
    let mut client = TestWorker::new();
    let (_frames, receiver) = mpsc::sync_channel(2);
    client.worker.live_frames = Some(Arc::new(std::sync::Mutex::new(receiver)));
    assert!(client.worker.chat_media_workers.is_none());
    assert!(client.worker.live.http.get().is_none());
    assert!(client
        .worker
        .queue_chat_media(
            &client.command_origin(),
            preview_attachment("unauthenticated")
        )
        .is_err());
    client.connection(server.url.clone());
    client.worker.connection.as_mut().unwrap().voice_channel = None;
    let updates = client.handle(Command::Select(Target::Channel("text-room".into())));
    assert!(updates.iter().any(|u| matches!(u, Update::Messages { .. })));
    let mut invalid = preview_attachment("invalid");
    invalid.url = "https://other.invalid/image.gif".into();
    assert!(client
        .worker
        .queue_chat_media(&client.command_origin(), invalid)
        .is_err());
    let mut oversized = preview_attachment("oversized");
    oversized.size = chat_media::MAX_COMPRESSED_BYTES as u64 + 1;
    assert!(client
        .worker
        .queue_chat_media(&client.command_origin(), oversized)
        .is_err());
    server.enable_encrypted_target(&mut client, Target::Channel("denied-room".into()));
    client.worker.command(Command::JoinVoiceRequest {
        channel_id: "denied-room".into(),
        request_id: 5,
    });
    assert!(client.drain().iter().any(|u| matches!(u,
        Update::VoiceJoinFailed { error, request_id:5, .. } if error == "join denied")));
    assert!(client.worker.chat_media_workers.is_none());
    assert!(client.worker.live.http.get().is_none());
    assert!(
        client.worker.live_frames.is_some(),
        "no publisher or poller consumed the receiver"
    );
    assert!(client.worker.live.snapshot().is_none());
    assert_eq!(server.requests.lock().unwrap().len(), 2);
}

#[cfg(windows)]
#[test]
fn protected_join_leave_and_rejoin_never_allocate_legacy_jpeg_transport() {
    let server = MockHttp::new(|path| match path {
        "/api/voice/join" => Reply::json(
            200,
            json!({"session_id":1,"key":"AA==","udp_port":4581,"wire_version":3}),
        ),
        "/api/voice/leave" => Reply {
            status: 204,
            headers: Vec::new(),
            body: Vec::new(),
        },
        "/api/bootstrap" => Reply::json(200, serde_json::to_value(Bootstrap::default()).unwrap()),
        _ => Reply::json(404, json!({"error":"missing"})),
    });
    let mut client = TestWorker::new();
    server.call_refresh_ack.store(true, Ordering::Release);
    client.connection(server.url.clone());
    server.enable_encrypted_target(&mut client, Target::Channel("voice-room".into()));
    client.worker.connection.as_mut().unwrap().voice_channel = None;
    let (_frames, receiver) = mpsc::sync_channel(2);
    client.worker.live_frames = Some(Arc::new(std::sync::Mutex::new(receiver)));
    for request_id in [1, 2] {
        let updates = client.handle(Command::JoinVoiceRequest {
            channel_id: "voice-room".into(),
            request_id,
        });
        assert!(updates.iter().any(|u|matches!(u,Update::VoiceJoined {request_id:id,keys,..} if *id==request_id && keys.local_user_id=="fixture-user")));
        assert!(client.worker.live.http.get().is_none());
        assert!(
            client.worker.live_frames.is_some(),
            "protected join cannot consume JPEG publisher input"
        );
        assert!(client.worker.chat_media_workers.is_none());
        assert!(client.worker.live.snapshot().is_none());
        client.handle(Command::LeaveVoice);
    }
    assert!(server
        .paths
        .lock()
        .unwrap()
        .iter()
        .all(|p| !p.starts_with("/api/media")));
}

#[test]
fn leaving_voice_clears_local_intent_before_a_failed_remote_acknowledgement() {
    let server = MockHttp::new(|_| Reply::json(503, json!({"error":"server unavailable"})));
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let unavailable_url = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    for url in [server.url.clone(), unavailable_url] {
        let mut client = TestWorker::new();
        client.connection(url.clone());
        let c = client.worker.connection.as_mut().unwrap();
        c.crypto_pending_join = Some(("voice-room".into(), 99));
        c.crypto_call_epoch = Some(7);
        c.frame_ids
            .insert("fixture-user/screen".into(), "old-frame".into());
        c.sharing = true;
        c.camera = true;
        *client.worker.live.context.lock().unwrap() = Some(LiveConnection {
            url,
            token: "fixture-token".into(),
            channel_id: "voice-room".into(),
            generation: client.worker.live.epoch.load(Ordering::SeqCst),
            sharing: true,
            camera: true,
        });
        let old_generation = client.worker.live.epoch.load(Ordering::SeqCst);
        assert!(client.worker.handle(Command::LeaveVoice).is_err());
        assert!(client
            .drain()
            .iter()
            .any(|u| matches!(u, Update::VoiceLeft)));
        let c = client.worker.connection.as_ref().unwrap();
        assert!(c.voice_channel.is_none());
        assert!(c.crypto_pending_join.is_none());
        assert!(c.crypto_call_epoch.is_none());
        assert!(c.frame_ids.is_empty());
        assert!(!c.sharing && !c.camera);
        assert!(client.worker.live.snapshot().is_none());
        assert!(client.worker.live.epoch.load(Ordering::SeqCst) > old_generation);
    }
    assert_eq!(*server.paths.lock().unwrap(), ["/api/voice/leave"]);
}

#[cfg(windows)]
#[test]
fn cancelled_protected_join_cannot_resume_when_server_returns() {
    let server = MockHttp::new(|path| match path {
        "/api/voice/leave" => Reply::json(503, json!({"error":"server unavailable"})),
        "/api/bootstrap" => Reply::json(200, serde_json::to_value(Bootstrap::default()).unwrap()),
        _ => Reply::json(404, json!({"error":"missing"})),
    });
    let mut client = TestWorker::new();
    client.connection(server.url.clone());
    server.enable_encrypted_target(&mut client, Target::Channel("voice-room".into()));
    let c = client.worker.connection.as_mut().unwrap();
    c.crypto_pending_join = Some(("voice-room".into(), 99));
    c.crypto_call_epoch = Some(7);
    client.worker.command(Command::LeaveVoice);
    assert!(client
        .drain()
        .iter()
        .any(|u| matches!(u, Update::VoiceLeft)));
    server.call_refresh_ack.store(true, Ordering::Release);
    for _ in 0..3 {
        client.worker.refresh();
        assert!(!client.drain().iter().any(|u| matches!(
            u,
            Update::VoiceJoined { .. } | Update::VoiceEncryptionChanged { .. }
        )));
    }
    assert!(client
        .worker
        .connection
        .as_ref()
        .unwrap()
        .voice_channel
        .is_none());
    assert!(server
        .paths
        .lock()
        .unwrap()
        .iter()
        .all(|p| p != "/api/voice/join"));
}

#[cfg(windows)]
fn recovery_fixture(server: &MockHttp) -> TestWorker {
    let mut client = TestWorker::new();
    client.connection(server.url.clone());
    server.enable_encrypted_target(&mut client, Target::Channel("voice-room".into()));
    let ticket:VoiceTicket=serde_json::from_value(json!({
        "session_id":11,"key":"AA==","udp_port":4581,"wire_version":3,
        "resume":{"token":base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([3;32]),"instance_id":"old-instance"}
    })).unwrap();
    client.worker.remember_voice_admission(&ticket);
    client
}

#[cfg(windows)]
#[test]
fn voice_recovery_has_one_deadline_and_never_releases_keys_before_admission() {
    let server = MockHttp::new(|_| Reply::json(503, json!({"error":"unavailable"})));
    let mut client = recovery_fixture(&server);
    client.worker.pause_voice_for_outage();
    let deadline = client
        .worker
        .connection
        .as_ref()
        .unwrap()
        .voice_recovery
        .as_ref()
        .unwrap()
        .deadline;
    for _ in 0..3 {
        client.worker.pause_voice_for_outage();
        client.worker.refresh_encryption();
        assert!(client.worker.protected_call_keys("voice-room").is_err());
        assert_eq!(
            client
                .worker
                .connection
                .as_ref()
                .unwrap()
                .voice_recovery
                .as_ref()
                .unwrap()
                .deadline,
            deadline
        );
    }
    assert!(
        server.paths.lock().unwrap().is_empty(),
        "no keys or ordinary join before admission"
    );
    client
        .worker
        .connection
        .as_mut()
        .unwrap()
        .voice_recovery
        .as_mut()
        .unwrap()
        .deadline = Instant::now() - Duration::from_millis(1);
    assert!(client.worker.voice_recovery_expired());
    client.worker.pause_voice_for_outage();
    let c = client.worker.connection.as_ref().unwrap();
    assert!(c.voice_channel.is_none() && c.voice_admission.is_none() && c.voice_recovery.is_none());
    assert!(client
        .drain()
        .iter()
        .any(|u| matches!(u, Update::VoiceLeft)));
}

#[cfg(windows)]
#[test]
fn voice_recovery_does_not_repair_same_instance_removal_or_fall_back_after_denial() {
    let server = MockHttp::new(|_| Reply::json(403, json!({"error":"recovery revoked"})));
    let mut client = recovery_fixture(&server);
    let mut bootstrap = Bootstrap {
        instance_id: "old-instance".into(),
        ..Default::default()
    };
    assert!(client.worker.recover_voice(&mut bootstrap));
    assert!(client
        .worker
        .connection
        .as_ref()
        .unwrap()
        .voice_channel
        .is_none());
    assert!(server.paths.lock().unwrap().is_empty());
    let mut client = recovery_fixture(&server);
    client
        .worker
        .connection
        .as_mut()
        .unwrap()
        .chat
        .update_bootstrap(&Bootstrap {
            capabilities: vec!["voice-restart-resume-v1".into(), "e2ee_mls_v1".into()],
            ..Default::default()
        });
    bootstrap.instance_id = "new-instance".into();
    assert!(client.worker.recover_voice(&mut bootstrap));
    assert!(client
        .worker
        .connection
        .as_ref()
        .unwrap()
        .voice_channel
        .is_none());
    client.worker.refresh_encryption();
    assert_eq!(*server.paths.lock().unwrap(), ["/api/voice/resume"]);
}

#[cfg(windows)]
#[test]
fn repeated_server_replacements_yield_between_resume_attempts_without_extending_deadline() {
    let replies = Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new()));
    for i in 0..4 {
        replies.lock().unwrap().push_back(json!({
            "session_id": 11, "key": "AA==", "udp_port": 4581, "wire_version": 3,
            "resume": {
                "token": base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([3; 32]),
                "instance_id": format!("restart-{i}"),
            },
        }));
        let b = Bootstrap {
            instance_id: format!("restart-{}", i + 1),
            ..Default::default()
        };
        replies
            .lock()
            .unwrap()
            .push_back(serde_json::to_value(b).unwrap());
    }
    let queued = replies.clone();
    let server = MockHttp::new(move |_| {
        queued
            .lock()
            .unwrap()
            .pop_front()
            .map(|body| Reply::json(200, body))
            .unwrap_or_else(|| Reply::json(503, json!({"error": "no scripted reply"})))
    });
    let mut client = recovery_fixture(&server);
    client
        .worker
        .connection
        .as_mut()
        .unwrap()
        .chat
        .update_bootstrap(&Bootstrap {
            capabilities: vec!["voice-restart-resume-v1".into(), "e2ee_mls_v1".into()],
            ..Default::default()
        });
    client.worker.pause_voice_for_outage();
    let deadline = client
        .worker
        .connection
        .as_ref()
        .unwrap()
        .voice_recovery
        .as_ref()
        .unwrap()
        .deadline;
    let mut bootstrap = Bootstrap {
        instance_id: "restart-0".into(),
        ..Default::default()
    };
    for i in 0..4 {
        assert!(!client.worker.recover_voice(&mut bootstrap));
        assert_eq!(
            server.paths.lock().unwrap().len(),
            (i + 1) * 2,
            "one resume and one actual-presence fetch per refresh, never recursive retries"
        );
        let c = client.worker.connection.as_ref().unwrap();
        let recovery = c.voice_recovery.as_ref().unwrap();
        assert_eq!(recovery.deadline, deadline);
        assert!(!recovery.admitted);
        assert_eq!(bootstrap.instance_id, format!("restart-{}", i + 1));
        assert!(client.worker.protected_call_keys("voice-room").is_err());
    }
    assert!(replies.lock().unwrap().is_empty());
    client
        .worker
        .connection
        .as_mut()
        .unwrap()
        .voice_recovery
        .as_mut()
        .unwrap()
        .deadline = Instant::now() - Duration::from_millis(1);
    assert!(client.worker.recover_voice(&mut bootstrap));
    assert!(client
        .worker
        .connection
        .as_ref()
        .unwrap()
        .voice_channel
        .is_none());
    assert_eq!(
        server.paths.lock().unwrap().len(),
        8,
        "expiry makes no further network request"
    );
}

#[cfg(windows)]
#[test]
fn established_call_recovery_failure_retains_account_room_and_attempt_identity() {
    let server = MockHttp::new(|_| Reply::json(403, json!({"error":"access revoked"})));
    for expires in [false, true] {
        let mut client = recovery_fixture(&server);
        let c = client.worker.connection.as_mut().unwrap();
        c.voice_attempt = Some(93);
        let expected_origin = CommandOrigin::from(&*c);
        if expires {
            client.worker.pause_voice_for_outage();
            client
                .worker
                .connection
                .as_mut()
                .unwrap()
                .voice_recovery
                .as_mut()
                .unwrap()
                .deadline = Instant::now() - Duration::from_millis(1);
            assert!(client.worker.voice_recovery_expired());
        } else {
            assert!(client.worker.recover_voice(&mut Bootstrap {
                instance_id: "old-instance".into(),
                ..Default::default()
            }));
        }
        let updates = client.drain();
        let failures: Vec<_> = updates
            .iter()
            .filter_map(|update| match update {
                Update::VoiceCallFailed {
                    origin,
                    channel_id,
                    request_id,
                    error,
                } => Some((origin, channel_id, request_id, error)),
                _ => None,
            })
            .collect();
        assert_eq!(failures.len(), 1, "one correlated terminal event");
        let (origin, channel, request, error) = failures[0];
        assert_eq!(origin, &expected_origin);
        assert_eq!(channel, "voice-room");
        assert_eq!(*request, 93);
        assert!(!error.is_empty());
        assert!(
            !updates.iter().any(|u| matches!(
                u,
                Update::VoiceLeft
                    | Update::Error(_)
                    | Update::VoiceJoinFailed { .. }
                    | Update::VoiceJoined { .. }
            )),
            "no unscoped terminal event may cancel a newer call"
        );
        let c = client.worker.connection.as_ref().unwrap();
        assert!(
            c.voice_channel.is_none()
                && c.voice_attempt.is_none()
                && c.voice_admission.is_none()
                && c.voice_recovery.is_none()
                && c.crypto_pending_join.is_none()
                && c.crypto_call_epoch.is_none()
        );
        assert!(client.worker.live.snapshot().is_none());
    }
    assert!(
        server.paths.lock().unwrap().is_empty(),
        "terminal failure must not create a fallback join"
    );
}

#[cfg(windows)]
#[test]
fn pending_private_call_ends_with_its_request_identity_after_revocation_or_expiry() {
    let server = MockHttp::new(|_| Reply::json(403, json!({"error":"access revoked"})));
    for expires in [false, true] {
        let mut client = recovery_fixture(&server);
        let request_id = 91;
        client
            .worker
            .connection
            .as_mut()
            .unwrap()
            .crypto_pending_join = Some(("voice-room".into(), request_id));
        if expires {
            client.worker.pause_voice_for_outage();
            client.drain();
            client
                .worker
                .connection
                .as_mut()
                .unwrap()
                .voice_recovery
                .as_mut()
                .unwrap()
                .deadline = Instant::now() - Duration::from_millis(1);
            assert!(client.worker.voice_recovery_expired());
        } else {
            let mut bootstrap = Bootstrap {
                instance_id: "old-instance".into(),
                ..Default::default()
            };
            assert!(client.worker.recover_voice(&mut bootstrap));
        }
        let updates = client.drain();
        let mut pending = Some("voice-room".to_owned());
        let mut error = None;
        for update in &updates {
            if let Update::VoiceJoinFailed {
                channel_id,
                request_id: ack,
                error: message,
            } = update
            {
                if crate::voice_ui_state::accepts_join(
                    pending.as_deref(),
                    request_id,
                    channel_id,
                    *ack,
                ) {
                    pending = None;
                    error = Some(message.as_str());
                }
                assert!(
                    !crate::voice_ui_state::accepts_join(
                        Some("voice-room"),
                        request_id + 1,
                        channel_id,
                        *ack
                    ),
                    "the terminal result cannot cancel a newer same-room attempt"
                );
            }
        }
        assert!(
            pending.is_none(),
            "terminal event must clear the client's connecting intent: {expires}"
        );
        assert!(error.is_some_and(|message| !message.is_empty()));
        assert!(
            !updates.iter().any(|update| matches!(
                update,
                Update::VoiceLeft | Update::Error(_) | Update::VoiceJoined { .. }
            )),
            "pending-call termination uses only its request-scoped failure"
        );
        let c = client.worker.connection.as_ref().unwrap();
        assert!(
            c.voice_channel.is_none() && c.voice_admission.is_none() && c.voice_recovery.is_none()
        );
        assert!(c.crypto_pending_join.is_none() && c.crypto_call_epoch.is_none());
        assert!(!c.sharing && !c.camera && c.frame_ids.is_empty());
        assert!(client.worker.live.snapshot().is_none());
        client.worker.refresh_encryption();
        assert!(!client.drain().iter().any(|update| matches!(
            update,
            Update::VoiceJoined { .. } | Update::VoiceEncryptionChanged { .. }
        )));
    }
    assert!(
        server.paths.lock().unwrap().is_empty(),
        "revocation/expiry cannot issue a fallback join or key request"
    );
}

#[cfg(windows)]
#[test]
fn legacy_worker_start_failure_cannot_activate_plaintext_fallback_during_protected_join() {
    let server = MockHttp::new(|path| match path {
        "/api/voice/join" => Reply::json(
            200,
            json!({"session_id":1,"key":"AA==","udp_port":4581,"wire_version":3}),
        ),
        "/api/voice/leave" => Reply {
            status: 204,
            headers: Vec::new(),
            body: Vec::new(),
        },
        "/api/bootstrap" => Reply::json(200, serde_json::to_value(Bootstrap::default()).unwrap()),
        _ => Reply::json(404, json!({"error":"missing"})),
    });
    let mut client = TestWorker::new();
    server.call_refresh_ack.store(true, Ordering::Release);
    client.connection(server.url.clone());
    server.enable_encrypted_target(&mut client, Target::Channel("voice-room".into()));
    client.worker.connection.as_mut().unwrap().voice_channel = None;
    let (_frames, receiver) = mpsc::sync_channel(2);
    client.worker.live_frames = Some(Arc::new(std::sync::Mutex::new(receiver)));
    client
        .worker
        .live
        .fail_next_start
        .store(true, Ordering::SeqCst);
    let updates = client.handle(Command::JoinVoiceRequest {
        channel_id: "voice-room".into(),
        request_id: 93,
    });
    assert!(updates
        .iter()
        .any(|u| matches!(u, Update::VoiceJoined { request_id: 93, .. })));
    assert!(!updates
        .iter()
        .any(|u| matches!(u, Update::VoiceJoinFailed { .. })));
    assert!(
        client.worker.live.fail_next_start.load(Ordering::SeqCst),
        "the legacy startup path was never called"
    );
    assert!(client.worker.live.http.get().is_none());
    assert!(client.worker.live_frames.is_some());
    assert!(server
        .paths
        .lock()
        .unwrap()
        .iter()
        .all(|p| !p.starts_with("/api/media")));
}

#[test]
fn attachment_preview_fetch_is_authenticated_bounded_and_keeps_control_worker_free() {
    let fixture = AttachmentFixture::new();
    let mut png = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(2, 1)
        .write_to(&mut png, image::ImageFormat::Png)
        .unwrap();
    let (wire, preview, metadata) = encrypted_attachment(&png.into_inner(), "preview-1");
    let (_, oversized_attachment, oversized_metadata) = encrypted_attachment(&[0; 64], "oversized");
    let server = MockHttp::new(move |path| match path {
        "/api/uploads/preview-1" => {
            thread::sleep(Duration::from_millis(80));
            Reply {
                status: 200,
                headers: Vec::new(),
                body: wire.clone(),
            }
        }
        "/api/uploads/oversized" => Reply {
            status: 200,
            headers: Vec::new(),
            body: vec![0; chat_media::MAX_COMPRESSED_BYTES + 1],
        },
        _ => Reply::json(404, json!({"error":"missing"})),
    });
    let mut client = TestWorker::new();
    client.connection(server.url.clone());
    let mut crypto = fixture.session();
    crypto.add_file(metadata).unwrap();
    install_file_crypto(&mut client, crypto);
    assert!(client.worker.chat_media_workers.is_none());
    let started = Instant::now();
    client.worker.command(Command::FetchChatMedia {
        original: false,
        origin: client.command_origin(),
        attachment: preview,
    });
    assert!(client.worker.chat_media_workers.is_some());
    assert!(
        client.worker.live.http.get().is_none(),
        "GIFs create no live runtime"
    );
    assert!(started.elapsed() < Duration::from_millis(60));
    assert!(
        client.drain().is_empty(),
        "preview does not mark all chat controls busy"
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut fetched = false;
    while Instant::now() < deadline {
        client.worker.poll_chat_media_results();
        for update in client.drain() {
            if let Update::ChatMediaFetched {
                attachment_id,
                media,
            } = update
            {
                assert_eq!(attachment_id, "preview-1");
                assert_eq!(media.frames[0].image.size, [2, 1]);
                fetched = true;
            }
        }
        if fetched {
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }
    assert!(fetched);
    assert_eq!(
        server.requests.lock().unwrap()[0].authorization.as_deref(),
        Some("Bearer fixture-token")
    );
    let job = ChatMediaJob {
        _activity: None,
        original: true,
        avatar: false,
        attachment: oversized_attachment,
        url: server.url.clone(),
        token: "fixture-token".into(),
        epoch: 0,
        encrypted: Some(oversized_metadata),
        plain: false,
    };
    assert!(fetch_chat_media(&server.http, &job)
        .err()
        .unwrap()
        .contains("limit"));
    let count = server.paths.lock().unwrap().len();
    for address in [
        "https://other.invalid/file.gif",
        "/api/bootstrap",
        "/api/uploads/../preview-1",
        "//other.invalid/file.gif",
    ] {
        let mut attachment = preview_attachment("preview-1");
        attachment.url = address.into();
        assert!(fetch_chat_media(
            &server.http,
            &ChatMediaJob {
                _activity: None,
                original: true,
                avatar: false,
                attachment,
                url: server.url.clone(),
                token: "fixture-token".into(),
                epoch: 0,
                encrypted: None,
                plain: false,
            }
        )
        .is_err());
    }
    assert_eq!(
        server.paths.lock().unwrap().len(),
        count,
        "invalid URLs never receive a request or bearer"
    );
    #[cfg(windows)]
    client
        .worker
        .connection
        .as_ref()
        .unwrap()
        .crypto
        .as_ref()
        .unwrap()
        .remove_test_checkpoint();
    client.handle(Command::Logout);
    assert!(
        client.worker.chat_media_workers.is_none(),
        "logout releases preview queues/workers rather than keeping the pool idle"
    );
    assert!(client.worker.live.http.get().is_none());
}

#[test]
fn old_session_preview_results_are_discarded_before_delivery() {
    let fixture = AttachmentFixture::new();
    let mut png = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(1, 1)
        .write_to(&mut png, image::ImageFormat::Png)
        .unwrap();
    let (wire, attachment, metadata) = encrypted_attachment(&png.into_inner(), "preview-1");
    let server = MockHttp::new(move |_| {
        thread::sleep(Duration::from_millis(100));
        Reply {
            status: 200,
            headers: Vec::new(),
            body: wire.clone(),
        }
    });
    let mut client = TestWorker::new();
    client.connection(server.url.clone());
    let mut crypto = fixture.session();
    crypto.add_file(metadata).unwrap();
    install_file_crypto(&mut client, crypto);
    client
        .worker
        .queue_chat_media(&client.command_origin(), attachment)
        .unwrap();
    client
        .worker
        .chat_media_epoch
        .fetch_add(1, Ordering::SeqCst);
    thread::sleep(Duration::from_millis(150));
    client.worker.poll_chat_media_results();
    assert!(
        client.drain().is_empty(),
        "a prior session's pixels never enter the UI cache"
    );
}

#[test]
fn encrypted_gif_preview_authenticates_before_decode_and_public_avatars_still_work() {
    let mut gif = Vec::new();
    {
        let mut encoder = image::codecs::gif::GifEncoder::new(&mut gif);
        for (color, delay) in [([255, 0, 0, 255], 50), ([0, 0, 255, 255], 120)] {
            encoder
                .encode_frame(image::Frame::from_parts(
                    image::RgbaImage::from_pixel(2, 1, image::Rgba(color)),
                    0,
                    0,
                    image::Delay::from_numer_denom_ms(delay, 1),
                ))
                .unwrap();
        }
    }
    let (wire, attachment, metadata) = encrypted_attachment(&gif, "animated-1");
    assert!(!wire.windows(6).any(|part| part == b"GIF89a"));
    let body = Arc::new(std::sync::Mutex::new(wire.clone()));
    let server_body = body.clone();
    let mut png = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(2, 2)
        .write_to(&mut png, image::ImageFormat::Png)
        .unwrap();
    let png = png.into_inner();
    let server = MockHttp::new(move |path| Reply {
        status: 200,
        headers: Vec::new(),
        body: if path.contains("/avatar?") {
            png.clone()
        } else {
            server_body.lock().unwrap().clone()
        },
    });
    let mut job = ChatMediaJob {
        _activity: None,
        original: true,
        avatar: false,
        attachment,
        encrypted: Some(metadata),
        plain: false,
        url: server.url.clone(),
        token: "fixture-token".into(),
        epoch: 0,
    };
    let decoded = fetch_chat_media(&server.http, &job).unwrap();
    assert_eq!(decoded.frames.len(), 2);
    assert_eq!(
        decoded
            .frames
            .iter()
            .map(|frame| frame.delay.as_millis())
            .collect::<Vec<_>>(),
        vec![50, 120]
    );
    assert_ne!(
        decoded.frames[0].image.pixels,
        decoded.frames[1].image.pixels
    );

    body.lock().unwrap()[12] ^= 1;
    assert!(
        fetch_chat_media(&server.http, &job).is_err(),
        "modified ciphertext cannot become pixels"
    );
    *body.lock().unwrap() = wire.clone();
    let (_, _, wrong) = encrypted_attachment(&gif, "animated-1");
    job.encrypted = Some(wrong);
    assert!(
        fetch_chat_media(&server.http, &job).is_err(),
        "a different key cannot become pixels"
    );
    job.encrypted = None;
    let count = server.requests.lock().unwrap().len();
    assert!(fetch_chat_media(&server.http, &job)
        .err()
        .unwrap()
        .contains("Plaintext fallback"));
    assert_eq!(
        server.requests.lock().unwrap().len(),
        count,
        "missing keys are rejected before authenticated fetch"
    );

    let avatar = ChatMediaJob {
        _activity: None,
        original: true,
        avatar: true,
        attachment: Attachment {
            id: "avatar:person-id:7".into(),
            name: "avatar.png".into(),
            mime: "image/png".into(),
            size: 0,
            url: "/api/profiles/person-id/avatar?v=7".into(),
        },
        url: server.url.clone(),
        token: "fixture-token".into(),
        epoch: 0,
        encrypted: None,
        plain: false,
    };
    assert_eq!(
        fetch_chat_media(&server.http, &avatar).unwrap().frames[0]
            .image
            .size,
        [2, 2]
    );
    assert!(server
        .requests
        .lock()
        .unwrap()
        .iter()
        .all(|request| request.authorization.as_deref() == Some("Bearer fixture-token")));
}

#[cfg(windows)]
#[test]
fn failed_voice_join_identifies_its_room_without_a_generic_async_error() {
    let server = MockHttp::new(|_| Reply::json(403, json!({"error":"join denied"})));
    let mut client = TestWorker::new();
    client.connection(server.url.clone());
    server.enable_encrypted_target(&mut client, Target::Channel("requested-room".into()));
    client.worker.command(Command::JoinVoiceRequest {
        channel_id: "requested-room".into(),
        request_id: 0,
    });
    let updates = client.drain();
    let failure = updates.iter().find_map(|update| match update {
        Update::VoiceJoinFailed {
            channel_id,
            error,
            request_id,
        } => Some((channel_id.as_str(), error.as_str(), *request_id)),
        _ => None,
    });
    assert_eq!(failure, Some(("requested-room", "join denied", 0)));
    assert!(!updates
        .iter()
        .any(|u| matches!(u, Update::VoiceJoined { .. } | Update::Error(_))));
}

#[cfg(windows)]
#[test]
fn same_room_rejoin_acks_and_failures_preserve_each_http_request_identity() {
    let joins = Arc::new(AtomicU64::new(0));
    let request_joins = joins.clone();
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let release_rx = std::sync::Mutex::new(release_rx);
    let server = Arc::new(MockHttp::new(move |path| match path {
        "/api/voice/join" => {
            let index = request_joins.fetch_add(1, Ordering::SeqCst);
            if index == 0 {
                entered_tx.send(()).unwrap();
                // Main test queues cancellation/rejoin before releasing this
                // exact HTTP request. Channel closure also releases cleanup.
                let _ = release_rx
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(5));
            }
            if index == 2 {
                Reply::json(403, json!({"error":"new join denied"}))
            } else {
                Reply::json(
                    200,
                    json!({"session_id":index+1,"key":"AA==","udp_port":4581}),
                )
            }
        }
        "/api/voice/leave" => Reply {
            status: 204,
            headers: Vec::new(),
            body: Vec::new(),
        },
        "/api/bootstrap" => Reply::json(200, serde_json::to_value(Bootstrap::default()).unwrap()),
        _ => Reply::json(404, json!({"error":"missing"})),
    }));
    let (commands, command_rx) = mpsc::channel();
    let (results, result_rx) = mpsc::channel();
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let worker_server = server.clone();
    let worker = thread::spawn(move || {
        // Protected device actors retain a thread-bound ownership lease.
        worker_server
            .call_refresh_ack
            .store(true, Ordering::Release);
        let mut client = TestWorker::new();
        client.connection(worker_server.url.clone());
        worker_server.enable_encrypted_target(&mut client, Target::Channel("same-room".into()));
        ready_tx.send(()).unwrap();
        for _ in 0..4 {
            let Ok(command) = command_rx.recv() else {
                break;
            };
            client.worker.command(command);
            let updates = client.drain();
            for update in &updates {
                if let Update::VoiceJoined { request_id, .. } = update {
                    assert_eq!(
                        client.worker.connection.as_ref().unwrap().voice_attempt,
                        Some(*request_id)
                    );
                }
                if matches!(update, Update::VoiceLeft | Update::VoiceJoinFailed { .. }) {
                    assert!(client
                        .worker
                        .connection
                        .as_ref()
                        .unwrap()
                        .voice_attempt
                        .is_none());
                }
            }
            if results.send(updates).is_err() {
                break;
            }
        }
    });
    ready_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("owned encrypted worker fixture setup completed");
    commands
        .send(Command::JoinVoiceRequest {
            channel_id: "same-room".into(),
            request_id: 11,
        })
        .unwrap();
    entered_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("original voice join reached the owned HTTP handler");
    // Cancel and rejoin while the old HTTP request is deliberately held open.
    commands.send(Command::LeaveVoice).unwrap();
    commands
        .send(Command::JoinVoiceRequest {
            channel_id: "same-room".into(),
            request_id: 22,
        })
        .unwrap();
    commands
        .send(Command::JoinVoiceRequest {
            channel_id: "same-room".into(),
            request_id: 33,
        })
        .unwrap();
    release_tx.send(()).unwrap();
    let mut acknowledgements = Vec::new();
    let mut failure = None;
    for _ in 0..4 {
        for update in result_rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            match update {
                Update::VoiceJoined {
                    channel_id,
                    request_id,
                    ..
                } => {
                    assert_eq!(channel_id, "same-room");
                    acknowledgements.push(request_id);
                }
                Update::VoiceJoinFailed {
                    channel_id,
                    request_id,
                    error,
                } => {
                    assert_eq!(channel_id, "same-room");
                    failure = Some((request_id, error));
                }
                Update::Error(error) => {
                    panic!("join failures must be tagged rather than generic: {error}")
                }
                _ => {}
            }
        }
    }
    worker.join().unwrap();
    assert_eq!(acknowledgements, [11, 22]);
    assert_eq!(failure, Some((33, "new join denied".into())));
    for request in server
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r.path == "/api/voice/join")
    {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(body["channel_id"], "same-room");
        assert!(body["device_id"].as_str().is_some_and(|id| !id.is_empty()));
    }
}

#[test]
fn live_stream_rotation_serves_every_stream_under_the_decoded_batch_memory_cap() {
    let rgb = image::RgbImage::from_pixel(2048, 1024, image::Rgb([2, 4, 6]));
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new(&mut jpeg)
        .encode_image(&rgb)
        .unwrap();
    let mut list = Vec::new();
    for index in 0..5 {
        list.push(json!({"user":{"id":format!("user-{index}"),"username":"stream","role":"member"},
            "kind":"screen","frame_id":format!("frame-{index}"),"url":"/api/unused","width":2048,"height":1024}));
    }
    let server = MockHttp::new(move |path| {
        if path.starts_with("/api/media/frames?") {
            Reply::json(200, json!(list))
        } else if path.starts_with("/api/media/live/") {
            Reply {
                status: 200,
                headers: Vec::new(),
                body: jpeg.clone(),
            }
        } else {
            Reply::json(404, json!({"error":"missing"}))
        }
    });
    let mut client = TestWorker::new();
    client.connection(server.url.clone());
    client.worker.sync_live_transport().unwrap();
    let context = client.worker.live.snapshot().unwrap();
    let mut observed = std::collections::HashSet::new();
    for _ in 0..5 {
        client.worker.live.poll(&context, &mut HashMap::new());
        for update in client.drain() {
            if let Update::DecodedFrames { frames, .. } = update {
                assert!(
                    frames.len() <= 2,
                    "full-size RGBA frames obey the16MiB batch cap"
                );
                for (frame, _) in frames {
                    observed.insert(frame.user.id);
                }
            }
        }
        client.worker.live.pending.store(false, Ordering::SeqCst);
    }
    assert_eq!(
        observed.len(),
        5,
        "the first streams must not permanently starve later streams"
    );
}

#[test]
fn live_preview_mailbox_is_single_batch_and_rejects_invalidated_room_generations() {
    let rgb = image::RgbImage::from_pixel(2, 1, image::Rgb([4, 5, 6]));
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new(&mut jpeg)
        .encode_image(&rgb)
        .unwrap();
    let server = MockHttp::new(move |path| match path {
        "/api/media/frames?channel_id=voice-room" => Reply::json(
            200,
            json!([{
                "user":{"id":"stream-user","username":"stream","role":"member"},"kind":"screen","frame_id":"current-1",
                "url":"/api/media/frames/old-id","width":2,"height":1
            }]),
        ),
        "/api/media/live/stream-user/screen" => Reply {
            status: 200,
            headers: Vec::new(),
            body: jpeg.clone(),
        },
        _ => Reply::json(404, json!({"error":"missing"})),
    });
    let mut client = TestWorker::new();
    client.connection(server.url.clone());
    client.worker.sync_live_transport().unwrap();
    let context = client.worker.live.snapshot().unwrap();
    let mut ids = HashMap::new();
    client.worker.live.poll(&context, &mut ids);
    assert!(
        client.worker.live.pending.load(Ordering::SeqCst),
        "live batch was not queued; fixture requests: {:?}",
        server.paths.lock().unwrap()
    );
    client.worker.live.poll(&context, &mut HashMap::new());
    let updates = client.drain();
    assert_eq!(updates.len(), 1, "at most one decoded batch can be queued");
    assert!(
        matches!(&updates[0], Update::DecodedFrames { channel_id, frames, .. } if channel_id=="voice-room" && frames.len()==1 && frames[0].1.size==[2,1])
    );
    client.worker.live.pending.store(false, Ordering::SeqCst);
    client.worker.live.epoch.fetch_add(1, Ordering::SeqCst);
    client.worker.live.poll(&context, &mut HashMap::new());
    assert!(
        client.drain().is_empty(),
        "old room/session batch is rejected after invalidation"
    );
    assert!(!server
        .paths
        .lock()
        .unwrap()
        .iter()
        .any(|path| path == "/api/media/frames/old-id"));
}

#[test]
fn stop_capture_waits_for_inflight_publication_before_deleting_the_frame() {
    let server = MockHttp::new(|path| {
        if path.starts_with("/api/media/frame?") {
            thread::sleep(Duration::from_millis(100));
        }
        Reply {
            status: 204,
            headers: Vec::new(),
            body: Vec::new(),
        }
    });
    let mut client = TestWorker::new();
    client.connection(server.url.clone());
    client.worker.connection.as_mut().unwrap().sharing = true;
    client.worker.capture_epochs[0].store(1, Ordering::SeqCst);
    client.worker.sync_live_transport().unwrap();
    let rgb = image::RgbImage::from_pixel(1, 1, image::Rgb([4, 5, 6]));
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new(&mut jpeg)
        .encode_image(&rgb)
        .unwrap();
    let live = client.worker.live.clone();
    let pending = thread::spawn(move || {
        live.publish(CaptureFrame {
            generation: 1,
            kind: "screen".into(),
            jpeg,
            width: 1,
            height: 1,
        })
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    while server.paths.lock().unwrap().is_empty() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert!(!server.paths.lock().unwrap().is_empty());
    client.worker.capture_epochs[0].store(2, Ordering::SeqCst);
    client
        .worker
        .handle(Command::StopCapture("screen".into()))
        .unwrap();
    pending.join().unwrap();
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].method, "POST");
    assert_eq!(requests[1].method, "DELETE");
    assert!(!client.worker.live.snapshot().unwrap().sharing);
}

fn fixture_message(content: &str) -> Value {
    serde_json::to_value(Message {
        id: "message-1".into(),
        channel_id: Some("sent-room".into()),
        author: User {
            id: "fixture-user".into(),
            username: "fixture".into(),
            role: "member".into(),
            ..Default::default()
        },
        content: content.into(),
        created_at: "2026-10-03T00:00:00Z".into(),
        ..Default::default()
    })
    .unwrap()
}

fn rich_ack(request: &RecordedRequest) -> Reply {
    let sent: SendMessage = serde_json::from_slice(&request.body).unwrap();
    Reply::json(
        200,
        serde_json::to_value(Message {
            id: format!("ack-{}", sent.client_id.as_deref().unwrap()),
            sequence: 42,
            author: User {
                id: "fixture-user".into(),
                ..Default::default()
            },
            channel_id: sent.channel_id,
            recipient_id: sent.recipient_id,
            content: sent.content,
            client_id: sent.client_id,
            reply_to_id: sent.reply_to_id,
            mentions: sent.mentions,
            attachments: sent
                .attachment_ids
                .into_iter()
                .map(|id| Attachment {
                    url: format!("/api/uploads/{id}"),
                    id,
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        })
        .unwrap(),
    )
}

#[cfg(windows)]
#[test]
fn queued_private_chat_actions_reject_switched_identity_before_http_or_ratchet_changes() {
    let server = MockHttp::new(|_| panic!("stale private action reached HTTP"));
    let mut client = TestWorker::new();
    client.connection(server.url.clone());
    let target = Target::Channel("sent-room".into());
    server.enable_encrypted_target(&mut client, target.clone());
    let origin = client.command_origin();
    let before = client
        .worker
        .connection
        .as_ref()
        .unwrap()
        .crypto
        .as_ref()
        .unwrap()
        .device
        .checkpoint()
        .unwrap()
        .to_vec();
    for field in ["url", "server", "user"] {
        let mut stale = origin.clone();
        match field {
            "url" => stale.url.push_str("/previous-community"),
            "server" => stale.server_id.push_str("-previous-server"),
            _ => stale.user_id.push_str("-previous-account"),
        }
        let commands = [
            Command::SendRich {
                binding: None,
                origin: stale.clone(),
                target: target.clone(),
                content: "private queued draft must not cross accounts".into(),
                attachment_ids: Vec::new(),
                client_id: operation_id(),
                reply_to_id: None,
                mentions: Vec::new(),
            },
            Command::EditMessage {
                origin: stale.clone(),
                message_id: "old-message".into(),
                content: "private queued edit must not cross accounts".into(),
            },
            Command::React {
                origin: stale.clone(),
                target: target.clone(),
                message_id: "old-message".into(),
                target_client_id: Some("old-client-id".into()),
                emoji: "👍".into(),
                active: true,
            },
            Command::CommunitySearch {
                origin: stale,
                query: "private queued search".into(),
                filters: Default::default(),
                remaining: None,
                request_id: 912,
            },
        ];
        for command in commands {
            assert!(client.worker.handle(command).is_err());
        }
    }
    assert!(server.requests.lock().unwrap().is_empty());
    assert_eq!(
        client
            .worker
            .connection
            .as_ref()
            .unwrap()
            .crypto
            .as_ref()
            .unwrap()
            .device
            .checkpoint()
            .unwrap()
            .as_slice(),
        before
    );
}

#[cfg(windows)]
#[test]
fn forged_send_acknowledgements_never_commit_or_clear_the_exact_pending_ciphertext() {
    for field in [
        "author",
        "content",
        "client_id",
        "route",
        "attachments",
        "reply",
        "id",
        "sequence",
    ] {
        let server = MockHttp::with_request(move |request| {
            assert_eq!(request.path, "/api/messages");
            let mut ack: Message = serde_json::from_slice(&rich_ack(request).body).unwrap();
            match field {
                "author" => ack.author.id = "other-user".into(),
                "content" => ack.content = "injected plaintext".into(),
                "client_id" => ack.client_id = Some("other-client-id".into()),
                "route" => ack.channel_id = Some("other-channel".into()),
                "attachments" => ack.attachments.push(Attachment {
                    id: "injected-file".into(),
                    ..Default::default()
                }),
                "reply" => ack.reply_to_id = Some("injected-reply".into()),
                "id" => ack.id.clear(),
                _ => ack.sequence = 0,
            }
            Reply::json(200, serde_json::to_value(ack).unwrap())
        });
        let mut client = TestWorker::new();
        client.connection(server.url.clone());
        let target = Target::Channel("sent-room".into());
        server.enable_encrypted_target(&mut client, target.clone());
        let origin = client.command_origin();
        let client_id = operation_id();
        let attempt = || Command::SendRich {
            binding: None,
            origin: origin.clone(),
            target: target.clone(),
            content: "protected draft preserved after forged acknowledgement".into(),
            attachment_ids: Vec::new(),
            client_id: client_id.clone(),
            reply_to_id: None,
            mentions: Vec::new(),
        };
        assert!(client.worker.handle(attempt()).is_err(), "{field}");
        assert!(!client
            .drain()
            .iter()
            .any(|u| matches!(u, Update::SentRich { .. })));
        assert!(client.worker.handle(attempt()).is_err(), "{field}");
        let requests = server.message_requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[0], requests[1],
            "{field}: uncertain response retains exact wire and ID"
        );
        let sent: SendMessage = serde_json::from_slice(&requests[0]).unwrap();
        assert!(sent.content.starts_with(crate::e2ee::MESSAGE_PREFIX));
        assert!(!sent.content.contains("protected draft"));
    }
}

#[cfg(windows)]
fn staged_control_worker(
    server: &MockHttp,
    community: &str,
    welcome: bool,
) -> (TestWorker, String, PathBuf) {
    let root = test_work_root().join(format!("control-ack-{}", uuid::Uuid::new_v4()));
    let mut crypto =
        crate::e2ee::CryptoSession::open_for_test(root.clone(), community, "fixture-user").unwrap();
    let scope = format!("{community}/channel/pending-room");
    let add = if welcome {
        let mut peer = relay_crypto::Device::new(relay_crypto::Identity {
            server_id: community.into(),
            user_id: "owned-peer".into(),
            device_id: uuid::Uuid::new_v4().to_string(),
        })
        .unwrap();
        let public = peer.key_package().unwrap();
        crypto
            .with_device(|device| device.approve(&public, &public.fingerprint))
            .unwrap();
        vec![public]
    } else {
        Vec::new()
    };
    crypto
        .stage_membership(
            &scope,
            CryptoTarget {
                channel_id: Some("pending-room".into()),
                peer_id: None,
            },
            uuid::Uuid::new_v4().to_string(),
            0,
            &add,
            &[],
            true,
        )
        .unwrap();
    let mut client = TestWorker::new();
    client.connection(server.url.clone());
    let connection = client.worker.connection.as_mut().unwrap();
    connection.server_id = community.into();
    connection.crypto = Some(crypto);
    (client, scope, root)
}

#[cfg(windows)]
fn control_ack(request: &RecordedRequest, community: &str) -> CryptoContext {
    assert_eq!(
        (request.method.as_str(), request.path.as_str()),
        ("POST", "/api/e2ee/events")
    );
    let batch: AppendCryptoEvents = serde_json::from_slice(&request.body).unwrap();
    CryptoContext {
        server_id: community.into(),
        target: batch.target.clone(),
        group_id: Some(batch.group_id.clone()),
        sequence: batch.expected_sequence + batch.events.len() as u64,
        events: batch
            .events
            .iter()
            .enumerate()
            .map(|(n, event)| CryptoEvent {
                sequence: batch.expected_sequence + n as u64 + 1,
                group_id: batch.group_id.clone(),
                epoch: batch.epoch,
                kind: event.kind.clone(),
                author_user_id: "fixture-user".into(),
                author_device_id: batch.device_id.clone(),
                recipient_device_id: event.recipient_device_id.clone(),
                body: event.body.clone(),
                operation_id: batch.operation_id.clone(),
            })
            .collect(),
        ..Default::default()
    }
}

#[cfg(windows)]
#[test]
fn forged_control_acknowledgements_preserve_exact_durable_commit_and_cursor() {
    for field in [
        "body",
        "server",
        "target",
        "group",
        "author",
        "device",
        "event_group",
        "epoch",
        "kind",
        "recipient",
        "operation",
        "sequence",
        "missing",
        "duplicate",
        "head",
        "overflow",
        "other_operation_in_batch",
    ] {
        let community = uuid::Uuid::new_v4().to_string();
        let reply_community = community.clone();
        let server = MockHttp::with_request(move |request| {
            let mut ack = control_ack(request, &reply_community);
            match field {
                "body" => ack.events[1].body = "altered opaque commit".into(),
                "server" => ack.server_id = "other-community".into(),
                "target" => ack.target.channel_id = Some("other-room".into()),
                "group" => ack.group_id = Some("other-group".into()),
                "author" => ack.events[1].author_user_id = "other-user".into(),
                "device" => ack.events[1].author_device_id = "other-device".into(),
                "event_group" => ack.events[1].group_id = "other-group".into(),
                "epoch" => ack.events[1].epoch += 1,
                "kind" => ack.events[1].kind = "welcome".into(),
                "recipient" => ack.events[1].recipient_device_id = Some("other-device".into()),
                "operation" => ack.events[1].operation_id = "other-operation".into(),
                "sequence" => {
                    ack.events[1].sequence = 800;
                    ack.sequence = 800;
                }
                "missing" => {
                    ack.events.remove(1);
                }
                "duplicate" => ack.events.push(ack.events[1].clone()),
                "head" => ack.sequence = 1,
                "overflow" => ack.sequence = u64::MAX,
                _ => ack.events.insert(
                    1,
                    CryptoEvent {
                        operation_id: "foreign-operation".into(),
                        ..ack.events[1].clone()
                    },
                ),
            }
            Reply::json(200, serde_json::to_value(ack).unwrap())
        });
        let (mut client, scope, _) = staged_control_worker(&server, &community, false);
        let pending = serde_json::to_vec(
            &client
                .worker
                .crypto()
                .unwrap()
                .pending_control(&scope)
                .unwrap(),
        )
        .unwrap();
        let before = client
            .worker
            .crypto()
            .unwrap()
            .device
            .checkpoint()
            .unwrap()
            .to_vec();
        for _ in 0..2 {
            assert!(
                client.worker.flush_crypto_control(&scope).is_err(),
                "forged {field} must not acknowledge a key change"
            );
            let crypto = client.worker.crypto().unwrap();
            assert_eq!(
                crypto.cursor(&scope),
                0,
                "forged {field} advanced the durable cursor"
            );
            assert_eq!(
                serde_json::to_vec(&crypto.pending_control(&scope).unwrap()).unwrap(),
                pending
            );
            assert_eq!(crypto.device.checkpoint().unwrap().as_slice(), before);
        }
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[0].body, requests[1].body,
            "forged {field} altered the retry"
        );
    }
}

#[cfg(windows)]
#[test]
fn valid_control_ack_skips_only_its_hidden_welcome_and_never_a_later_operation() {
    let community = uuid::Uuid::new_v4().to_string();
    let reply_community = community.clone();
    let server = MockHttp::with_request(move |request| {
        let mut ack = control_ack(request, &reply_community);
        assert_eq!(ack.events.len(), 3, "real staged create/commit/Welcome");
        ack.events
            .retain(|event| event.recipient_device_id.is_none());
        ack.sequence += 1;
        ack.events.push(CryptoEvent {
            sequence: ack.sequence,
            group_id: ack.group_id.clone().unwrap(),
            epoch: 2,
            operation_id: "later-operation".into(),
            kind: "commit".into(),
            ..Default::default()
        });
        Reply::json(200, serde_json::to_value(ack).unwrap())
    });
    let (mut client, scope, _) = staged_control_worker(&server, &community, true);
    client.worker.flush_crypto_control(&scope).unwrap();
    let crypto = client.worker.crypto().unwrap();
    assert!(crypto.pending_control(&scope).is_none());
    assert_eq!(
        crypto.cursor(&scope),
        3,
        "the unseen later operation must still be fetched"
    );
}

#[cfg(windows)]
#[test]
fn rejected_control_ack_can_retry_exact_protected_commit_after_actor_reopen() {
    let community = uuid::Uuid::new_v4().to_string();
    let reply_community = community.clone();
    let attempts = Arc::new(AtomicU64::new(0));
    let reply_attempts = attempts.clone();
    let server = MockHttp::with_request(move |request| {
        let mut ack = control_ack(request, &reply_community);
        if reply_attempts.fetch_add(1, Ordering::Relaxed) == 0 {
            ack.events[1].body = "altered commit".into();
        }
        Reply::json(200, serde_json::to_value(ack).unwrap())
    });
    let (mut client, scope, root) = staged_control_worker(&server, &community, false);
    assert!(client.worker.flush_crypto_control(&scope).is_err());
    let pending = serde_json::to_vec(
        &client
            .worker
            .crypto()
            .unwrap()
            .pending_control(&scope)
            .unwrap(),
    )
    .unwrap();
    drop(client.worker.connection.as_mut().unwrap().crypto.take());
    let reopened =
        crate::e2ee::CryptoSession::open_for_test(root, &community, "fixture-user").unwrap();
    assert_eq!(
        serde_json::to_vec(&reopened.pending_control(&scope).unwrap()).unwrap(),
        pending
    );
    assert_eq!(reopened.cursor(&scope), 0);
    client.worker.connection.as_mut().unwrap().crypto = Some(reopened);
    client.worker.flush_crypto_control(&scope).unwrap();
    assert!(client
        .worker
        .crypto()
        .unwrap()
        .pending_control(&scope)
        .is_none());
    assert_eq!(client.worker.crypto().unwrap().cursor(&scope), 2);
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0].body, requests[1].body,
        "reopen must retain ciphertext, epoch, operation ID and sequence"
    );
}

#[cfg(windows)]
#[test]
fn pending_control_update_never_sends_to_a_changed_account_or_community() {
    for field in ["community", "account"] {
        let community = uuid::Uuid::new_v4().to_string();
        let server = MockHttp::new(|_| Reply::json(500, json!({"error":"No transfer is allowed"})));
        let (mut client, scope, _) = staged_control_worker(&server, &community, false);
        let pending = serde_json::to_vec(
            &client
                .worker
                .crypto()
                .unwrap()
                .pending_control(&scope)
                .unwrap(),
        )
        .unwrap();
        let connection = client.worker.connection.as_mut().unwrap();
        if field == "community" {
            connection.server_id = "changed-community".into();
        } else {
            connection.session.user.id = "changed-account".into();
        }
        assert!(client.worker.flush_crypto_control(&scope).is_err());
        assert!(
            server.requests.lock().unwrap().is_empty(),
            "{field} must be rejected before transfer"
        );
        assert_eq!(
            serde_json::to_vec(
                &client
                    .worker
                    .crypto()
                    .unwrap()
                    .pending_control(&scope)
                    .unwrap()
            )
            .unwrap(),
            pending
        );
        assert_eq!(client.worker.crypto().unwrap().cursor(&scope), 0);
    }
}

fn profile_worker(server: &MockHttp) -> TestWorker {
    let mut client = TestWorker::new();
    client.connection(server.url.clone());
    client.worker.connection.as_mut().unwrap().chat = extended::ChatSession::new(&Bootstrap {
        capabilities: vec!["profiles-v1".into()],
        ..Default::default()
    });
    client
}

#[test]
fn queued_profile_saves_reject_changed_community_server_and_account_before_any_transfer() {
    let original =
        MockHttp::new(|_| Reply::json(500, json!({"error":"Unexpected profile request"})));
    let replacement =
        MockHttp::new(|_| Reply::json(500, json!({"error":"Unexpected profile request"})));
    let avatar_path = test_work_root().join(format!("queued-profile-{}.png", operation_id()));
    fs::create_dir_all(avatar_path.parent().unwrap()).unwrap();
    fs::write(&avatar_path, b"private avatar from the original community").unwrap();

    for changed in ["community", "server", "account"] {
        let mut client = profile_worker(&original);
        let queued_origin = client.command_origin();
        let connection = client.worker.connection.as_mut().unwrap();
        match changed {
            "community" => connection.url = replacement.url.clone(),
            "server" => connection.server_id = "replacement-server".into(),
            "account" => {
                connection.session.user.id = "replacement-account".into();
                connection.session.token = "replacement-token".into();
            }
            _ => unreachable!(),
        }

        for (request_id, avatar) in [
            (31, ProfileAvatarChange::Keep),
            (32, ProfileAvatarChange::Upload(avatar_path.clone())),
        ] {
            client.worker.command(Command::SaveProfile {
                origin: queued_origin.clone(),
                display_name: "Original community name".into(),
                bio: "Private profile text from the original community".into(),
                appearance: None,
                avatar,
                request_id,
            });
            assert!(client.drain().iter().any(|update| {
                matches!(update, Update::RequestFailed {request_id: id, error}
                    if *id == request_id && error == "This request belongs to another account or community.")
            }), "{changed}: stale saves must fail synchronously with their request identity");
            // Avatar reads live exclusively in the spawned transfer. An empty
            // transfer registry and zero permits prove it was never started,
            // including for the existing, invalid avatar source above.
            assert!(client.worker.transfers.is_empty(), "{changed}");
            assert_eq!(
                client.worker.transfer_active.load(Ordering::SeqCst),
                0,
                "{changed}"
            );
            assert!(
                client.worker.transfer_results.try_recv().is_err(),
                "{changed}"
            );
        }
    }
    assert!(original.requests.lock().unwrap().is_empty());
    assert!(replacement.requests.lock().unwrap().is_empty());
    fs::remove_file(avatar_path).unwrap();
}

#[test]
fn profile_saves_are_background_scoped_and_partial_avatar_failure_keeps_saved_text() {
    let user = User {
        id: "fixture-user".into(),
        username: "immutable".into(),
        profile: UserProfile {
            display_name: "Visible".into(),
            bio: "About".into(),
            version: 1,
            ..Default::default()
        },
        ..Default::default()
    };
    let body = serde_json::to_value(&user).unwrap();
    let server = MockHttp::new(move |path| {
        if path == "/api/profile" {
            Reply::json(200, body.clone())
        } else {
            Reply::json(400, json!({"error":"Avatar failure"}))
        }
    });
    let mut client = profile_worker(&server);
    let immediate = client.handle(Command::SaveProfile {
        origin: client.command_origin(),
        display_name: "Visible".into(),
        bio: "About".into(),
        appearance: None,
        avatar: ProfileAvatarChange::Remove,
        request_id: 19,
    });
    assert!(immediate.is_empty());
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut updates = vec![];
    while Instant::now() < deadline {
        client.worker.poll_transfers();
        updates.extend(client.drain());
        if updates
            .iter()
            .any(|update| matches!(update, Update::ProfileSaveFailed { .. }))
        {
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }
    assert!(updates.iter().any(|update|matches!(update,Update::ProfileSaveFailed{request_id:19,user:Some(user),error,..} if user.profile.display_name=="Visible" && error.contains("Avatar failure"))));
    assert!(!updates.iter().any(|update| matches!(
        update,
        Update::UploadFinished { .. } | Update::UploadProgress { .. }
    )));
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].method, "PUT");
    assert_eq!(requests[1].method, "DELETE");
    assert!(requests
        .iter()
        .all(|request| request.authorization.as_deref() == Some("Bearer fixture-token")));
}

#[test]
fn avatar_routes_bind_user_and_version_and_cannot_fetch_another_api_or_external_url() {
    let user = User {
        id: "person-id".into(),
        profile: UserProfile {
            version: 7,
            ..Default::default()
        },
        ..Default::default()
    };
    let good = Attachment {
        id: avatar_attachment_id(&user),
        mime: "image/png".into(),
        url: "/api/profiles/person-id/avatar?v=7".into(),
        ..Default::default()
    };
    assert!(validate_avatar_attachment(&good).is_ok());
    for url in [
        "https://outside.invalid/picture.png",
        "/api/admin/backup",
        "/api/profiles/other/avatar?v=7",
        "/api/profiles/person-id/avatar?v=6",
        "/api/profiles/person-id/avatar?v=7&redirect=1",
    ] {
        assert!(validate_avatar_attachment(&Attachment {
            url: url.into(),
            ..good.clone()
        })
        .is_err());
    }
    assert!(validate_chat_attachment(&good).is_err());
}

#[test]
fn a_profile_save_finishing_after_account_switch_cannot_update_the_new_session_or_avatar() {
    let (entered, started) = mpsc::sync_channel(1);
    let (release, waiting) = mpsc::sync_channel(1);
    let waiting = Arc::new(std::sync::Mutex::new(waiting));
    let old_user = User {
        id: "fixture-user".into(),
        profile: UserProfile {
            display_name: "Old account".into(),
            ..Default::default()
        },
        ..Default::default()
    };
    let body = serde_json::to_value(old_user).unwrap();
    let server = MockHttp::new(move |path| {
        assert_eq!(
            path, "/api/profile",
            "a cancelled save must not continue with its avatar"
        );
        entered.send(()).unwrap();
        waiting
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(3))
            .unwrap();
        Reply::json(200, body.clone())
    });
    let mut client = profile_worker(&server);
    client.handle(Command::SaveProfile {
        appearance: None,
        origin: client.command_origin(),
        display_name: "Old account".into(),
        bio: String::new(),
        avatar: ProfileAvatarChange::Remove,
        request_id: 20,
    });
    started.recv_timeout(Duration::from_secs(3)).unwrap();
    assert!(
        client.worker.work.busy(),
        "the actual held profile transfer blocks an update without UI Busy"
    );
    client.worker.cancel_transfers();
    client.worker.connection.as_mut().unwrap().session.user = User {
        id: "new-account".into(),
        username: "New account".into(),
        ..Default::default()
    };
    release.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while client.worker.transfer_active.load(Ordering::SeqCst) > 0 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(client.worker.transfer_active.load(Ordering::SeqCst), 0);
    assert!(
        !client.worker.work.busy(),
        "cancellation only releases the gate after the real transfer returned"
    );
    client.worker.poll_transfers();
    assert!(client.drain().is_empty());
    assert_eq!(
        client.worker.connection.as_ref().unwrap().session.user.id,
        "new-account"
    );
    assert_eq!(server.requests.lock().unwrap().len(), 1);
}

#[cfg(windows)]
#[test]
fn rich_send_retries_a_lost_commit_response_with_exactly_the_same_identifier() {
    let attempts = Arc::new(AtomicU64::new(0));
    let count = attempts.clone();
    let server = MockHttp::with_request(move |request| {
        assert_eq!(request.path, "/api/messages");
        if count.fetch_add(1, Ordering::SeqCst) == 0 {
            Reply {
                status: 0,
                headers: Vec::new(),
                body: Vec::new(),
            }
        } else {
            rich_ack(request)
        }
    });
    let mut client = TestWorker::new();
    client.connection(server.url.clone());
    server.enable_encrypted_target(&mut client, Target::Channel("sent-room".into()));
    client.worker.connection.as_mut().unwrap().protocol_version = API_PROTOCOL_VERSION;
    let target = Target::Channel("sent-room".into());
    let updates = client.handle(Command::SendRich {
        binding: None,
        origin: client.command_origin(),
        target: target.clone(),
        content: "retry fixture".into(),
        attachment_ids: vec!["fixture-upload".into()],
        client_id: "stable-client-id".into(),
        reply_to_id: Some("quoted-message".into()),
        mentions: vec!["other-user".into()],
    });
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert!(updates.iter().any(|u| matches!(u, Update::SentRich{client_id, target:t, ..} if client_id == "stable-client-id" && t==&target)));
    let requests = server.message_requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0], requests[1]);
    let sent: Value = serde_json::from_slice(&requests[0]).unwrap();
    assert_eq!(sent["client_id"], "stable-client-id");
    let scope = client.worker.crypto_scope(&target).unwrap();
    let cached = client
        .worker
        .crypto()
        .unwrap()
        .decrypt_content(&scope, sent["content"].as_str().unwrap())
        .unwrap();
    assert_eq!(cached.envelope.content, "retry fixture");
    assert_eq!(
        cached.envelope.reply_to_id.as_deref(),
        Some("quoted-message")
    );
    assert_eq!(cached.envelope.attachments[0].id, "fixture-upload");
}

#[cfg(windows)]
#[test]
fn authenticated_legacy_reaction_fetches_scoped_old_original_once_and_clears_on_account_switch() {
    let fixture = AttachmentFixture::new();
    let mut crypto = fixture.session();
    let target = Target::Channel("old-room".into());
    let context = existing_upload_group(&mut crypto, &target);
    let scope = format!("{}/channel/old-room", context.server_id);
    let wire = crypto
        .encrypted_content(crate::e2ee::Envelope {
            version: 1,
            scope,
            client_id: "reaction-to-legacy".into(),
            content: String::new(),
            reply_to_id: None,
            mentions: Vec::new(),
            attachments: Vec::new(),
            action: Some(crate::e2ee::MessageAction::Reaction {
                message_id: "old-original".into(),
                target_client_id: None,
                emoji: "+1".into(),
                active: true,
            }),
        })
        .unwrap();
    let action = Message {
        id: "reaction-delivery".into(),
        client_id: Some("reaction-to-legacy".into()),
        content: wire,
        sequence: 90,
        channel_id: Some("old-room".into()),
        author: User {
            id: "fixture-user".into(),
            ..Default::default()
        },
        ..Default::default()
    };
    let server = MockHttp::new(|path| {
        assert_eq!(path, "/api/history?around=old-original&channel_id=old-room");
        Reply::json(
            200,
            serde_json::to_value(HistoryPage {
                messages: vec![Message {
                    id: "old-original".into(),
                    sequence: 1,
                    content: "Legacy history".into(),
                    channel_id: Some("old-room".into()),
                    author: User {
                        id: "other-member".into(),
                        ..Default::default()
                    },
                    reactions: vec![Reaction {
                        emoji: "+1".into(),
                        user_ids: vec!["legacy-author".into()],
                    }],
                    ..Default::default()
                }],
                ..Default::default()
            })
            .unwrap(),
        )
    });
    let mut client = TestWorker::new();
    client.connection(server.url.clone());
    install_file_crypto(&mut client, crypto);
    let mut delivered = vec![action.clone()];
    client.worker.decrypt_messages(&target, &mut delivered);
    assert!(
        delivered.is_empty(),
        "the authenticated reaction control remains hidden"
    );
    assert_eq!(client.worker.legacy_reactions.sizes(), (1, 0));
    client.drain();
    client.worker.connection.as_mut().unwrap().session.user.id = "replacement-account".into();
    client.worker.poll_legacy_reactions();
    assert_eq!(client.worker.legacy_reactions.sizes(), (0, 0));
    assert!(
        server.requests.lock().unwrap().is_empty(),
        "an old queued reaction never authenticates as the replacement account"
    );
    client.worker.connection.as_mut().unwrap().session.user.id = "fixture-user".into();
    let mut redelivered = vec![action.clone()];
    client.worker.decrypt_messages(&target, &mut redelivered);
    client.worker.poll_legacy_reactions();
    assert!(client.drain().iter().any(|update| matches!(update,
        Update::ReactionSnapshot { message_id, target_client_id: None, rows, target: t, .. }
        if message_id=="old-original" && t==&target && rows[0].user_ids==vec!["fixture-user", "legacy-author"])));
    let mut repeated = vec![action];
    client.worker.decrypt_messages(&target, &mut repeated);
    client.worker.poll_legacy_reactions();
    assert!(client
        .drain()
        .iter()
        .any(|update| matches!(update, Update::ReactionSnapshot { .. })));
    assert_eq!(
        server.requests.lock().unwrap().len(),
        1,
        "resolved originals are reused rather than fetched on every poll"
    );
    assert_eq!(
        server.requests.lock().unwrap()[0].authorization.as_deref(),
        Some("Bearer fixture-token")
    );
    client.worker.connection.as_mut().unwrap().session.user.id = "replacement-account".into();
    client.worker.poll_legacy_reactions();
    assert_eq!(client.worker.legacy_reactions.sizes(), (0, 0));
    assert!(client.drain().is_empty());
}

#[cfg(windows)]
#[test]
fn legacy_reaction_context_rejects_wrong_room_and_protected_originals() {
    for protected in [false, true] {
        let fixture = AttachmentFixture::new();
        let mut crypto = fixture.session();
        let target = Target::Channel("room".into());
        let context = existing_upload_group(&mut crypto, &target);
        let wire = crypto
            .encrypted_content(crate::e2ee::Envelope {
                version: 1,
                scope: format!("{}/channel/room", context.server_id),
                client_id: "legacy-intent".into(),
                content: String::new(),
                reply_to_id: None,
                mentions: Vec::new(),
                attachments: Vec::new(),
                action: Some(crate::e2ee::MessageAction::Reaction {
                    message_id: "original".into(),
                    target_client_id: None,
                    emoji: "+1".into(),
                    active: true,
                }),
            })
            .unwrap();
        let server = MockHttp::new(move |_| {
            Reply::json(
                200,
                serde_json::to_value(HistoryPage {
                    messages: vec![Message {
                        id: "original".into(),
                        content: if protected {
                            format!("{}opaque", crate::e2ee::MESSAGE_PREFIX)
                        } else {
                            "Legacy content".into()
                        },
                        channel_id: Some(if protected { "room" } else { "another-room" }.into()),
                        reactions: vec![Reaction {
                            emoji: "+1".into(),
                            user_ids: vec!["host-claimed".into()],
                        }],
                        ..Default::default()
                    }],
                    ..Default::default()
                })
                .unwrap(),
            )
        });
        let mut client = TestWorker::new();
        client.connection(server.url.clone());
        install_file_crypto(&mut client, crypto);
        let mut messages = vec![Message {
            id: "control".into(),
            sequence: 10,
            content: wire,
            client_id: Some("legacy-intent".into()),
            channel_id: Some("room".into()),
            author: User {
                id: "fixture-user".into(),
                ..Default::default()
            },
            ..Default::default()
        }];
        client.worker.decrypt_messages(&target, &mut messages);
        client.worker.poll_legacy_reactions();
        assert!(!client
            .drain()
            .iter()
            .any(|update| matches!(update, Update::ReactionSnapshot { .. })));
        assert_eq!(client.worker.legacy_reactions.sizes(), (0, 0));
    }
}

#[cfg(windows)]
#[test]
fn encrypted_reactions_retry_uncertain_commits_with_exact_wire_and_never_send_plaintext_emoji() {
    let fixture = AttachmentFixture::new();
    let mut crypto = fixture.session();
    let target = Target::Channel("sent-room".into());
    let context = existing_upload_group(&mut crypto, &target);
    let scope = format!("{}/channel/sent-room", context.server_id);
    let action = crate::e2ee::MessageAction::Reaction {
        message_id: "original".into(),
        target_client_id: None,
        emoji: "❤".into(),
        active: true,
    };
    let wire = crypto
        .encrypted_content(crate::e2ee::Envelope {
            version: 1,
            scope,
            client_id: "stable-reaction-id".into(),
            content: String::new(),
            reply_to_id: None,
            mentions: Vec::new(),
            attachments: Vec::new(),
            action: Some(action),
        })
        .unwrap();
    let committed = Message {
        id: "reaction-action".into(),
        sequence: 2,
        client_id: Some("stable-reaction-id".into()),
        content: wire.clone(),
        channel_id: Some("sent-room".into()),
        author: User {
            id: "fixture-user".into(),
            ..Default::default()
        },
        ..Default::default()
    };
    let attempts = Arc::new(AtomicU64::new(0));
    let count = attempts.clone();
    let server = MockHttp::new(move |path| {
        if path == "/api/history?around=original&channel_id=sent-room" {
            return Reply::json(
                200,
                serde_json::to_value(HistoryPage {
                    messages: vec![Message {
                        id: "original".into(),
                        content: "older Legacy message".into(),
                        channel_id: Some("sent-room".into()),
                        author: User {
                            id: "other-user".into(),
                            ..Default::default()
                        },
                        ..Default::default()
                    }],
                    ..Default::default()
                })
                .unwrap(),
            );
        }
        if path.starts_with("/api/history?") {
            return Reply::json(200, serde_json::to_value(HistoryPage::default()).unwrap());
        }
        if path.starts_with("/api/e2ee/context?") {
            return Reply::json(200, serde_json::to_value(&context).unwrap());
        }
        assert_eq!(path, "/api/messages");
        if count.fetch_add(1, Ordering::SeqCst) == 0 {
            Reply {
                status: 0,
                body: Vec::new(),
                headers: Vec::new(),
            }
        } else {
            Reply::json(200, serde_json::to_value(&committed).unwrap())
        }
    });
    let mut client = TestWorker::new();
    client.connection(server.url.clone());
    install_file_crypto(&mut client, crypto);
    client.handle(Command::React {
        origin: client.command_origin(),
        target,
        message_id: "original".into(),
        target_client_id: None,
        emoji: "❤".into(),
        active: true,
    });
    client.worker.poll_legacy_reactions();
    assert!(client.drain().iter().any(|update| matches!(update,
        Update::ReactionSnapshot { message_id, target_client_id: None, rows, .. }
            if message_id=="original" && rows.iter().any(|row|row.emoji=="❤" && row.user_ids==vec!["fixture-user"]))));
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    let requests = server.requests.lock().unwrap();
    let sends: Vec<_> = requests
        .iter()
        .filter(|r| r.path == "/api/messages")
        .collect();
    assert_eq!(sends.len(), 2);
    assert_eq!(sends[0].body, sends[1].body);
    let body: Value = serde_json::from_slice(&sends[0].body).unwrap();
    assert_eq!(body["client_id"], "stable-reaction-id");
    assert_eq!(body["content"], wire);
    assert!(body["content"]
        .as_str()
        .unwrap()
        .starts_with(crate::e2ee::MESSAGE_PREFIX));
    assert!(!String::from_utf8_lossy(&sends[0].body).contains('❤'));
    assert!(requests.iter().all(|r| !r.path.contains("/reactions")));
}

#[cfg(windows)]
#[test]
fn definite_reaction_cooldown_never_auto_retries_and_retains_original_encrypted_action_id() {
    let fixture = AttachmentFixture::new();
    let mut crypto = fixture.session();
    let target = Target::Channel("sent-room".into());
    let context = existing_upload_group(&mut crypto, &target);
    let server = MockHttp::new(move |path| {
        if path.starts_with("/api/history?") {
            Reply::json(200, serde_json::to_value(HistoryPage::default()).unwrap())
        } else if path.starts_with("/api/e2ee/context?") {
            Reply::json(200, serde_json::to_value(&context).unwrap())
        } else {
            assert_eq!(path, "/api/messages");
            Reply::json(
                429,
                json!({"error":"Try again later","code":"message_rate_limit","retry_after_ms":1000}),
            )
        }
    });
    let mut client = TestWorker::new();
    client.connection(server.url.clone());
    install_file_crypto(&mut client, crypto);
    for _ in 0..2 {
        assert_eq!(
            client
                .worker
                .handle(Command::React {
                    origin: client.command_origin(),
                    target: target.clone(),
                    message_id: "original".into(),
                    target_client_id: None,
                    emoji: "❤".into(),
                    active: true
                })
                .unwrap_err(),
            "Try again later"
        );
        client.drain();
    }
    let requests = server.requests.lock().unwrap();
    let sends: Vec<_> = requests
        .iter()
        .filter(|r| r.path == "/api/messages")
        .collect();
    assert_eq!(
        sends.len(),
        2,
        "one request per explicit attempt; no retry of a definite429"
    );
    assert_eq!(
        sends[0].body, sends[1].body,
        "explicit retry retains original ciphertext and ID"
    );
    assert!(!String::from_utf8_lossy(&sends[0].body).contains('❤'));
}

#[cfg(windows)]
#[test]
fn definite_send_rejection_and_history_failure_keep_their_request_scope() {
    let server = MockHttp::new(|_| Reply::json(403, json!({"error":"private channel denied"})));
    let mut client = TestWorker::new();
    client.connection(server.url.clone());
    server.enable_encrypted_target(&mut client, Target::Channel("private-room".into()));
    client.worker.connection.as_mut().unwrap().protocol_version = API_PROTOCOL_VERSION;
    let target = Target::Channel("private-room".into());
    client.worker.command(Command::SendRich {
        binding: None,
        origin: client.command_origin(),
        target: target.clone(),
        content: "fixture".into(),
        attachment_ids: Vec::new(),
        client_id: "fixture-unique-id".into(),
        reply_to_id: None,
        mentions: Vec::new(),
    });
    let updates = client.drain();
    assert!(updates.iter().any(|u| matches!(u, Update::SendFailed{target:t,error,..} if t==&target && error=="private channel denied")));
    assert_eq!(
        server.message_requests().len(),
        1,
        "definite rejection must never retry"
    );
    client.worker.command(Command::History {
        target,
        before: Some(99),
        request_id: 123,
    });
    assert!(client.drain().iter().any(|u|matches!(u,Update::RequestFailed{request_id:123,error} if error=="private channel denied")));
    assert!(server
        .requests
        .lock()
        .unwrap()
        .last()
        .unwrap()
        .path
        .contains("channel_id=private-room"));
}

fn enable_chat_features(client: &mut TestWorker, slowmode_seconds: u32) {
    let mut bootstrap = Bootstrap {
        capabilities: vec!["pins-v1".into(), "typing-v1".into(), "slowmode-v1".into()],
        channels: vec![Channel {
            id: "sent-room".into(),
            kind: "text".into(),
            slowmode_seconds,
            ..Default::default()
        }],
        ..Default::default()
    };
    if client.worker.connection.as_ref().unwrap().crypto.is_some() {
        bootstrap.capabilities.push("e2ee_mls_v1".into());
    }
    client.worker.connection.as_mut().unwrap().chat = extended::ChatSession::new(&bootstrap);
}

fn retry_message(origin: CommandOrigin, target: Target) -> Command {
    Command::SendRich {
        binding: None,
        origin,
        target,
        content: "unchanged draft".into(),
        attachment_ids: vec!["kept-attachment".into()],
        client_id: "rate-limited-stable-id".into(),
        reply_to_id: Some("kept-reply".into()),
        mentions: vec!["kept-mention".into()],
    }
}

#[cfg(windows)]
#[test]
fn http_429_keeps_send_payload_and_suppresses_queued_sends_until_explicit_retry() {
    let attempts = Arc::new(AtomicU64::new(0));
    let count = attempts.clone();
    let server = MockHttp::with_request(move |request| {
        assert_eq!(request.path, "/api/messages");
        if count.fetch_add(1, Ordering::SeqCst) == 0 {
            Reply::json(
                429,
                json!({"error":"Wait for slowmode", "code":"slowmode", "retry_after_ms":100, "channel_id":"sent-room"}),
            )
        } else {
            rich_ack(request)
        }
    });
    let mut client = TestWorker::new();
    client.connection(server.url.clone());
    server.enable_encrypted_target(&mut client, Target::Channel("sent-room".into()));
    enable_chat_features(&mut client, 0);
    let target = Target::Channel("sent-room".into());
    client
        .worker
        .command(retry_message(client.command_origin(), target.clone()));
    let updates = client.drain();
    assert!(updates.iter().any(
        |u| matches!(u, Update::SendRateLimited { target:t, code, retry_after_ms, .. }
        if t == &target && code == "slowmode" && *retry_after_ms > 0 && *retry_after_ms <= 100)
    ));
    assert!(!updates
        .iter()
        .any(|u| matches!(u, Update::SentRich { .. } | Update::SendFailed { .. })));
    client
        .worker
        .command(retry_message(client.command_origin(), target.clone()));
    assert!(client
        .drain()
        .iter()
        .any(|u| matches!(u, Update::SendRateLimited { .. })));
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        1,
        "queued retries must not cause repeated HTTP requests"
    );
    thread::sleep(Duration::from_millis(120));
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        1,
        "expiry alone never automatically resends a draft"
    );
    let updates = client.handle(retry_message(client.command_origin(), target.clone()));
    assert!(updates.iter().any(
        |u| matches!(u, Update::SentRich { client_id, target: sent_target, .. }
        if client_id == "rate-limited-stable-id" && sent_target == &target)
    ));
    let requests = server.message_requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0], requests[1],
        "explicit retry preserves idempotency ID, reply, mentions and attachment IDs"
    );
    let sent: Value = serde_json::from_slice(&requests[1]).unwrap();
    let scope = client.worker.crypto_scope(&target).unwrap();
    let cached = client
        .worker
        .crypto()
        .unwrap()
        .decrypt_content(&scope, sent["content"].as_str().unwrap())
        .unwrap();
    assert_eq!(cached.envelope.content, "unchanged draft");
    assert_eq!(cached.envelope.reply_to_id.as_deref(), Some("kept-reply"));
    assert_eq!(cached.envelope.attachments[0].id, "kept-attachment");
}

#[cfg(windows)]
#[test]
fn http_retry_after_fallback_is_account_scoped_and_edit_cooldowns_do_not_acknowledge() {
    let stored = Arc::new(std::sync::Mutex::new(None::<Message>));
    let original = stored.clone();
    let server = MockHttp::with_request(move |request| {
        if request.path.starts_with("/api/history?around=") {
            return Reply::json(
                200,
                serde_json::to_value(HistoryPage {
                    messages: vec![original.lock().unwrap().clone().unwrap()],
                    ..Default::default()
                })
                .unwrap(),
            );
        }
        Reply::json(
            429,
            json!({"error":"Too many requests","retry_after_ms":-1}),
        )
        .header("Retry-After", "2")
    });
    let mut client = TestWorker::new();
    client.connection(server.url.clone());
    server.enable_encrypted_target(&mut client, Target::Channel("sent-room".into()));
    client.worker.command(retry_message(
        client.command_origin(),
        Target::Channel("sent-room".into()),
    ));
    assert!(client.drain().iter().any(
        |u| matches!(u, Update::SendRateLimited { code, retry_after_ms, .. }
        if code == "rate_limited" && *retry_after_ms > 1000 && *retry_after_ms <= 2000)
    ));
    client.worker.command(retry_message(
        client.command_origin(),
        Target::Direct("another-peer".into()),
    ));
    assert!(client.drain().iter().any(|u| matches!(u, Update::SendRateLimited { target:Target::Direct(peer), .. } if peer == "another-peer")));
    assert_eq!(
        server.message_requests().len(),
        1,
        "account limit spans channel and DM sends"
    );
    let target = Target::Channel("sent-room".into());
    *stored.lock().unwrap() = Some(server.encrypted_message(
        &mut client,
        &target,
        "edited-message",
        "original private text",
    ));
    client.worker.connection.as_mut().unwrap().current = Some(target);
    client.worker.command(Command::EditMessage {
        origin: client.command_origin(),
        message_id: "edited-message".into(),
        content: "retain edited draft".into(),
    });
    let updates = client.drain();
    assert!(updates.iter().any(|u| matches!(u, Update::EditRateLimited {message_id, retry_after_ms, ..} if message_id == "edited-message" && *retry_after_ms > 1000)));
    assert!(!updates
        .iter()
        .any(|u| matches!(u, Update::Edited(_) | Update::EditFailed { .. })));
    client.worker.command(Command::EditMessage {
        origin: client.command_origin(),
        message_id: "edited-message".into(),
        content: "later edit remains local".into(),
    });
    assert!(client
        .drain()
        .iter()
        .any(|u| matches!(u, Update::EditRateLimited { .. })));
    assert_eq!(
        server
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.method == "PATCH" || (r.method == "POST" && r.path == "/api/messages"))
            .count(),
        2
    );
}

#[cfg(windows)]
#[test]
fn successful_slowmode_send_blocks_new_ids_but_allows_committed_idempotent_retry() {
    let server = MockHttp::with_request(|request| {
        if request.path == "/api/messages" {
            rich_ack(request)
        } else {
            Reply::json(404, json!({"error":"missing"}))
        }
    });
    let mut client = TestWorker::new();
    client.connection(server.url.clone());
    server.enable_encrypted_target(&mut client, Target::Channel("sent-room".into()));
    enable_chat_features(&mut client, 10);
    let target = Target::Channel("sent-room".into());
    client.handle(retry_message(client.command_origin(), target.clone()));
    client.worker.command(Command::SendRich {
        binding: None,
        origin: client.command_origin(),
        target: target.clone(),
        content: "next draft".into(),
        attachment_ids: Vec::new(),
        client_id: "different-id".into(),
        reply_to_id: None,
        mentions: Vec::new(),
    });
    assert!(client
        .drain()
        .iter()
        .any(|u| matches!(u, Update::SendRateLimited {code,..} if code == "slowmode")));
    assert_eq!(server.message_requests().len(), 1);
    assert!(client
        .handle(retry_message(client.command_origin(), target))
        .iter()
        .any(|u| matches!(u, Update::SentRich { .. })));
    assert_eq!(
        server.message_requests().len(),
        2,
        "a duplicate committed ID may bypass local slowmode safely"
    );
    server.enable_encrypted_target(&mut client, Target::Direct("separate-conversation".into()));
    client.handle(Command::SendRich {
        binding: None,
        origin: client.command_origin(),
        target: Target::Direct("separate-conversation".into()),
        content: "separate conversation".into(),
        attachment_ids: Vec::new(),
        client_id: "new-dm-send-id".into(),
        reply_to_id: None,
        mentions: Vec::new(),
    });
    assert_eq!(
        server.message_requests().len(),
        3,
        "channel slowmode must not block DMs"
    );
}

#[test]
fn pins_load_and_mutations_return_their_exact_request_scope_and_reject_foreign_pins() {
    let pin = json!({"message":fixture_message("pinned content"), "pinned_by":{"id":"moderator","username":"mod","role":"moderator"}, "pinned_at":"2026-10-04T01:00:00Z"});
    let server = MockHttp::new(move |path| match path {
        "/api/channels/sent-room/pins" => Reply::json(200, json!([pin])),
        "/api/channels/sent-room/pins/message-1" => Reply::json(204, Value::Null),
        "/api/channels/foreign-room/pins" => Reply::json(200, json!([pin])),
        _ => Reply::json(404, json!({"error":"missing"})),
    });
    let mut client = TestWorker::new();
    client.connection(server.url.clone());
    client.worker.command(Command::FetchPins {
        channel_id: "sent-room".into(),
        request_id: 1,
    });
    assert!(client.drain().iter().any(|u| matches!(u, Update::RequestFailed {request_id:1,error} if error.contains("Update this server"))));
    assert!(
        server.requests.lock().unwrap().is_empty(),
        "old servers receive no unsupported pin requests"
    );
    enable_chat_features(&mut client, 0);
    let updates = client.handle(Command::FetchPins {
        channel_id: "sent-room".into(),
        request_id: 2,
    });
    assert!(updates.iter().any(|u| matches!(u, Update::PinsLoaded {url,channel_id,pins,request_id:2} if url == &server.url && channel_id == "sent-room" && pins.len() == 1)));
    for (pinned, request_id) in [(true, 3), (false, 4)] {
        let updates = client.handle(Command::SetPin {
            channel_id: "sent-room".into(),
            message_id: "message-1".into(),
            pinned,
            request_id,
        });
        assert!(updates
            .iter()
            .any(|u| matches!(u, Update::PinsLoaded {request_id:id,..} if *id == request_id)));
    }
    let requests = server.requests.lock().unwrap();
    assert_eq!(
        requests
            .iter()
            .map(|r| r.method.as_str())
            .collect::<Vec<_>>(),
        ["GET", "PUT", "GET", "DELETE", "GET"]
    );
    assert!(requests
        .iter()
        .all(|r| r.authorization.as_deref() == Some("Bearer fixture-token")));
    drop(requests);
    client.worker.command(Command::FetchPins {
        channel_id: "foreign-room".into(),
        request_id: 5,
    });
    assert!(client.drain().iter().any(|u| matches!(u, Update::RequestFailed {request_id:5,error} if error.contains("another conversation"))));
    client.worker.command(Command::SetPin {
        channel_id: "sent-room/../secret".into(),
        message_id: "message-1".into(),
        pinned: true,
        request_id: 6,
    });
    assert!(client
        .drain()
        .iter()
        .any(|u| matches!(u, Update::RequestFailed { request_id: 6, .. })));
    assert_eq!(server.requests.lock().unwrap().len(), 6);
}

#[test]
fn typing_heartbeats_are_throttled_clear_on_navigation_and_do_not_run_on_old_servers() {
    let server = MockHttp::new(|path| {
        assert_eq!(path, "/api/typing");
        Reply::json(204, Value::Null)
    });
    let mut client = TestWorker::new();
    client.connection(server.url.clone());
    let target = Target::Channel("sent-room".into());
    client.handle(Command::SelectConversation(target.clone()));
    client.handle(Command::Typing {
        target: target.clone(),
        typing: true,
    });
    assert!(server.requests.lock().unwrap().is_empty());
    enable_chat_features(&mut client, 0);
    for _ in 0..10 {
        client.handle(Command::Typing {
            target: target.clone(),
            typing: true,
        });
    }
    assert_eq!(server.requests.lock().unwrap().len(), 1);
    client.handle(Command::SelectConversation(Target::Direct("friend".into())));
    client.handle(Command::Typing {
        target,
        typing: true,
    });
    let requests = server.requests.lock().unwrap();
    assert_eq!(
        requests.len(),
        2,
        "old conversation is stopped and a queued old heartbeat is discarded"
    );
    let first: Value = serde_json::from_slice(&requests[0].body).unwrap();
    let last: Value = serde_json::from_slice(&requests[1].body).unwrap();
    assert_eq!(
        first,
        json!({"channel_id":"sent-room","peer_id":null,"typing":true})
    );
    assert_eq!(
        last,
        json!({"channel_id":"sent-room","peer_id":null,"typing":false})
    );
    assert!(
        !String::from_utf8_lossy(&requests[0].body).contains("draft"),
        "typing never transmits draft text"
    );
}

#[test]
fn typing_and_pin_hints_are_scoped_expired_and_cannot_revive_a_previous_session() {
    let server = MockHttp::new(|path| {
        assert_eq!(path, "/api/typing?channel_id=sent-room");
        Reply::json(
            200,
            json!({"server_time_ms":500000000, "users":[
                {"user":{"id":"expired-user","username":"expired","role":"member"},"expires_at_ms":500000000},
                {"user":{"id":"fixture-user","username":"self","role":"member"},"expires_at_ms":500008000},
                {"user":{"id":"live-user","username":"live","role":"member"},"expires_at_ms":500000001}
            ]}),
        )
    });
    let mut client = TestWorker::new();
    client.connection(server.url.clone());
    enable_chat_features(&mut client, 0);
    client.handle(Command::SelectConversation(Target::Channel(
        "sent-room".into(),
    )));
    client.worker.poll_chat_events();
    assert!(client.drain().iter().any(|u| matches!(u, Update::Typing {url,target:Target::Channel(id),state}
        if url == &server.url && id == "sent-room" && state.users.len() == 1 && state.users[0].user.id == "live-user")));
    thread::sleep(Duration::from_millis(5));
    client.worker.poll_chat_events();
    assert!(client
        .drain()
        .iter()
        .any(|u| matches!(u, Update::Typing {state,..} if state.users.is_empty())));
    let inbox = client
        .worker
        .connection
        .as_ref()
        .unwrap()
        .chat
        .events
        .clone();
    for value in [
        json!({"kind":"typing","channel_id":"other-room","user_id":"other-user","typing":true}),
        json!({"kind":"typing","user_id":"outsider","recipient_id":"another-account","typing":true}),
        json!({"kind":"typing","channel_id":"sent-room","recipient_id":"fixture-user","user_id":"outsider","typing":true}),
        json!({"kind":"pins","channel_id":"secret-room"}),
        json!({"kind":"pins","channel_id":"sent-room"}),
    ] {
        extended::queue_chat_event(
            &inbox,
            &client.worker.refresh_pending,
            serde_json::from_value(value).unwrap(),
        );
    }
    client.worker.poll_chat_events();
    let updates = client.drain();
    assert_eq!(updates.len(), 1);
    assert!(
        matches!(&updates[0], Update::PinsChanged {url,channel_id} if url == &server.url && channel_id == "sent-room")
    );
    assert_eq!(
        server.requests.lock().unwrap().len(),
        1,
        "foreign/ambiguous hints must not fetch private state"
    );
    client.connection(server.url.clone()); // A new account/session owns a distinct event inbox.
    enable_chat_features(&mut client, 0);
    extended::queue_chat_event(
        &inbox,
        &client.worker.refresh_pending,
        serde_json::from_value(json!({"kind":"pins","channel_id":"sent-room"})).unwrap(),
    );
    client.worker.poll_chat_events();
    assert!(client.drain().is_empty());
}

#[test]
fn asynchronous_upload_cancel_and_session_epoch_discard_late_success() {
    let fixture = AttachmentFixture::new();
    let mut crypto = fixture.session();
    let target = Target::Direct("original-peer".into());
    let context = serde_json::to_value(existing_upload_group(&mut crypto, &target)).unwrap();
    let (entered_tx, entered_rx) = mpsc::channel();
    let releases = Arc::new((std::sync::Mutex::new(false), Condvar::new()));
    let gate = releases.clone();
    // Only this test thread opens the gate, and only after StartUpload has
    // returned; otherwise the held response is answered when the gate expires.
    let answered = Arc::new(AtomicBool::new(false));
    let expired = Arc::new(AtomicBool::new(false));
    let (server_answered, server_expired) = (answered.clone(), expired.clone());
    let server = MockHttp::new(move |path| {
        if path.starts_with("/api/e2ee/context?") {
            Reply::json(200, context.clone())
        } else if path.starts_with("/api/history?after=") {
            Reply::json(200, serde_json::to_value(HistoryPage::default()).unwrap())
        } else if path.starts_with("/api/uploads?") {
            entered_tx.send(()).unwrap();
            let (lock, ready) = &*gate;
            let released = *ready
                .wait_timeout_while(lock.lock().unwrap(), Duration::from_secs(4), |released| {
                    !*released
                })
                .unwrap()
                .0;
            server_expired.fetch_or(!released, Ordering::SeqCst);
            server_answered.store(true, Ordering::SeqCst);
            if released {
                Reply::json(
                    200,
                    json!({"id":"late-upload","name":"opaque.bin","mime":"application/octet-stream","size":48,"url":"/api/uploads/late-upload"}),
                )
            } else {
                Reply::json(503, json!({"error":"fixture upload gate expired"}))
            }
        } else {
            Reply {
                status: 204,
                headers: Vec::new(),
                body: Vec::new(),
            }
        }
    });
    let path = fixture.0.join("fixture.gif");
    fs::write(&path, b"synthetic upload fixture").unwrap();
    let mut client = TestWorker::new();
    client.connection(server.url.clone());
    install_file_crypto(&mut client, crypto);
    client.handle(Command::StartUpload {
        origin: client.command_origin(),
        target: Target::Direct("original-peer".into()),
        path: path.clone(),
        operation_id: "fixture-transfer".into(),
    });
    // Causal rather than wall-clock: synchronous key preparation may be slow
    // under load, but the call must never wait for the held upload response.
    assert!(
        !answered.load(Ordering::SeqCst),
        "StartUpload returned only after the held upload response"
    );
    entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    client.handle(Command::SelectConversation(Target::Channel(
        "different-room".into(),
    )));
    client.handle(Command::CancelUpload {
        operation_id: "fixture-transfer".into(),
    });
    *releases.0.lock().unwrap() = true;
    releases.1.notify_all();
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut cancelled = false;
    while Instant::now() < deadline && !cancelled {
        client.worker.poll_transfers();
        for update in client.drain() {
            assert!(!matches!(update, Update::UploadFinished { .. }));
            if let Update::UploadFailed {
                target,
                operation_id,
                cancelled: true,
                ..
            } = update
            {
                assert_eq!(target, Target::Direct("original-peer".into()));
                assert_eq!(operation_id, "fixture-transfer");
                cancelled = true;
            }
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(cancelled);
    *releases.0.lock().unwrap() = false;
    answered.store(false, Ordering::SeqCst);
    client.handle(Command::StartUpload {
        origin: client.command_origin(),
        target: Target::Direct("original-peer".into()),
        path,
        operation_id: "old-session-transfer".into(),
    });
    assert!(
        !answered.load(Ordering::SeqCst),
        "StartUpload returned only after the held upload response"
    );
    entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    client.worker.cancel_transfers();
    *releases.0.lock().unwrap() = true;
    releases.1.notify_all();
    let deadline = Instant::now() + Duration::from_millis(400);
    while Instant::now() < deadline {
        client.worker.poll_transfers();
        assert!(client.drain().is_empty());
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !expired.load(Ordering::SeqCst),
        "both held uploads were released by the test, not by the fixture timeout"
    );
    assert!(
        client
            .worker
            .connection
            .as_ref()
            .unwrap()
            .crypto
            .as_ref()
            .unwrap()
            .file("late-upload")
            .unwrap()
            .is_none(),
        "cancelled or stale transfers never persist their keys"
    );
}

#[test]
fn preparing_an_up_to_date_conversation_writes_no_checkpoint_until_its_cursor_advances() {
    let fixture = AttachmentFixture::new();
    let mut crypto = fixture.session();
    let target = Target::Direct("original-peer".into());
    let context = Arc::new(std::sync::Mutex::new(existing_upload_group(
        &mut crypto,
        &target,
    )));
    let served = context.clone();
    let server = MockHttp::new(move |path| {
        if path.starts_with("/api/e2ee/context?") {
            Reply::json(200, serde_json::to_value(&*served.lock().unwrap()).unwrap())
        } else {
            Reply::json(200, serde_json::to_value(HistoryPage::default()).unwrap())
        }
    });
    let mut client = TestWorker::new();
    client.connection(server.url.clone());
    install_file_crypto(&mut client, crypto);
    let scope = client.worker.crypto_scope(&target).unwrap();
    let revision = |client: &mut TestWorker| {
        client
            .worker
            .crypto()
            .unwrap()
            .checkpoint_revision_for_test()
    };
    let before = revision(&mut client);
    for _ in 0..2 {
        client.worker.ensure_crypto_conversation(&target).unwrap();
        assert_eq!(
            revision(&mut client),
            before,
            "an unchanged key-event cursor needs no durable checkpoint before every send"
        );
    }
    // Sequence gaps (Welcomes to other devices) still advance the cursor durably.
    context.lock().unwrap().sequence = 3;
    client.worker.ensure_crypto_conversation(&target).unwrap();
    assert_eq!(client.worker.crypto().unwrap().cursor(&scope), 3);
    assert_eq!(revision(&mut client), before + 1);
}

#[test]
fn encrypted_upload_keeps_filename_and_keys_local_and_only_authenticated_download_commits() {
    let fixture = AttachmentFixture::new();
    let mut crypto = fixture.session();
    let target = Target::Channel("encrypted-room".into());
    let context = serde_json::to_value(existing_upload_group(&mut crypto, &target)).unwrap();
    let source = b"private synthetic attachment contents";
    let response_body = Arc::new(std::sync::Mutex::new(Vec::new()));
    let served = response_body.clone();
    let server = MockHttp::new(move |path| {
        if path.starts_with("/api/e2ee/context?") {
            Reply::json(200, context.clone())
        } else if path.starts_with("/api/history?after=") {
            Reply::json(200, serde_json::to_value(HistoryPage::default()).unwrap())
        } else if path.starts_with("/api/uploads?") {
            Reply::json(
                200,
                json!({"id":"encrypted-file","name":"opaque.bin","mime":"application/octet-stream","size":source.len()+24,"url":"/api/uploads/encrypted-file"}),
            )
        } else {
            Reply {
                status: 200,
                headers: Vec::new(),
                body: served.lock().unwrap().clone(),
            }
        }
    });
    let source_path = fixture.0.join("private-original-name.txt");
    fs::write(&source_path, source).unwrap();
    let mut client = TestWorker::new();
    client.connection(server.url.clone());
    install_file_crypto(&mut client, crypto);
    client.worker.command(Command::StartUpload {
        origin: client.command_origin(),
        target: target.clone(),
        path: source_path.clone(),
        operation_id: "encrypted-upload".into(),
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut uploaded = None;
    while Instant::now() < deadline && uploaded.is_none() {
        client.worker.poll_transfers();
        for update in client.drain() {
            match update {
                Update::UploadFinished {
                    target: received,
                    attachment,
                    ..
                } => {
                    assert_eq!(received, target);
                    uploaded = Some(attachment);
                }
                Update::UploadFailed { error, .. } => panic!("encrypted upload failed: {error}"),
                _ => {}
            }
        }
        thread::sleep(Duration::from_millis(5));
    }
    let attachment = uploaded.expect("encrypted upload completes");
    assert_eq!(attachment.name, "private-original-name.txt");
    assert_eq!(attachment.size, source.len() as u64);
    let metadata = client
        .worker
        .connection
        .as_ref()
        .unwrap()
        .crypto
        .as_ref()
        .unwrap()
        .file(&attachment.id)
        .unwrap()
        .expect("key metadata persisted before UploadFinished");
    let wire = {
        let requests = server.requests.lock().unwrap();
        let request = requests
            .iter()
            .find(|r| r.path.starts_with("/api/uploads?"))
            .unwrap();
        assert!(!request.path.contains("private-original-name"));
        assert!(
            request.path.contains(".bin") && request.path.contains("application%2Foctet-stream")
        );
        assert_eq!(
            request.authorization.as_deref(),
            Some("Bearer fixture-token")
        );
        assert_eq!(request.body.len(), source.len() + 24);
        assert!(!request
            .body
            .windows(source.len())
            .any(|bytes| bytes == source));
        assert_eq!(
            &*relay_crypto::files::decrypt(&request.body, &metadata.file, &metadata.file.binding)
                .unwrap(),
            source
        );
        request.body.clone()
    };
    let saved = fixture.0.join("explicitly-saved.txt");
    *response_body.lock().unwrap() = wire.clone();
    client.worker.command(Command::StartDownload {
        origin: client.command_origin(),
        attachment: attachment.clone(),
        path: saved.clone(),
        operation_id: "valid-download".into(),
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut finished = false;
    while Instant::now() < deadline && !finished {
        client.worker.poll_transfers();
        for update in client.drain() {
            match update {
                Update::DownloadFinished { .. } => finished = true,
                Update::DownloadFailed { error, .. } => panic!("valid decryption failed: {error}"),
                _ => {}
            }
        }
        thread::sleep(Duration::from_millis(5));
    }
    assert!(finished);
    assert_eq!(fs::read(&saved).unwrap(), source);
    for wrong_key in [false, true] {
        if wrong_key {
            let (_, _, mut wrong) = encrypted_attachment(source, "encrypted-file");
            wrong.name = metadata.name.clone();
            client
                .worker
                .connection
                .as_mut()
                .unwrap()
                .crypto
                .as_mut()
                .unwrap()
                .add_file(wrong)
                .unwrap();
            *response_body.lock().unwrap() = wire.clone();
        } else {
            response_body.lock().unwrap()[12] ^= 1;
        }
        client.worker.command(Command::StartDownload {
            origin: client.command_origin(),
            attachment: attachment.clone(),
            path: saved.clone(),
            operation_id: format!("invalid-{wrong_key}"),
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut failed = false;
        while Instant::now() < deadline && !failed {
            client.worker.poll_transfers();
            for update in client.drain() {
                match update {
                    Update::DownloadFailed { .. } => failed = true,
                    Update::DownloadFinished { .. } => panic!("unauthenticated file was committed"),
                    _ => {}
                }
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert!(failed);
        assert_eq!(fs::read(&saved).unwrap(), source);
        assert!(
            !fs::read_dir(&fixture.0).unwrap().any(|entry| entry
                .unwrap()
                .path()
                .extension()
                .is_some_and(|ext| ext == "partial")),
            "failed authentication creates no plaintext temporary file"
        );
    }
}

#[test]
fn queued_attachment_commands_reject_changed_origin_before_keys_http_or_files() {
    let fixture = AttachmentFixture::new();
    let server = MockHttp::new(|path| panic!("stale attachment request must not fetch {path}"));
    let mut client = TestWorker::new();
    client.connection(server.url.clone());
    let (_, attachment, metadata) = encrypted_attachment(b"private fixture", "same-file-id");
    let mut crypto = fixture.session();
    crypto.add_file(metadata).unwrap();
    install_file_crypto(&mut client, crypto);
    let valid = client.command_origin();
    let destination = fixture.0.join("existing.txt");
    let source = fixture.0.join("selected.txt");
    fs::write(&destination, b"keep existing document").unwrap();
    fs::write(&source, b"private selection").unwrap();
    for field in 0..3 {
        let mut stale = valid.clone();
        match field {
            0 => stale.url.push_str("/previous-community"),
            1 => stale.server_id.push_str("-previous-server"),
            _ => stale.user_id.push_str("-previous-account"),
        }
        client.worker.command(Command::FetchChatMedia {
            original: false,
            origin: stale.clone(),
            attachment: attachment.clone(),
        });
        assert!(
            client.drain().is_empty(),
            "a stale failure cannot poison the replacement account's same-ID preview"
        );
        client.worker.command(Command::StartDownload {
            origin: stale.clone(),
            attachment: attachment.clone(),
            path: destination.clone(),
            operation_id: format!("stale-download-{field}"),
        });
        assert!(client.drain().iter().any(|update| matches!(update,
            Update::DownloadFailed { error, .. } if error.contains("another account or community"))));
        client.worker.command(Command::StartUpload {
            origin: stale,
            target: Target::Channel("same-channel-id".into()),
            path: source.clone(),
            operation_id: format!("stale-upload-{field}"),
        });
        assert!(client.drain().iter().any(|update| matches!(update,
            Update::UploadFailed { error, .. } if error.contains("another account or community"))));
    }
    assert!(server.requests.lock().unwrap().is_empty());
    assert!(client.worker.chat_media_workers.is_none());
    assert!(client.worker.transfer_http.is_none());
    assert!(client.worker.transfers.is_empty());
    assert_eq!(fs::read(destination).unwrap(), b"keep existing document");
    assert_eq!(
        fs::read_dir(&fixture.0)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "partial"))
            .count(),
        0
    );
}

#[test]
fn attachment_commands_reject_current_account_with_a_foreign_encryption_actor() {
    let fixture = AttachmentFixture::new();
    let server = MockHttp::new(|path| panic!("foreign actor must not fetch {path}"));
    let mut client = TestWorker::new();
    client.connection(server.url.clone());
    let (_, attachment, metadata) = encrypted_attachment(b"private fixture", "same-file-id");
    let mut crypto = fixture.session();
    crypto.add_file(metadata).unwrap();
    install_file_crypto(&mut client, crypto);
    for server_changed in [false, true] {
        let connection = client.worker.connection.as_mut().unwrap();
        connection.session.user.id = if server_changed {
            "fixture-user"
        } else {
            "another-account"
        }
        .into();
        if server_changed {
            connection.server_id.push_str("-replacement");
        }
        let origin = client.command_origin();
        assert!(client
            .worker
            .queue_chat_media(&origin, attachment.clone())
            .unwrap_err()
            .contains("actor"));
        client.worker.command(Command::StartDownload {
            origin: origin.clone(),
            attachment: attachment.clone(),
            path: fixture.0.join("blocked.txt"),
            operation_id: format!("foreign-download-{server_changed}"),
        });
        assert!(client.drain().iter().any(|update| matches!(update,
            Update::DownloadFailed { error, .. } if error.contains("actor"))));
        client.worker.command(Command::StartUpload {
            origin,
            target: Target::Channel("same-channel-id".into()),
            path: fixture.0.join("unread-path"),
            operation_id: format!("foreign-upload-{server_changed}"),
        });
        assert!(client.drain().iter().any(|update| matches!(update,
            Update::UploadFailed { error, .. } if error.contains("actor"))));
    }
    assert!(server.requests.lock().unwrap().is_empty());
    assert!(!fixture.0.join("blocked.txt").exists());
}

#[test]
fn encrypted_download_cancel_or_account_switch_never_commits_existing_destination() {
    let fixture = AttachmentFixture::new();
    let (wire, attachment, metadata) =
        encrypted_attachment(b"private saved fixture", "cancel-file");
    let (entered_tx, entered_rx) = mpsc::channel();
    let release = Arc::new((std::sync::Mutex::new(false), Condvar::new()));
    let gate = release.clone();
    let server = MockHttp::new(move |_| {
        entered_tx.send(()).unwrap();
        let (lock, ready) = &*gate;
        let (released, _) = ready
            .wait_timeout_while(lock.lock().unwrap(), Duration::from_secs(4), |released| {
                !*released
            })
            .unwrap();
        assert!(*released);
        Reply {
            status: 200,
            headers: Vec::new(),
            body: wire.clone(),
        }
    });
    let mut client = TestWorker::new();
    client.connection(server.url.clone());
    let mut crypto = fixture.session();
    crypto.add_file(metadata).unwrap();
    install_file_crypto(&mut client, crypto);
    let saved = fixture.0.join("keep.txt");
    fs::write(&saved, b"existing document").unwrap();
    for epoch_change in [false, true] {
        *release.0.lock().unwrap() = false;
        let operation_id = format!("cancel-{epoch_change}");
        client.worker.command(Command::StartDownload {
            origin: client.command_origin(),
            attachment: attachment.clone(),
            path: saved.clone(),
            operation_id: operation_id.clone(),
        });
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        if epoch_change {
            assert!(
                client.worker.work.busy(),
                "the actual authenticated download blocks installation while its response is held"
            );
            client.worker.cancel_transfers();
        } else {
            assert!(
                client.worker.work.busy(),
                "the actual authenticated download blocks installation while its response is held"
            );
            client
                .worker
                .command(Command::CancelUpload { operation_id });
        }
        *release.0.lock().unwrap() = true;
        release.1.notify_all();
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && client.worker.transfer_active.load(Ordering::SeqCst) > 0
        {
            client.worker.poll_transfers();
            assert!(!client
                .drain()
                .iter()
                .any(|u| matches!(u, Update::DownloadFinished { .. })));
            thread::sleep(Duration::from_millis(5));
        }
        client.worker.poll_transfers();
        assert!(!client
            .drain()
            .iter()
            .any(|u| matches!(u, Update::DownloadFinished { .. })));
        assert_eq!(client.worker.transfer_active.load(Ordering::Acquire), 0);
        assert!(
            !client.worker.work.busy(),
            "the update gate clears only after the cancelled download returns"
        );
        assert_eq!(fs::read(&saved).unwrap(), b"existing document");
        assert!(!fs::read_dir(&fixture.0).unwrap().any(|entry| entry
            .unwrap()
            .path()
            .extension()
            .is_some_and(|ext| ext == "partial")));
    }
}

#[test]
fn encrypted_edit_preserves_signed_metadata_and_retries_lost_ack_with_identical_ciphertext() {
    let fixture = AttachmentFixture::new();
    let mut crypto = fixture.session();
    let target = Target::Channel("encrypted-edit-room".into());
    let context = existing_upload_group(&mut crypto, &target);
    let scope = format!("{}/channel/encrypted-edit-room", context.server_id);
    let (_, attachment, metadata) = encrypted_attachment(b"private attachment", "edit-file");
    let envelope = crate::e2ee::Envelope {
        version: 1,
        scope: scope.clone(),
        client_id: "original-send-id".into(),
        content: "original private text".into(),
        reply_to_id: Some("original-reply".into()),
        mentions: vec!["original-mentioned-user".into()],
        attachments: vec![metadata.clone()],
        action: None,
    };
    let original_wire = crypto.encrypted_content(envelope).unwrap();
    crypto.committed("original-send-id").unwrap();
    let original = Message {
        id: "message-1".into(),
        sequence: 1,
        client_id: Some("original-send-id".into()),
        channel_id: Some("encrypted-edit-room".into()),
        recipient_id: None,
        author: context.roster[0].clone(),
        content: original_wire.clone(),
        attachments: vec![attachment],
        ..Default::default()
    };
    crypto
        .bind_delivery(&original_wire, &original.id, original.sequence)
        .unwrap();
    let current = Arc::new(std::sync::Mutex::new(original));
    let served = current.clone();
    let patches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let patch_count = patches.clone();
    let server = MockHttp::with_request(move |request| {
        if request.path.starts_with("/api/e2ee/context?") {
            Reply::json(200, serde_json::to_value(&context).unwrap())
        } else if request.path.starts_with("/api/history?around=") {
            Reply::json(
                200,
                serde_json::to_value(HistoryPage {
                    messages: vec![served.lock().unwrap().clone()],
                    latest_sequence: 1,
                    ..Default::default()
                })
                .unwrap(),
            )
        } else if request.path.starts_with("/api/history?after=") {
            Reply::json(
                200,
                serde_json::to_value(HistoryPage {
                    latest_sequence: 1,
                    ..Default::default()
                })
                .unwrap(),
            )
        } else if request.method == "PATCH" && request.path == "/api/messages/message-1" {
            let edit: EditMessage = serde_json::from_slice(&request.body).unwrap();
            assert!(edit.content.starts_with(crate::e2ee::MESSAGE_PREFIX));
            served.lock().unwrap().content = edit.content;
            // The first request was committed by the server, but its reply was lost.
            if patch_count.fetch_add(1, Ordering::SeqCst) == 0 {
                Reply {
                    status: 0,
                    body: Vec::new(),
                    headers: Vec::new(),
                }
            } else {
                Reply::json(
                    200,
                    serde_json::to_value(served.lock().unwrap().clone()).unwrap(),
                )
            }
        } else {
            panic!("unexpected encrypted edit request {}", request.path);
        }
    });
    let mut client = TestWorker::new();
    client.connection(server.url.clone());
    install_file_crypto(&mut client, crypto);
    client.worker.connection.as_mut().unwrap().current = Some(target);
    assert!(client
        .worker
        .edit_message("message-1", "edited private text".into())
        .is_err());
    assert!(client
        .worker
        .edit_message("message-1", "edited private text".into())
        .unwrap());
    let requests = server.requests.lock().unwrap();
    let submitted: Vec<_> = requests.iter().filter(|r| r.method == "PATCH").collect();
    assert_eq!(submitted.len(), 2);
    assert_eq!(
        submitted[0].body, submitted[1].body,
        "retry reuses the durable MLS ciphertext"
    );
    assert!(!String::from_utf8_lossy(&submitted[0].body).contains("edited private text"));
    let updated = current.lock().unwrap();
    let cached = client
        .worker
        .connection
        .as_mut()
        .unwrap()
        .crypto
        .as_mut()
        .unwrap()
        .decrypt_content(&scope, &updated.content)
        .unwrap();
    assert_eq!(cached.envelope.content, "edited private text");
    assert_eq!(cached.envelope.client_id, "original-send-id");
    assert_eq!(
        cached.envelope.reply_to_id.as_deref(),
        Some("original-reply")
    );
    assert_eq!(cached.envelope.mentions, vec!["original-mentioned-user"]);
    assert_eq!(
        serde_json::to_value(&cached.envelope.attachments[0]).unwrap(),
        serde_json::to_value(metadata).unwrap()
    );
    assert!(cached.envelope.action.is_none());
    assert_eq!(updated.sequence, 1);
}

#[test]
fn encrypted_edit_refuses_legacy_servers_oversized_content_and_wrong_identifiers_before_write() {
    let server = MockHttp::new(|path| panic!("invalid or plaintext edit must not request {path}"));
    let mut client = TestWorker::new();
    client.connection(server.url.clone());
    client.worker.connection.as_mut().unwrap().current = Some(Target::Channel("room".into()));
    let error = client
        .worker
        .edit_message("message-1", "private edit".into())
        .unwrap_err();
    assert!(error.contains("end-to-end"), "{error}");
    assert!(client
        .worker
        .edit_message("message-1", "x".repeat(10001))
        .is_err());
    assert!(client
        .worker
        .edit_message("../message-1", "private edit".into())
        .is_err());
    assert!(server.requests.lock().unwrap().is_empty());
}

#[cfg(windows)]
fn changed_edit_after_failed_response(first_status: u16) {
    let stored = Arc::new(std::sync::Mutex::new(None::<Message>));
    let remote = stored.clone();
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let patch_attempts = attempts.clone();
    let server = MockHttp::with_request(move |request| {
        if request.path.starts_with("/api/history?around=") {
            return Reply::json(
                200,
                serde_json::to_value(HistoryPage {
                    messages: vec![remote.lock().unwrap().clone().unwrap()],
                    ..Default::default()
                })
                .unwrap(),
            );
        }
        if request.method == "PATCH" && request.path == "/api/messages/pending-edit" {
            let edit: EditMessage = serde_json::from_slice(&request.body).unwrap();
            assert!(edit.content.starts_with(crate::e2ee::MESSAGE_PREFIX));
            let attempt = patch_attempts.fetch_add(1, Ordering::SeqCst);
            if attempt == 0 && first_status == 429 {
                return Reply::json(
                    429,
                    json!({"error":"Wait before editing again", "retry_after_ms":100}),
                );
            }
            remote.lock().unwrap().as_mut().unwrap().content = edit.content;
            if attempt == 0 {
                return Reply {
                    status: first_status,
                    body: Vec::new(),
                    headers: Vec::new(),
                };
            }
            return Reply::json(
                200,
                serde_json::to_value(remote.lock().unwrap().clone().unwrap()).unwrap(),
            );
        }
        panic!("unexpected changed-edit request {}", request.path);
    });
    let mut client = TestWorker::new();
    client.connection(server.url.clone());
    let target = Target::Channel("pending-edit-room".into());
    server.enable_encrypted_target(&mut client, target.clone());
    *stored.lock().unwrap() = Some(server.encrypted_message(
        &mut client,
        &target,
        "pending-edit",
        "original private body",
    ));
    client.worker.connection.as_mut().unwrap().current = Some(target.clone());
    let first = client
        .worker
        .edit_message("pending-edit", "first private edit".into());
    if first_status == 429 {
        assert!(!first.unwrap());
        assert!(client.drain().iter().any(|u| matches!(u, Update::EditRateLimited {message_id, ..} if message_id == "pending-edit")));
        thread::sleep(Duration::from_millis(150));
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            1,
            "a cooldown never emits an automatic retry"
        );
    } else {
        assert!(first.is_err());
    }
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert!(
        client
            .worker
            .edit_message("pending-edit", "later private edit".into())
            .unwrap(),
        "a changed explicit edit supersedes the pending failed intent"
    );
    assert!(client
        .worker
        .edit_message("pending-edit", "later private edit".into())
        .unwrap());
    let requests = server.requests.lock().unwrap();
    let patches: Vec<_> = requests
        .iter()
        .filter(|request| request.method == "PATCH")
        .collect();
    assert_eq!(patches.len(), 3);
    assert_ne!(
        patches[0].body, patches[1].body,
        "changed intent creates a new MLS generation"
    );
    assert_eq!(
        patches[1].body, patches[2].body,
        "identical retry keeps its exact protected wire"
    );
    for patch in patches {
        let body = String::from_utf8_lossy(&patch.body);
        assert!(!body.contains("first private edit") && !body.contains("later private edit"));
    }
    let current = stored.lock().unwrap().clone().unwrap();
    let scope = client.worker.crypto_scope(&target).unwrap();
    let crypto = client
        .worker
        .connection
        .as_mut()
        .unwrap()
        .crypto
        .as_mut()
        .unwrap();
    assert_eq!(
        crypto
            .decrypt_content(&scope, &current.content)
            .unwrap()
            .envelope
            .content,
        "later private edit"
    );
    assert_eq!(current.id, "pending-edit");
    assert_eq!(current.sequence, 1);
}

#[cfg(windows)]
#[test]
fn changed_encrypted_edit_after_429_waits_for_explicit_retry_and_replaces_pending_intent() {
    changed_edit_after_failed_response(429);
}

#[cfg(windows)]
#[test]
fn changed_encrypted_edit_after_lost_response_supersedes_already_published_pending_intent() {
    changed_edit_after_failed_response(0);
}

#[test]
fn resuming_an_already_connected_community_never_revokes_its_token() {
    let server = MockHttp::new(|_| panic!("resume must reuse this in-memory session"));
    let mut client = TestWorker::new();
    client.connection(server.url.clone());
    client.handle(Command::Resume {
        url: server.url.clone(),
        expected_server_id: None,
    });
    assert_eq!(
        client.worker.connection.as_ref().unwrap().session.token,
        "fixture-token"
    );
    assert!(server.requests.lock().unwrap().is_empty());
}

#[test]
fn failed_download_and_invalid_backup_never_replace_existing_files() {
    let fixture = AttachmentFixture::new();
    let server = MockHttp::new(|path| match path {
        "/api/admin/backup" => Reply {
            status: 200,
            headers: Vec::new(),
            body: b"this is not a SQLite backup fixture".to_vec(),
        },
        "/api/uploads/fixture-file" => Reply {
            status: 200,
            headers: Vec::new(),
            body: b"short".to_vec(),
        },
        _ => Reply::json(404, json!({"error":"missing"})),
    });
    let root = &fixture.0;
    let backup = root.join("keep.sqlite3");
    let download = root.join("keep.txt");
    fs::write(&backup, b"existing backup fixture").unwrap();
    fs::write(&download, b"existing document fixture").unwrap();
    let mut client = TestWorker::new();
    client.connection(server.url.clone());
    let (_, _, metadata) = encrypted_attachment(&[0; 40], "fixture-file");
    let mut crypto = fixture.session();
    crypto.add_file(metadata).unwrap();
    install_file_crypto(&mut client, crypto);
    client.handle(Command::SaveBackup {
        path: backup.clone(),
        operation_id: "backup-fixture".into(),
    });
    client.handle(Command::StartDownload {
        origin: client.command_origin(),
        attachment: Attachment {
            id: "fixture-file".into(),
            url: "/api/uploads/fixture-file".into(),
            size: 40,
            name: "fixture.txt".into(),
            mime: "text/plain".into(),
        },
        path: download.clone(),
        operation_id: "download-fixture".into(),
    });
    let deadline = Instant::now() + Duration::from_secs(3);
    let (mut backup_failed, mut download_failed) = (false, false);
    while Instant::now() < deadline && !(backup_failed && download_failed) {
        client.worker.poll_transfers();
        for update in client.drain() {
            match update {
                Update::BackupFailed {
                    operation_id,
                    error,
                    ..
                } => {
                    assert_eq!(operation_id, "backup-fixture");
                    assert!(error.contains("SQLite"));
                    backup_failed = true;
                }
                Update::DownloadFailed {
                    operation_id,
                    error,
                    ..
                } => {
                    assert_eq!(operation_id, "download-fixture");
                    assert!(error.contains("size"));
                    download_failed = true;
                }
                Update::BackupSaved { .. } | Update::DownloadFinished { .. } => {
                    panic!("invalid transfer was committed")
                }
                _ => {}
            }
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(backup_failed && download_failed);
    assert_eq!(fs::read(&backup).unwrap(), b"existing backup fixture");
    assert_eq!(fs::read(&download).unwrap(), b"existing document fixture");
    assert_eq!(
        fs::read_dir(root)
            .unwrap()
            .filter(|entry| entry.as_ref().unwrap().file_type().unwrap().is_file())
            .count(),
        2,
        "temporary files were removed"
    );
    fs::remove_file(backup).unwrap();
    fs::remove_file(download).unwrap();
}

#[test]
fn send_acknowledges_original_target_and_payload_even_when_refresh_fails() {
    let server = MockHttp::new(|path| match path {
        "/api/messages" => Reply::json(200, fixture_message("submitted text")),
        "/api/messages?limit=100&peer_id=current-peer" => {
            Reply::json(503, json!({"error":"refresh unavailable"}))
        }
        _ => Reply::json(404, json!({"error":"missing"})),
    });
    let mut client = TestWorker::new();
    client.connection(server.url.clone());
    client.worker.connection.as_mut().unwrap().current =
        Some(Target::Direct("current-peer".into()));
    let target = Target::Channel("sent-room".into());
    client.worker.command(Command::Send {
        target: target.clone(),
        content: "submitted text".into(),
        attachment_ids: vec!["upload-1".into()],
    });
    let updates = client.drain();
    assert!(updates.iter().any(|u| matches!(u, Update::Sent {target:t,content,attachment_ids} if t==&target && content=="submitted text" && attachment_ids==&vec!["upload-1".to_owned()])));
    assert!(updates
        .iter()
        .any(|u| matches!(u, Update::Error(error) if error=="refresh unavailable")));
    assert!(
        !updates
            .iter()
            .any(|u| matches!(u, Update::SendFailed { .. })),
        "a committed message must never be presented as a failed send"
    );
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests[0].method, "POST");
    let sent: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(sent["channel_id"], "sent-room");
    assert_eq!(sent["content"], "submitted text");
    assert_eq!(sent["attachment_ids"], json!(["upload-1"]));
}

#[test]
fn failed_send_acknowledges_the_submitted_conversation_without_success() {
    let server = MockHttp::new(|_| Reply::json(403, json!({"error":"send denied"})));
    let mut client = TestWorker::new();
    client.connection(server.url.clone());
    client.worker.connection.as_mut().unwrap().current =
        Some(Target::Channel("different-current-room".into()));
    let target = Target::Direct("original-peer".into());
    client.worker.command(Command::Send {
        target: target.clone(),
        content: "retry later".into(),
        attachment_ids: Vec::new(),
    });
    let updates = client.drain();
    let failure = updates.iter().find_map(|update| match update {
        Update::SendFailed { target, error, .. } => Some((target, error.as_str())),
        _ => None,
    });
    assert_eq!(failure, Some((&target, "send denied")));
    assert!(!updates
        .iter()
        .any(|u| matches!(u, Update::Sent { .. } | Update::Error(_))));
    assert_eq!(
        server.requests.lock().unwrap().len(),
        1,
        "failed sends must not refresh or retry"
    );
    client.worker.connection = None;
    client.worker.command(Command::Send {
        target: target.clone(),
        content: "offline draft".into(),
        attachment_ids: Vec::new(),
    });
    let updates = client.drain();
    assert!(updates.iter().any(|u| matches!(u, Update::SendFailed {target:t,error,..} if t==&target && error=="Connect to a server first.")));
    assert!(!updates.iter().any(|u| matches!(u, Update::Sent { .. })));
    assert_eq!(server.requests.lock().unwrap().len(), 1);
}

#[cfg(windows)]
#[test]
fn encrypted_edit_uses_opaque_content_patch_and_a_distinct_acknowledgement_then_refreshes() {
    let stored = Arc::new(std::sync::Mutex::new(None::<Message>));
    let remote = stored.clone();
    let server = MockHttp::with_request(move |request| {
        if request.path.starts_with("/api/history?around=") {
            return Reply::json(
                200,
                serde_json::to_value(HistoryPage {
                    messages: vec![remote.lock().unwrap().clone().unwrap()],
                    ..Default::default()
                })
                .unwrap(),
            );
        }
        if request.path == "/api/messages/message-1" && request.method == "PATCH" {
            let edit: EditMessage = serde_json::from_slice(&request.body).unwrap();
            let mut stored = remote.lock().unwrap();
            let original = stored.as_mut().unwrap();
            original.content = edit.content;
            original.edited_at = Some("2026-10-04T00:00:00Z".into());
            return Reply::json(200, serde_json::to_value(original).unwrap());
        }
        if request.path == "/api/messages?limit=100&channel_id=sent-room" {
            return Reply::json(200, json!([remote.lock().unwrap().clone().unwrap()]));
        }
        Reply::json(404, json!({"error":"missing"}))
    });
    let mut client = TestWorker::new();
    client.connection(server.url.clone());
    let target = Target::Channel("sent-room".into());
    server.enable_encrypted_target(&mut client, target.clone());
    *stored.lock().unwrap() =
        Some(server.encrypted_message(&mut client, &target, "message-1", "original private text"));
    client.worker.connection.as_mut().unwrap().current = Some(target);
    let updates = client.handle(Command::EditMessage {
        origin: client.command_origin(),
        message_id: "message-1".into(),
        content: "edited ❤ draft".into(),
    });
    assert!(updates
        .iter()
        .any(|u| matches!(u,Update::Edited(id) if id=="message-1")));
    assert!(
        !updates
            .iter()
            .any(|u| matches!(u, Update::Sent { .. } | Update::SentRich { .. })),
        "an edit must not clear a normal composer draft"
    );
    assert_eq!(last_messages(&updates)[0].content, "edited ❤ draft");
    let requests = server.requests.lock().unwrap();
    let patches: Vec<_> = requests.iter().filter(|r| r.method == "PATCH").collect();
    assert_eq!(patches.len(), 1);
    let body: Value = serde_json::from_slice(&patches[0].body).unwrap();
    assert_eq!(body.as_object().unwrap().len(), 1);
    assert!(body["content"]
        .as_str()
        .unwrap()
        .starts_with(crate::e2ee::MESSAGE_PREFIX));
    assert!(!String::from_utf8_lossy(&patches[0].body).contains('❤'));
    assert!(requests
        .iter()
        .any(|r| r.path.starts_with("/api/history?around=message-1")));
}

#[test]
fn uploads_keep_their_original_target_after_conversation_changes() {
    let server = MockHttp::new(|path| {
        assert!(path.starts_with("/api/uploads?"));
        Reply::json(
            200,
            json!({"id":"upload-1","name":"fixture.txt","mime":"application/octet-stream","size":7,"url":"/api/uploads/upload-1"}),
        )
    });
    let mut client = TestWorker::new();
    client.connection(server.url.clone());
    client.worker.connection.as_mut().unwrap().current = Some(Target::Channel("new-room".into()));
    let work = test_work_root();
    fs::create_dir_all(&work).unwrap();
    let path = work.join(format!(
        "network-upload-{}-{}.txt",
        std::process::id(),
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::write(&path, b"fixture").unwrap();
    let target = Target::Direct("original-peer".into());
    let result = client.worker.handle(Command::Upload {
        target: target.clone(),
        path: path.clone(),
    });
    fs::remove_file(path).unwrap();
    result.unwrap();
    assert!(client.drain().iter().any(|u| matches!(u,Update::Uploaded {target:t,attachment} if t==&target && attachment.id=="upload-1")));
    assert_eq!(server.requests.lock().unwrap()[0].body, b"fixture");
}

#[test]
fn json_parser_rejects_malformed_and_oversized_success_and_error_bodies() {
    let server = MockHttp::new(|path| match path {
        "/valid" => Reply::json(200, json!({"value":42})),
        "/malformed" => Reply {
            status: 200,
            headers: Vec::new(),
            body: b"{".to_vec(),
        },
        "/oversized" => Reply {
            status: 200,
            headers: Vec::new(),
            body: vec![b' '; 2 * 1024 * 1024 + 1],
        },
        "/error" => Reply::json(400, json!({"error":"precise failure"})),
        "/large-error" => Reply::json(400, json!({"error":"x".repeat(20 * 1024)})),
        _ => Reply::json(404, json!({"error":"missing"})),
    });
    let valid: Value = parse(server.response("/valid")).unwrap();
    assert_eq!(valid["value"], 42);
    let malformed = parse::<Value>(server.response("/malformed")).unwrap_err();
    assert!(malformed.contains("incompatible"));
    let oversized = parse::<Value>(server.response("/oversized")).unwrap_err();
    assert!(oversized.contains("too large"));
    assert_eq!(
        parse::<Value>(server.response("/error")).unwrap_err(),
        "precise failure"
    );
    let error = parse::<Value>(server.response("/large-error")).unwrap_err();
    assert!(
        error.len() < 100,
        "oversized server error must never reach the UI"
    );
    let exact: Value = read_json(server.response("/valid"), 12).unwrap();
    assert_eq!(exact["value"], 42);
    assert!(read_json::<Value>(server.response("/valid"), 11)
        .unwrap_err()
        .contains("too large"));
}

#[test]
fn failed_bootstrap_never_replaces_existing_connection_or_stops_events() {
    for response in [
        Reply::json(503, json!({"error":"bootstrap unavailable"})),
        Reply {
            status: 200,
            headers: Vec::new(),
            body: b"{".to_vec(),
        },
    ] {
        let server = MockHttp::new(move |path| match path {
            "/api/login" => Reply::json(
                200,
                json!({"token":"new-session", "user":{"id":"new-user", "username":"new", "role":"member"}}),
            ),
            "/api/bootstrap" => response.clone(),
            _ => Reply::json(404, json!({"error":"missing"})),
        });
        let mut client = TestWorker::new();
        client.connection("http://prior-server.test".into());
        client.worker.connection.as_mut().unwrap().current =
            Some(Target::Channel("previous-channel".into()));
        let previous_events = client.worker.socket_stop.clone();
        let result = client.worker.handle(Command::Auth {
            authority: None,
            url: server.url.clone(),
            expected_server_id: None,
            request: AuthRequest {
                second_factor: None,
                initial_protection: None,
                username: "new".into(),
                password: "test-password".into(),
                invite: String::new(),
            },
            register: false,
            request_id: 0,
        });
        assert!(result.is_err());
        let connection = client.worker.connection.as_ref().unwrap();
        assert_eq!(connection.url, "http://prior-server.test");
        assert_eq!(connection.session.token, "fixture-token");
        assert_eq!(
            connection.current,
            Some(Target::Channel("previous-channel".into()))
        );
        assert!(!previous_events.load(Ordering::Relaxed));
        assert!(
            client.drain().is_empty(),
            "failed bootstrap emits no connected state"
        );
        assert_eq!(
            &*server.paths.lock().unwrap(),
            &["/api/login", "/api/bootstrap"]
        );
    }
}

fn invitation_health(id: &str) -> Value {
    json!({"ok":true,"server_name":"Owned connection test","server_id":id,
        "protocol_version":API_PROTOCOL_VERSION,"rtc_enabled":true})
}

#[test]
fn invitation_identity_check_precedes_password_and_preserves_existing_account() {
    let server = MockHttp::new(|_| {
        Reply::json(
            200,
            invitation_health("4be44aef-8c98-4db6-a9d5-81c2eb311330"),
        )
    });
    let mut client = TestWorker::new();
    client.connection("http://previous-community.test".into());
    let events = client.worker.socket_stop.clone();
    let result = client.worker.handle(Command::Auth {
        authority: None,
        url: server.url.clone(),
        expected_server_id: Some("49eb3c90-3520-4932-ae14-2f63ec80c38b".into()),
        request: AuthRequest {
            second_factor: None,
            initial_protection: None,
            username: "owned-user".into(),
            password: "owned-password".into(),
            invite: "owned-code".into(),
        },
        register: true,
        request_id: 0,
    });
    assert!(result.unwrap_err().contains("different community"));
    assert_eq!(
        client.worker.connection.as_ref().unwrap().url,
        "http://previous-community.test"
    );
    assert!(!events.load(Ordering::Relaxed));
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[0].path, "/api/health");
    assert!(requests[0].body.is_empty() && requests[0].authorization.is_none());
    assert!(client.drain().is_empty());
}

#[test]
fn connection_check_rejects_redirects_oversize_and_incompatible_health() {
    let destination = MockHttp::new(|_| {
        Reply::json(
            200,
            invitation_health("4be44aef-8c98-4db6-a9d5-81c2eb311330"),
        )
    });
    let client = TestWorker::new();
    for response in [
        Reply::json(302, json!({})).header("Location", &format!("{}/api/health", destination.url)),
        Reply::json(503, json!({"error":"unavailable"})),
        Reply::json(200, json!({"ok":true})),
        Reply {
            status: 200,
            headers: vec![],
            body: vec![b' '; 16 * 1024 + 1],
        },
        Reply::json(
            200,
            json!({"ok":true,"server_name":"Owned","server_id":"4be44aef-8c98-4db6-a9d5-81c2eb311330","protocol_version":999,"rtc_enabled":true}),
        ),
    ] {
        let server = MockHttp::new(move |_| response.clone());
        assert!(check_server(&client.worker.http, &server.url).is_err());
        assert_eq!(server.requests.lock().unwrap().len(), 1);
    }
    assert!(destination.requests.lock().unwrap().is_empty());
}

#[test]
fn connection_check_is_bounded_and_does_not_block_commands() {
    let server = MockHttp::new(|_| {
        thread::sleep(Duration::from_millis(2300));
        Reply::json(
            200,
            invitation_health("4be44aef-8c98-4db6-a9d5-81c2eb311330"),
        )
    });
    let mut client = TestWorker::new();
    let start = Instant::now();
    client
        .worker
        .handle(Command::CheckServer {
                locator_invite: None,
            cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            authority: None,
            url: server.url.clone(),
            request_id: 1,
        })
        .unwrap();
    // The command worker remains available while the anonymous probe runs.
    client
        .worker
        .handle(Command::RememberSession { enabled: false })
        .unwrap();
    assert!(start.elapsed() < Duration::from_secs(1));
    client
        .worker
        .handle(Command::CheckServer {
                locator_invite: None,
            cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            authority: None,
            url: server.url.clone(),
            request_id: 2,
        })
        .unwrap();
    let rejected = client.drain();
    assert!(rejected.iter().any(|update| matches!(
        update,
        Update::ServerChecked {
            request_id: 2,
            result: Err(_),
            ..
        }
    )));
    let deadline = Instant::now() + Duration::from_secs(4);
    let mut timed_out = false;
    while Instant::now() < deadline {
        if client.drain().iter().any(|update| {
            matches!(
                update,
                Update::ServerChecked {
                    request_id: 1,
                    result: Err(_),
                    ..
                }
            )
        }) {
            timed_out = true;
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(timed_out);
    assert!(start.elapsed() < Duration::from_millis(3000));
    assert_eq!(server.paths.lock().unwrap().len(), 1);
}

#[test]
fn failed_auth_is_reported_with_its_own_request_id() {
    let server = MockHttp::new(|path| match path {
        "/api/register" => Reply::json(409, json!({"error": "Username is already taken"})),
        _ => Reply::json(404, json!({})),
    });
    let mut client = TestWorker::new();
    client.worker.command(Command::Auth {
        authority: None,
        url: server.url.clone(),
        expected_server_id: None,
        register: true,
        request_id: 41,
        request: AuthRequest {
            second_factor: None,
            initial_protection: None,
            username: "fixture-name".into(),
            password: "fixture-password".into(),
            invite: String::new(),
        },
    });
    let updates = client.drain();
    assert!(updates.iter().any(|update| matches!(
        update,
        Update::RequestFailed { request_id: 41, error } if error == "Username is already taken"
    )));
    // The form correlates by id; nothing arrives as an untyped error.
    assert!(!updates
        .iter()
        .any(|update| matches!(update, Update::Error(_))));
}

#[test]
fn account_login_canceled_during_bootstrap_revokes_new_session_and_keeps_the_old_account() {
    const ID: &str = "4be44aef-8c98-4db6-a9d5-81c2eb311330";
    let cancel = Arc::new(AtomicBool::new(false));
    let trigger = cancel.clone();
    let server = MockHttp::new(move |path| match path {
        "/api/health" => Reply::json(200, invitation_health(ID)),
        "/api/account-login" => Reply::json(
            200,
            json!({"token":"owned-new-session", "user":{"id":"owned-new-user","username":"owned-new","role":"member"}}),
        ),
        "/api/bootstrap" => {
            trigger.store(true, Ordering::Release);
            Reply::json(
                200,
                serde_json::to_value(Bootstrap {
                    server_id: ID.into(),
                    user: User {
                        id: "owned-new-user".into(),
                        ..Default::default()
                    },
                    ..Default::default()
                })
                .unwrap(),
            )
        }
        "/api/logout" => Reply::json(200, json!({})),
        _ => Reply::json(404, json!({})),
    });
    let mut client = TestWorker::new();
    client.connection("http://previous-community.test".into());
    client
        .worker
        .account_login_finish(
            None,
            server.url.clone(),
            ID.into(),
            uuid::Uuid::new_v4().to_string(),
            zeroize::Zeroizing::new("owned-synthetic-proof".into()),
            cancel,
            51,
        )
        .unwrap();
    assert_eq!(
        client.worker.connection.as_ref().unwrap().url,
        "http://previous-community.test"
    );
    assert!(client.drain().is_empty());
    let requests = server.requests.lock().unwrap();
    assert_eq!(
        requests.iter().map(|r| r.path.as_str()).collect::<Vec<_>>(),
        vec![
            "/api/health",
            "/api/account-login",
            "/api/bootstrap",
            "/api/logout"
        ]
    );
    assert!(requests[0].authorization.is_none() && requests[0].body.is_empty());
    assert_eq!(
        requests[3].authorization.as_deref(),
        Some("Bearer owned-new-session")
    );
}

#[test]
fn account_login_checks_the_server_pin_before_sending_any_proof() {
    let server = MockHttp::new(|path| match path {
        "/api/health" => Reply::json(
            200,
            invitation_health("4be44aef-8c98-4db6-a9d5-81c2eb311330"),
        ),
        _ => Reply::json(404, json!({})),
    });
    let mut client = TestWorker::new();
    client.connection("http://previous-community.test".into());
    let result = client.worker.account_login_finish(
            None,
        server.url.clone(),
        "49eb3c90-3520-4932-ae14-2f63ec80c38b".into(),
        uuid::Uuid::new_v4().to_string(),
        zeroize::Zeroizing::new("owned-synthetic-proof".into()),
        Arc::new(AtomicBool::new(false)),
        52,
    );
    assert!(result.unwrap_err().contains("identity"));
    assert_eq!(server.requests.lock().unwrap().len(), 1);
    assert_eq!(
        client.worker.connection.as_ref().unwrap().url,
        "http://previous-community.test"
    );
}

#[test]
#[ignore = "requires an explicitly owned loopback identity provider and real Relay server"]
fn owned_live_account_first_joins_and_reopens_the_same_community_identity() {
    use crate::central_account::{Controller, Provider};
    let config: Value = serde_json::from_str(
        &std::env::var("RELAY_OWN_LINK_JOURNEY").expect("own journey config required"),
    )
    .unwrap();
    let service = Provider::owned_loopback_fixture(config["issuer"].as_str().unwrap());
    let community = config["server_url"].as_str().unwrap().to_string();
    assert!(community.starts_with("http://127.0.0.1:"));
    let ctx = eframe::egui::Context::default();
    let mut global = Controller::default();
    global.provider = Some(service.clone());
    global.start(ctx.clone());
    let deadline = Instant::now() + Duration::from_secs(60);
    while global.account.is_none() {
        if let Some(url) = global.poll() {
            println!("RELAY_OWN_LINK_BROWSER={url}");
            std::io::stdout().flush().unwrap();
        }
        assert!(global.error.is_none(), "{:?}", global.error);
        assert!(Instant::now() < deadline, "own global sign-in deadline");
        thread::sleep(Duration::from_millis(10));
    }
    let account = global.account.clone().unwrap();
    assert_eq!(account.handle, "relay_friend");
    let mut client = TestWorker::new();
    client.worker.remember_session = false;
    let mut first_user = None;
    for attempt in 1..=2 {
        client
            .worker
            .account_login_begin(
            None,
                community.clone(),
                Some(config["server_id"].as_str().unwrap().into()),
                zeroize::Zeroizing::new(if attempt == 1 {
                    config["invite"].as_str().unwrap().into()
                } else {
                    String::new()
                }),
                attempt == 1,
                None,
                attempt,
            )
            .unwrap();
        let challenge = client
            .drain()
            .into_iter()
            .find_map(|u| {
                if let Update::AccountLoginChallenge { data, .. } = u {
                    Some(data)
                } else {
                    None
                }
            })
            .expect("destination challenge");
        let mut browser = Controller::default();
        browser.provider = Some(service.clone());
        browser.start_community(
            ctx.clone(),
            challenge["nonce"].as_str().unwrap().into(),
            account.clone(),
        );
        let deadline = Instant::now() + Duration::from_secs(60);
        let proof = loop {
            if let Some(url) = browser.poll() {
                println!("RELAY_OWN_LINK_BROWSER={url}");
                std::io::stdout().flush().unwrap();
            }
            assert!(browser.error.is_none(), "{:?}", browser.error);
            if let Some(proof) = browser.take_link_proof() {
                break proof;
            }
            assert!(Instant::now() < deadline, "own community approval deadline");
            thread::sleep(Duration::from_millis(10));
        };
        browser.disconnect();
        client
            .worker
            .account_login_finish(
            None,
                community.clone(),
                config["server_id"].as_str().unwrap().into(),
                challenge["challenge_id"].as_str().unwrap().into(),
                proof.id_token,
                Arc::new(AtomicBool::new(false)),
                attempt,
            )
            .unwrap();
        let (session, bootstrap) = client
            .drain()
            .into_iter()
            .find_map(|u| {
                if let Update::AccountLoginComplete {
                    session,
                    bootstrap,
                    request_id,
                    ..
                } = u
                {
                    assert_eq!(request_id, attempt);
                    Some((session, bootstrap))
                } else {
                    None
                }
            })
            .expect("account-first connected");
        assert!(bootstrap.account_login_only);
        assert_eq!(bootstrap.protection.level, ProtectionLevel::Protected);
        assert_eq!(session.user.profile.display_name, "relay_friend");
        if let Some(first) = &first_user {
            assert_eq!(&session.user.id, first);
        } else {
            first_user = Some(session.user.id.clone());
        }
    }
    client
        .worker
        .handle(Command::ForgetCommunity { url: community })
        .unwrap();
    global.disconnect();
    println!("OWN_ACCOUNT_FIRST_JOURNEY_OK: global code/PKCE sign-in, invitation membership without community password, real worker bootstrap, second SSO preserves local identity, logout");
}

#[test]
fn only_the_recorded_wait_error_becomes_a_send_wait() {
    let target = Target::Channel("general".into());
    let message = "Waiting for this conversation's key manager to verify and approve your device.";
    let origin = CommandOrigin {
        url: "http://127.0.0.1:4580".into(),
        server_id: "server-a".into(),
        user_id: "user-a".into(),
    };
    let wait = || Some((SendWait::DeviceApproval, message.to_owned()));
    assert!(matches!(
        send_failure(target.clone(), message.into(), wait(), Some(origin.clone())),
        Update::SendWaiting { target: t, origin: o, wait: SendWait::DeviceApproval }
            if t == target && o == origin
    ));
    // Another error after a wait was recorded stays a real failure.
    assert!(matches!(
        send_failure(target.clone(), "Unable to reach the server.".into(), wait(), Some(origin.clone())),
        Update::SendFailed { error, .. } if error == "Unable to reach the server."
    ));
    // The same text without a recorded wait is not guessed to be one.
    assert!(matches!(
        send_failure(target.clone(), message.into(), None, Some(origin)),
        Update::SendFailed { .. }
    ));
    // Without a connection there is no account to bind the wait to.
    assert!(matches!(
        send_failure(target, message.into(), wait(), None),
        Update::SendFailed { .. }
    ));
}

#[cfg(windows)]
#[test]
fn rate_limited_send_names_the_connection_it_ran_on() {
    let server = MockHttp::with_request(move |request| {
        assert_eq!(request.path, "/api/messages");
        Reply::json(
            429,
            json!({"error":"Wait for slowmode", "code":"slowmode", "retry_after_ms":100, "channel_id":"sent-room"}),
        )
    });
    let mut client = TestWorker::new();
    client.connection(server.url.clone());
    server.enable_encrypted_target(&mut client, Target::Channel("sent-room".into()));
    enable_chat_features(&mut client, 0);
    let target = Target::Channel("sent-room".into());
    let origin = client.command_origin();
    client
        .worker
        .command(retry_message(origin.clone(), target.clone()));
    assert!(client.drain().iter().any(|update| matches!(
        update,
        Update::SendRateLimited { target: t, origin: Some(o), .. } if t == &target && o == &origin
    )));
}

#[test]
fn matching_invitation_allows_auth_but_changed_bootstrap_is_revoked() {
    const ID: &str = "4be44aef-8c98-4db6-a9d5-81c2eb311330";
    let server = MockHttp::new(|path| match path {
        "/api/health" => Reply::json(200, invitation_health(ID)),
        "/api/register" => Reply::json(
            200,
            json!({"token":"owned-new-session", "user":{"id":"owned-new-user","username":"owned-new","role":"member"}}),
        ),
        "/api/bootstrap" => Reply::json(
            200,
            serde_json::to_value(Bootstrap {
                server_id: "49eb3c90-3520-4932-ae14-2f63ec80c38b".into(),
                ..Default::default()
            })
            .unwrap(),
        ),
        "/api/logout" => Reply::json(200, json!({})),
        _ => Reply::json(404, json!({})),
    });
    let mut client = TestWorker::new();
    client.connection("http://previous-community.test".into());
    let result = client.worker.handle(Command::Auth {
        authority: None,
        url: server.url.clone(),
        expected_server_id: Some(ID.into()),
        register: true,
        request_id: 0,
        request: AuthRequest {
            second_factor: None,
            initial_protection: None,
            username: "owned-new".into(),
            password: "owned-password".into(),
            invite: "owned-code".into(),
        },
    });
    assert!(result.unwrap_err().contains("changed during sign-in"));
    assert_eq!(
        client.worker.connection.as_ref().unwrap().url,
        "http://previous-community.test"
    );
    assert!(client.drain().is_empty());
    let requests = server.requests.lock().unwrap();
    assert_eq!(
        requests.iter().map(|r| r.path.as_str()).collect::<Vec<_>>(),
        vec![
            "/api/health",
            "/api/register",
            "/api/bootstrap",
            "/api/logout"
        ]
    );
    assert!(requests[0].body.is_empty() && requests[0].authorization.is_none());
    let registration: AuthRequest = serde_json::from_slice(&requests[1].body).unwrap();
    assert_eq!(registration.invite, "owned-code");
    assert_eq!(
        requests[3].authorization.as_deref(),
        Some("Bearer owned-new-session")
    );
}

#[test]
fn invitation_generation_rejects_a_stale_community_origin() {
    let server = MockHttp::new(|_| Reply::json(200, json!({"code":"owned-code"})));
    let mut client = TestWorker::new();
    client.connection(server.url.clone());
    let mut origin = client.command_origin();
    origin.user_id = "old-account".into();
    assert!(client
        .worker
        .handle(Command::Invite {
            origin,
            request_id: None
        })
        .is_err());
    assert!(server.requests.lock().unwrap().is_empty());
    let origin = client.command_origin();
    let updates = client.handle(Command::Invite {
        origin: origin.clone(),
        request_id: Some(73),
    });
    assert!(updates.iter().any(|update| matches!(update, Update::Invite { origin: returned, code, request_id: Some(73), .. } if returned == &origin && code == "owned-code")));
}

#[test]
fn mark_community_read_uses_only_its_own_channel_markers_and_session() {
    for background in [false, true] {
        let server = MockHttp::with_request(|request| {
            assert_eq!(
                request.authorization.as_deref(),
                Some("Bearer fixture-token")
            );
            match request.path.as_str() {
                "/api/bootstrap" => Reply::json(
                    200,
                    serde_json::to_value(Bootstrap {
                        server_id: "owned-server".into(),
                        user: User {
                            id: "fixture-user".into(),
                            ..Default::default()
                        },
                        protocol_version: API_PROTOCOL_VERSION,
                        channels: vec![
                            Channel {
                                id: "unread".into(),
                                ..Default::default()
                            },
                            Channel {
                                id: "read".into(),
                                ..Default::default()
                            },
                        ],
                        read_states: vec![
                            ReadState {
                                channel_id: Some("unread".into()),
                                unread_count: 3,
                                ..Default::default()
                            },
                            ReadState {
                                channel_id: Some("unread".into()),
                                unread_count: 2,
                                ..Default::default()
                            },
                            ReadState {
                                channel_id: Some("read".into()),
                                ..Default::default()
                            },
                            ReadState {
                                channel_id: Some("not-visible".into()),
                                unread_count: 1,
                                ..Default::default()
                            },
                            ReadState {
                                peer_id: Some("private-friend".into()),
                                unread_count: 1,
                                ..Default::default()
                            },
                        ],
                        ..Default::default()
                    })
                    .unwrap(),
                ),
                "/api/read-state" => {
                    assert_eq!(request.method, "PATCH");
                    let read: MarkRead = serde_json::from_slice(&request.body).unwrap();
                    assert_eq!(read.channel_id.as_deref(), Some("unread"));
                    assert_eq!(read.peer_id, None);
                    assert_eq!(read.last_read_sequence, u64::MAX);
                    Reply::json(
                        200,
                        serde_json::to_value(ReadState {
                            channel_id: read.channel_id,
                            last_read_sequence: 42,
                            ..Default::default()
                        })
                        .unwrap(),
                    )
                }
                _ => panic!("Unexpected server action request"),
            }
        });
        let mut client = TestWorker::new();
        client.connection(server.url.clone());
        client.worker.connection.as_mut().unwrap().server_id = "owned-server".into();
        let origin = client.command_origin();
        if background {
            let saved = client.worker.connection.take().unwrap();
            client.worker.saved_connections.insert(
                server.url.clone(),
                SavedConnection {
                    connection: saved,
                    stop: Arc::new(AtomicBool::new(false)),
                    dirty: Arc::new(AtomicBool::new(false)),
                },
            );
            client.connection("http://127.0.0.1:4580".into());
            client.worker.connection.as_mut().unwrap().session.token =
                "active-token-never-sent".into();
        }
        let active_origin = client.command_origin();
        let updates = client.handle(Command::MarkCommunityRead { origin });
        assert_eq!(client.command_origin(), active_origin);
        assert_eq!(
            client
                .worker
                .connection
                .as_ref()
                .unwrap()
                .voice_channel
                .as_deref(),
            Some("voice-room")
        );
        assert!(updates.iter().any(|update| if background {
            matches!(update, Update::CommunityBootstrap { url, .. } if url == &server.url)
        } else {
            matches!(update, Update::Bootstrap(_))
        }));
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests.iter().filter(|r| r.method == "PATCH").count(), 1);
    }
}

#[test]
fn mark_community_read_rejects_stale_account_and_changed_server_before_writing() {
    for changed in ["account", "server"] {
        let server = MockHttp::new(move |_| {
            Reply::json(
                200,
                serde_json::to_value(Bootstrap {
                    server_id: if changed == "server" {
                        "replaced-server"
                    } else {
                        "owned-server"
                    }
                    .into(),
                    user: User {
                        id: "fixture-user".into(),
                        ..Default::default()
                    },
                    protocol_version: API_PROTOCOL_VERSION,
                    ..Default::default()
                })
                .unwrap(),
            )
        });
        let mut client = TestWorker::new();
        client.connection(server.url.clone());
        client.worker.connection.as_mut().unwrap().server_id = "owned-server".into();
        let mut origin = client.command_origin();
        if changed == "account" {
            origin.user_id = "previous-account".into();
        }
        assert!(client
            .worker
            .handle(Command::MarkCommunityRead { origin })
            .is_err());
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests.len(), usize::from(changed == "server"));
        assert!(requests.iter().all(|r| r.method == "GET"));
        assert!(client.drain().is_empty());
    }
}

#[test]
fn invitation_copy_failures_keep_the_request_identity_and_never_use_a_stale_account() {
    let server = MockHttp::new(|_| Reply::json(403, json!({"error":"Not allowed"})));
    let mut client = TestWorker::new();
    client.connection(server.url.clone());
    let mut stale = client.command_origin();
    stale.user_id = "old-account".into();
    let updates = client.handle(Command::Invite {
        origin: stale.clone(),
        request_id: Some(81),
    });
    assert!(updates.iter().any(|update| matches!(update,
        Update::InviteFailed {origin, request_id:81, ..} if origin == &stale)));
    assert!(server.requests.lock().unwrap().is_empty());
    let current = client.command_origin();
    let updates = client.handle(Command::Invite {
        origin: current.clone(),
        request_id: Some(82),
    });
    assert!(updates.iter().any(|update| matches!(update,
        Update::InviteFailed {origin, request_id:82, error} if origin == &current && !error.is_empty())));
    assert!(!updates
        .iter()
        .any(|update| matches!(update, Update::Invite { .. })));
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, "/api/invites");
}

#[test]
fn disappearing_and_oversized_frame_does_not_discard_later_live_frame() {
    let rgb = image::RgbImage::from_pixel(1, 1, image::Rgb([1, 2, 3]));
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new(&mut jpeg)
        .encode_image(&rgb)
        .unwrap();
    let live_jpeg = jpeg.clone();
    let frame = |id: &str, user: &str| {
        json!({
            "user":{"id":user,"username":user,"role":"member"},"kind":"screen", "frame_id":id,
            "url":format!("/api/media/frames/{id}"),"width":1,"height":1
        })
    };
    let frames = json!([frame("gone", "a"), frame("huge", "b"), frame("live", "c")]);
    let server = MockHttp::new(move |path| match path {
        "/api/media/frames?channel_id=voice-room" => Reply::json(200, frames.clone()),
        "/api/media/frames/gone" => Reply::json(404, json!({"error":"frame replaced"})),
        "/api/media/frames/huge" => Reply {
            status: 200,
            headers: Vec::new(),
            body: vec![0; MAX_FRAME_BYTES + 1],
        },
        "/api/media/frames/live" => Reply {
            status: 200,
            headers: Vec::new(),
            body: jpeg.clone(),
        },
        _ => Reply::json(404, json!({"error":"missing"})),
    });
    let mut client = TestWorker::new();
    client.connection(server.url.clone());
    client.worker.poll_frames();
    let updates = client.drain();
    let frames = updates
        .into_iter()
        .find_map(|update| match update {
            Update::Frames(frames) => Some(frames),
            _ => None,
        })
        .unwrap_or_else(|| {
            panic!(
                "remaining live frame must survive stale-frame404; requests: {:?}",
                server.paths.lock().unwrap()
            )
        });
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].0.frame_id, "live");
    assert_eq!(frames[0].1, live_jpeg);
    let connection = client.worker.connection.as_ref().unwrap();
    assert_eq!(
        connection.frame_ids.get("c:screen").map(String::as_str),
        Some("live")
    );
    assert!(!connection.frame_ids.contains_key("a:screen"));
    assert!(!connection.frame_ids.contains_key("b:screen"));
    client.worker.poll_frames();
    assert!(
        client.drain().is_empty(),
        "unchanged frame is not emitted again"
    );
    assert_eq!(
        server
            .paths
            .lock()
            .unwrap()
            .iter()
            .filter(|p| *p == "/api/media/frames/live")
            .count(),
        1
    );
}

#[test]
fn cancelled_capture_epochs_never_upload_to_a_later_room_or_restart() {
    let server = MockHttp::new(|_| Reply {
        status: 204,
        headers: Vec::new(),
        body: Vec::new(),
    });
    let mut client = TestWorker::new();
    client.connection(server.url.clone());
    client.worker.connection.as_mut().unwrap().sharing = true;
    let rgb = image::RgbImage::from_pixel(1, 1, image::Rgb([1, 2, 3]));
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new(&mut jpeg)
        .encode_image(&rgb)
        .unwrap();
    let frame = |kind: &str, generation| CaptureFrame {
        kind: kind.into(),
        generation,
        jpeg: jpeg.clone(),
        width: 1,
        height: 1,
    };
    client.worker.upload_frame(frame("screen", 0));
    client.worker.capture_epochs[0].store(2, Ordering::SeqCst);
    client.worker.upload_frame(frame("screen", 0));
    client.worker.upload_frame(frame("screen", 1));
    assert!(server.paths.lock().unwrap().is_empty());
    client.worker.upload_frame(frame("screen", 2));
    assert_eq!(server.paths.lock().unwrap().len(), 1);

    // The UI invalidates an epoch immediately, before queued StopCapture/LeaveVoice
    // commands change Worker.connection. Late frames must already be rejected here.
    client.worker.capture_epochs[0].store(3, Ordering::SeqCst);
    client.worker.upload_frame(frame("screen", 2));
    assert_eq!(server.paths.lock().unwrap().len(), 1);
    client.worker.connection.as_mut().unwrap().voice_channel = None;
    client.worker.upload_frame(frame("screen", 3));
    assert_eq!(server.paths.lock().unwrap().len(), 1);

    client.worker.capture_epochs[0].store(4, Ordering::SeqCst);
    client.worker.connection.as_mut().unwrap().voice_channel = Some("new-room".into());
    client.worker.upload_frame(frame("screen", 2));
    client.worker.upload_frame(frame("screen", 3));
    client.worker.upload_frame(frame("screen", 0));
    assert_eq!(server.paths.lock().unwrap().len(), 1);
    client.worker.upload_frame(frame("screen", 4));
    assert_eq!(server.paths.lock().unwrap().len(), 2);

    // Restarting a camera invalidates only camera frames; the screen stays active.
    client.worker.connection.as_mut().unwrap().camera = true;
    client.worker.capture_epochs[1].store(8, Ordering::SeqCst);
    client.worker.upload_frame(frame("camera", 7));
    assert_eq!(server.paths.lock().unwrap().len(), 2);
    client.worker.upload_frame(frame("screen", 4));
    client.worker.upload_frame(frame("camera", 8));
    assert_eq!(server.paths.lock().unwrap().len(), 4);
    assert!(client.drain().is_empty());
    assert!(server
        .paths
        .lock()
        .unwrap()
        .iter()
        .all(|path| path.starts_with("/api/media/frame?")));
}

#[test]
fn slow_background_community_does_not_block_active_selection_or_revive_forgotten_login() {
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let release_rx = std::sync::Mutex::new(release_rx);
    let server = MockHttp::new(move |_| {
        entered_tx.send(()).unwrap();
        release_rx
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        Reply::json(200, serde_json::to_value(Bootstrap::default()).unwrap())
    });
    let mut client = TestWorker::new();
    client.connection(server.url.clone());
    let old = client.worker.connection.take().unwrap();
    client.worker.saved_connections.insert(
        server.url.clone(),
        SavedConnection {
            connection: old,
            stop: Arc::new(AtomicBool::new(false)),
            dirty: Arc::new(AtomicBool::new(true)),
        },
    );
    client.connection("http://127.0.0.1:4580".into());
    client.worker.background_at = Instant::now() - Duration::from_secs(1);
    let began = Instant::now();
    client.worker.refresh_background();
    assert!(began.elapsed() < Duration::from_millis(200));
    entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    client.handle(Command::SelectConversation(Target::Direct("friend".into())));
    assert_eq!(
        client.worker.connection.as_ref().unwrap().current,
        Some(Target::Direct("friend".into()))
    );
    // A logout/forget can finish while a previous HTTP request is still in flight.
    client.worker.saved_connections.remove(&server.url);
    release_tx.send(()).unwrap();
    let until = Instant::now() + Duration::from_secs(2);
    while client.worker.background_active.load(Ordering::SeqCst) != 0 {
        assert!(Instant::now() < until);
        thread::sleep(Duration::from_millis(5));
    }
    client.worker.refresh_background();
    assert!(client.drain().is_empty());
    assert!(client.worker.saved_connections.is_empty());
}

#[test]
fn background_refresh_preserves_new_events_and_rejects_previous_account_results() {
    let mut client = TestWorker::new();
    let url = "http://127.0.0.1:4580".to_string();
    client.connection(url.clone());
    let connection = client.worker.connection.take().unwrap();
    client.worker.saved_connections.insert(
        url.clone(),
        SavedConnection {
            connection,
            stop: Arc::new(AtomicBool::new(false)),
            dirty: Arc::new(AtomicBool::new(true)),
        },
    );
    client
        .worker
        .background_completed
        .send(BackgroundResult {
            url: url.clone(),
            token: "previous-account".into(),
            protections: Vec::new(),
            keys: Ok(Vec::new()),
            bootstrap: Ok(Bootstrap::default()),
            changes: None,
        })
        .unwrap();
    client.worker.refresh_background();
    assert!(client.drain().is_empty());
    let bootstrap = Bootstrap {
        latest_change_sequence: 4,
        ..Default::default()
    };
    client
        .worker
        .background_completed
        .send(BackgroundResult {
            url: url.clone(),
            token: "fixture-token".into(),
            bootstrap: Ok(bootstrap),
            protections: Vec::new(),
            keys: Ok(Vec::new()),
            changes: Some(Ok(ChangePage {
                changes: Vec::new(),
                next_cursor: 4,
                has_more: false,
            })),
        })
        .unwrap();
    client.worker.refresh_background();
    let saved = &client.worker.saved_connections[&url];
    assert!(
        saved.dirty.load(Ordering::Relaxed),
        "events after the request began must trigger another refresh"
    );
    assert_eq!(saved.connection.change_cursor, 4);
    assert!(client
        .drain()
        .iter()
        .any(|update| matches!(update, Update::CommunityBootstrap { .. })));
}

#[test]
fn background_key_failure_preserves_change_cursor_and_emits_no_partial_messages() {
    let mut client = TestWorker::new();
    let url = "http://127.0.0.1:4580".to_string();
    client.connection(url.clone());
    let connection = client.worker.connection.take().unwrap();
    let original_cursor = connection.change_cursor;
    client.worker.saved_connections.insert(
        url.clone(),
        SavedConnection {
            connection,
            stop: Arc::new(AtomicBool::new(false)),
            dirty: Arc::new(AtomicBool::new(false)),
        },
    );
    client
        .worker
        .background_completed
        .send(BackgroundResult {
            url: url.clone(),
            token: "fixture-token".into(),
            bootstrap: Ok(Bootstrap {
                latest_change_sequence: 99,
                social: SocialState {
                    blocked: vec!["changed-public-policy".into()],
                    ..Default::default()
                },
                ..Default::default()
            }),
            changes: Some(Ok(ChangePage {
                next_cursor: 99,
                ..Default::default()
            })),
            keys: Err("Interrupted authenticated key catch-up".into()),
            protections: Vec::new(),
        })
        .unwrap();
    client.worker.refresh_background();
    let saved = &client.worker.saved_connections[&url];
    assert_eq!(saved.connection.change_cursor, original_cursor);
    assert!(saved.dirty.load(Ordering::Relaxed));
    let updates = client.drain();
    assert_eq!(
        updates.len(),
        1,
        "No partial private delivery or protection result may escape a key failure"
    );
    assert!(
        matches!(&updates[0], Update::CommunityBootstrap { url: owner, bootstrap }
        if owner == &url && bootstrap.social.blocked == ["changed-public-policy"]),
        "Verified public access policy must update while private catch-up remains pending"
    );
}

#[test]
fn background_message_steps_yield_and_cancel_without_advancing_the_cursor() {
    let mut client = TestWorker::new();
    let url = "http://127.0.0.1:4580".to_string();
    client.connection(url.clone());
    let connection = client.worker.connection.take().unwrap();
    client.worker.saved_connections.insert(
        url.clone(),
        SavedConnection {
            connection,
            stop: Arc::new(AtomicBool::new(false)),
            dirty: Arc::new(AtomicBool::new(false)),
        },
    );
    client.connection("http://active-community.test".into());
    client.worker.connection.as_mut().unwrap().current =
        Some(Target::Channel("active-room".into()));
    let changes = ["one", "two"]
        .into_iter()
        .enumerate()
        .map(|(i, room)| MessageChange {
            sequence: i as u64 + 1,
            message_id: format!("message-{room}"),
            channel_id: Some(room.into()),
            message: Some(Message {
                id: format!("message-{room}"),
                channel_id: Some(room.into()),
                content: "legacy owned fixture".into(),
                ..Default::default()
            }),
            ..Default::default()
        })
        .collect();
    client
        .worker
        .background_completed
        .send(BackgroundResult {
            url: url.clone(),
            token: "fixture-token".into(),
            bootstrap: Ok(Bootstrap {
                latest_change_sequence: 2,
                ..Default::default()
            }),
            changes: Some(Ok(ChangePage {
                changes,
                next_cursor: 2,
                has_more: false,
            })),
            keys: Ok(Vec::new()),
            protections: Vec::new(),
        })
        .unwrap();
    client.worker.refresh_background();
    assert_eq!(client.worker.background_pending.len(), 1);
    assert_eq!(
        client.worker.saved_connections[&url]
            .connection
            .change_cursor,
        0
    );
    assert!(
        client.drain().is_empty(),
        "partial changes must not reach notification/UI projections"
    );
    client.worker.handle(Command::ClearSelection).unwrap();
    assert!(client.worker.connection.as_ref().unwrap().current.is_none());
    // Removing the saved connection is the local Forget boundary, independently
    // of a remote logout acknowledgement. No actor/cursor is restored by the job.
    client.worker.saved_connections.remove(&url);
    client.worker.refresh_background();
    assert!(client.worker.background_pending.is_empty());
    assert!(client.drain().is_empty());
}

#[test]
fn background_steps_deliver_one_ordered_page_after_all_targets_finish() {
    let mut client = TestWorker::new();
    let url = "http://127.0.0.1:4580".to_string();
    client.connection(url.clone());
    let connection = client.worker.connection.take().unwrap();
    client.worker.saved_connections.insert(
        url.clone(),
        SavedConnection {
            connection,
            stop: Arc::new(AtomicBool::new(false)),
            dirty: Arc::new(AtomicBool::new(false)),
        },
    );
    let changes = ["one", "two", "one"]
        .into_iter()
        .enumerate()
        .map(|(i, room)| MessageChange {
            sequence: i as u64 + 1,
            message_id: format!("message-{room}-{i}"),
            channel_id: Some(room.into()),
            message: Some(Message {
                id: format!("message-{room}-{i}"),
                channel_id: Some(room.into()),
                content: "owned legacy fixture".into(),
                ..Default::default()
            }),
            ..Default::default()
        })
        .collect();
    client
        .worker
        .background_completed
        .send(BackgroundResult {
            url: url.clone(),
            token: "fixture-token".into(),
            bootstrap: Ok(Bootstrap {
                latest_change_sequence: 3,
                ..Default::default()
            }),
            changes: Some(Ok(ChangePage {
                changes,
                next_cursor: 3,
                has_more: false,
            })),
            keys: Ok(Vec::new()),
            protections: Vec::new(),
        })
        .unwrap();
    for _ in 0..2 {
        client.worker.refresh_background();
        assert_eq!(
            client.worker.saved_connections[&url]
                .connection
                .change_cursor,
            0
        );
        assert!(client.drain().is_empty());
    }
    client.worker.refresh_background();
    let updates = client.drain();
    let pages: Vec<_> = updates
        .iter()
        .filter_map(|u| match u {
            Update::Changes { changes, .. } => Some(changes),
            _ => None,
        })
        .collect();
    assert_eq!(pages.len(), 1);
    assert_eq!(
        pages[0]
            .iter()
            .map(|change| change.sequence)
            .collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    assert_eq!(
        client.worker.saved_connections[&url]
            .connection
            .change_cursor,
        3
    );
    client.worker.refresh_background();
    assert!(client.drain().is_empty());
}

#[test]
fn active_refresh_rejects_changed_identity_before_messages_and_stops_owned_work() {
    for change_account in [false, true] {
        let server = MockHttp::new(move |path| {
            assert_eq!(
                path, "/api/bootstrap",
                "no requests after identity mismatch"
            );
            Reply::json(
                200,
                serde_json::to_value(Bootstrap {
                    server_id: if change_account {
                        "owned-community"
                    } else {
                        "replacement-community"
                    }
                    .into(),
                    user: User {
                        id: if change_account {
                            "other-account"
                        } else {
                            "fixture-user"
                        }
                        .into(),
                        ..Default::default()
                    },
                    protocol_version: API_PROTOCOL_VERSION,
                    ..Default::default()
                })
                .unwrap(),
            )
        });
        let mut client = TestWorker::new();
        client.connection(server.url.clone());
        let c = client.worker.connection.as_mut().unwrap();
        c.server_id = "owned-community".into();
        c.current = Some(Target::Channel("old-channel".into()));
        let transfer = Arc::new(AtomicBool::new(false));
        client
            .worker
            .transfers
            .insert("old-transfer".into(), transfer.clone());
        let socket_stop = client.worker.socket_stop.clone();
        let media_epoch = client.worker.live.epoch.load(Ordering::SeqCst);
        client.worker.refresh();
        assert!(client.worker.connection.is_none());
        assert!(socket_stop.load(Ordering::SeqCst) && transfer.load(Ordering::SeqCst));
        assert!(client.worker.live.epoch.load(Ordering::SeqCst) > media_epoch);
        assert!(client.worker.live.context.lock().unwrap().is_none());
        let updates = client.drain();
        assert!(updates.iter().any(|update| matches!(
            update,
            Update::ConnectionState {
                status: ConnectionStatus::SignInRequired,
                ..
            }
        )));
        assert!(updates.iter().any(|update| matches!(update,
            Update::Error(error) if error == IDENTITY_CHANGED)));
        assert!(!updates.iter().any(|update| matches!(
            update,
            Update::Bootstrap(_)
                | Update::Messages { .. }
                | Update::Changes { .. }
                | Update::Disconnected
        )));
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "GET");
    }
}

#[test]
fn background_http_identity_mismatch_never_fetches_changes_or_replaces_active_account() {
    let server = MockHttp::new(|path| {
        assert_eq!(path, "/api/bootstrap");
        Reply::json(
            200,
            serde_json::to_value(Bootstrap {
                server_id: "replacement-community".into(),
                user: User {
                    id: "fixture-user".into(),
                    ..Default::default()
                },
                protocol_version: API_PROTOCOL_VERSION,
                ..Default::default()
            })
            .unwrap(),
        )
    });
    let mut client = TestWorker::new();
    client.connection(server.url.clone());
    let mut connection = client.worker.connection.take().unwrap();
    connection.server_id = "owned-community".into();
    let stopped = Arc::new(AtomicBool::new(false));
    client.worker.saved_connections.insert(
        server.url.clone(),
        SavedConnection {
            connection,
            stop: stopped.clone(),
            dirty: Arc::new(AtomicBool::new(true)),
        },
    );
    client.connection("http://127.0.0.1:4580".into());
    client.worker.background_at = Instant::now() - Duration::from_secs(1);
    client.worker.refresh_background();
    let until = Instant::now() + Duration::from_secs(3);
    while client.worker.saved_connections.contains_key(&server.url) {
        assert!(Instant::now() < until);
        thread::sleep(Duration::from_millis(5));
        client.worker.refresh_background();
    }
    assert!(stopped.load(Ordering::SeqCst));
    assert_eq!(
        client.worker.connection.as_ref().unwrap().url,
        "http://127.0.0.1:4580"
    );
    assert!(!client.drain().iter().any(|update| matches!(
        update,
        Update::CommunityBootstrap { .. } | Update::Changes { .. }
    )));
    assert_eq!(server.requests.lock().unwrap().len(), 1);
    assert!(!client.worker.background_retry.contains_key(&server.url));
}

#[test]
fn delayed_background_identity_mismatch_drops_already_fetched_changes() {
    let mut client = TestWorker::new();
    let url = "http://127.0.0.1:4599".to_string();
    client.connection(url.clone());
    let mut connection = client.worker.connection.take().unwrap();
    connection.server_id = "owned-community".into();
    let stopped = Arc::new(AtomicBool::new(false));
    client.worker.saved_connections.insert(
        url.clone(),
        SavedConnection {
            connection,
            stop: stopped.clone(),
            dirty: Arc::new(AtomicBool::new(false)),
        },
    );
    client
        .worker
        .background_completed
        .send(BackgroundResult {
            url: url.clone(),
            token: "fixture-token".into(),
            bootstrap: Ok(Bootstrap {
                server_id: "owned-community".into(),
                user: User {
                    id: "other-user".into(),
                    ..Default::default()
                },
                ..Default::default()
            }),
            changes: Some(Ok(ChangePage {
                next_cursor: 99,
                ..Default::default()
            })),
            keys: Ok(Vec::new()),
            protections: Vec::new(),
        })
        .unwrap();
    client.worker.refresh_background();
    assert!(stopped.load(Ordering::SeqCst));
    assert!(!client.worker.saved_connections.contains_key(&url));
    assert!(!client.drain().iter().any(|update| matches!(
        update,
        Update::CommunityBootstrap { .. } | Update::Changes { .. }
    )));
}

#[test]
fn saved_switch_preflight_preserves_call_and_serializes_only_a_validated_transition() {
    for outcome in ["http-failure", "identity-change", "success"] {
        let (entered_tx, entered_rx) = mpsc::channel();
        let releases = Arc::new((std::sync::Mutex::new(false), Condvar::new()));
        let release = releases.clone();
        let destination = MockHttp::new(move |path| {
            assert_eq!(path, "/api/bootstrap");
            entered_tx.send(()).unwrap();
            let (lock, ready) = &*release;
            let released = *ready
                .wait_timeout_while(lock.lock().unwrap(), Duration::from_secs(4), |released| {
                    !*released
                })
                .unwrap()
                .0;
            if !released || outcome == "http-failure" {
                return Reply::json(503, json!({"error":"destination unavailable"}));
            }
            Reply::json(
                200,
                serde_json::to_value(Bootstrap {
                    server_id: if outcome == "identity-change" {
                        "replacement-community"
                    } else {
                        "owned-destination"
                    }
                    .into(),
                    user: User {
                        id: "fixture-user".into(),
                        ..Default::default()
                    },
                    protocol_version: API_PROTOCOL_VERSION,
                    ..Default::default()
                })
                .unwrap(),
            )
        });
        let active = MockHttp::new(|path| {
            assert_eq!(path, "/api/voice/leave");
            Reply::json(200, json!({}))
        });
        let destination_url = destination.url.clone();
        let active_url = active.url.clone();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let (done_tx, done_rx) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            let mut client = TestWorker::new();
            client.connection(destination_url.clone());
            let mut saved = client.worker.connection.take().unwrap();
            saved.server_id = "owned-destination".into();
            saved.protocol_version = API_PROTOCOL_VERSION;
            saved.voice_channel = None;
            client.worker.saved_connections.insert(
                destination_url.clone(),
                SavedConnection {
                    connection: saved,
                    stop: Arc::new(AtomicBool::new(false)),
                    dirty: Arc::new(AtomicBool::new(false)),
                },
            );
            client.connection(active_url.clone());
            let c = client.worker.connection.as_mut().unwrap();
            c.server_id = "active-community".into();
            c.voice_attempt = Some(93);
            c.sharing = true;
            c.camera = true;
            c.frame_ids
                .insert("fixture-user/screen".into(), "current-frame".into());
            client.worker.live.epoch.store(41, Ordering::SeqCst);
            *client.worker.live.context.lock().unwrap() = Some(LiveConnection {
                url: active_url.clone(),
                token: "fixture-token".into(),
                channel_id: "voice-room".into(),
                generation: 41,
                sharing: true,
                camera: true,
            });
            ready_tx.send(client.worker.live.clone()).unwrap();
            let result = client.worker.handle(Command::SwitchCommunity {
                url: destination_url.clone(),
            });
            let c = client.worker.connection.as_ref().unwrap();
            if outcome == "success" {
                result.unwrap();
                assert_eq!(c.url, destination_url);
                assert!(c.voice_channel.is_none());
                assert!(client.worker.live.snapshot().is_none());
                assert!(client.worker.live.epoch.load(Ordering::SeqCst) > 41);
                assert!(client.drain().iter().any(|u| matches!(
                    u, Update::Connected { url, .. } if url == &destination_url
                )));
            } else {
                assert_eq!(
                    result.unwrap_err(),
                    if outcome == "identity-change" {
                        IDENTITY_CHANGED
                    } else {
                        "destination unavailable"
                    }
                );
                assert_eq!(c.url, active_url);
                assert_eq!(c.server_id, "active-community");
                assert_eq!(c.voice_channel.as_deref(), Some("voice-room"));
                assert_eq!(c.voice_attempt, Some(93));
                assert!(c.sharing && c.camera);
                assert_eq!(c.frame_ids["fixture-user/screen"], "current-frame");
                assert_eq!(client.worker.live.epoch.load(Ordering::SeqCst), 41);
                assert!(client.worker.live.snapshot().is_some());
                assert!(!client.drain().iter().any(|u| matches!(
                    u,
                    Update::Connected { .. } | Update::VoiceLeft | Update::VoiceCallFailed { .. }
                )));
            }
            done_tx.send(()).unwrap();
        });
        let live = ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let entered = entered_rx.recv_timeout(Duration::from_secs(5));
        // Observe the real worker while its authenticated HTTP request is held.
        // Retain the gate through the reply to check the successful commit waits.
        let gate = live.gate.try_lock().ok();
        let gate_available = gate.is_some();
        let before = live.snapshot().unwrap();
        let context_preserved = before.url == active.url
            && before.channel_id == "voice-room"
            && before.token == "fixture-token"
            && before.generation == 41
            && before.sharing
            && before.camera
            && live.epoch.load(Ordering::SeqCst) == 41;
        *releases.0.lock().unwrap() = true;
        releases.1.notify_all();
        let commit_waited = outcome != "success"
            || !gate_available
            || matches!(
                done_rx.recv_timeout(Duration::from_millis(100)),
                Err(mpsc::RecvTimeoutError::Timeout)
            );
        drop(gate);
        let completed = done_rx.recv_timeout(Duration::from_secs(5));
        worker.join().unwrap();
        assert!(entered.is_ok(), "{outcome}: HTTP preflight did not start");
        assert!(
            gate_available,
            "{outcome}: HTTP preflight held the media gate"
        );
        assert!(
            context_preserved,
            "{outcome}: HTTP preflight invalidated active media"
        );
        assert!(commit_waited, "successful switch bypassed the media gate");
        assert!(completed.is_ok(), "{outcome}: switch did not finish");
        let requests = active.requests.lock().unwrap();
        assert_eq!(requests.len(), usize::from(outcome == "success"));
        assert!(requests.iter().all(|r| r.path == "/api/voice/leave"));
        assert_eq!(destination.requests.lock().unwrap().len(), 1);
    }
}

#[test]
fn authenticated_call_clock_retains_response_receipt_while_ui_updates_are_undrained() {
    let server = MockHttp::new(|_| panic!("receipt test must not start a fetch"));
    let alerts = crate::call_alerts::Alerts::new(
        crate::notifications::Notifications::new(eframe::egui::Context::default()),
        None,
    );
    let mut client = TestWorker::new();
    client.connection(server.url.clone());
    client.worker.call_alerts = Some(alerts.clone());
    client.worker.connection.as_mut().unwrap().server_id = "owned-call-service".into();
    let mut bootstrap = Bootstrap {
        server_id: "owned-call-service".into(),
        user: User {
            id: "fixture-user".into(),
            ..Default::default()
        },
        capabilities: vec!["private-calls-v1".into()],
        protocol_version: API_PROTOCOL_VERSION,
        ..Default::default()
    };
    bootstrap.social.server_time = 100;
    let received = Instant::now() - Duration::from_secs(10);
    let _watch = client
        .worker
        .call_alert_watch(&bootstrap, received)
        .unwrap();
    while client.worker.updates.try_send(Update::Busy(false)).is_ok() {}
    let origin = CommandOrigin::from(client.worker.connection.as_ref().unwrap());
    let clock = alerts.clock(&origin).unwrap();
    assert!((110..=111).contains(&clock.unix_seconds));
    assert!(clock.server_sample_age.unwrap() >= Duration::from_secs(10));
    bootstrap.user.id = "wrong-account".into();
    bootstrap.social.server_time = 999;
    client
        .worker
        .record_call_bootstrap(&origin, &bootstrap, Instant::now());
    assert!((110..=111).contains(&alerts.clock(&origin).unwrap().unix_seconds));
    assert_eq!(client.drain().len(), 32);
    assert!(server.requests.lock().unwrap().is_empty());
}

#[test]
fn forgotten_call_watcher_stops_authenticated_fetches_before_logout_is_drained() {
    let server = MockHttp::new(|_| panic!("forgotten watcher must not fetch"));
    let alerts = crate::call_alerts::Alerts::new(
        crate::notifications::Notifications::new(eframe::egui::Context::default()),
        None,
    );
    let mut client = TestWorker::new();
    client.connection(server.url.clone());
    client.worker.call_alerts = Some(alerts.clone());
    client.worker.connection.as_mut().unwrap().server_id = "owned-call-service".into();
    let bootstrap = Bootstrap {
        server_id: "owned-call-service".into(),
        user: User {
            id: "fixture-user".into(),
            ..Default::default()
        },
        capabilities: vec!["private-calls-v1".into()],
        protocol_version: API_PROTOCOL_VERSION,
        ..Default::default()
    };
    let mut old = client
        .worker
        .call_alert_watch(&bootstrap, Instant::now())
        .unwrap();
    alerts.forget_url(&server.url);
    thread::sleep(Duration::from_millis(210));
    old.tick(true);
    // Re-authenticating the same origin must not revive the old credential.
    let _replacement = client
        .worker
        .call_alert_watch(&bootstrap, Instant::now())
        .unwrap();
    old.tick(true);
    assert!(server.requests.lock().unwrap().is_empty());
}

#[test]
fn call_alert_subscriber_refreshes_and_stops_with_a_full_unpainted_ui_queue() {
    let accepted = Arc::new(AtomicBool::new(false));
    let server_accepted = accepted.clone();
    let server = MockHttp::new(move |path| {
        assert_eq!(path, "/api/bootstrap");
        let mut b = Bootstrap {
            server_id: "owned-call-service".into(),
            user: User {
                id: "fixture-user".into(),
                ..Default::default()
            },
            protocol_version: API_PROTOCOL_VERSION,
            capabilities: vec!["private-calls-v1".into()],
            ..Default::default()
        };
        b.members.push(User {
            id: "friend".into(),
            username: "Owned friend".into(),
            ..Default::default()
        });
        b.social.friends.push("friend".into());
        b.social.calls.push(relay_protocol::PrivateCall {
            id: "27234a1a-4207-4919-96c0-12fafc8675e8".into(),
            channel_id: "pair-room".into(),
            caller_id: "friend".into(),
            recipient_id: "fixture-user".into(),
            state: if server_accepted.load(Ordering::Acquire) {
                "accepted"
            } else {
                "ringing"
            }
            .into(),
            expires_at: SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs()
                + 40,
        });
        Reply::json(200, serde_json::to_value(b).unwrap())
    });
    let alerts = crate::call_alerts::Alerts::new(
        crate::notifications::Notifications::new(eframe::egui::Context::default()),
        None,
    );
    let mut policy = crate::call_alerts::Policy::default();
    policy.audio.sounds_enabled = false;
    alerts.configure(policy); // This HTTP/queue fixture opens no playback or toast.
    let mut client = TestWorker::new();
    client.connection(server.url.clone());
    client.worker.call_alerts = Some(alerts.clone());
    client.worker.connection.as_mut().unwrap().server_id = "owned-call-service".into();
    let bootstrap = Bootstrap {
        server_id: "owned-call-service".into(),
        user: User {
            id: "fixture-user".into(),
            ..Default::default()
        },
        protocol_version: API_PROTOCOL_VERSION,
        capabilities: vec!["private-calls-v1".into()],
        ..Default::default()
    };
    let mut watch = client
        .worker
        .call_alert_watch(&bootstrap, Instant::now())
        .unwrap();
    // No GUI update/drain is performed. The same entrypoint used by the event
    // subscriber must observe a call even while Worker::emit would be blocked.
    let mut queued = 0;
    while client.worker.updates.try_send(Update::Busy(false)).is_ok() {
        queued += 1;
    }
    assert_eq!(queued, 32);
    thread::sleep(Duration::from_millis(210));
    watch.tick(true);
    assert_eq!(alerts.active_rings(), 1);
    accepted.store(true, Ordering::Release);
    thread::sleep(Duration::from_millis(210));
    watch.tick(true);
    assert_eq!(alerts.active_rings(), 0);
    assert_eq!(client.drain().len(), queued);
    client.worker.live.shutdown.store(true, Ordering::Release);
    watch.tick(false);
    assert_eq!(alerts.active_rings(), 0);
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests
        .iter()
        .all(|r| r.authorization.as_deref() == Some("Bearer fixture-token")));
}

#[test]
fn switching_to_changed_identity_preserves_active_connection_and_never_sends_logout() {
    let server = MockHttp::new(|path| {
        assert_eq!(path, "/api/bootstrap");
        Reply::json(
            200,
            serde_json::to_value(Bootstrap {
                server_id: "replacement-community".into(),
                user: User {
                    id: "fixture-user".into(),
                    ..Default::default()
                },
                protocol_version: API_PROTOCOL_VERSION,
                ..Default::default()
            })
            .unwrap(),
        )
    });
    let mut client = TestWorker::new();
    client.connection(server.url.clone());
    let mut connection = client.worker.connection.take().unwrap();
    connection.server_id = "owned-community".into();
    client.worker.saved_connections.insert(
        server.url.clone(),
        SavedConnection {
            connection,
            stop: Arc::new(AtomicBool::new(false)),
            dirty: Arc::new(AtomicBool::new(false)),
        },
    );
    client.connection("http://127.0.0.1:4580".into());
    let generation = client.worker.live.epoch.load(Ordering::SeqCst);
    assert_eq!(
        client
            .worker
            .handle(Command::SwitchCommunity {
                url: server.url.clone()
            })
            .unwrap_err(),
        IDENTITY_CHANGED
    );
    assert_eq!(
        client.worker.connection.as_ref().unwrap().url,
        "http://127.0.0.1:4580"
    );
    assert_eq!(client.worker.live.epoch.load(Ordering::SeqCst), generation);
    assert_eq!(server.requests.lock().unwrap().len(), 1);
}

#[test]
fn bootstrap_identity_allows_profile_changes_but_never_adopts_an_unconfirmed_modern_scope() {
    let mut bootstrap = Bootstrap {
        server_id: "owned-community".into(),
        user: User {
            id: "owned-user".into(),
            username: "new-name".into(),
            role: "admin".into(),
            ..Default::default()
        },
        protocol_version: API_PROTOCOL_VERSION,
        ..Default::default()
    };
    assert!(validate_bootstrap_identity("owned-community", "owned-user", &bootstrap).is_ok());
    assert_eq!(
        validate_bootstrap_identity("", "owned-user", &bootstrap).unwrap_err(),
        IDENTITY_UNCONFIRMED
    );
    bootstrap.user.id.clear();
    assert_eq!(
        validate_bootstrap_identity("owned-community", "owned-user", &bootstrap).unwrap_err(),
        IDENTITY_CHANGED
    );
}

fn last_messages(updates: &[Update]) -> &[Message] {
    updates
        .iter()
        .rev()
        .find_map(|update| match update {
            Update::Messages { messages, .. } => Some(messages.as_slice()),
            _ => None,
        })
        .expect("worker should emit refreshed message list")
}

#[test]
fn remembered_login_checks_public_pin_before_token_and_authenticated_scope_afterwards() {
    const PIN: &str = "6f6087b7-a4a8-4dba-b4c5-58bf76b59d6b";
    const OTHER: &str = "f79a7de8-05d0-4297-b56d-7f34cc03a56d";
    for case in [
        "health-id",
        "bootstrap-id",
        "bootstrap-user",
        "valid",
        "legacy",
    ] {
        let server = MockHttp::with_request(move |request| match request.path.as_str() {
            "/api/health" => {
                assert!(
                    request.authorization.is_none(),
                    "public probe must not receive a token"
                );
                Reply::json(
                    200,
                    invitation_health(if case == "health-id" { OTHER } else { PIN }),
                )
            }
            "/api/bootstrap" => {
                assert_ne!(
                    case, "health-id",
                    "a replaced community received a saved credential"
                );
                assert_eq!(
                    request.authorization.as_deref(),
                    Some("Bearer fixture-token")
                );
                Reply::json(
                    200,
                    serde_json::to_value(Bootstrap {
                        server_id: if case == "bootstrap-id" { OTHER } else { PIN }.into(),
                        user: User {
                            id: if case == "bootstrap-user" {
                                "another-user"
                            } else {
                                "fixture-user"
                            }
                            .into(),
                            username: "renamed".into(),
                            ..Default::default()
                        },
                        protocol_version: API_PROTOCOL_VERSION,
                        ..Default::default()
                    })
                    .unwrap(),
                )
            }
            _ => panic!("unexpected remembered-login request"),
        });
        let remembered = crate::credential_vault::RememberedSession {
            authority: None,
            server_id: (case != "legacy").then(|| PIN.into()),
            session: Session {
                token: "fixture-token".into(),
                user: User {
                    id: "fixture-user".into(),
                    ..Default::default()
                },
            },
        };
        let client = TestWorker::new();
        let result = resume_remembered(
            &client.worker.http,
            &server.url,
            &remembered,
            (case == "legacy").then_some(PIN),
        );
        if matches!(case, "valid" | "legacy") {
            let (session, bootstrap, _, _) = result.unwrap();
            assert_eq!(session.user.username, "renamed");
            assert_eq!(bootstrap.server_id, PIN);
            assert_eq!(session.token, remembered.session.token);
        } else {
            assert_eq!(result.unwrap_err(), IDENTITY_CHANGED);
        }
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests.len(), if case == "health-id" { 1 } else { 2 });
        assert_eq!(requests[0].path, "/api/health");
        assert!(requests.iter().all(|request| request.method == "GET"));
        assert_eq!(remembered.session.user.username, "");
        assert!(
            client.worker.connection.is_none(),
            "validation alone must not replace active UI"
        );
    }
}

#[test]
fn unconfirmed_or_conflicting_remembered_pin_never_sends_a_request() {
    let server = MockHttp::new(|_| panic!("invalid remembered scope must be rejected before HTTP"));
    let client = TestWorker::new();
    let mut remembered = crate::credential_vault::RememberedSession {
        authority: None,
        server_id: None,
        session: Session {
            token: "fixture-token".into(),
            user: User {
                id: "fixture-user".into(),
                ..Default::default()
            },
        },
    };
    assert!(resume_remembered(&client.worker.http, &server.url, &remembered, None).is_err());
    assert!(resume_remembered(
        &client.worker.http,
        &server.url,
        &remembered,
        Some("invalid-pin")
    )
    .is_err());
    remembered.server_id = Some("6f6087b7-a4a8-4dba-b4c5-58bf76b59d6b".into());
    assert!(resume_remembered(
        &client.worker.http,
        &server.url,
        &remembered,
        Some("f79a7de8-05d0-4297-b56d-7f34cc03a56d")
    )
    .is_err());
    assert!(server.requests.lock().unwrap().is_empty());
}

#[test]
fn cached_resume_with_conflicting_pin_preserves_connection_and_media_generation() {
    let server = MockHttp::new(|_| panic!("known session conflict must not use HTTP or vault"));
    let mut client = TestWorker::new();
    client.connection(server.url.clone());
    client.worker.connection.as_mut().unwrap().server_id =
        "6f6087b7-a4a8-4dba-b4c5-58bf76b59d6b".into();
    let generation = client.worker.live.epoch.load(Ordering::SeqCst);
    assert_eq!(
        client
            .worker
            .handle(Command::Resume {
                url: server.url.clone(),
                expected_server_id: Some("f79a7de8-05d0-4297-b56d-7f34cc03a56d".into()),
            })
            .unwrap_err(),
        IDENTITY_CHANGED
    );
    assert_eq!(client.worker.live.epoch.load(Ordering::SeqCst), generation);
    assert_eq!(
        client.worker.connection.as_ref().unwrap().session.token,
        "fixture-token"
    );
    assert!(server.requests.lock().unwrap().is_empty());
}

#[test]
fn fresh_sign_in_rejects_bootstrap_for_another_account_without_replacing_current_session() {
    let server = MockHttp::new(|path| match path {
        "/api/login" => Reply::json(
            200,
            json!({"token":"new-token","user":{"id":"new-user","username":"new","role":"member"}}),
        ),
        "/api/bootstrap" => Reply::json(
            200,
            serde_json::to_value(Bootstrap {
                server_id: "6f6087b7-a4a8-4dba-b4c5-58bf76b59d6b".into(),
                user: User {
                    id: "other-user".into(),
                    ..Default::default()
                },
                ..Default::default()
            })
            .unwrap(),
        ),
        _ => panic!("no logout, history or encryption after account mismatch"),
    });
    let mut client = TestWorker::new();
    client.connection("http://prior-community.test".into());
    let stopped = client.worker.socket_stop.clone();
    let epoch = client.worker.live.epoch.load(Ordering::SeqCst);
    assert!(client
        .worker
        .handle(Command::Auth {
            authority: None,
            url: server.url.clone(),
            expected_server_id: None,
            register: false,
            request_id: 0,
            request: AuthRequest {
                second_factor: None,
                initial_protection: None,
                username: "new".into(),
                password: "owned-fixture-password".into(),
                invite: String::new()
            },
        })
        .unwrap_err()
        .contains("different account"));
    assert_eq!(
        client.worker.connection.as_ref().unwrap().url,
        "http://prior-community.test"
    );
    assert!(!stopped.load(Ordering::SeqCst));
    assert_eq!(client.worker.live.epoch.load(Ordering::SeqCst), epoch);
    assert!(client.drain().is_empty());
    assert_eq!(server.requests.lock().unwrap().len(), 2);
}

#[test]
fn idle_refresh_reconciles_auth_and_changes_without_reloading_unchanged_chat() {
    let messages = Arc::new(AtomicUsize::new(0));
    let bootstraps = Arc::new(AtomicUsize::new(0));
    let calls = messages.clone();
    let checks = bootstraps.clone();
    let server = MockHttp::new(move |path| {
        let body = if path.starts_with("/api/bootstrap") {
            checks.fetch_add(1, Ordering::SeqCst);
            serde_json::to_vec(&Bootstrap {
                protocol_version: API_PROTOCOL_VERSION,
                server_id: "fixture-id".into(),
                user: User {
                    id: "fixture-user".into(),
                    ..Default::default()
                },
                ..Default::default()
            })
            .unwrap()
        } else if path.starts_with("/api/changes") {
            serde_json::to_vec(&json!({"changes": [], "next_cursor": 0, "has_more": false}))
                .unwrap()
        } else {
            calls.fetch_add(1, Ordering::SeqCst);
            b"[]".to_vec()
        };
        Reply {
            status: 200,
            headers: Vec::new(),
            body,
        }
    });
    let mut client = TestWorker::new();
    client.connection(server.url.clone());
    let connection = client.worker.connection.as_mut().unwrap();
    connection.voice_channel = None;
    connection.server_id = "fixture-id".into();
    connection.protocol_version = API_PROTOCOL_VERSION;
    connection.current = Some(Target::Channel("text".into()));
    let mut emitted = 0;
    for _ in 0..5 {
        client.worker.refresh();
        emitted += client
            .drain()
            .iter()
            .filter(|u| matches!(u, Update::Bootstrap(_)))
            .count();
    }
    assert_eq!(
        bootstraps.load(Ordering::SeqCst),
        5,
        "auth/permission checks remain frequent"
    );
    assert_eq!(messages.load(Ordering::SeqCst), 0);
    assert_eq!(emitted, 1, "unchanged bootstrap must not wake UI again");
    client.worker.message_repair_at = Instant::now() - Duration::from_secs(31);
    client.worker.refresh();
    client.drain();
    assert_eq!(
        messages.load(Ordering::SeqCst),
        1,
        "repair still reloads chat"
    );
    client.worker.load_messages().unwrap();
    assert!(
        client
            .drain()
            .iter()
            .any(|u| matches!(u, Update::Messages { .. })),
        "explicit conversation loads always deliver"
    );
}
#[test]
fn worker_waits_for_real_deadlines_instead_of_forty_millisecond_polling() {
    let mut client = TestWorker::new();
    // A build with a compiled-in account provider also schedules the invitation
    // locator; this check is about the connection's own deadlines.
    client.worker.host_locator_service = None;
    let now = Instant::now();
    assert!(client.worker.idle_delay(now).is_none());
    client.connection("http://fixture.invalid".into());
    let delay = client.worker.idle_delay(now).unwrap();
    assert!(delay > Duration::from_secs(4) && delay <= Duration::from_secs(5));
    client.worker.refresh_pending.store(true, Ordering::Relaxed);
    assert!(client.worker.idle_delay(now).unwrap() <= Duration::from_millis(400));
}

#[test]
fn unread_navigation_worker_uses_bounded_after_page_without_mark_read() {
    let server = MockHttp::with_request(|request| {
        assert_eq!(request.method, "GET");
        assert_eq!(
            request.authorization.as_deref(),
            Some("Bearer fixture-token")
        );
        let url = reqwest::Url::parse(&format!("http://fixture{}", request.path)).unwrap();
        assert_eq!(url.path(), "/api/history");
        let query: HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(query.len(), 3);
        assert_eq!(query.get("after").unwrap(), "20");
        assert_eq!(query.get("limit").unwrap(), "100");
        assert_eq!(query.get("channel_id").unwrap(), "room");
        Reply::json(
            200,
            serde_json::to_value(HistoryPage {
                messages: vec![Message {
                    id: "unread-21".into(),
                    channel_id: Some("room".into()),
                    sequence: 21,
                    author: User {
                        id: "peer".into(),
                        ..Default::default()
                    },
                    ..Default::default()
                }],
                latest_sequence: 200,
                ..Default::default()
            })
            .unwrap(),
        )
    });
    let mut client = TestWorker::new();
    client.connection(server.url.clone());
    client.worker.connection.as_mut().unwrap().protocol_version = API_PROTOCOL_VERSION;
    let origin = client.command_origin();
    let updates = client.handle(Command::HistoryAfter {
        origin: origin.clone(),
        target: Target::Channel("room".into()),
        after: 20,
        request_id: 38,
    });
    assert!(updates.iter().any(|update| matches!(update, Update::UnreadHistory { origin: returned, target: Target::Channel(room), page, next_after: Some(21), request_id: 38 } if returned == &origin && room == "room" && page.messages[0].id == "unread-21")));
    assert_eq!(server.requests.lock().unwrap().len(), 1);
    let mut stale = origin;
    stale.user_id = "another-account".into();
    assert!(client
        .worker
        .handle(Command::HistoryAfter {
            origin: stale,
            target: Target::Channel("room".into()),
            after: 20,
            request_id: 39
        })
        .is_err());
    assert_eq!(server.requests.lock().unwrap().len(), 1);
}

#[test]
fn unread_navigation_worker_failure_keeps_request_id_and_rejects_wrong_target() {
    let server = MockHttp::new(|_| {
        Reply::json(
            200,
            serde_json::to_value(HistoryPage {
                messages: vec![Message {
                    id: "wrong-room".into(),
                    channel_id: Some("other".into()),
                    sequence: 21,
                    ..Default::default()
                }],
                latest_sequence: 21,
                ..Default::default()
            })
            .unwrap(),
        )
    });
    let mut client = TestWorker::new();
    client.connection(server.url.clone());
    client.worker.connection.as_mut().unwrap().protocol_version = API_PROTOCOL_VERSION;
    client.worker.command(Command::HistoryAfter {
        origin: client.command_origin(),
        target: Target::Channel("room".into()),
        after: 20,
        request_id: 40,
    });
    let updates = client.drain();
    assert!(updates
        .iter()
        .any(|update| matches!(update, Update::RequestFailed { request_id: 40, .. })));
    assert!(!updates
        .iter()
        .any(|update| matches!(update, Update::UnreadHistory { .. })));
}

#[test]
fn authenticator_challenge_sends_only_explicit_factor_and_keeps_existing_account() {
    for factor in [
        None,
        Some("123456"),
        Some("01234567-89ABCDEF-01234567-89ABCDEF"),
    ] {
        let server = MockHttp::with_request(move |request| {
            assert_eq!(request.method, "POST");
            assert_eq!(request.path, "/api/login");
            assert!(request.authorization.is_none());
            let input: Value = serde_json::from_slice(&request.body).unwrap();
            assert_eq!(input["second_factor"].as_str(), factor);
            Reply::json(401, json!({"error":relay_protocol::AUTHENTICATOR_REQUIRED}))
        });
        let mut test = TestWorker::new();
        test.connection("https://existing.invalid".into());
        let before = test.command_origin();
        let input = AuthRequest {
            second_factor: factor.map(str::to_owned),
            initial_protection: None,
            username: "new-user".into(),
            password: "private-fixture-password".into(),
            invite: String::new(),
        };
        let debug = format!("{input:?}");
        assert!(!debug.contains("private-fixture-password"));
        if let Some(factor) = factor {
            assert!(!debug.contains(factor));
        }
        let error = test
            .worker
            .handle(Command::Auth {
                authority: None,
                url: server.url.clone(),
                expected_server_id: None,
                request: input,
                register: false,
                request_id: 41,
            })
            .unwrap_err();
        assert_eq!(error, relay_protocol::AUTHENTICATOR_REQUIRED);
        assert_eq!(test.command_origin(), before);
        assert_eq!(
            server.requests.lock().unwrap().len(),
            1,
            "a challenge must not fetch/bootstrap a new account"
        );
    }
}

#[test]
fn game_activity_background_write_and_retry_keep_the_selected_voice_account_unchanged() {
    let altered = Arc::new(AtomicBool::new(false));
    let remote_altered = altered.clone();
    let deletes = Arc::new(AtomicUsize::new(0));
    let remote_deletes = deletes.clone();
    let friends = MockHttp::with_request(move |r| {
        assert_eq!(
            r.authorization.as_deref(),
            Some("Bearer friends-only-fixture")
        );
        match (r.method.as_str(), r.path.as_str()) {
            ("GET", "/api/bootstrap") => {
                let mut b = Bootstrap::default();
                b.server_id = if remote_altered.load(Ordering::SeqCst) {
                    "replaced-service"
                } else {
                    "friends"
                }
                .into();
                b.user.id = "fixture-user".into();
                b.capabilities = vec![
                    "people-v1".into(),
                    relay_protocol::game_activity::CAPABILITY.into(),
                ];
                Reply::json(200, serde_json::to_value(b).unwrap())
            }
            ("PUT", "/api/activity") => {
                assert_eq!(
                    serde_json::from_slice::<Value>(&r.body).unwrap()["game_id"],
                    "terraria"
                );
                Reply::json(
                    200,
                    json!({"game_id":"terraria","name":"Terraria","join":null,"expires_at":99999}),
                )
            }
            ("DELETE", "/api/activity") if remote_deletes.fetch_add(1, Ordering::SeqCst) == 0 => {
                Reply::json(503, json!({"error":"fixture unavailable"}))
            }
            ("DELETE", "/api/activity") => Reply {
                status: 204,
                body: vec![],
                headers: vec![],
            },
            _ => panic!("Unexpected activity route"),
        }
    });
    let selected =
        MockHttp::new(|_| panic!("The selected community must receive no activity request"));
    let mut test = TestWorker::new();
    test.connection(friends.url.clone());
    let c = test.worker.connection.as_mut().unwrap();
    c.server_id = "friends".into();
    c.session.token = "friends-only-fixture".into();
    let origin = test.command_origin();
    test.worker.saved_connections.insert(
        friends.url.clone(),
        SavedConnection {
            connection: test.worker.connection.take().unwrap(),
            stop: Arc::new(AtomicBool::new(false)),
            dirty: Arc::new(AtomicBool::new(false)),
        },
    );
    test.connection(selected.url.clone());
    let c = test.worker.connection.as_mut().unwrap();
    c.server_id = "community".into();
    c.session.token = "selected-only-fixture".into();
    let before = test.command_origin();
    test.worker.command(Command::SetGameActivity {
        origin: origin.clone(),
        share: Some(relay_protocol::game_activity::Share {
            game_id: "terraria".into(),
            join: None,
        }),
        request_id: 71,
    });
    assert!(test.drain().iter().any(|u| matches!(
        u,
        Update::GameActivityResult {
            request_id: 71,
            result: Ok(Some(_)),
            ..
        }
    )));
    for (id, success) in [(72, false), (73, true)] {
        test.worker.command(Command::SetGameActivity {
            origin: origin.clone(),
            share: None,
            request_id: id,
        });
        let updates = test.drain();
        assert!(updates.iter().any(|u| matches!(u, Update::GameActivityResult { request_id, result, .. } if *request_id == id && result.is_ok() == success)));
        assert!(!updates
            .iter()
            .any(|u| matches!(u, Update::Error(_) | Update::Busy(_))));
    }
    let count = friends.requests.lock().unwrap().len();
    altered.store(true, Ordering::SeqCst);
    test.worker.command(Command::SetGameActivity {
        origin: origin.clone(),
        share: None,
        request_id: 74,
    });
    assert!(test.drain().iter().any(|u| matches!(
        u,
        Update::GameActivityResult {
            request_id: 74,
            result: Err(_),
            ..
        }
    )));
    assert_eq!(friends.requests.lock().unwrap().len(), count + 1); // GET only, no mutation
    test.worker
        .saved_connections
        .get_mut(&friends.url)
        .unwrap()
        .connection
        .session
        .user
        .id = "replacement-account".into();
    test.worker.command(Command::SetGameActivity {
        origin,
        share: None,
        request_id: 75,
    });
    assert!(test.drain().iter().any(|u| matches!(
        u,
        Update::GameActivityResult {
            request_id: 75,
            result: Err(_),
            ..
        }
    )));
    assert_eq!(friends.requests.lock().unwrap().len(), count + 1); // rejected before any request
    assert_eq!(test.command_origin(), before);
    assert_eq!(
        test.worker
            .connection
            .as_ref()
            .unwrap()
            .voice_channel
            .as_deref(),
        Some("voice-room")
    );
    assert!(selected.requests.lock().unwrap().is_empty());
}

#[test]
fn a_verified_host_move_adopts_only_matching_account_authority_and_provider_without_logging_out_the_shared_token(
) {
    use base64::Engine;
    use ed25519_dalek::SigningKey;
    use sha2::{Digest, Sha256};
    let key = SigningKey::from_bytes(&[74; 32]);
    let public = key.verifying_key();
    let mut hash = Sha256::new();
    hash.update(b"relay-host-identity-v1\0");
    hash.update(public.as_bytes());
    let hash = hash.finalize();
    let mut id: [u8; 16] = hash[..16].try_into().unwrap();
    id[6] = (id[6] & 15) | 128;
    id[8] = (id[8] & 63) | 128;
    let pin = relay_protocol::handover::RoutePin {
        server_id: uuid::Uuid::new_v4().to_string(),
        term: 2,
        host: relay_protocol::handover::HostIdentity {
            host_id: uuid::Uuid::from_bytes(id).to_string(),
            public_key: base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(public.as_bytes()),
        },
    };
    for fault in ["none", "account", "authority", "provider"] {
        let expected = pin.clone();
        let incoming = pin.clone();
        let target = MockHttp::new(move |path| {
            if path != "/api/bootstrap" {
                return Reply::json(409, json!({"error":"fixture has no other API"}));
            }
            let mut bootstrap = Bootstrap {
                server_id: incoming.server_id.clone(),
                host_authority: Some(incoming.clone()),
                user: User {
                    id: if fault == "account" {
                        "another-account"
                    } else {
                        "fixture-user"
                    }
                    .into(),
                    username: "fixture".into(),
                    ..Default::default()
                },
                ..Default::default()
            };
            if fault == "authority" {
                bootstrap.host_authority.as_mut().unwrap().term += 1;
            }
            if fault == "provider" {
                bootstrap.account_provider = Some(AccountProvider {
                    issuer: "https://different-issuer.example".into(),
                    client_id: "relay".into(),
                    community_url: String::new(),
                });
            }
            Reply::json(200, json!(bootstrap))
        });
        let source = MockHttp::new(|_| Reply::json(409, json!({"error":"source retired"})));
        let mut test = TestWorker::new();
        test.connection(source.url.clone());
        test.worker.connection.as_mut().unwrap().server_id = expected.server_id.clone();
        let origin = test.command_origin();
        let session = test.worker.connection.as_ref().unwrap().session.clone();
        let result = test.worker.adopt_session_checked(
            target.url.clone(),
            Some(expected.server_id.clone()),
            session,
            None,
            Some(MovingSession {
                origin: origin.clone(),
                pin: expected,
                provider: None,
                aliases: Vec::new(),
                remembered: false,
                resuming: false,
            }),
            None,
        );
        if fault == "none" {
            result.unwrap();
            assert_eq!(test.worker.connection.as_ref().unwrap().url, target.url);
            assert!(!test.worker.saved_connections.contains_key(&source.url));
            assert!(test.drain().iter().any(|u|matches!(u,Update::CommunityMoved {origin:old,url,..} if old==&origin&&url==&target.url)));
        } else {
            assert!(result.is_err(), "{fault}");
            assert_eq!(test.command_origin(), origin);
            assert!(!test
                .drain()
                .iter()
                .any(|u| matches!(u, Update::CommunityMoved { .. } | Update::Connected { .. })));
        }
        assert!(!source
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|r| r.path == "/api/logout"));
        assert!(!target
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|r| r.path == "/api/logout"));
    }
}

#[test]
#[cfg(windows)]
fn remembered_host_move_restarts_the_bound_account_and_removes_old_vault_aliases_when_forgotten() {
    let profile = std::path::PathBuf::from(
        std::env::var_os("RELAY_PROFILE_DIR")
            .expect("this native vault trial needs its dedicated QA profile"),
    );
    let base = std::path::PathBuf::from(std::env::var_os("RELAY_TEST_WORK_ROOT").unwrap());
    assert_eq!(
        profile.parent(),
        base.parent(),
        "the vault fixture must use the owned QA namespace"
    );
    let (_, _, source_pin, g) = crate::host_route::tests::fixture();
    let target_pin = relay_protocol::handover::RoutePin {
        server_id: source_pin.server_id.clone(),
        host: g.receipt.body.target_identity,
        term: 4,
    };
    let source_url = format!("https://{}.owned-resume.invalid", uuid::Uuid::new_v4());
    let old = crate::credential_vault::RememberedSession {
        server_id: Some(source_pin.server_id.clone()),
        session: Session {
            token: "owned-restart-token".into(),
            user: User {
                id: "owned-restart-user".into(),
                username: "fixture".into(),
                ..Default::default()
            },
        },
        authority: Some(crate::host_route::RememberedAuthority {
            pin: source_pin.clone(),
            account_provider: None,
            previous_urls: Vec::new(),
        }),
    };
    let incoming = target_pin.clone();
    let target = MockHttp::new(move |path| {
        if path != "/api/bootstrap" {
            return Reply::json(
                409,
                json!({"error":"no device or other API in this fixture"}),
            );
        }
        Reply::json(
            200,
            json!(Bootstrap {
                server_id: incoming.server_id.clone(),
                host_authority: Some(incoming.clone()),
                user: User {
                    id: "owned-restart-user".into(),
                    username: "fixture".into(),
                    ..Default::default()
                },
                ..Default::default()
            }),
        )
    });
    crate::credential_vault::save_bound(
        &source_url,
        &source_pin.server_id,
        &old.session,
        old.authority.as_ref(),
    )
    .unwrap();
    let mut test = TestWorker::new();
    let origin = CommandOrigin {
        url: source_url.clone(),
        server_id: source_pin.server_id.clone(),
        user_id: old.session.user.id.clone(),
    };
    test.worker.pending_host_resume = Some(PendingHostResume {
        origin: origin.clone(),
        remembered: old,
        job: crate::host_route::Job::ready(Ok(Some(crate::host_route::VerifiedMove {
            url: target.url.clone(),
            pin: target_pin.clone(),
        }))),
    });
    test.worker.poll_host_resume();
    assert!(test.worker.pending_host_resume.is_none());
    assert_eq!(test.command_origin().url, target.url);
    assert!(test
        .drain()
        .iter()
        .any(|u| matches!(u,Update::CommunityMoved {origin:old,..} if old==&origin)));
    let saved = crate::credential_vault::load(&target.url).unwrap().unwrap();
    assert_eq!(saved.authority.as_ref().unwrap().pin, target_pin);
    assert!(saved.authority.unwrap().previous_urls.contains(&source_url));
    // Source record stays until the new record exists, allowing a crash before
    // the protected preferences save to resume from the original address.
    assert!(crate::credential_vault::load(&source_url)
        .unwrap()
        .is_some());
    crate::credential_vault::forget(&target.url).unwrap();
    assert!(crate::credential_vault::load(&source_url)
        .unwrap()
        .is_none());
    assert!(crate::credential_vault::load(&target.url)
        .unwrap()
        .is_none());
}
#[test]
fn remembered_host_move_is_cancelled_by_logout_before_a_completed_probe_can_send_a_token() {
    let (_, _, pin, _) = crate::host_route::tests::fixture();
    let target =
        MockHttp::new(|_| panic!("a cancelled reconnect must not contact the destination"));
    let mut test = TestWorker::new();
    test.worker.pending_host_resume = Some(PendingHostResume {
        origin: CommandOrigin {
            url: "https://source.example".into(),
            server_id: pin.server_id.clone(),
            user_id: "fixture-user".into(),
        },
        remembered: crate::credential_vault::RememberedSession {
            server_id: Some(pin.server_id.clone()),
            session: Session {
                token: "cancelled-token".into(),
                user: User {
                    id: "fixture-user".into(),
                    ..Default::default()
                },
            },
            authority: Some(crate::host_route::RememberedAuthority {
                pin: pin.clone(),
                account_provider: None,
                previous_urls: Vec::new(),
            }),
        },
        job: crate::host_route::Job::ready(Ok(Some(crate::host_route::VerifiedMove {
            url: target.url.clone(),
            pin,
        }))),
    });
    test.worker.handle(Command::Logout).unwrap();
    test.worker.poll_host_resume();
    assert!(test.worker.connection.is_none());
    assert!(test.worker.pending_host_resume.is_none());
    assert!(target.requests.lock().unwrap().is_empty());
}

#[test]
fn invitation_authority_is_proved_before_password_invite_and_account_proof_are_sent() {
    use relay_protocol::handover::{RouteChallenge, RouteState};
    for fault in ["none", "wrong-key", "frozen", "replayed"] {
        let (a, b, pin, _) = crate::host_route::tests::fixture();
        let server_pin = pin.clone();
        let id = pin.server_id.clone();
        let server = MockHttp::with_request(move |request| match request.path.as_str() {
            "/api/health" => Reply::json(200, invitation_health(&id)),
            "/api/hosting/handover/route" => {
                let request: RouteChallenge = serde_json::from_slice(&request.body).unwrap();
                let proof = crate::host_route::tests::sign(
                    if fault == "wrong-key" { &b } else { &a },
                    RouteState {
                        pin: server_pin.clone(),
                        challenge: if fault == "replayed" {
                            "A".repeat(43)
                        } else {
                            request.challenge
                        },
                        writes_allowed: fault != "frozen",
                        moved: None,
                        history: Vec::new(),
                    },
                );
                Reply::json(200, serde_json::to_value(proof).unwrap())
            }
            _ => Reply::json(
                503,
                json!({"error":"owned endpoint reached after verification"}),
            ),
        });
        let mut client = TestWorker::new();
        assert_eq!(
            client
                .worker
                .confirm_login_authority(&server.url, Some(&pin.server_id), Some(&pin))
                .is_ok(),
            fault == "none"
        );
        let auth = client.worker.handle(Command::Auth {
            authority: Some(pin.clone()),
            url: server.url.clone(),
            expected_server_id: Some(pin.server_id.clone()),
            request: AuthRequest {
                username: "owned-user".into(),
                password: "never-leak-password".into(),
                invite: "never-leak-invite".into(),
                second_factor: None,
                initial_protection: None,
            },
            register: false,
            request_id: 71,
        });
        assert!(auth.is_err());
        assert!(client
            .worker
            .account_login_begin(
                Some(pin.clone()),
                server.url.clone(),
                Some(pin.server_id.clone()),
                zeroize::Zeroizing::new("never-leak-invite".into()),
                true,
                None,
                72
            )
            .is_err());
        assert!(client
            .worker
            .account_login_finish(
                Some(pin.clone()),
                server.url.clone(),
                pin.server_id.clone(),
                uuid::Uuid::new_v4().to_string(),
                zeroize::Zeroizing::new("never-leak-account-proof".into()),
                Arc::new(AtomicBool::new(false)),
                73
            )
            .is_err());
        let requests = server.requests.lock().unwrap();
        let authenticated: Vec<_> = requests
            .iter()
            .filter(|r| {
                matches!(
                    r.path.as_str(),
                    "/api/login" | "/api/account-login/challenge" | "/api/account-login"
                )
            })
            .collect();
        assert_eq!(
            authenticated.len(),
            if fault == "none" { 3 } else { 0 },
            "{fault}"
        );
        if fault != "none" {
            for request in requests.iter() {
                assert!(request.authorization.is_none());
                assert!(!String::from_utf8_lossy(&request.body).contains("never-leak"));
            }
        }
    }
}

#[test]
fn locator_issuance_is_background_and_retains_the_original_community_credentials() {
    let (started_tx, started_rx)=mpsc::sync_channel(1);
    let (release_tx, release_rx)=mpsc::sync_channel(1);
    let release=std::sync::Mutex::new(release_rx);
    let server=MockHttp::with_request(move |request| {
        assert_eq!(request.authorization.as_deref(),Some("Bearer original-community"));
        assert_eq!(request.path,relay_protocol::host_locator::ISSUE_PATH);
        let _=started_tx.send(());
        release.lock().unwrap().recv_timeout(Duration::from_secs(3)).unwrap();
        Reply::json(401,json!({"error":"Sign in again"}))
    });
    let mut client=TestWorker::new();
    client.connection(server.url.clone());
    let (_,_,pin,_)=crate::host_route::tests::fixture();
    let request=client.worker.http.post(format!("{}{}",server.url,relay_protocol::host_locator::ISSUE_PATH))
        .bearer_auth("original-community").timeout(Duration::from_secs(3)).json(&json!({"owned":"fixture"}));
    client.worker.spawn_locator_issue(request,"https://directory.invalid/people".into(),pin);
    started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    // The request is actually blocked now, but the command worker is free and
    // switching its account cannot rewrite the already captured bearer/URL.
    assert_eq!(client.worker.background_active.load(Ordering::Acquire),1);
    client.worker.connection.as_mut().unwrap().session.token="another-community".into();
    client.worker.connection.as_mut().unwrap().current=Some(Target::Channel("unrelated-call".into()));
    client.worker.live.shutdown.store(true,Ordering::Release);
    release_tx.send(()).unwrap();
    let until=Instant::now()+Duration::from_secs(3);
    while client.worker.background_active.load(Ordering::Acquire)>0 {
        assert!(Instant::now()<until,"background issuance did not finish");
        let observed=client.worker.wake.generation();
        if client.worker.background_active.load(Ordering::Acquire)>0 {
            client.worker.wake.wait(observed,Some(Duration::from_millis(50)));
        }
    }
    assert_eq!(client.worker.connection.as_ref().unwrap().session.token,"another-community");
    assert_eq!(client.worker.connection.as_ref().unwrap().current,Some(Target::Channel("unrelated-call".into())));
    assert!(client.drain().iter().any(|u|matches!(u,Update::Notice(_))));
}

#[test]
fn a_rejected_locator_is_acknowledged_only_by_verified_same_directory_state() {
    use ed25519_dalek::Signer;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use relay_protocol::host_locator as locator;
    for (status, valid, expected, requests) in [(400,true,true,2),(400,false,false,2),(503,true,false,1)] {
        let (a,b,pin,grant)=crate::host_route::tests::fixture();
        let now=std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
        let mut queued=locator::Record{id:"c".repeat(64),anchor:pin.clone(),current:pin.clone(),history:vec![],
            issued_ms:now-2000,expires_ms:now+60_000,nonce:URL_SAFE_NO_PAD.encode([3;12]),
            ciphertext:URL_SAFE_NO_PAD.encode([7;32]),signature:String::new()};
        queued.signature=URL_SAFE_NO_PAD.encode(a.sign(&locator::record_message(&queued)).to_bytes());
        let mut stored=queued.clone();
        stored.current=relay_protocol::handover::RoutePin{server_id:pin.server_id.clone(),host:grant.receipt.body.target_identity.clone(),term:grant.next_term};
        stored.history=vec![grant];stored.issued_ms=now-1000;
        stored.signature=URL_SAFE_NO_PAD.encode(b.sign(&locator::record_message(&stored)).to_bytes());
        if !valid {stored.id="d".repeat(64);}
        let expected_id=queued.id.clone();
        let response=serde_json::to_value(stored).unwrap();
        let server=MockHttp::with_request(move |request| {
            assert!(request.authorization.is_none(),"community credentials reached the directory");
            assert_eq!(request.method,"POST");
            if request.path==locator::PUBLISH_PATH {return Reply::json(status,json!({"error":"rejected"}));}
            assert_eq!(request.path,locator::LOOKUP_PATH);
            assert_eq!(serde_json::from_slice::<Value>(&request.body).unwrap(),json!({"id":expected_id}));
            Reply::json(200,response.clone())
        });
        assert_eq!(crate::host_locator::publish_at(&server.url,&queued).is_ok(),expected);
        assert_eq!(server.requests.lock().unwrap().len(),requests);
    }
}

#[test]
fn local_pairing_http_never_receives_a_plain_code_or_account_credential(){
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD,Engine};
    use chacha20poly1305::{aead::{Aead,KeyInit,Payload},ChaCha20Poly1305,Nonce};
    use hmac::{Hmac,Mac};
    use sha2::{Digest,Sha256};
    use relay_protocol::lan_pairing as model;
    let (_,_,pin,_)=crate::host_route::tests::fixture();let server=pin.server_id.clone();let raw=[93u8;16];
    let display=format!("LAN-{}",URL_SAFE_NO_PAD.encode(raw));
    let key:[u8;32]=Sha256::digest(model::material(&server,&raw,model::Purpose::Seal)).into();
    let auth:[u8;32]=Sha256::digest(model::material(&server,&raw,model::Purpose::Auth)).into();
    let invitation=model::Invitation{server_id:server.clone(),code:"synthetic-invitation-bearer".into(),authority:pin,
        destination:relay_protocol::host_locator::Destination{url:"https://pairing-host.example".into(),iroh:None}};
    let mut envelope=model::Envelope{server_id:server.clone(),id:format!("{:x}",Sha256::digest(model::material(&server,&raw,model::Purpose::Lookup))),
        expires_ms:crate::lan_pairing::now_ms().unwrap()+60_000,nonce:URL_SAFE_NO_PAD.encode([9u8;12]),ciphertext:String::new()};
    envelope.ciphertext=URL_SAFE_NO_PAD.encode(ChaCha20Poly1305::new_from_slice(&key).unwrap().encrypt(Nonce::from_slice(&[9u8;12]),
        Payload{msg:&serde_json::to_vec(&invitation).unwrap(),aad:&model::associated_data(&envelope)}).unwrap());
    let expected_id=envelope.id.clone();let body=serde_json::to_value(envelope).unwrap();
    let responder=MockHttp::with_request(move |request|{
        assert_eq!(request.method,"POST");assert_eq!(request.path,model::REDEEM_PATH);assert!(request.authorization.is_none());
        let wire=String::from_utf8_lossy(&request.body);assert!(!wire.contains(&URL_SAFE_NO_PAD.encode(raw)));assert!(!wire.contains("synthetic-invitation-bearer"));
        let request:model::Redeem=serde_json::from_slice(&request.body).unwrap();assert_eq!(request.id,expected_id);
        let mut mac=<Hmac<Sha256> as Mac>::new_from_slice(&auth).unwrap();mac.update(&model::redeem_message(&request));
        mac.verify_slice(&URL_SAFE_NO_PAD.decode(&request.tag).unwrap()).unwrap();Reply::json(200,body.clone())
    });
    let found=crate::lan_discovery::Found{name:"Local fixture".into(),server_id:server,ip:"192.168.1.8".parse().unwrap(),port:4580};
    let joined=crate::lan_pairing::fetch_for_test(&found,&display,&responder.url).unwrap();
    assert_eq!(joined.server,"https://pairing-host.example");assert_eq!(joined.code,"synthetic-invitation-bearer");
    assert_eq!(responder.requests.lock().unwrap().len(),1);
}

#[test]
fn friends_gateway_requests_keep_the_accepted_account_and_require_its_own_capability() {
    let mut test = TestWorker::new();
    test.connection("https://friends.example".into());
    let mut bootstrap = Bootstrap::default();
    bootstrap.capabilities = vec!["people-v1".into(), "gateway-broker-v1".into()];
    test.worker.connection.as_mut().unwrap().chat = extended::ChatSession::new(&bootstrap);
    let ticket: VoiceTicket = serde_json::from_value(json!({
        "session_id":201,"key":base64::engine::general_purpose::STANDARD.encode([1u8;32]),
        "udp_port":4581,"wire_version":4
    })).unwrap();
    // Community capability is deliberately insufficient on a Friends service.
    assert!(test.worker.voice_gateway(&ticket).is_none());
    bootstrap.capabilities.push("friends-gateway-broker-v1".into());
    test.worker.connection.as_mut().unwrap().chat = extended::ChatSession::new(&bootstrap);
    let captured = test.worker.voice_gateway(&ticket).unwrap();
    // A later foreground account change cannot retarget this call's requests.
    test.connection("https://community.example".into());
    test.worker.connection.as_mut().unwrap().session.token = "different-fixture-token".into();
    let request = captured.prepared_list_for_test();
    assert_eq!(request.url().as_str(), "https://friends.example/api/hosting/voice-gateways");
    assert_eq!(request.headers().get("authorization").unwrap(), "Bearer fixture-token");
    // A newly found ordinary HTTP address cannot carry the shared-account bearer.
    test.connection("http://192.168.1.9:4580".into());
    test.worker.connection.as_mut().unwrap().chat = extended::ChatSession::new(&bootstrap);
    assert!(test.worker.voice_gateway(&ticket).is_none());
    assert!(test.drain().is_empty());
}

#[test]
fn prepared_media_routes_recheck_their_exact_lease_and_never_close_the_normal_route() {
    let changing = Arc::new(AtomicBool::new(false));
    let changed = changing.clone();
    let lease = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([3u8;32]);
    let named = lease.clone();
    let control = MockHttp::with_request(move |request| {
        assert_eq!(request.method, "POST");
        assert_eq!(request.authorization.as_deref(), Some("Bearer synthetic-helper-control"));
        match request.path.as_str() {
            "/prepared-route" => {
                let body: Value = serde_json::from_slice(&request.body).unwrap();
                assert!(body.get("token").is_some() && body.get("endpoint_id").is_some());
            },
            "/prepared-route-status" | "/close-prepared-route" => {
                let body: Value = serde_json::from_slice(&request.body).unwrap();
                assert_eq!(body, json!({"lease":named}));
            },
            _ => panic!("prepared media must never change a normal route"),
        }
        if request.path == "/close-prepared-route" { return Reply::json(200, json!({"closed":true})); }
        Reply::json(200,json!({"lease":named,"http_url":if changed.load(Ordering::Acquire){"http://127.0.0.1:49202"}else{"http://127.0.0.1:49201"},"voice_udp_port":49203,"expires_in_secs":120}))
    });
    let id = uuid::Uuid::new_v4().simple().to_string().repeat(2);
    let saved = crate::iroh_route::SavedIroh {endpoint_id:id,relay:None,token:"A".repeat(43)};
    let before = crate::iroh_route::saved_for(&saved.address());
    let prepared = crate::iroh_route::PreparedRoute::open_for_test(&saved,&control.url,"synthetic-helper-control").unwrap();
    assert_eq!(prepared.gateway().unwrap(),"http://127.0.0.1:49201");
    assert_eq!(prepared.signaling(&saved.address().replace("http://","ws://")).unwrap(),"ws://127.0.0.1:49201/");
    assert!(prepared.signaling("ws://other.iroh:4580/").is_err());
    changing.store(true,Ordering::Release);
    assert!(prepared.gateway().is_err(),"a changed port is not silently adopted");
    drop(prepared);
    assert_eq!(crate::iroh_route::saved_for(&saved.address()),before);
    let requests=control.requests.lock().unwrap();
    assert_eq!(requests.last().unwrap().path,"/close-prepared-route");
    assert!(requests.iter().all(|r|matches!(r.path.as_str(),"/prepared-route"|"/prepared-route-status"|"/close-prepared-route")));
}
