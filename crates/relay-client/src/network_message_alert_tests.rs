//! Observer fixtures use owned loopback HTTP, no E2EE actor or desktop output.
use super::*;

fn bootstrap(latest: u64) -> Bootstrap {
    let mut b = Bootstrap {
        server_id: "owned-friends".into(),
        user: User {
            id: "fixture-user".into(),
            ..Default::default()
        },
        protocol_version: API_PROTOCOL_VERSION,
        capabilities: vec!["people-v1".into()],
        latest_change_sequence: latest,
        ..Default::default()
    };
    b.members.push(User {
        id: "friend".into(),
        username: "Owned friend".into(),
        ..Default::default()
    });
    b.social.friends.push("friend".into());
    b
}
fn page() -> ChangePage {
    ChangePage {
        changes: vec![MessageChange {
            sequence: 101,
            kind: "created".into(),
            message_id: "fresh-dm".into(),
            user_id: "friend".into(),
            recipient_id: Some("fixture-user".into()),
            message: Some(Message {
                id: "fresh-dm".into(),
                recipient_id: Some("fixture-user".into()),
                author: User {
                    id: "friend".into(),
                    ..Default::default()
                },
                content: "unopened-ciphertext".into(),
                ..Default::default()
            }),
            ..Default::default()
        }],
        next_cursor: 101,
        has_more: false,
    }
}
fn client(server: &MockHttp) -> (TestWorker, crate::call_alerts::Alerts) {
    let alerts = crate::call_alerts::Alerts::new(
        crate::notifications::Notifications::new(eframe::egui::Context::default()),
        None,
    );
    // No native output is needed to prove that the observer consumes and owns
    // its exact stream without draining the UI channel.
    alerts.configure(crate::call_alerts::Policy::default());
    let mut client = TestWorker::new();
    client.connection(format!("{}/people", server.url));
    client.worker.connection.as_mut().unwrap().server_id = "owned-friends".into();
    client.worker.call_alerts = Some(alerts.clone());
    (client, alerts)
}

#[test]
fn friends_message_observer_consumes_fresh_changes_while_ui_queue_is_full() {
    let server = MockHttp::new(|path| {
        if path == "/people/api/bootstrap" {
            Reply::json(200, serde_json::to_value(bootstrap(101)).unwrap())
        } else {
            assert_eq!(path, "/people/api/changes?after=100&limit=200");
            Reply::json(200, serde_json::to_value(page()).unwrap())
        }
    });
    let (client, alerts) = client(&server);
    let mut watch = client
        .worker
        .call_alert_watch(&bootstrap(100), Instant::now())
        .unwrap();
    let origin = client.command_origin();
    let generation = alerts.generation(&origin).unwrap();
    while client.worker.updates.try_send(Update::Busy(false)).is_ok() {}
    thread::sleep(Duration::from_millis(210));
    watch.tick(true);
    assert_eq!(alerts.message_cursor(&origin, generation), Some(101));
    assert!(alerts.claim_ui_messages(&origin, &page().changes)[0].is_err());
    assert_eq!(client.drain().len(), 32);
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests
        .iter()
        .all(|r| r.method == "GET" && r.authorization.as_deref() == Some("Bearer fixture-token")));
    assert!(
        requests
            .iter()
            .all(|r| !r.path.contains("e2ee") && !r.path.contains("read")),
        "metadata notification never opens keys or acknowledges a message"
    );
}

