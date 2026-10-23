//! Presentation-only errors use synthetic actors and owned loopback fixtures.
use super::*;
use crate::e2ee::{CryptoPeerStatus, CryptoProblemSource};

fn fixture(server: &MockHttp, target: &Target) -> TestWorker {
    let mut client = TestWorker::new();
    client.connection(server.url.clone());
    server.enable_encrypted_target(&mut client, target.clone());
    client.worker.connection.as_mut().unwrap().voice_channel = None;
    client
}

fn latest_status(updates: Vec<Update>) -> crate::e2ee::CryptoUiStatus {
    updates
        .into_iter()
        .filter_map(|update| match update {
            Update::CryptoStatus { status } => Some(status),
            _ => None,
        })
        .next_back()
        .expect("a presentation status must be published")
}

fn directory_reply(path: &str) -> Reply {
    match path {
        "/api/e2ee/directory" => Reply::json(200, json!([])),
        "/api/bootstrap" => Reply::json(200, serde_json::to_value(Bootstrap::default()).unwrap()),
        _ => Reply::json(404, json!({"error":"fixture endpoint not needed"})),
    }
}

#[test]
fn a_published_problem_survives_cached_local_verification_and_ordinary_refresh() {
    let server = MockHttp::new(directory_reply);
    let mut client = fixture(&server, &Target::Channel("owned-status".into()));
    client
        .worker
        .connection
        .as_mut()
        .unwrap()
        .crypto
        .as_mut()
        .unwrap()
        .status
        .peers
        .push(CryptoPeerStatus {
            user_id: "cached-peer".into(),
            name: "Cached peer".into(),
            device_id: "cached-device".into(),
            fingerprint: "a".repeat(64),
            verified: true,
            changed: false,
        });
    client
        .worker
        .publish_encryption_failure("A protected operation failed");
    let status = latest_status(client.drain());
    assert_eq!(
        status.problem.as_deref(),
        Some("A protected operation failed")
    );
    assert!(status.peers[0].verified);
    client.worker.refresh_encryption();
    // Snapshot suppression is allowed when the cached failure is unchanged.
    for update in client.drain() {
        if let Update::CryptoStatus { status } = update {
            assert_eq!(
                status.problem.as_deref(),
                Some("A protected operation failed")
            );
            assert!(status.peers[0].verified);
        }
    }
    assert_eq!(
        client
            .worker
            .connection
            .as_ref()
            .unwrap()
            .crypto
            .as_ref()
            .unwrap()
            .status
            .problem
            .as_deref(),
        Some("A protected operation failed")
    );
}

#[test]
fn a_successful_directory_retry_clears_only_its_actual_directory_failure() {
    let available = Arc::new(AtomicBool::new(false));
    let reply_available = available.clone();
    let server = MockHttp::new(move |path| {
        if path == "/api/e2ee/directory" && !reply_available.load(Ordering::Acquire) {
            Reply::json(503, json!({"error":"Directory temporarily unavailable"}))
        } else {
            directory_reply(path)
        }
    });
    let mut client = fixture(&server, &Target::Channel("owned-status".into()));
    client.worker.command(Command::CryptoRefresh);
    let status = latest_status(client.drain());
    assert!(status
        .problem
        .as_deref()
        .is_some_and(|message| message.contains("Directory temporarily unavailable")));
    available.store(true, Ordering::Release);
    let status = latest_status(client.handle(Command::CryptoRefresh));
    assert!(status.problem.is_none());
    assert!(status.status.contains("Device list refreshed"));
}

#[test]
fn directory_or_another_conversation_success_cannot_erase_a_conversation_problem() {
    let server = MockHttp::new(directory_reply);
    let first = Target::Channel("first-status".into());
    let second = Target::Channel("second-status".into());
    let mut client = fixture(&server, &first);
    server.enable_encrypted_target(&mut client, second.clone());
    let source = CryptoProblemSource::Conversation(client.worker.crypto_scope(&first).unwrap());
    client
        .worker
        .crypto()
        .unwrap()
        .note_problem(source, "Waiting for the first conversation");
    let status = latest_status(client.handle(Command::CryptoRefresh));
    assert_eq!(
        status.problem.as_deref(),
        Some("Waiting for the first conversation")
    );
    client.worker.ensure_crypto_conversation(&second).unwrap();
    for update in client.drain() {
        if let Update::CryptoStatus { status } = update {
            assert_eq!(
                status.problem.as_deref(),
                Some("Waiting for the first conversation")
            );
        }
    }
    client.worker.ensure_crypto_conversation(&first).unwrap();
    let status = latest_status(client.drain());
    assert!(status.problem.is_none());
    assert_eq!(status.status, "Keys are ready for this conversation.");
}

