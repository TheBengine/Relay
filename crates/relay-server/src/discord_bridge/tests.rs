use super::*;
use crate::tests::{new_invite, request, signup};
use serde_json::{json, Value};
use std::sync::atomic::Ordering;

fn policy(state: &AppState) -> DiscordBridgeSettings {
    let channel: String = lock(&state.0.db)
        .query_row(
            "SELECT id FROM channels WHERE kind='voice' AND visibility='public' LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    DiscordBridgeSettings {
        enabled: true,
        application_id: "12345".into(),
        guild_id: "67890".into(),
        channel_id: Some(channel),
        status_enabled: false,
        status_channel_id: None,
        public_label: "Let's play".into(),
        join_instructions: "Open Relay and ask the host for an invitation.".into(),
    }
}

async fn save(app: &Router, token: &str, p: &DiscordBridgeSettings) -> (StatusCode, Value) {
    request(app, "PATCH", "/api/discord", Some(token), json!(p)).await
}

#[tokio::test]
async fn policy_is_off_admin_only_secret_free_and_persistent_without_schema_migration() {
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open(temp.path(), "private-name-not-exported", 0).unwrap();
    let app = router(state.clone());
    let admin = signup(&app, "Admin", "").await;
    let member = signup(&app, "Member", &new_invite(&app, &admin).await).await;
    let mut changes = state.0.discord_changes.subscribe();
    assert_eq!(
        request(&app, "GET", "/api/discord", None, Value::Null)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        request(
            &app,
            "GET",
            "/api/discord",
            Some(&member.token),
            Value::Null
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let (_, default) = request(&app, "GET", "/api/discord", Some(&admin.token), Value::Null).await;
    assert_eq!(default["settings"]["enabled"], false);
    assert!(default["public_preview"].is_null());
    let p = policy(&state);
    assert_eq!(save(&app, &member.token, &p).await.0, StatusCode::FORBIDDEN);
    let mut forged = json!(p);
    forged["token"] = json!("never-accepted-secret");
    assert_eq!(
        request(&app, "PATCH", "/api/discord", Some(&admin.token), forged)
            .await
            .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let original_id = state.server_id().unwrap();
    let schema: u32 = lock(&state.0.db)
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    let (status, value) = save(&app, &admin.token, &p).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!value.to_string().contains("token"));
    assert!(!value.to_string().contains("private-name-not-exported"));
    assert!(changes.has_changed().unwrap());
    changes.borrow_and_update();
    assert_eq!(save(&app, &admin.token, &p).await.0, StatusCode::OK);
    assert!(
        !changes.has_changed().unwrap(),
        "saving unchanged settings must not restart the Gateway"
    );
    drop(app);
    drop(state);
    let reopened = AppState::open(temp.path(), "private-name-not-exported", 0).unwrap();
    assert_eq!(settings(&lock(&reopened.0.db)).unwrap(), p);
    assert_eq!(reopened.server_id().unwrap(), original_id);
    assert_eq!(
        lock(&reopened.0.db)
            .pragma_query_value::<u32, _>(None, "user_version", |r| r.get(0))
            .unwrap(),
        schema
    );
}

#[tokio::test]
async fn current_channel_privacy_acl_deletion_and_identity_fail_closed() {
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open(temp.path(), "test", 0).unwrap();
    let app = router(state.clone());
    let admin = signup(&app, "Admin", "").await;
    let p = policy(&state);
    let channel = p.channel_id.as_deref().unwrap();
    assert_eq!(save(&app, &admin.token, &p).await.0, StatusCode::OK);
    assert!(command_content(&state, &p, 12345, Some(67890)).contains("0 people"));
    for (application, guild) in [(1, Some(67890)), (12345, None), (12345, Some(1))] {
        assert_eq!(command_content(&state, &p, application, guild), UNAVAILABLE);
    }
    {
        let db = lock(&state.0.db);
        db.execute(
            "UPDATE channels SET visibility='private' WHERE id=?1",
            [channel],
        )
        .unwrap();
    }
    assert_eq!(command_content(&state, &p, 12345, Some(67890)), UNAVAILABLE);
    assert_eq!(
        save(&app, &admin.token, &p).await.0,
        StatusCode::BAD_REQUEST
    );
    {
        let db = lock(&state.0.db);
        db.execute(
            "UPDATE channels SET visibility='public' WHERE id=?1",
            [channel],
        )
        .unwrap();
        db.execute(
            "INSERT INTO channel_permissions VALUES(?1,'role','member',15)",
            [channel],
        )
        .unwrap();
    }
    assert_eq!(command_content(&state, &p, 12345, Some(67890)), UNAVAILABLE);
    assert_eq!(
        save(&app, &admin.token, &p).await.0,
        StatusCode::BAD_REQUEST
    );
    {
        let db = lock(&state.0.db);
        db.execute(
            "DELETE FROM channel_permissions WHERE channel_id=?1",
            [channel],
        )
        .unwrap();
        db.execute("UPDATE channels SET kind='text' WHERE id=?1", [channel])
            .unwrap();
    }
    assert_eq!(command_content(&state, &p, 12345, Some(67890)), UNAVAILABLE);
    {
        let db = lock(&state.0.db);
        db.execute("UPDATE channels SET kind='voice' WHERE id=?1", [channel])
            .unwrap();
    }
    let mut changed = p.clone();
    changed.public_label = "New approved label".into();
    assert_eq!(save(&app, &admin.token, &changed).await.0, StatusCode::OK);
    assert!(
        command_content(&state, &p, 12345, Some(67890)).contains("New approved label"),
        "the active binding uses freshly approved text without reconnecting"
    );
    lock(&state.0.db)
        .execute("DELETE FROM channels WHERE voice_parent_id=?1", [channel])
        .unwrap();
    lock(&state.0.db)
        .execute("DELETE FROM channels WHERE id=?1", [channel])
        .unwrap();
    assert_eq!(
        command_content(&state, &changed, 12345, Some(67890)),
        UNAVAILABLE
    );
    changed.enabled = false;
    assert_eq!(
        save(&app, &admin.token, &changed).await.0,
        StatusCode::OK,
        "opt out even when the old channel is gone"
    );
}

pub(crate) fn ticket(session: &Session, channel: &str) -> voice::VoiceSession {
    voice::VoiceSession {
        private_call_id: None,
        recovery_deadline: None,
        user: session.user.clone(),
        channel_id: channel.into(),
        device_id: None,
        token_hash: token_hash(&session.token),
        key: [0; 32],
        state: VoiceState::default(),
        remote: None,
        tunnel_sender: None,
        tunnel_id: None,
        in_sequence: None,
        replay: Default::default(),
        out_sequence: 0,
        last_seen: now(),
        expires: now() + 60,
        direct: Default::default(),
        can_speak: true,
        can_stream: true,
        whisper_channels: Default::default(),
    }
}

#[tokio::test]
async fn public_count_deduplicates_and_excludes_stale_revoked_removed_and_other_rooms() {
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open(temp.path(), "test", 0).unwrap();
    let app = router(state.clone());
    let admin = signup(&app, "PrivateAdminName", "").await;
    let member = signup(&app, "PrivateMemberName", &new_invite(&app, &admin).await).await;
    let p = policy(&state);
    let channel = p.channel_id.as_deref().unwrap();
    assert_eq!(save(&app, &admin.token, &p).await.0, StatusCode::OK);
    {
        let mut tickets = lock(&state.0.voice);
        tickets.insert(1, ticket(&admin, channel));
        tickets.insert(2, ticket(&admin, channel));
        tickets.insert(3, ticket(&member, channel));
        let mut stale = ticket(&member, channel);
        stale.last_seen = now() - 60;
        tickets.insert(4, stale);
        tickets.insert(5, ticket(&member, "private-other-room"));
    }
    let content = command_content(&state, &p, 12345, Some(67890));
    assert!(content.contains("2 people"));
    for secret in [
        &admin.user.username,
        &member.user.username,
        &admin.token,
        &member.token,
        &p.channel_id.clone().unwrap(),
    ] {
        assert!(!content.contains(secret));
    }
    lock(&state.0.db)
        .execute(
            "DELETE FROM sessions WHERE token_hash=?1",
            [token_hash(&member.token)],
        )
        .unwrap();
    assert!(command_content(&state, &p, 12345, Some(67890)).contains("1 person"));
    lock(&state.0.db)
        .execute(
            "UPDATE membership SET status='removed' WHERE user_id=?1",
            [&admin.user.id],
        )
        .unwrap();
    assert!(command_content(&state, &p, 12345, Some(67890)).contains("0 people"));
}

#[test]
fn public_text_is_bounded_literal_with_no_mentions_embeds_or_public_response() {
    let mut p = DiscordBridgeSettings {
        enabled: true,
        application_id: "12345".into(),
        guild_id: "67890".into(),
        channel_id: Some("room".into()),
        status_enabled: false,
        status_channel_id: None,
        public_label: "\\".repeat(80),
        join_instructions: "\\".repeat(700),
    };
    assert!(p.valid());
    let content = format!(
        "Relay · {}\n9999 people in the call\n\nHow to join\n{}",
        literal(&p.public_label),
        literal(&p.join_instructions)
    );
    assert!(content.chars().count() < 2000);
    let value = json!(response(
        "@everyone <@12345> [fake](https://example.invalid)".into()
    ));
    assert_eq!(value["type"], 4);
    assert_eq!(value["data"]["flags"], 68);
    assert_eq!(value["data"]["allowed_mentions"]["parse"], json!([]));
    assert_eq!(literal("<@12345> **hi**"), "\\<@12345\\> \\*\\*hi\\*\\*");
    p.public_label.push('x');
    assert!(!p.valid());
    for invalid in ["0", "-1", "123 abc", "18446744073709551616", "01x"] {
        assert!(snowflake(invalid).is_none());
    }
    p.enabled = false;
    p.public_label.clear();
    p.join_instructions = "illegal\u{0000}".into();
    assert!(!p.valid());
    let mut seen = Seen::default();
    assert!(seen.insert(InteractionId::new(1)));
    assert!(!seen.insert(InteractionId::new(1)));
    for n in 2..=800 {
        assert!(seen.insert(InteractionId::new(n)));
    }
    assert_eq!(seen.ids.len(), 512);
    assert_eq!(seen.order.len(), 512);
}

async fn wait_status(state: &AppState, expected: &'static str) {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if *lock(&state.0.discord_status) == expected {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn disabled_missing_token_and_shutdown_create_no_http_or_gateway_client() {
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open(temp.path(), "test", 0).unwrap();
    let app = router(state.clone());
    let admin = signup(&app, "Admin", "").await;
    let running = tokio::spawn(run(state.clone(), None));
    tokio::task::yield_now().await;
    assert_eq!(state.0.discord_attempts.load(Ordering::SeqCst), 0);
    let mut p = policy(&state);
    assert_eq!(save(&app, &admin.token, &p).await.0, StatusCode::OK);
    wait_status(&state, "needs_token").await;
    assert_eq!(state.0.discord_attempts.load(Ordering::SeqCst), 0);
    p.enabled = false;
    assert_eq!(save(&app, &admin.token, &p).await.0, StatusCode::OK);
    wait_status(&state, "off").await;
    state.request_shutdown();
    tokio::time::timeout(Duration::from_secs(1), running)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(state.0.discord_attempts.load(Ordering::SeqCst), 0);
}

#[derive(Clone)]
struct Fixture {
    ws: String,
    remaining: u64,
    callbacks: Arc<Mutex<HashMap<String, usize>>>,
    events: tokio::sync::mpsc::UnboundedSender<Value>,
    send: Arc<tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<Value>>>,
}

#[tokio::test]
async fn host_key_rotation_replaces_gateway_and_removal_stops_reconnects() {
    use crate::discord_host::tests::host_request;
    let (events_tx, mut events) = tokio::sync::mpsc::unbounded_channel();
    let (_send, send_rx) = tokio::sync::mpsc::unbounded_channel();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let local = Router::new()
        .route("/socket", get(fixture_ws))
        .fallback(fixture_rest)
        .with_state(Fixture {
            ws: format!("ws://{address}/socket"),
            remaining: 1000,
            callbacks: Arc::new(Mutex::new(HashMap::new())),
            events: events_tx,
            send: Arc::new(tokio::sync::Mutex::new(send_rx)),
        });
    let fixture = tokio::spawn(async move {
        axum::serve(listener, local).await.unwrap();
    });
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open_with_config(
        temp.path(),
        "private",
        0,
        ServerConfig {
            local_control_token: Some("fixture-control-0123456789-0123456789".into()),
            ..Default::default()
        },
    )
    .unwrap();
    // Set before starting any SDK work. Both HTTP and WSS use this local fixture.
    *lock(&state.0.test_discord_proxy) = Some(format!("http://{address}"));
    let app = router(state.clone());
    let admin = signup(&app, "Admin", "").await;
    let running = tokio::spawn(run(state.clone(), None));
    assert_eq!(
        host_request(
            &state,
            &admin.token,
            "PUT",
            json!({"token":"fixture-host-generation-one-token"})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        state.0.discord_attempts.load(Ordering::SeqCst),
        0,
        "saving must not enable sharing"
    );
    assert_eq!(
        save(&app, &admin.token, &policy(&state)).await.0,
        StatusCode::OK
    );
    assert_eq!(
        event_where(&mut events, |v| v["op"] == 2).await["fixture_generation"],
        1
    );
    wait_status(&state, "online").await;
    assert_eq!(
        host_request(
            &state,
            &admin.token,
            "PUT",
            json!({"token":"fixture-host-generation-two-token"})
        )
        .await
        .0,
        StatusCode::OK
    );
    event_where(&mut events, |v| v["gateway_closed"] == true).await;
    assert_eq!(
        event_where(&mut events, |v| v["op"] == 2).await["fixture_generation"],
        2
    );
    wait_status(&state, "online").await;
    assert_eq!(
        host_request(&state, &admin.token, "DELETE", Value::Null)
            .await
            .0,
        StatusCode::OK
    );
    event_where(&mut events, |v| v["gateway_closed"] == true).await;
    wait_status(&state, "needs_token").await;
    assert_eq!(state.0.discord_attempts.load(Ordering::SeqCst), 2);
    assert_eq!(
        host_request(&state, &admin.token, "POST", Value::Null)
            .await
            .0,
        StatusCode::OK
    );
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        state.0.discord_attempts.load(Ordering::SeqCst),
        2,
        "retry after removal must not connect"
    );
    state.request_shutdown();
    tokio::time::timeout(Duration::from_secs(1), running)
        .await
        .unwrap()
        .unwrap();
    fixture.abort();
    let _ = fixture.await;
}

async fn fixture_rest(
    State(f): State<Fixture>,
    method: axum::http::Method,
    uri: axum::http::Uri,
    body: axum::body::Bytes,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let path = uri.path();
    let value: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    f.events
        .send(json!({"method":method.as_str(),"path":path,"body":value}))
        .unwrap();
    if path.ends_with("/gateway/bot") {
        return Json(json!({"url":f.ws,"shards":1,"session_start_limit":{"remaining":f.remaining,"total":1000,"reset_after":86400000,"max_concurrency":1}})).into_response();
    }
    if path.ends_with("/gateway") {
        return Json(json!({"url":f.ws})).into_response();
    }
    if method == axum::http::Method::POST && path.ends_with("/guilds/67890/commands") {
        return Json(json!({"id":"902","application_id":"12345","guild_id":"67890","type":1,"name":"relay","description":"Show the shared Relay call and how to join","version":"1"})).into_response();
    }
    if method == axum::http::Method::POST && path.ends_with("/callback") {
        *lock(&f.callbacks).entry(path.into()).or_default() += 1;
    }
    if (method == axum::http::Method::POST && path.ends_with("/callback"))
        || (method == axum::http::Method::DELETE && path.ends_with("/guilds/67890/commands/902"))
    {
        return StatusCode::NO_CONTENT.into_response();
    }
    StatusCode::NOT_FOUND.into_response()
}

async fn fixture_ws(
    State(f): State<Fixture>,
    upgrade: axum::extract::WebSocketUpgrade,
) -> axum::response::Response {
    upgrade.on_upgrade(move |mut socket| async move {
        use axum::extract::ws::Message;
        let mut send = f.send.lock().await;
        socket.send(Message::Text(json!({"op":10,"d":{"heartbeat_interval":1000}}).to_string().into())).await.unwrap();
        loop {
            tokio::select! {
                packet = socket.recv() => {
                    let Some(Ok(Message::Text(text))) = packet else { break; };
                    let value: Value = serde_json::from_str(&text).unwrap();
                    // Record protocol facts, never credentials/authorization headers.
                    let received = value["d"]["token"].as_str().unwrap_or_default();
                    let generation = match received.strip_prefix("Bot ").unwrap_or(received) {
                        "fixture-host-generation-one-token" => 1,
                        "fixture-host-generation-two-token" => 2,
                        _ => 0,
                    };
                    f.events.send(json!({"op":value["op"],"intents":value["d"]["intents"],"fixture_generation":generation})).unwrap();
                    if value["op"] == 2 {
                        let ready = json!({"op":0,"s":1,"t":"READY","d":{"v":10,"user":{"id":"12345","username":"FixtureBot","bot":true},"guilds":[],"session_id":"fixture-session","resume_gateway_url":f.ws,"application":{"id":"12345","flags":0}}});
                        socket.send(Message::Text(ready.to_string().into())).await.unwrap();
                    } else if value["op"] == 1 {
                        socket.send(Message::Text(json!({"op":11}).to_string().into())).await.unwrap();
                    }
                },
                packet = send.recv() => {
                    let Some(packet) = packet else { break; };
                    if socket.send(Message::Text(packet.to_string().into())).await.is_err() { break; }
                },
            }
        }
        let _ = f.events.send(json!({"gateway_closed":true}));
    })
}

async fn event_where(
    events: &mut tokio::sync::mpsc::UnboundedReceiver<Value>,
    predicate: impl Fn(&Value) -> bool,
) -> Value {
    let mut observed = Vec::new();
    let result = tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let value = events.recv().await.expect("fixture still running");
            if predicate(&value) {
                return value;
            }
            observed.push(value);
        }
    })
    .await;
    result.unwrap_or_else(|_| panic!("Loopback fixture events: {observed:?}"))
}