#[test]
fn old_message_fetch_cannot_deliver_or_claim_after_logout_and_same_origin_rebind() {
    // A valid old reply and an interrupted/rejected old fetch must both lose
    // authority after logout, even when the exact same account logs in again.
    for status in [200, 503] {
        let (entered, observed) = mpsc::sync_channel(1);
        let (release, waiting) = mpsc::sync_channel(1);
        let waiting = std::sync::Mutex::new(waiting);
        let server = MockHttp::new(move |path| {
            if path == "/people/api/bootstrap" {
                return Reply::json(200, serde_json::to_value(bootstrap(101)).unwrap());
            }
            assert_eq!(path, "/people/api/changes?after=100&limit=200");
            entered.send(()).unwrap();
            waiting
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(3))
                .unwrap();
            Reply::json(status, serde_json::to_value(page()).unwrap())
        });
        let (client, alerts) = client(&server);
        let mut old = client
            .worker
            .call_alert_watch(&bootstrap(100), Instant::now())
            .unwrap();
        let origin = client.command_origin();
        let runner = thread::spawn(move || {
            thread::sleep(Duration::from_millis(210));
            old.tick(true);
        });
        observed.recv_timeout(Duration::from_secs(2)).unwrap();
        alerts.forget_url(&origin.url);
        let _current = client
            .worker
            .call_alert_watch(&bootstrap(101), Instant::now())
            .unwrap();
        let generation = alerts.generation(&origin).unwrap();
        release.send(()).unwrap();
        runner.join().unwrap();
        assert_eq!(alerts.message_cursor(&origin, generation), Some(101));
        assert!(
            alerts.claim_ui_messages(&origin, &page().changes)[0].is_err(),
            "new login baseline excludes the old reply"
        );
        assert!(client.drain().is_empty());
    }
}

#[test]
fn friends_changes_auth_revocation_stops_observer_and_keeps_no_ui_claim_authority() {
    let server = MockHttp::new(|path| {
        if path == "/people/api/bootstrap" {
            Reply::json(200, serde_json::to_value(bootstrap(101)).unwrap())
        } else {
            Reply::json(401, json!({"error":"session revoked"}))
        }
    });
    let (client, alerts) = client(&server);
    let mut watch = client
        .worker
        .call_alert_watch(&bootstrap(100), Instant::now())
        .unwrap();
    let origin = client.command_origin();
    thread::sleep(Duration::from_millis(210));
    watch.tick(true);
    assert!(alerts.generation(&origin).is_none());
    assert!(alerts.claim_ui_messages(&origin, &page().changes)[0].is_err());
    watch.tick(true);
    assert_eq!(
        server.requests.lock().unwrap().len(),
        2,
        "no credential reuse after revocation"
    );
}

#[test]
fn explicit_empty_cursor_and_missing_legacy_cursor_are_distinct_at_response_receipt() {
    for explicit in [false, true] {
        let server = MockHttp::new(move |path| {
            if path == "/people/api/bootstrap" {
                let mut value = serde_json::to_value(bootstrap(0)).unwrap();
                if !explicit {
                    value
                        .as_object_mut()
                        .unwrap()
                        .remove("latest_change_sequence");
                }
                Reply::json(200, value)
            } else {
                assert_eq!(path, "/people/api/changes?after=0&limit=200");
                let mut fresh = page();
                fresh.changes[0].sequence = 1;
                fresh.next_cursor = 1;
                Reply::json(200, serde_json::to_value(fresh).unwrap())
            }
        });
        let (client, alerts) = client(&server);
        let mut value = serde_json::to_value(bootstrap(0)).unwrap();
        if !explicit {
            value
                .as_object_mut()
                .unwrap()
                .remove("latest_change_sequence");
        }
        let parsed: super::super::call_alerts::BootstrapReply =
            serde_json::from_value(value).unwrap();
        assert_eq!(parsed.cursor, explicit.then_some(0));
        let mut watch = client
            .worker
            .alert_watch(&parsed.bootstrap, Instant::now(), parsed.cursor)
            .unwrap();
        let origin = client.command_origin();
        let mut fresh = page();
        fresh.changes[0].sequence = 1;
        fresh.next_cursor = 1;
        // UI can win the race before the first background page, but history
        // from a server omitting its cursor must never claim live authority.
        let claims = alerts.claim_ui_messages(&origin, &fresh.changes);
        assert_eq!(claims[0].is_ok(), explicit);
        thread::sleep(Duration::from_millis(210));
        watch.tick(true);
        assert!(alerts.claim_ui_messages(&origin, &fresh.changes)[0].is_err());
        assert_eq!(server.requests.lock().unwrap().len(), 2);
    }
}