#[test]
fn a_new_problem_inside_a_successful_action_cannot_be_cleared_by_its_earlier_generation() {
    let server = MockHttp::new(directory_reply);
    let target = Target::Channel("owned-status".into());
    let mut client = fixture(&server, &target);
    let source = CryptoProblemSource::Conversation(client.worker.crypto_scope(&target).unwrap());
    let crypto = client.worker.crypto().unwrap();
    crypto.note_problem(source.clone(), "Previous problem");
    let generation = crypto.problem_generation();
    // An invalid opaque call request can be retired without failing the whole
    // conversation setup; that diagnostic must still remain visible afterward.
    crypto.note_problem(source.clone(), "New authenticated-control check failed");
    crypto.finish_problem_action(&source, generation, "Keys are ready");
    assert_eq!(
        crypto.status.problem.as_deref(),
        Some("New authenticated-control check failed")
    );
    assert_eq!(
        crypto.status.status,
        "New authenticated-control check failed"
    );
}

#[test]
fn directory_failure_and_recovery_keep_an_earlier_conversation_problem() {
    let available = Arc::new(AtomicBool::new(false));
    let reply_available = available.clone();
    let server = MockHttp::new(move |path| {
        if path == "/api/e2ee/directory" && !reply_available.load(Ordering::Acquire) {
            Reply::json(503, json!({"error":"Directory unavailable"}))
        } else {
            directory_reply(path)
        }
    });
    let target = Target::Channel("unresolved-conversation".into());
    let mut client = fixture(&server, &target);
    let source = CryptoProblemSource::Conversation(client.worker.crypto_scope(&target).unwrap());
    client
        .worker
        .crypto()
        .unwrap()
        .note_problem(source, "Conversation has not recovered");
    client.worker.command(Command::CryptoRefresh);
    assert!(latest_status(client.drain())
        .problem
        .unwrap()
        .contains("Directory unavailable"));
    available.store(true, Ordering::Release);
    let status = latest_status(client.handle(Command::CryptoRefresh));
    assert_eq!(
        status.problem.as_deref(),
        Some("Conversation has not recovered")
    );
    client.worker.ensure_crypto_conversation(&target).unwrap();
    assert!(latest_status(client.drain()).problem.is_none());
}

#[test]
fn automatic_initialization_retry_recovers_without_clearing_generic_problems() {
    let available = Arc::new(AtomicBool::new(false));
    let reply_available = available.clone();
    let server = MockHttp::with_request(move |request| {
        if request.path == "/api/e2ee/device" && request.method == "PUT" {
            if reply_available.load(Ordering::Acquire) {
                Reply::json(200, json!(CryptoDevice::default()))
            } else {
                Reply::json(503, json!({"error":"Registration unavailable"}))
            }
        } else if let Some(query) = request.path.strip_prefix("/api/e2ee/key-packages?") {
            let device_id = url::form_urlencoded::parse(query.as_bytes())
                .find_map(|(name, value)| (name == "device_id").then(|| value.into_owned()))
                .unwrap();
            Reply::json(
                200,
                json!({"device_id":device_id,"unused":16,"reserved":0,"consumed":0}),
            )
        } else {
            directory_reply(&request.path)
        }
    });
    let mut client = fixture(&server, &Target::Channel("initialization-retry".into()));
    client.worker.connection.as_mut().unwrap().crypto_registered = false;
    client.worker.refresh_encryption();
    assert!(latest_status(client.drain())
        .problem
        .unwrap()
        .contains("Registration unavailable"));
    available.store(true, Ordering::Release);
    client.worker.refresh_encryption();
    assert!(latest_status(client.drain()).problem.is_none());
    assert!(client.worker.connection.as_ref().unwrap().crypto_registered);

    client
        .worker
        .publish_encryption_failure("A separate operation still failed");
    client.drain();
    client.worker.refresh_encryption();
    assert_eq!(
        client.worker.crypto().unwrap().status.problem.as_deref(),
        Some("A separate operation still failed")
    );
}

#[test]
fn automatic_call_inbox_retry_recovers_and_preserves_an_unresolved_conversation() {
    let available = Arc::new(AtomicBool::new(false));
    let reply_available = available.clone();
    let server = MockHttp::new(move |path| {
        if path.starts_with("/api/e2ee/call-inbox?") {
            if reply_available.load(Ordering::Acquire) {
                Reply::json(200, json!([]))
            } else {
                Reply::json(503, json!({"error":"Private call inbox unavailable"}))
            }
        } else {
            directory_reply(path)
        }
    });
    let target = Target::Channel("inbox-retry".into());
    let mut client = fixture(&server, &target);
    client
        .worker
        .connection
        .as_mut()
        .unwrap()
        .chat
        .update_bootstrap(&Bootstrap {
            capabilities: vec!["e2ee_mls_v1".into(), "e2ee-call-freshness-v1".into()],
            ..Default::default()
        });
    client.worker.refresh_encryption();
    assert!(latest_status(client.drain())
        .problem
        .unwrap()
        .contains("Private call inbox unavailable"));
    available.store(true, Ordering::Release);
    client.worker.refresh_encryption();
    assert!(latest_status(client.drain()).problem.is_none());

    let source = CryptoProblemSource::Conversation(client.worker.crypto_scope(&target).unwrap());
    client
        .worker
        .crypto()
        .unwrap()
        .note_problem(source, "Conversation is still waiting");
    available.store(false, Ordering::Release);
    client.worker.refresh_encryption();
    client.drain();
    available.store(true, Ordering::Release);
    client.worker.refresh_encryption();
    assert_eq!(
        latest_status(client.drain()).problem.as_deref(),
        Some("Conversation is still waiting")
    );
}