fn command_event(id: u64) -> Value {
    json!({"op":0,"s":id,"t":"INTERACTION_CREATE","d":{"id":id.to_string(),"application_id":"12345","type":2,"data":{"id":"902","name":"relay","type":1},"guild_id":"67890","channel_id":"777","user":{"id":"888","username":"NeverExportDiscordName"},"token":"fixture-interaction-token","version":1,"locale":"en-US","entitlements":[],"authorizing_integration_owners":{},"context":0,"attachment_size_limit":10000000}})
}

#[tokio::test]
async fn wrong_application_stops_before_registration_and_exhausted_quota_starts_no_gateway() {
    for quota_exhausted in [false, true] {
        let (events_tx, mut events) = tokio::sync::mpsc::unbounded_channel();
        let (_send, send_rx) = tokio::sync::mpsc::unbounded_channel();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let fixture = Fixture {
            ws: format!("ws://{address}/socket"),
            remaining: if quota_exhausted { 0 } else { 1000 },
            callbacks: Arc::new(Mutex::new(HashMap::new())),
            events: events_tx,
            send: Arc::new(tokio::sync::Mutex::new(send_rx)),
        };
        let local = Router::new()
            .route("/socket", get(fixture_ws))
            .fallback(fixture_rest)
            .with_state(fixture);
        let server = tokio::spawn(async move {
            axum::serve(listener, local).await.unwrap();
        });
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::open(temp.path(), "test", 0).unwrap();
        let app = router(state.clone());
        let admin = signup(&app, "Admin", "").await;
        let mut p = policy(&state);
        if !quota_exhausted {
            p.application_id = "999".into();
        }
        assert_eq!(save(&app, &admin.token, &p).await.0, StatusCode::OK);
        *lock(&state.0.test_discord_proxy) = Some(format!("http://{address}"));
        let running = tokio::spawn(run(
            state.clone(),
            Some("fixture-only-invalid-token".into()),
        ));
        wait_status(
            &state,
            if quota_exhausted {
                "session_limit"
            } else {
                "wrong_application"
            },
        )
        .await;
        state.request_shutdown();
        tokio::time::timeout(Duration::from_secs(7), running)
            .await
            .unwrap()
            .unwrap();
        while let Ok(event) = events.try_recv() {
            assert!(
                event["method"] != "POST" && event["method"] != "DELETE",
                "no registration or callback for an unapproved app/quota"
            );
            if quota_exhausted {
                assert!(
                    event["op"].is_null(),
                    "no Gateway connection when allowance is zero"
                );
            }
        }
        server.abort();
        let _ = server.await;
    }
}

