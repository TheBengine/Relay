use super::*;
use crate::tests::{
    create_test_channel, new_invite, request, request_with_headers, send_test_message, signup,
};
use futures_util::StreamExt;
use http_body_util::BodyExt;
use std::time::Instant;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pending_database_socket_validation_is_shutdown_cancellable_and_timeout_fails_closed() {
    for shutdown in [true, false] {
        let data = tempfile::tempdir().unwrap();
        let state = AppState::open(data.path(), "blocked authorization", 0).unwrap();
        let (holding_ready, ready) = tokio::sync::oneshot::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        let held_state = state.clone();
        let holder = std::thread::spawn(move || {
            let _guard = lock(&held_state.0.db);
            holding_ready.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        ready.await.unwrap();
        let (started, started_rx) = tokio::sync::oneshot::channel();
        let check_state = state.clone();
        let validation = tokio::task::spawn_blocking(move || {
            started.send(()).unwrap();
            let _guard = lock(&check_state.0.db);
            Ok(())
        });
        started_rx.await.unwrap();
        // The DB is demonstrably held and the worker has entered validation;
        // it cannot produce its successful result until this fixture releases it.
        let wait_state = state.clone();
        let waiter =
            tokio::spawn(async move { rtc::wait_for_validation(&wait_state, validation).await });
        if shutdown {
            state.request_shutdown();
        }
        let result =
            tokio::time::timeout(Duration::from_secs(if shutdown { 1 } else { 4 }), waiter).await;
        // Always release fixture resources before asserting, including failures.
        release.send(()).unwrap();
        holder.join().unwrap();
        assert!(
            !result
                .expect("pending DB must not delay socket shutdown/authorization deadline")
                .unwrap(),
            "shutdown or timed-out authorization never assumes success"
        );
    }
}

fn event_request(address: SocketAddr, token: &str) -> axum::http::Request<()> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut request = format!("ws://{address}/api/events")
        .into_client_request()
        .unwrap();
    request.headers_mut().insert(
        header::AUTHORIZATION,
        format!("Bearer {token}").parse().unwrap(),
    );
    request
}

async fn next_event(
    socket: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> Event {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match socket.next().await {
                Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) => {
                    return serde_json::from_str::<Event>(&text).unwrap()
                }
                Some(Ok(_)) => {}
                _ => panic!("socket ended before the expected scoped event"),
            }
        }
    })
    .await
    .expect("expected scoped event")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_typing_events_reach_only_dm_pair_and_private_channel_readers() {
    let data = tempfile::tempdir().unwrap();
    let state = AppState::open(data.path(), "private hints", 0).unwrap();
    let app = router(state.clone());
    let admin = signup(&app, "Admin", "").await;
    let member = signup(&app, "Member", &new_invite(&app, &admin).await).await;
    let outsider = signup(&app, "Outsider", &new_invite(&app, &admin).await).await;
    let channel = create_test_channel(&app, &admin, "text", true).await;
    request(&app,"PUT",&format!("/api/channels/{channel}/permissions"),Some(&admin.token),serde_json::json!({"entries":[{"subject_kind":"user","subject_id":member.user.id,"read":true,"send":true,"join":false,"stream":false}]})).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let shutdown = state.clone();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move { shutdown.shutdown_requested().await })
        .await
        .unwrap();
    });
    let mut sockets = Vec::new();
    for session in [&admin, &member, &outsider] {
        sockets.push(
            tokio_tungstenite::connect_async(event_request(address, &session.token))
                .await
                .unwrap()
                .0,
        );
    }
    let app = router(state.clone());
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/typing",
            Some(&member.token),
            serde_json::json!({"peer_id":admin.user.id,"typing":true})
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    for socket in sockets.iter_mut().take(2) {
        let event = next_event(socket).await;
        assert_eq!(event.kind, "typing");
        assert_eq!(event.user_id.as_deref(), Some(member.user.id.as_str()));
        assert_eq!(event.recipient_id.as_deref(), Some(admin.user.id.as_str()));
    }
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/typing",
            Some(&member.token),
            serde_json::json!({"channel_id":channel,"typing":true})
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    for socket in sockets.iter_mut().take(2) {
        let event = next_event(socket).await;
        assert_eq!(event.channel_id.as_deref(), Some(channel.as_str()));
        assert!(event.recipient_id.is_none());
    }
    let pin = seed_message(&state, &admin, &channel, "private pin");
    assert_eq!(
        request(
            &app,
            "PUT",
            &format!("/api/channels/{channel}/pins/{pin}"),
            Some(&admin.token),
            serde_json::Value::Null
        )
        .await
        .0,
        StatusCode::OK
    );
    for socket in sockets.iter_mut().take(2) {
        assert_eq!(next_event(socket).await.kind, "messages");
        assert_eq!(next_event(socket).await.kind, "pins");
    }
    // Drain heartbeat control frames while proving no text notice reaches an outsider.
    assert!(tokio::time::timeout(Duration::from_millis(250), async {
        loop {
            if let Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) =
                sockets[2].next().await
            {
                panic!("private activity leaked as text: {text}");
            }
        }
    })
    .await
    .is_err());
    let key = chat_controls::TypingKey {
        user_id: member.user.id.clone(),
        target: chat_controls::TypingTarget::Channel(channel.clone()),
    };
    let queued = Event {
        kind: "typing".into(),
        channel_id: Some(channel.clone()),
        user_id: Some(member.user.id.clone()),
        recipient_id: None,
        typing: Some(true),
        expires_at_ms: Some(lock(&state.0.typing).get(&key).unwrap().expires_at_ms),
    };
    request(
        &app,
        "PUT",
        &format!("/api/channels/{channel}/permissions"),
        Some(&admin.token),
        serde_json::json!({"entries":[]}),
    )
    .await;
    assert!(
        !chat_controls::typing_notice_valid(&state, &queued),
        "queued hint cannot disclose revoked activity"
    );
    state.request_shutdown();
    tokio::time::timeout(Duration::from_secs(6), server)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn concurrent_slowmode_sends_commit_only_once_and_typing_capacity_is_per_user_not_per_login()
{
    let data = tempfile::tempdir().unwrap();
    let state = AppState::open(data.path(), "concurrent", 0).unwrap();
    let app = router(state.clone());
    let admin = signup(&app, "Admin", "").await;
    let member = signup(&app, "Member", &new_invite(&app, &admin).await).await;
    let a = create_test_channel(&app, &admin, "text", false).await;
    let b = create_test_channel(&app, &admin, "text", false).await;
    let c = create_test_channel(&app, &admin, "text", false).await;
    request(
        &app,
        "PATCH",
        &format!("/api/channels/{a}"),
        Some(&admin.token),
        serde_json::json!({"slowmode_seconds":10}),
    )
    .await;
    let (first, second) = tokio::join!(
        request(
            &app,
            "POST",
            "/api/messages",
            Some(&member.token),
            serde_json::json!({"channel_id":a,"content":"race first","client_id":"race-a"})
        ),
        request(
            &app,
            "POST",
            "/api/messages",
            Some(&member.token),
            serde_json::json!({"channel_id":a,"content":"race second","client_id":"race-b"})
        )
    );
    assert!(matches!(
        (first.0, second.0),
        (StatusCode::OK, StatusCode::TOO_MANY_REQUESTS)
            | (StatusCode::TOO_MANY_REQUESTS, StatusCode::OK)
    ));
    assert_eq!(
        lock(&state.0.db)
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE author_id=?1",
                [&member.user.id],
                |r| r.get::<_, u64>(0)
            )
            .unwrap(),
        1
    );
    for channel in [&a, &b] {
        assert_eq!(
            request(
                &app,
                "POST",
                "/api/typing",
                Some(&member.token),
                serde_json::json!({"channel_id":channel,"typing":true})
            )
            .await
            .0,
            StatusCode::NO_CONTENT
        );
    }
    let (status, _, error) = request_with_headers(
        &app,
        "POST",
        "/api/typing",
        Some(&member.token),
        serde_json::json!({"channel_id":c,"typing":true}),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(error["code"], "service_busy");
    assert_eq!(lock(&state.0.typing).len(), 2);
    let second_session = {
        let db = lock(&state.0.db);
        new_session(&db, member.user.clone()).unwrap()
    };
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/typing",
            Some(&second_session.token),
            serde_json::json!({"channel_id":c,"typing":true})
        )
        .await
        .0,
        StatusCode::TOO_MANY_REQUESTS
    );
    request(
        &app,
        "POST",
        "/api/typing",
        Some(&second_session.token),
        serde_json::json!({"channel_id":a,"typing":true}),
    )
    .await;
    request(
        &app,
        "POST",
        "/api/typing",
        Some(&member.token),
        serde_json::json!({"channel_id":a,"typing":false}),
    )
    .await;
    assert_eq!(
        lock(&state.0.typing).len(),
        2,
        "old session stop cannot cancel newer session activity"
    );
    request(
        &app,
        "POST",
        "/api/logout",
        Some(&second_session.token),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(
        lock(&state.0.typing).len(),
        1,
        "logout removes only activity owned by its session"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_rtc_socket_admission_is_before_upstream_connect_releases_and_observes_shutdown() {
    let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_address = upstream_listener.local_addr().unwrap();
    let mock = Router::new().route(
        "/rtc",
        get(|ws: WebSocketUpgrade| async move {
            ws.on_upgrade(|mut socket| async move {
                while let Some(Ok(message)) = socket.recv().await {
                    if matches!(message, WsMessage::Close(_)) {
                        break;
                    }
                }
            })
        }),
    );
    let upstream = tokio::spawn(async move {
        axum::serve(upstream_listener, mock).await.unwrap();
    });
    let config = RtcConfig::new(
        "ws://127.0.0.1".into(),
        format!("http://{upstream_address}"),
        "test-key".into(),
        "test-secret-long-enough".into(),
    )
    .unwrap();
    let data = tempfile::tempdir().unwrap();
    let state = AppState::open_with_config(
        data.path(),
        "rtc sockets",
        0,
        ServerConfig {
            rtc: Some(config),
            ..Default::default()
        },
    )
    .unwrap();
    let app = router(state.clone());
    let admin = signup(&app, "Admin", "").await;
    let channel = create_test_channel(&app, &admin, "voice", false).await;
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/voice/join",
            Some(&admin.token),
            serde_json::json!({"channel_id":channel})
        )
        .await
        .0,
        StatusCode::OK
    );
    let (_, ticket) = request(
        &app,
        "POST",
        "/api/rtc/token",
        Some(&admin.token),
        serde_json::json!({"channel_id":channel}),
    )
    .await;
    let ticket: RtcTicket = serde_json::from_value(ticket).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let shutdown = state.clone();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move { shutdown.shutdown_requested().await })
        .await
        .unwrap();
    });
    let url = format!("ws://{address}/rtc?access_token={}", ticket.token);
    let mut first = tokio_tungstenite::connect_async(&url).await.unwrap().0;
    let mut second = tokio_tungstenite::connect_async(&url).await.unwrap().0;
    let error = tokio_tungstenite::connect_async(&url).await.unwrap_err();
    let tokio_tungstenite::tungstenite::Error::Http(response) = error else {
        panic!("expected HTTP admission refusal");
    };
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(response.headers()[header::RETRY_AFTER], "1");
    first.close(None).await.unwrap();
    // A client Close reaches its peer before the detached bridge finishes both
    // bounded closes and drops its permit. Retry only the expected admission
    // refusal, rather than depending on an arbitrary scheduling sleep.
    let mut third = tokio::time::timeout(Duration::from_secs(6), async {
        loop {
            match tokio_tungstenite::connect_async(&url).await {
                Ok((socket, _)) => break socket,
                Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
                    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
                    assert_eq!(response.headers()[header::RETRY_AFTER], "1");
                }
                Err(_) => panic!("unexpected RTC reconnect failure"),
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("closed RTC bridge releases admission within its bounded cleanup deadline");
    // At most a pending 3s send plus two 1s close operations, allowing 1s for
    // scheduling. The deadline starts at shutdown, not after awaiting Axum.
    let cleanup_deadline = tokio::time::Instant::now() + Duration::from_secs(6);
    state.request_shutdown();
    for socket in [&mut second, &mut third] {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                match socket.next().await {
                    Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_)))
                    | None
                    | Some(Err(_)) => break,
                    _ => {}
                }
            }
        })
        .await
        .expect("RTC shutdown closes every active signaling bridge");
    }
    tokio::time::timeout(Duration::from_secs(6), server)
        .await
        .expect("RTC does not hold graceful stop")
        .unwrap();
    // Axum's HTTP listener completion does not join upgraded connection tasks.
    // Assert both slots are simultaneously free within the actual close bound;
    // partial probes drop immediately and never mask a leaked permit.
    let (a, b) = tokio::time::timeout_at(cleanup_deadline, async {
        loop {
            if let Some(a) = state.0.rtc_connections.try_acquire(&admin.user.id) {
                if let Some(b) = state.0.rtc_connections.try_acquire(&admin.user.id) {
                    break (a, b);
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("both RTC RAII permits release within bounded shutdown cleanup");
    drop((a, b));
    upstream.abort();
}

fn seed_message(state: &AppState, author: &Session, channel: &str, content: &str) -> String {
    let message = id();
    let db = lock(&state.0.db);
    db.execute("INSERT INTO messages(id,channel_id,author_id,content,created_at,created) VALUES(?1,?2,?3,?4,?5,?6)",params![message,channel,author.user.id,content,chrono::Utc::now().to_rfc3339(),now()]).unwrap();
    db.execute(
        "INSERT INTO message_order(message_id) VALUES(?1)",
        [&message],
    )
    .unwrap();
    message
}

#[test]
fn additive_chat_control_fields_preserve_older_protocol_json_defaults() {
    let channel: Channel = serde_json::from_value(
        serde_json::json!({"id":"old","name":"old","kind":"text","topic":""}),
    )
    .unwrap();
    assert_eq!(channel.slowmode_seconds, 0);
    assert!(!channel.slowmode_bypass);
    assert!(!channel.can_manage_pins);
    let created: CreateChannel =
        serde_json::from_value(serde_json::json!({"name":"old","kind":"text"})).unwrap();
    assert_eq!(created.slowmode_seconds, 0);
    let edit: EditChannel =
        serde_json::from_value(serde_json::json!({"topic":"still editable"})).unwrap();
    assert!(edit.slowmode_seconds.is_none());
    let message:Message=serde_json::from_value(serde_json::json!({"id":"m","channel_id":"old","recipient_id":null,"author":{"id":"u","username":"User","role":"member"},"content":"old","created_at":"2026-01-01T00:00:00Z"})).unwrap();
    assert!(message.pinned_at.is_none());
    let event: Event = serde_json::from_value(
        serde_json::json!({"kind":"messages","channel_id":"old","user_id":"u"}),
    )
    .unwrap();
    assert!(event.recipient_id.is_none());
    assert!(event.typing.is_none());
    assert!(event.expires_at_ms.is_none());
    let failure: ApiFailure =
        serde_json::from_value(serde_json::json!({"error":"old server failure"})).unwrap();
    assert!(failure.code.is_none());
    assert!(failure.retry_after_ms.is_none());
    let typing: TypingState = serde_json::from_value(serde_json::json!({"users":[]})).unwrap();
    assert_eq!(typing.server_time_ms, 0);
}

#[tokio::test]
async fn slowmode_is_transactional_deduplicated_persistent_and_staff_bypass_is_explicit() {
    let data = tempfile::tempdir().unwrap();
    let state = AppState::open(data.path(), "slowmode", 0).unwrap();
    let app = router(state.clone());
    let admin = signup(&app, "Admin", "").await;
    let member = signup(&app, "Member", &new_invite(&app, &admin).await).await;
    let channel = create_test_channel(&app, &admin, "text", false).await;
    let path = format!("/api/channels/{channel}");
    assert_eq!(
        request(
            &app,
            "PATCH",
            &path,
            Some(&member.token),
            serde_json::json!({"slowmode_seconds":5})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    for value in [
        serde_json::json!(-1),
        serde_json::json!(MAX_SLOWMODE_SECONDS + 1),
    ] {
        let status = request(
            &app,
            "PATCH",
            &path,
            Some(&admin.token),
            serde_json::json!({"slowmode_seconds":value}),
        )
        .await
        .0;
        assert!(matches!(
            status,
            StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY
        ));
    }
    let (_, edited) = request(
        &app,
        "PATCH",
        &path,
        Some(&admin.token),
        serde_json::json!({"slowmode_seconds":5}),
    )
    .await;
    assert_eq!(edited["slowmode_seconds"], 5);
    assert_eq!(edited["slowmode_bypass"], true);
    let (_, view) = request(
        &app,
        "GET",
        &path,
        Some(&member.token),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(view["slowmode_bypass"], false);
    let body = serde_json::json!({"channel_id":channel,"content":"first","client_id":"first-send"});
    let first = send_test_message(&app, &member, body.clone()).await;
    let before = lock(&state.0.db)
        .query_row(
            "SELECT last_sent_ms FROM channel_send_cooldowns WHERE user_id=?1 AND channel_id=?2",
            params![member.user.id, channel],
            |r| r.get::<_, u64>(0),
        )
        .unwrap();
    let mut notices = state.0.events.subscribe();
    assert_eq!(
        send_test_message(&app, &member, body.clone()).await.id,
        first.id
    );
    assert!(matches!(
        notices.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));
    assert_eq!(
        lock(&state.0.db)
            .query_row(
                "SELECT COUNT(*) FROM message_send_log WHERE user_id=?1",
                [&member.user.id],
                |r| r.get::<_, u64>(0)
            )
            .unwrap(),
        1
    );
    assert_eq!(lock(&state.0.db).query_row("SELECT last_sent_ms FROM channel_send_cooldowns WHERE user_id=?1 AND channel_id=?2",params![member.user.id,channel],|r|r.get::<_,u64>(0)).unwrap(),before);
    let (status, headers, failure) = request_with_headers(
        &app,
        "POST",
        "/api/messages",
        Some(&member.token),
        serde_json::json!({"channel_id":channel,"content":"too soon","client_id":"second-send"}),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(failure["code"], "slowmode");
    assert_eq!(failure["channel_id"], channel);
    let remaining = failure["retry_after_ms"].as_u64().unwrap();
    assert!((1..=5000).contains(&remaining));
    assert_eq!(
        headers[header::RETRY_AFTER]
            .to_str()
            .unwrap()
            .parse::<u64>()
            .unwrap(),
        remaining.div_ceil(1000)
    );
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/messages",
            Some(&member.token),
            serde_json::json!({"channel_id":channel,"content":"changed","client_id":"first-send"})
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    let admin_one = send_test_message(
        &app,
        &admin,
        serde_json::json!({"channel_id":channel,"content":"staff"}),
    )
    .await;
    let admin_two = send_test_message(
        &app,
        &admin,
        serde_json::json!({"channel_id":channel,"content":"staff again"}),
    )
    .await;
    assert_ne!(admin_one.id, admin_two.id);
    let voice = create_test_channel(&app, &admin, "voice", false).await;
    assert_eq!(
        request(
            &app,
            "PATCH",
            &format!("/api/channels/{voice}"),
            Some(&admin.token),
            serde_json::json!({"slowmode_seconds":1})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    drop(notices);
    drop(app);
    drop(state);
    let restarted = AppState::open(data.path(), "slowmode", 0).unwrap();
    let app = router(restarted.clone());
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/messages",
            Some(&member.token),
            serde_json::json!({"channel_id":channel,"content":"restart cannot bypass"})
        )
        .await
        .0,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(send_test_message(&app, &member, body).await.id, first.id);
    // Advance this fixture's durable timestamp without wall-clock sleeps.
    lock(&restarted.0.db)
        .execute(
            "UPDATE channel_send_cooldowns SET last_sent_ms=?1 WHERE user_id=?2",
            params![now_ms().saturating_sub(5000), member.user.id],
        )
        .unwrap();
    send_test_message(
        &app,
        &member,
        serde_json::json!({"channel_id":channel,"content":"after deadline"}),
    )
    .await;
    request(
        &app,
        "PATCH",
        &path,
        Some(&admin.token),
        serde_json::json!({"slowmode_seconds":0}),
    )
    .await;
    send_test_message(
        &app,
        &member,
        serde_json::json!({"channel_id":channel,"content":"disabled"}),
    )
    .await;
}

#[tokio::test]
async fn successful_new_send_burst_is_account_wide_and_rollback_delete_restart_cannot_refund() {
    let data = tempfile::tempdir().unwrap();
    let state = AppState::open(data.path(), "burst", 0).unwrap();
    let app = router(state.clone());
    let admin = signup(&app, "Admin", "").await;
    let member = signup(&app, "Member", &new_invite(&app, &admin).await).await;
    let a = create_test_channel(&app, &admin, "text", false).await;
    let b = create_test_channel(&app, &admin, "text", false).await;
    assert_eq!(request(&app,"POST","/api/messages",Some(&admin.token),serde_json::json!({"channel_id":a,"content":"invalid attachment","attachment_ids":["missing"]})).await.0,StatusCode::FORBIDDEN);
    assert_eq!(
        lock(&state.0.db)
            .query_row("SELECT COUNT(*) FROM message_send_log", [], |r| r
                .get::<_, u64>(0))
            .unwrap(),
        0
    );
    let mut first = None;
    for index in 0..5 {
        let target = match index % 3 {
            0 => serde_json::json!({"channel_id":a}),
            1 => serde_json::json!({"channel_id":b}),
            _ => serde_json::json!({"recipient_id":member.user.id}),
        };
        let mut body = target;
        body["content"] = format!("burst {index}").into();
        body["client_id"] = format!("burst-{index}").into();
        let message = send_test_message(&app, &admin, body.clone()).await;
        if index == 0 {
            first = Some((body, message));
        }
    }
    let (body, message) = first.unwrap();
    assert_eq!(
        send_test_message(&app, &admin, body.clone()).await.id,
        message.id
    );
    let (status, headers, failure) = request_with_headers(
        &app,
        "POST",
        "/api/messages",
        Some(&admin.token),
        serde_json::json!({"recipient_id":member.user.id,"content":"sixth"}),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(failure["code"], "message_rate_limit");
    assert!(failure.get("channel_id").is_none());
    assert!(headers.contains_key(header::RETRY_AFTER));
    request(
        &app,
        "DELETE",
        &format!("/api/messages/{}", message.id),
        Some(&admin.token),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(
        request(&app, "POST", "/api/messages", Some(&admin.token), body)
            .await
            .0,
        StatusCode::GONE
    );
    send_test_message(
        &app,
        &member,
        serde_json::json!({"channel_id":a,"content":"another account"}),
    )
    .await;
    drop(app);
    drop(state);
    let state = AppState::open(data.path(), "burst", 0).unwrap();
    let app = router(state.clone());
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/messages",
            Some(&admin.token),
            serde_json::json!({"channel_id":b,"content":"restart cannot bypass"})
        )
        .await
        .0,
        StatusCode::TOO_MANY_REQUESTS
    );
    lock(&state.0.db)
        .execute(
            "UPDATE message_send_log SET sent_at_ms=?1 WHERE user_id=?2",
            params![now_ms().saturating_sub(5000), admin.user.id],
        )
        .unwrap();
    send_test_message(
        &app,
        &admin,
        serde_json::json!({"channel_id":b,"content":"after window"}),
    )
    .await;
    assert_eq!(
        lock(&state.0.db)
            .query_row(
                "SELECT COUNT(*) FROM message_send_log WHERE user_id=?1",
                [&admin.user.id],
                |r| r.get::<_, u64>(0)
            )
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn pins_are_moderated_channel_scoped_idempotent_durable_and_cascading() {
    let data = tempfile::tempdir().unwrap();
    let state = AppState::open(data.path(), "pins", 0).unwrap();
    let app = router(state.clone());
    let admin = signup(&app, "Admin", "").await;
    let member = signup(&app, "Member", &new_invite(&app, &admin).await).await;
    let moderator = signup(&app, "Moderator", &new_invite(&app, &admin).await).await;
    lock(&state.0.db)
        .execute(
            "UPDATE users SET role='moderator' WHERE id=?1",
            [&moderator.user.id],
        )
        .unwrap();
    let channel = create_test_channel(&app, &admin, "text", true).await;
    let other = create_test_channel(&app, &admin, "text", false).await;
    let message = send_test_message(
        &app,
        &admin,
        serde_json::json!({"channel_id":channel,"content":"durable pin"}),
    )
    .await;
    let route = format!("/api/channels/{channel}/pins/{}", message.id);
    let list = format!("/api/channels/{channel}/pins");
    for token in [&member.token, &moderator.token] {
        assert_eq!(
            request(&app, "PUT", &route, Some(token), serde_json::Value::Null)
                .await
                .0,
            StatusCode::FORBIDDEN
        );
    }
    let permissions = serde_json::json!({"entries":[{"subject_kind":"user","subject_id":moderator.user.id,"read":true,"send":false,"join":false,"stream":false}]});
    assert_eq!(
        request(
            &app,
            "PUT",
            &format!("/api/channels/{channel}/permissions"),
            Some(&admin.token),
            permissions
        )
        .await
        .0,
        StatusCode::OK
    );
    let cursor = latest_change(&lock(&state.0.db)).unwrap();
    let mut events = state.0.events.subscribe();
    let (status, pin) = request(
        &app,
        "PUT",
        &route,
        Some(&moderator.token),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(pin["message"]["pinned_at"], pin["pinned_at"]);
    assert_eq!(pin["pinned_by"]["id"], moderator.user.id);
    let first_event = events.recv().await.unwrap();
    assert_eq!(
        first_event.event.channel_id.as_deref(),
        Some(channel.as_str())
    );
    assert!(first_event.audience.is_none());
    assert_eq!(events.recv().await.unwrap().event.kind, "pins");
    let changed = latest_change(&lock(&state.0.db)).unwrap();
    assert!(changed > cursor);
    assert_eq!(
        request(
            &app,
            "PUT",
            &route,
            Some(&moderator.token),
            serde_json::Value::Null
        )
        .await
        .1,
        pin
    );
    assert_eq!(latest_change(&lock(&state.0.db)).unwrap(), changed);
    assert!(events.try_recv().is_err());
    assert_eq!(
        request(
            &app,
            "GET",
            &list,
            Some(&member.token),
            serde_json::Value::Null
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            &app,
            "PUT",
            &format!("/api/channels/{other}/pins/{}", message.id),
            Some(&admin.token),
            serde_json::Value::Null
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let dm = send_test_message(
        &app,
        &admin,
        serde_json::json!({"recipient_id":member.user.id,"content":"DM never a channel pin"}),
    )
    .await;
    assert_eq!(
        request(
            &app,
            "PUT",
            &format!("/api/channels/{other}/pins/{}", dm.id),
            Some(&admin.token),
            serde_json::Value::Null
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    request(
        &app,
        "PATCH",
        &format!("/api/messages/{}", message.id),
        Some(&admin.token),
        serde_json::json!({"content":"edited pinned text"}),
    )
    .await;
    drop(events);
    drop(app);
    drop(state);
    let state = AppState::open(data.path(), "pins", 0).unwrap();
    let app = router(state.clone());
    let (status, list) = request(
        &app,
        "GET",
        &list,
        Some(&moderator.token),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["message"]["content"], "edited pinned text");
    assert_eq!(list[0]["pinned_at"], pin["pinned_at"]);
    assert_eq!(
        request(
            &app,
            "DELETE",
            &route,
            Some(&moderator.token),
            serde_json::Value::Null
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    let cursor = latest_change(&lock(&state.0.db)).unwrap();
    assert_eq!(
        request(
            &app,
            "DELETE",
            &route,
            Some(&moderator.token),
            serde_json::Value::Null
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(latest_change(&lock(&state.0.db)).unwrap(), cursor);
    request(
        &app,
        "PUT",
        &route,
        Some(&admin.token),
        serde_json::Value::Null,
    )
    .await;
    request(
        &app,
        "DELETE",
        &format!("/api/messages/{}", message.id),
        Some(&admin.token),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(
        lock(&state.0.db)
            .query_row("SELECT COUNT(*) FROM channel_pins", [], |r| r
                .get::<_, u64>(0))
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn pin_cap_bounds_unique_messages_and_schema_migration_preserves_old_data() {
    let data = tempfile::tempdir().unwrap();
    let state = AppState::open(data.path(), "cap", 0).unwrap();
    let app = router(state.clone());
    let admin = signup(&app, "Admin", "").await;
    let channel = create_test_channel(&app, &admin, "text", false).await;
    let mut ids = Vec::new();
    for index in 0..=MAX_CHANNEL_PINS {
        ids.push(seed_message(
            &state,
            &admin,
            &channel,
            &format!("seed {index}"),
        ));
    }
    for message in ids.iter().take(MAX_CHANNEL_PINS) {
        assert_eq!(
            request(
                &app,
                "PUT",
                &format!("/api/channels/{channel}/pins/{message}"),
                Some(&admin.token),
                serde_json::Value::Null
            )
            .await
            .0,
            StatusCode::OK
        );
    }
    assert_eq!(
        request(
            &app,
            "PUT",
            &format!("/api/channels/{channel}/pins/{}", ids[0]),
            Some(&admin.token),
            serde_json::Value::Null
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        request(
            &app,
            "PUT",
            &format!("/api/channels/{channel}/pins/{}", ids[MAX_CHANNEL_PINS]),
            Some(&admin.token),
            serde_json::Value::Null
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    let list = request(
        &app,
        "GET",
        &format!("/api/channels/{channel}/pins"),
        Some(&admin.token),
        serde_json::Value::Null,
    )
    .await
    .1;
    assert_eq!(list.as_array().unwrap().len(), MAX_CHANNEL_PINS);
    // Preserve a real v4 schema and migrate it through the public open path.
    drop(app);
    drop(state);
    let db = Connection::open(data.path().join("relay.sqlite3")).unwrap();
    crate::tests::discard_post_profile_additions(&db);
    db.execute_batch("DROP TABLE crypto_call_requests;DROP TABLE crypto_key_package_consumptions;DROP TABLE crypto_welcome_packages;DROP TABLE crypto_operations;DROP TABLE crypto_events;DROP TABLE crypto_groups;DROP TABLE crypto_key_packages;DROP TABLE crypto_devices;DROP TABLE user_profiles;DROP TRIGGER channel_pin_scope;DROP TABLE channel_pins;DROP TABLE channel_send_cooldowns;DROP TABLE message_send_log;ALTER TABLE channels DROP COLUMN slowmode_seconds;DELETE FROM schema_migrations WHERE version>=5;PRAGMA user_version=4;").unwrap();
    db.execute(
        "INSERT INTO metadata(key,value) VALUES('migration-fixture','kept')",
        [],
    )
    .unwrap();
    drop(db);
    let reopened = AppState::open(data.path(), "migrated", 0).unwrap();
    assert_eq!(
        get_channel(&lock(&reopened.0.db), &channel)
            .unwrap()
            .slowmode_seconds,
        0
    );
    assert_eq!(
        get_message(&lock(&reopened.0.db), &ids[0]).unwrap().content,
        "seed 0"
    );
    assert!(data
        .path()
        .join("backups")
        .read_dir()
        .unwrap()
        .next()
        .is_some());
    assert_eq!(
        lock(&reopened.0.db)
            .pragma_query_value(None, "user_version", |r| r.get::<_, u32>(0))
            .unwrap(),
        SCHEMA_VERSION
    );
}

#[tokio::test]
async fn typing_is_ephemeral_private_bounded_and_current_permissions_revoke_it() {
    let data = tempfile::tempdir().unwrap();
    let state = AppState::open(data.path(), "typing", 0).unwrap();
    let app = router(state.clone());
    let admin = signup(&app, "Admin", "").await;
    let member = signup(&app, "Member", &new_invite(&app, &admin).await).await;
    let outsider = signup(&app, "Outsider", &new_invite(&app, &admin).await).await;
    let mut events = state.0.events.subscribe();
    let cursor = latest_change(&lock(&state.0.db)).unwrap();
    let direct = serde_json::json!({"peer_id":admin.user.id,"typing":true});
    assert_eq!(
        request(&app, "POST", "/api/typing", Some(&member.token), direct)
            .await
            .0,
        StatusCode::NO_CONTENT
    );
    let event = events.recv().await.unwrap();
    assert_eq!(event.event.kind, "typing");
    assert_eq!(event.event.typing, Some(true));
    assert_eq!(
        event.event.user_id.as_deref(),
        Some(member.user.id.as_str())
    );
    assert_eq!(
        event.event.recipient_id.as_deref(),
        Some(admin.user.id.as_str())
    );
    assert_eq!(
        event.audience.unwrap(),
        vec![member.user.id.clone(), admin.user.id.clone()]
    );
    let (status, snapshot) = request(
        &app,
        "GET",
        &format!("/api/typing?peer_id={}", member.user.id),
        Some(&admin.token),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let snapshot: TypingState = serde_json::from_value(snapshot).unwrap();
    assert_eq!(snapshot.users.len(), 1);
    assert!((1..=TYPING_TTL_MS).contains(
        &snapshot.users[0]
            .expires_at_ms
            .saturating_sub(snapshot.server_time_ms)
    ));
    assert_eq!(
        request(
            &app,
            "GET",
            &format!("/api/typing?peer_id={}", member.user.id),
            Some(&outsider.token),
            serde_json::Value::Null
        )
        .await
        .1["users"],
        serde_json::json!([])
    );
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/typing",
            Some(&member.token),
            serde_json::json!({"peer_id":"unknown","typing":true})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/typing",
            Some(&member.token),
            serde_json::json!({"peer_id":admin.user.id,"typing":true,"content":"not accepted"})
        )
        .await
        .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        latest_change(&lock(&state.0.db)).unwrap(),
        cursor,
        "typing never enters durable changes"
    );
    for entry in lock(&state.0.typing).values_mut() {
        entry.expires = Instant::now() - Duration::from_millis(1);
    }
    assert_eq!(
        request(
            &app,
            "GET",
            &format!("/api/typing?peer_id={}", member.user.id),
            Some(&admin.token),
            serde_json::Value::Null
        )
        .await
        .1["users"],
        serde_json::json!([])
    );
    let channel = create_test_channel(&app, &admin, "text", false).await;
    request(
        &app,
        "POST",
        "/api/typing",
        Some(&member.token),
        serde_json::json!({"channel_id":channel,"typing":true}),
    )
    .await;
    let one = request(
        &app,
        "GET",
        &format!("/api/typing?channel_id={channel}"),
        Some(&admin.token),
        serde_json::Value::Null,
    )
    .await
    .1;
    assert_eq!(one["users"].as_array().unwrap().len(), 1);
    request(&app,"PUT",&format!("/api/channels/{channel}/permissions"),Some(&admin.token),serde_json::json!({"entries":[{"subject_kind":"user","subject_id":member.user.id,"read":true,"send":false,"join":false,"stream":false}]})).await;
    assert!(lock(&state.0.typing).is_empty());
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/typing",
            Some(&member.token),
            serde_json::json!({"channel_id":channel,"typing":true})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    request(
        &app,
        "POST",
        "/api/typing",
        Some(&member.token),
        serde_json::json!({"peer_id":admin.user.id,"typing":true}),
    )
    .await;
    send_test_message(
        &app,
        &member,
        serde_json::json!({"recipient_id":admin.user.id,"content":"sending clears hint"}),
    )
    .await;
    assert!(lock(&state.0.typing).is_empty());
    request(
        &app,
        "POST",
        "/api/typing",
        Some(&member.token),
        serde_json::json!({"peer_id":admin.user.id,"typing":true}),
    )
    .await;
    request(
        &app,
        "POST",
        "/api/logout",
        Some(&member.token),
        serde_json::Value::Null,
    )
    .await;
    assert!(lock(&state.0.typing).is_empty());
    drop(events);
    drop(app);
    drop(state);
    let reopened = AppState::open(data.path(), "typing", 0).unwrap();
    assert!(lock(&reopened.0.typing).is_empty());
}

#[tokio::test]
async fn full_signin_rate_table_does_not_block_signed_in_users_or_grow_without_bound() {
    let data = tempfile::tempdir().unwrap();
    let state = AppState::open(data.path(), "separate rates", 0).unwrap();
    let app = router(state.clone());
    let admin = signup(&app, "Admin", "").await;
    {
        let mut rates = lock(&state.0.auth_rates);
        rates.clear();
        for n in 0..10_000 {
            rates.insert(format!("auth:source-{n}"), (now_ms(), 15));
        }
    }
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/login",
            None,
            serde_json::json!({"username":"Nobody","password":"invalid-password"})
        )
        .await
        .0,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(lock(&state.0.auth_rates).len(), 10_000);
    // A new authenticated mutation still has its own allowance.
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/invites",
            Some(&admin.token),
            serde_json::json!({})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(lock(&state.0.mutation_rates).len(), 1);
    for (start, _) in lock(&state.0.auth_rates).values_mut() {
        *start = now_ms().saturating_sub(60_001);
    }
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/login",
            None,
            serde_json::json!({"username":"","password":""})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        lock(&state.0.auth_rates).len(),
        1,
        "expired sources are reclaimed"
    );
}

#[tokio::test]
async fn middleware_rate_limit_returns_actual_remaining_cooldown_and_all_429s_are_structured() {
    let data = tempfile::tempdir().unwrap();
    let state = AppState::open(data.path(), "rates", 0).unwrap();
    let app = router(state.clone());
    let admin = signup(&app, "Admin", "").await;
    lock(&state.0.mutation_rates).insert(
        format!("mutation:{}", admin.user.id),
        (now_ms().saturating_sub(20_000), 300),
    );
    let (status, headers, failure) = request_with_headers(
        &app,
        "POST",
        "/api/typing",
        Some(&admin.token),
        serde_json::json!({"peer_id":admin.user.id,"typing":true}),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(failure["code"], "rate_limited");
    let delay = failure["retry_after_ms"].as_u64().unwrap();
    assert!((1..=40_000).contains(&delay));
    assert_eq!(
        headers[header::RETRY_AFTER]
            .to_str()
            .unwrap()
            .parse::<u64>()
            .unwrap(),
        delay.div_ceil(1000)
    );
    let response = ApiError::new(StatusCode::TOO_MANY_REQUESTS, "worker busy").into_response();
    assert_eq!(response.headers()[header::RETRY_AFTER], "1");
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let body: ApiFailure = serde_json::from_slice(&body).unwrap();
    assert_eq!(body.code.as_deref(), Some("service_busy"));
    assert_eq!(body.retry_after_ms, Some(1000));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_event_sockets_enforce_account_budget_release_slots_revoke_and_stop_without_peer_activity(
) {
    let data = tempfile::tempdir().unwrap();
    let state = AppState::open(data.path(), "sockets", 0).unwrap();
    let app = router(state.clone());
    let admin = signup(&app, "Admin", "").await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let shutdown = state.clone();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move { shutdown.shutdown_requested().await })
        .await
        .unwrap();
    });
    // A valid session in a URL must never authorize an event stream.
    let query_error = tokio_tungstenite::connect_async(format!(
        "ws://{address}/api/events?token={}",
        admin.token
    ))
    .await
    .unwrap_err();
    let tokio_tungstenite::tungstenite::Error::Http(response) = query_error else {
        panic!("expected missing-header authentication refusal");
    };
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let request = || event_request(address, &admin.token);
    let mut sockets = Vec::new();
    for _ in 0..4 {
        sockets.push(tokio_tungstenite::connect_async(request()).await.unwrap().0);
    }
    let error = tokio_tungstenite::connect_async(request())
        .await
        .unwrap_err();
    let tokio_tungstenite::tungstenite::Error::Http(response) = error else {
        panic!("expected HTTP admission refusal");
    };
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(response.headers()[header::RETRY_AFTER], "1");
    sockets.pop().unwrap().close(None).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let replacement = tokio_tungstenite::connect_async(request()).await.unwrap().0;
    sockets.push(replacement);
    lock(&state.0.db)
        .execute(
            "DELETE FROM sessions WHERE token_hash=?1",
            [token_hash(&admin.token)],
        )
        .unwrap();
    for socket in &mut sockets {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                match socket.next().await {
                    Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_)))
                    | None
                    | Some(Err(_)) => break,
                    _ => {}
                }
            }
        })
        .await
        .expect("idle socket is revoked without client input or notices");
    }
    let session = {
        let db = lock(&state.0.db);
        new_session(&db, admin.user.clone()).unwrap()
    };
    let mut held = tokio_tungstenite::connect_async(event_request(address, &session.token))
        .await
        .unwrap()
        .0;
    state.request_shutdown();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match held.next().await {
                Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_)))
                | None
                | Some(Err(_)) => break,
                _ => {}
            }
        }
    })
    .await
    .expect("shutdown closes a held socket");
    tokio::time::timeout(Duration::from_secs(6), server)
        .await
        .expect("held event sockets do not hang graceful host stop")
        .unwrap();
    tokio::time::timeout(Duration::from_millis(20), state.shutdown_requested())
        .await
        .expect("late observers see persistent shutdown");
}