#[test]
fn stale_identity_actions_do_not_publish_or_record_problems_in_a_replacement_connection() {
    let old_server = MockHttp::new(directory_reply);
    let current_server = MockHttp::new(directory_reply);
    let target = Target::Channel("replacement-status".into());
    let mut client = fixture(&old_server, &target);
    let old_origin = client.command_origin();
    let old_device = client
        .worker
        .crypto()
        .unwrap()
        .device
        .identity()
        .device_id
        .clone();
    client.connection(current_server.url.clone());
    current_server.enable_encrypted_target(&mut client, target.clone());
    client.worker.connection.as_mut().unwrap().voice_channel = None;
    let current_device = client
        .worker
        .crypto()
        .unwrap()
        .device
        .identity()
        .device_id
        .clone();
    let source = CryptoProblemSource::Conversation(client.worker.crypto_scope(&target).unwrap());
    client
        .worker
        .crypto()
        .unwrap()
        .note_problem(source, "The current conversation needs attention");
    client.drain();

    let commands = [
        Command::CryptoVerify {
            origin: old_origin.clone(),
            user_id: "old-peer".into(),
            device_id: "old-device".into(),
            fingerprint: "a".repeat(64),
        },
        Command::CryptoUnverify {
            origin: old_origin.clone(),
            user_id: "old-peer".into(),
            device_id: "old-device".into(),
        },
        Command::CryptoResetDevice {
            confirmation: "RESET DEVICE".into(),
            url: old_origin.url.clone(),
            user_id: old_origin.user_id.clone(),
            expected_device_id: old_device,
        },
        Command::CryptoRevokeOwnDevice {
            url: old_origin.url,
            user_id: old_origin.user_id,
            device_id: "old-secondary-device".into(),
        },
    ];
    for command in commands {
        client.worker.command(command);
        let updates = client.drain();
        assert!(updates
            .iter()
            .any(|update| matches!(update, Update::Error(_))));
        assert!(!updates
            .iter()
            .any(|update| matches!(update, Update::CryptoStatus { .. })));
        let crypto = client.worker.crypto().unwrap();
        assert_eq!(
            crypto.status.problem.as_deref(),
            Some("The current conversation needs attention")
        );
        assert_eq!(crypto.device.identity().device_id, current_device);
    }
    assert!(current_server.paths.lock().unwrap().is_empty());
}

#[test]
fn bounded_problem_tracking_remains_conservative_on_overflow_and_caps_utf8_diagnostics() {
    let server = MockHttp::new(directory_reply);
    let mut client = fixture(&server, &Target::Channel("bounded-status".into()));
    let crypto = client.worker.crypto().unwrap();
    let source = CryptoProblemSource::Conversation("utf8-diagnostic".into());
    crypto.note_problem(source.clone(), &"ø".repeat(5000));
    assert_eq!(crypto.status.problem.as_ref().unwrap().len(), 2048);
    crypto.finish_problem_action(&source, crypto.problem_generation(), "Recovered");
    for index in 0..33 {
        crypto.note_problem(
            CryptoProblemSource::Conversation(format!("scope-{index}")),
            "Needs attention",
        );
    }
    let generation = crypto.problem_generation();
    for index in 0..33 {
        crypto.finish_problem_action(
            &CryptoProblemSource::Conversation(format!("scope-{index}")),
            generation,
            "Recovered",
        );
    }
    assert!(
        crypto.status.problem.is_some(),
        "Untracked overflow may not become an all-clear"
    );
}

#[test]
fn a_long_scoped_error_is_not_duplicated_as_a_sticky_generic_error() {
    let available = Arc::new(AtomicBool::new(false));
    let reply_available = available.clone();
    let server = MockHttp::new(move |path| {
        if path == "/api/e2ee/directory" && !reply_available.load(Ordering::Acquire) {
            Reply::json(503, json!({"error":"ø".repeat(5000)}))
        } else {
            directory_reply(path)
        }
    });
    let mut client = fixture(&server, &Target::Channel("long-status".into()));
    client.worker.command(Command::CryptoRefresh);
    let status = latest_status(client.drain());
    assert!(status.problem.unwrap().len() <= 2048);
    available.store(true, Ordering::Release);
    assert!(latest_status(client.handle(Command::CryptoRefresh))
        .problem
        .is_none());
}