#[tokio::test]
async fn real_sdk_loopback_gateway_registers_only_relay_responds_and_disconnects_on_opt_out() {
    let (events_tx, mut events) = tokio::sync::mpsc::unbounded_channel();
    let (send, send_rx) = tokio::sync::mpsc::unbounded_channel();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let proxy = format!("http://{address}");
    let callbacks = Arc::new(Mutex::new(HashMap::new()));
    let fixture = Fixture {
        ws: format!("ws://{address}/socket"),
        remaining: 1000,
        callbacks: callbacks.clone(),
        events: events_tx,
        send: Arc::new(tokio::sync::Mutex::new(send_rx)),
    };
    let app = Router::new()
        .route("/socket", get(fixture_ws))
        .fallback(fixture_rest)
        .with_state(fixture);
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open(temp.path(), "PrivateServerName", 0).unwrap();
    let app = router(state.clone());
    let admin = signup(&app, "PrivateRelayName", "").await;
    let mut p = policy(&state);
    assert_eq!(save(&app, &admin.token, &p).await.0, StatusCode::OK);
    *lock(&state.0.test_discord_proxy) = Some(proxy);
    let running = tokio::spawn(run(
        state.clone(),
        Some("fixture-bot-token-never-sent-to-discord".into()),
    ));
    let identify = event_where(&mut events, |v| v["op"] == 2).await;
    assert_eq!(identify["intents"], 0);
    let registered = event_where(&mut events, |v| {
        v["method"] == "POST"
            && v["path"]
                .as_str()
                .is_some_and(|p| p.ends_with("/guilds/67890/commands"))
    })
    .await;
    assert_eq!(registered["body"]["name"], "relay");
    wait_status(&state, "online").await;
    send.send(command_event(1001)).unwrap();
    let callback = event_where(&mut events, |v| {
        v["path"]
            .as_str()
            .is_some_and(|p| p.ends_with("/interactions/1001/fixture-interaction-token/callback"))
    })
    .await;
    assert_eq!(callback["body"]["data"]["flags"], 68);
    assert_eq!(
        callback["body"]["data"]["allowed_mentions"]["parse"],
        json!([])
    );
    assert!(callback["body"]["data"]["content"]
        .as_str()
        .unwrap()
        .contains("0 people"));
    assert!(!callback.to_string().contains("NeverExportDiscordName"));
    assert!(!callback.to_string().contains("PrivateRelayName"));
    send.send(command_event(1001)).unwrap();
    p.public_label = "Updated public label".into();
    assert_eq!(save(&app, &admin.token, &p).await.0, StatusCode::OK);
    send.send(command_event(1003)).unwrap();
    let updated = event_where(&mut events, |v| {
        v["path"]
            .as_str()
            .is_some_and(|p| p.ends_with("/interactions/1003/fixture-interaction-token/callback"))
    })
    .await;
    assert!(updated["body"]["data"]["content"]
        .as_str()
        .unwrap()
        .contains("Updated public label"));
    assert_eq!(
        state.0.discord_attempts.load(Ordering::SeqCst),
        1,
        "editing public text does not rebuild the Gateway client"
    );
    lock(&state.0.db)
        .execute(
            "UPDATE channels SET visibility='private' WHERE id=?1",
            [p.channel_id.as_deref().unwrap()],
        )
        .unwrap();
    send.send(command_event(1004)).unwrap();
    let hidden = event_where(&mut events, |v| {
        v["path"]
            .as_str()
            .is_some_and(|p| p.ends_with("/interactions/1004/fixture-interaction-token/callback"))
    })
    .await;
    assert_eq!(hidden["body"]["data"]["content"], UNAVAILABLE);
    assert_eq!(
        lock(&callbacks).values().copied().sum::<usize>(),
        3,
        "the duplicate interaction receives no extra callback"
    );
    p.enabled = false;
    assert_eq!(save(&app, &admin.token, &p).await.0, StatusCode::OK);
    event_where(&mut events, |v| {
        v["method"] == "DELETE"
            && v["path"]
                .as_str()
                .is_some_and(|p| p.ends_with("/guilds/67890/commands/902"))
    })
    .await;
    wait_status(&state, "off").await;
    assert_eq!(state.0.discord_attempts.load(Ordering::SeqCst), 1);
    state.request_shutdown();
    tokio::time::timeout(Duration::from_secs(2), running)
        .await
        .unwrap()
        .unwrap();
    server.abort();
    let _ = server.await;
}