#[test]
fn ordinary_changes_failure_releases_owned_feed_and_retry_keeps_shared_dedup() {
    let failures = Arc::new(AtomicUsize::new(0));
    let seen = failures.clone();
    let server = MockHttp::new(move |path| {
        if path == "/people/api/bootstrap" {
            Reply::json(200, serde_json::to_value(bootstrap(100)).unwrap())
        } else {
            assert_eq!(path, "/people/api/changes?after=100&limit=200");
            if seen.fetch_add(1, Ordering::SeqCst) == 0 {
                Reply::json(
                    200,
                    serde_json::to_value(ChangePage {
                        changes: vec![],
                        next_cursor: 100,
                        has_more: false,
                    })
                    .unwrap(),
                )
            } else {
                Reply::json(503, json!({"error":"temporary outage"}))
            }
        }
    });
    let (client, alerts) = client(&server);
    let mut watch = client
        .worker
        .call_alert_watch(&bootstrap(100), Instant::now())
        .unwrap();
    let origin = client.command_origin();
    thread::sleep(Duration::from_millis(210));
    watch.tick(true);
    thread::sleep(Duration::from_millis(210));
    watch.tick(true);
    let claims = alerts.claim_ui_messages(&origin, &page().changes);
    assert!(
        claims[0].as_ref().unwrap().as_ref().unwrap().valid(),
        "UI fallback returns after ordinary changes failure"
    );
    assert!(alerts.claim_ui_messages(&origin, &page().changes)[0].is_err());
    assert_eq!(server.requests.lock().unwrap().len(), 4);
}

#[test]
fn delayed_shared_worker_and_initial_rebind_bootstrap_do_not_reset_subscriber_cursor() {
    let server = MockHttp::new(|path| {
        if path == "/people/api/bootstrap" {
            Reply::json(200, serde_json::to_value(bootstrap(101)).unwrap())
        } else {
            let mut fresh = page();
            if path == "/people/api/changes?after=101&limit=200" {
                fresh.next_cursor = 102;
                fresh.changes[0].sequence = 102;
                fresh.changes[0].message_id = "after-reconnect".into();
                fresh.changes[0].message.as_mut().unwrap().id = "after-reconnect".into();
            } else {
                assert_eq!(path, "/people/api/changes?after=100&limit=200");
            }
            Reply::json(200, serde_json::to_value(fresh).unwrap())
        }
    });
    let (client, alerts) = client(&server);
    let mut old = client
        .worker
        .call_alert_watch(&bootstrap(100), Instant::now())
        .unwrap();
    let origin = client.command_origin();
    thread::sleep(Duration::from_millis(210));
    old.tick(true);
    let old_generation = alerts.generation(&origin).unwrap();
    assert_eq!(alerts.message_cursor(&origin, old_generation), Some(101));
    client
        .worker
        .record_alert_bootstrap(&origin, &bootstrap(100), Instant::now(), Some(100));
    assert_eq!(alerts.message_cursor(&origin, old_generation), Some(101));
    let mut current = client
        .worker
        .call_alert_watch(&bootstrap(100), Instant::now())
        .unwrap();
    let generation = alerts.generation(&origin).unwrap();
    assert_ne!(generation, old_generation);
    assert_eq!(alerts.message_cursor(&origin, generation), Some(101));
    thread::sleep(Duration::from_millis(210));
    current.tick(true);
    assert_eq!(alerts.message_cursor(&origin, generation), Some(102));
    assert!(alerts.claim_ui_messages(&origin, &page().changes)[0].is_err());
    assert_eq!(server.requests.lock().unwrap().len(), 4);
}
