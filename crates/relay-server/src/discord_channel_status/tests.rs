use super::*;
use crate::tests::{new_invite, request, signup};
use axum::response::IntoResponse;
use serde_json::{json, Value};

fn policy(state: &AppState) -> DiscordBridgeSettings {
    let room: String = lock(&state.0.db)
        .query_row(
            "SELECT id FROM channels WHERE kind='voice' AND visibility='public' LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    DiscordBridgeSettings {
        enabled: true,
        status_enabled: true,
        status_channel_id: Some("33333".into()),
        application_id: "12345".into(),
        guild_id: "67890".into(),
        channel_id: Some(room),
        public_label: "Let's play".into(),
        join_instructions: "Ask the host for an invitation. @everyone".into(),
    }
}
fn save(state: &AppState, p: &DiscordBridgeSettings) {
    lock(&state.0.db).execute("INSERT INTO metadata(key,value) VALUES('discord-bridge-v1',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[serde_json::to_string(p).unwrap()]).unwrap();
    state
        .0
        .discord_changes
        .send_modify(|v| *v = v.wrapping_add(1));
}
fn pending(state: &AppState) -> Record {
    Record {
        version: 1,
        owner: owner(state),
        application: 12345,
        guild: 67890,
        channel: 33333,
        bot: 12345,
        nonce: "0123456789abcdef0123".into(),
        message: None,
        uncertain: true,
        missing: false,
    }
}
fn bot() -> Value {
    json!({"id":"12345","username":"FixtureBot","discriminator":"0000","bot":true})
}
fn message(content: &str, nonce: &str) -> Value {
    json!({"id":"44444","channel_id":"33333","author":bot(),"content":content,"timestamp":"2026-10-09T00:00:00.000000+00:00","edited_timestamp":null,"tts":false,"mention_everyone":false,"mentions":[],"mention_roles":[],"attachments":[],"embeds":[],"pinned":false,"type":0,"flags":4100,"nonce":nonce})
}
#[derive(Default)]
struct Behavior {
    ambiguous: bool,
    post_forbidden: bool,
    delete_forbidden: bool,
    wrong_guild: bool,
    delay_message: bool,
}
#[derive(Clone)]
struct Fixture {
    events: Arc<Mutex<Vec<Value>>>,
    message: Arc<Mutex<Option<Value>>>,
    behavior: Arc<Mutex<Behavior>>,
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
    url: String,
}
struct Local {
    f: Fixture,
    server: tokio::task::JoinHandle<()>,
}
impl Drop for Local {
    fn drop(&mut self) {
        self.server.abort();
    }
}
impl Local {
    async fn start(state: &AppState) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let f = Fixture {
            events: Default::default(),
            message: Default::default(),
            behavior: Default::default(),
            entered: Default::default(),
            release: Default::default(),
            url: url.clone(),
        };
        let app = Router::new()
            .route("/socket", get(ws))
            .fallback(rest)
            .with_state(f.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        *lock(&state.0.test_discord_proxy) = Some(url);
        Self { f, server }
    }
    fn http(&self, state: &AppState) -> Http {
        crate::discord_bridge::http_for(state, "fixture-only-loopback-token")
    }
    fn writes(&self, method: &str) -> Vec<Value> {
        lock(&self.f.events)
            .iter()
            .filter(|v| {
                v["method"] == method && v["path"].as_str().unwrap_or("").contains("/messages")
            })
            .cloned()
            .collect()
    }
    async fn wait(&self, method: &str) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while self.writes(method).is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }
}
async fn rest(
    State(f): State<Fixture>,
    method: axum::http::Method,
    uri: axum::http::Uri,
    bytes: axum::body::Bytes,
) -> axum::response::Response {
    let path = uri.path();
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    lock(&f.events).push(json!({"method":method.as_str(),"path":path,"body":body}));
    if path.ends_with("/gateway/bot") {
        return Json(json!({"url":f.url.replacen("http://","ws://",1)+"/socket","shards":1,"session_start_limit":{"remaining":100,"total":1000,"reset_after":86400000,"max_concurrency":1}})).into_response();
    }
    if path.ends_with("/gateway") {
        return Json(json!({"url":f.url.replacen("http://","ws://",1)+"/socket"})).into_response();
    }
    if path.ends_with("/oauth2/applications/@me") {
        return Json(json!({"id":"12345","name":"Fixture","icon":null,"description":"","bot_public":false,"bot_require_code_grant":false,"owner":bot(),"verify_key":"fixture","flags":0})).into_response();
    }
    if path.ends_with("/users/@me") {
        return Json(bot()).into_response();
    }
    if path.ends_with("/guilds/67890/commands") && method == axum::http::Method::POST {
        return Json(json!({"id":"902","application_id":"12345","guild_id":"67890","type":1,"name":"relay","description":"Show the shared Relay call and how to join","version":"1"})).into_response();
    }
    if path.ends_with("/guilds/67890/commands/902") && method == axum::http::Method::DELETE {
        return StatusCode::NO_CONTENT.into_response();
    }
    if path.ends_with("/channels/33333") || path.ends_with("/channels/33334") {
        let guild = if lock(&f.behavior).wrong_guild {
            "99999"
        } else {
            "67890"
        };
        return Json(json!({"id":path.rsplit('/').next().unwrap(),"guild_id":guild,"type":0,"name":"fixture","position":0,"permission_overwrites":[],"nsfw":false})).into_response();
    }
    if (path.ends_with("/channels/33333/messages") || path.ends_with("/channels/33334/messages"))
        && method == axum::http::Method::POST
    {
        if lock(&f.behavior).post_forbidden {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({"code":50013,"message":"fixture denied"})),
            )
                .into_response();
        }
        let mut m = message(
            body["content"].as_str().unwrap(),
            body["nonce"].as_str().unwrap(),
        );
        if path.ends_with("/channels/33334/messages") {
            m["id"] = json!("44445");
            m["channel_id"] = json!("33334");
        }
        m["flags"] = body["flags"].clone();
        *lock(&f.message) = Some(m.clone());
        // Server received POST but the acknowledgement is unusable. Never
        // rely on Discord's short nonce window for replay after restart.
        if lock(&f.behavior).ambiguous {
            return Json(json!({})).into_response();
        }
        return Json(m).into_response();
    }
    if path.ends_with("/channels/33333/messages/44444")
        || path.ends_with("/channels/33334/messages/44445")
    {
        if lock(&f.behavior).delay_message {
            f.entered.notify_one();
            f.release.notified().await;
        }
        if method == axum::http::Method::GET {
            if let Some(m) = lock(&f.message).clone() {
                return Json(m).into_response();
            }
        } else if method == axum::http::Method::PATCH {
            let mut stored = lock(&f.message);
            if let Some(m) = stored.as_mut() {
                m["content"] = body["content"].clone();
                if path.ends_with("/channels/33334/messages") {
                    m["id"] = json!("44445");
                    m["channel_id"] = json!("33334");
                }
                m["flags"] = body["flags"].clone();
                return Json(m.clone()).into_response();
            }
        } else if method == axum::http::Method::DELETE {
            if lock(&f.behavior).delete_forbidden {
                return (
                    StatusCode::FORBIDDEN,
                    Json(json!({"code":50013,"message":"fixture denied"})),
                )
                    .into_response();
            }
            *lock(&f.message) = None;
            return StatusCode::NO_CONTENT.into_response();
        }
    }
    (
        StatusCode::NOT_FOUND,
        Json(json!({"code":10008,"message":"fixture not found"})),
    )
        .into_response()
}
async fn ws(
    State(f): State<Fixture>,
    upgrade: axum::extract::WebSocketUpgrade,
) -> axum::response::Response {
    upgrade.on_upgrade(move |mut socket|async move {
        use axum::extract::ws::Message;
        socket.send(Message::Text(json!({"op":10,"d":{"heartbeat_interval":1000}}).to_string().into())).await.unwrap();
        while let Some(Ok(Message::Text(text)))=socket.recv().await {
            let value:Value=serde_json::from_str(&text).unwrap();
            if value["op"]==2 {
                lock(&f.events).push(json!({"identify_intents":value["d"]["intents"]}));
                let ready=json!({"op":0,"s":1,"t":"READY","d":{"v":10,"user":bot(),"guilds":[],"session_id":"fixture","resume_gateway_url":f.url.replacen("http://","ws://",1)+"/socket","application":{"id":"12345","flags":0}}});
                if socket.send(Message::Text(ready.to_string().into())).await.is_err(){break;}
            } else if value["op"]==1 && socket.send(Message::Text(json!({"op":11}).to_string().into())).await.is_err(){break;}
        }
    })
}
async fn fixture() -> (
    tempfile::TempDir,
    AppState,
    Router,
    Session,
    DiscordBridgeSettings,
    Local,
) {
    let t = tempfile::tempdir().unwrap();
    let s = AppState::open_with_config(
        t.path(),
        "PrivateCommunity",
        0,
        ServerConfig {
            local_control_token: Some("fixture-control-0123456789-0123456789".into()),
            ..Default::default()
        },
    )
    .unwrap();
    let app = router(s.clone());
    let admin = signup(&app, "PrivateAdminName", "").await;
    let p = policy(&s);
    save(&s, &p);
    let local = Local::start(&s).await;
    (t, s, app, admin, p, local)
}

#[tokio::test]
async fn rest_creates_once_edits_only_changes_and_persists_id_across_restart() {
    let (t, s, app, admin, p, l) = fixture().await;
    let http = l.http(&s);
    let text = snapshot(&s, &p).desired.unwrap().2;
    upsert(&s, &http, &p, 12345, 33333, 67890, &text)
        .await
        .unwrap();
    let post = l.writes("POST");
    assert_eq!(post.len(), 1);
    assert_eq!(post[0]["body"]["flags"], 4100);
    assert_eq!(post[0]["body"]["allowed_mentions"]["parse"], json!([]));
    assert_eq!(post[0]["body"]["enforce_nonce"], true);
    assert_eq!(post[0]["body"]["embeds"], json!([]));
    assert!(!text.contains("PrivateAdminName"));
    assert!(!text.contains("PrivateCommunity"));
    let saved = record(&s).unwrap().unwrap();
    assert_eq!(saved.message, Some(44444));
    assert!(!saved.uncertain);
    upsert(&s, &http, &p, 12345, 33333, 67890, &text)
        .await
        .unwrap();
    assert!(l.writes("PATCH").is_empty());
    let mut changed = p.clone();
    changed.public_label = "Updated label".into();
    save(&s, &changed);
    let text = snapshot(&s, &changed).desired.unwrap().2;
    upsert(&s, &http, &changed, 12345, 33333, 67890, &text)
        .await
        .unwrap();
    let patch = l.writes("PATCH");
    assert_eq!(patch.len(), 1);
    assert_eq!(patch[0]["body"]["flags"], 4100);
    assert_eq!(patch[0]["body"]["allowed_mentions"]["parse"], json!([]));
    let id = s.server_id().unwrap();
    drop(http);
    drop(app);
    drop(admin);
    drop(s);
    let reopened = AppState::open(t.path(), "PrivateCommunity", 0).unwrap();
    assert_eq!(reopened.server_id().unwrap(), id);
    assert!(record(&reopened).unwrap().as_ref() == Some(&saved));
    *lock(&reopened.0.test_discord_proxy) = Some(l.f.url.clone());
    upsert(
        &reopened,
        &l.http(&reopened),
        &changed,
        12345,
        33333,
        67890,
        &text,
    )
    .await
    .unwrap();
    assert_eq!(l.writes("POST").len(), 1);
    assert_eq!(l.writes("PATCH").len(), 1);
    assert!(!lock(&l.f.events)
        .iter()
        .any(|e| e["method"] == "GET"
            && e["path"].as_str().is_some_and(|p| p.ends_with("/messages"))));
}

#[tokio::test]
async fn ambiguous_creation_never_replays_and_recovery_requires_original_nonce() {
    let (t, s, app, admin, p, l) = fixture().await;
    lock(&l.f.behavior).ambiguous = true;
    let http = l.http(&s);
    let text = snapshot(&s, &p).desired.unwrap().2;
    assert_eq!(
        upsert(&s, &http, &p, 12345, 33333, 67890, &text).await,
        Err("uncertain")
    );
    assert!(record(&s).unwrap().unwrap().uncertain);
    drop(http);
    drop(app);
    drop(s);
    let s = AppState::open(t.path(), "PrivateCommunity", 0).unwrap();
    let app = router(s.clone());
    *lock(&s.0.test_discord_proxy) = Some(l.f.url.clone());
    lock(&s.0.discord_credential).environment(Some("fixture-only-loopback-token".into()));
    assert_eq!(
        upsert(&s, &l.http(&s), &p, 12345, 33333, 67890, &text).await,
        Err("uncertain")
    );
    assert_eq!(l.writes("POST").len(), 1);
    let nonce = lock(&l.f.message).as_ref().unwrap()["nonce"].clone();
    lock(&l.f.message).as_mut().unwrap()["nonce"] = json!("wrong");
    let path = "/api/discord/status/recover";
    assert_eq!(
        request(
            &app,
            "POST",
            path,
            Some(&admin.token),
            json!({"message_id":"44444"})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert!(record(&s).unwrap().unwrap().uncertain);
    lock(&l.f.message).as_mut().unwrap()["nonce"] = nonce;
    let (code, view) = request(
        &app,
        "POST",
        path,
        Some(&admin.token),
        json!({"message_id":"44444"}),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{view}");
    assert_eq!(record(&s).unwrap().unwrap().message, Some(44444));
    assert!(!record(&s).unwrap().unwrap().uncertain);
    assert!(!view.to_string().contains("fixture-only-loopback-token"));
    assert_eq!(l.writes("POST").len(), 1);
}

#[tokio::test]
async fn author_guild_host_bindings_never_touch_someone_elses_message() {
    let (_t, s, _app, _admin, p, l) = fixture().await;
    let http = l.http(&s);
    let text = snapshot(&s, &p).desired.unwrap().2;
    lock(&l.f.behavior).wrong_guild = true;
    assert_eq!(
        upsert(&s, &http, &p, 12345, 33333, 67890, &text).await,
        Err("wrong_channel")
    );
    assert!(l.writes("POST").is_empty());
    lock(&l.f.behavior).wrong_guild = false;
    upsert(&s, &http, &p, 12345, 33333, 67890, &text)
        .await
        .unwrap();
    lock(&l.f.message).as_mut().unwrap()["author"]["id"] = json!("77777");
    assert_eq!(
        upsert(&s, &http, &p, 12345, 33333, 67890, &text).await,
        Err("binding_changed")
    );
    assert_eq!(
        remove_record(&s, &http, 12345, 12345).await,
        Err("binding_changed")
    );
    assert!(l.writes("PATCH").is_empty());
    assert!(l.writes("DELETE").is_empty());
    let previous = record(&s).unwrap().unwrap();
    let mut moved = previous.clone();
    moved.owner = "different-host-binding".into();
    persist(&s, Some(&previous), Some(&moved)).unwrap();
    assert_eq!(visible_status(&s, &lock(&s.0.db)), "binding_changed");
    assert_eq!(
        remove_record(&s, &http, 12345, 12345).await,
        Err("binding_changed")
    );
    assert!(l.writes("DELETE").is_empty());
}

#[tokio::test]
async fn missing_message_pauses_until_confirmed_cleanup_then_explicit_reenable() {
    let (_t, s, _app, _admin, p, l) = fixture().await;
    let http = l.http(&s);
    let text = snapshot(&s, &p).desired.unwrap().2;
    upsert(&s, &http, &p, 12345, 33333, 67890, &text)
        .await
        .unwrap();
    *lock(&l.f.message) = None;
    assert_eq!(
        upsert(&s, &http, &p, 12345, 33333, 67890, &text).await,
        Err("message_missing")
    );
    assert_eq!(
        upsert(&s, &http, &p, 12345, 33333, 67890, &text).await,
        Err("message_missing")
    );
    assert_eq!(l.writes("POST").len(), 1);
    remove_record(&s, &http, 12345, 12345).await.unwrap();
    assert!(record(&s).unwrap().is_none());
    assert!(l.writes("DELETE").is_empty());
}

#[tokio::test]
async fn cleanup_failure_is_visible_and_keeps_exact_id_for_retry() {
    let (_t, s, app, admin, p, l) = fixture().await;
    lock(&s.0.discord_credential).environment(Some("fixture-only-loopback-token".into()));
    let http = l.http(&s);
    let text = snapshot(&s, &p).desired.unwrap().2;
    upsert(&s, &http, &p, 12345, 33333, 67890, &text)
        .await
        .unwrap();
    lock(&l.f.behavior).delete_forbidden = true;
    let path = "/api/discord/status/cleanup";
    assert_eq!(
        request(&app, "POST", path, Some(&admin.token), Value::Null)
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    assert!(!settings(&lock(&s.0.db)).unwrap().status_enabled);
    assert_eq!(record(&s).unwrap().unwrap().message, Some(44444));
    assert_eq!(visible_status(&s, &lock(&s.0.db)), "cleanup_needed");
    lock(&l.f.behavior).delete_forbidden = false;
    assert_eq!(
        request(&app, "POST", path, Some(&admin.token), Value::Null)
            .await
            .0,
        StatusCode::OK
    );
    assert!(record(&s).unwrap().is_none());
    assert_eq!(visible_status(&s, &lock(&s.0.db)), "off");
    assert_eq!(l.writes("POST").len(), 1);
}

#[tokio::test]
async fn cleanup_without_record_turns_off_and_admin_session_is_required() {
    let (_t, s, app, admin, _p, l) = fixture().await;
    let member = signup(&app, "Member", &new_invite(&app, &admin).await).await;
    for path in ["/api/discord/status/recover", "/api/discord/status/cleanup"] {
        assert_eq!(
            request(&app, "POST", path, None, json!({"message_id":"44444"}))
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            request(
                &app,
                "POST",
                path,
                Some(&member.token),
                json!({"message_id":"44444"})
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
    }
    let (_, v) = request(
        &app,
        "POST",
        "/api/discord/status/cleanup",
        Some(&admin.token),
        Value::Null,
    )
    .await;
    assert_eq!(v["settings"]["status_enabled"], false);
    assert!(lock(&l.f.events).is_empty());
    lock(&s.0.db)
        .execute(
            "DELETE FROM sessions WHERE token_hash=?1",
            [crate::session_hash(&admin.token)],
        )
        .unwrap();
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/discord/status/cleanup",
            Some(&admin.token),
            Value::Null
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn recovery_in_flight_rechecks_logout_and_never_commits_after_revocation() {
    let (_t, s, app, admin, _p, l) = fixture().await;
    lock(&s.0.discord_credential).environment(Some("fixture-only-loopback-token".into()));
    let r = pending(&s);
    persist(&s, None, Some(&r)).unwrap();
    *lock(&l.f.message) = Some(message("fixture", &r.nonce));
    lock(&l.f.behavior).delay_message = true;
    let token = admin.token.clone();
    let request_app = app.clone();
    let job = tokio::spawn(async move {
        request(
            &request_app,
            "POST",
            "/api/discord/status/recover",
            Some(&token),
            json!({"message_id":"44444"}),
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(3), l.f.entered.notified())
        .await
        .unwrap();
    lock(&s.0.db)
        .execute(
            "DELETE FROM sessions WHERE token_hash=?1",
            [crate::session_hash(&admin.token)],
        )
        .unwrap();
    l.f.release.notify_one();
    assert_eq!(job.await.unwrap().0, StatusCode::UNAUTHORIZED);
    assert!(record(&s).unwrap().as_ref() == Some(&r));
}

#[tokio::test]
async fn old_record_cas_cannot_erase_recovered_or_replaced_record() {
    let (_t, s, _app, admin, _p, _l) = fixture().await;
    let r = pending(&s);
    persist(&s, None, Some(&r)).unwrap();
    let mut known = r.clone();
    known.uncertain = false;
    known.message = Some(44444);
    persist(&s, Some(&r), Some(&known)).unwrap();
    assert!(persist(&s, Some(&r), None).is_err());
    assert!(record(&s).unwrap().as_ref() == Some(&known));
    let auth = s.authenticate_token(&admin.token).unwrap();
    let revision = lock(&s.0.discord_credential).revision();
    lock(&s.0.discord_credential).retry();
    assert!(persist_authorized(&s, &auth, revision, &known, &known).is_err());
    assert!(record(&s).unwrap().as_ref() == Some(&known));
}

#[tokio::test]
async fn policy_privacy_and_acl_changes_cancel_queued_public_content() {
    let (_t, s, _app, _admin, p, l) = fixture().await;
    let http = l.http(&s);
    let text = snapshot(&s, &p).desired.unwrap().2;
    lock(&s.0.db)
        .execute(
            "UPDATE channels SET visibility='private' WHERE id=?1",
            [p.channel_id.as_ref().unwrap()],
        )
        .unwrap();
    assert!(snapshot(&s, &p).desired.is_none());
    assert_eq!(
        upsert(&s, &http, &p, 12345, 33333, 67890, &text).await,
        Err("policy_changed")
    );
    assert!(l.writes("POST").is_empty());
    lock(&s.0.db)
        .execute(
            "UPDATE channels SET visibility='public' WHERE id=?1",
            [p.channel_id.as_ref().unwrap()],
        )
        .unwrap();
    upsert(&s, &http, &p, 12345, 33333, 67890, &text)
        .await
        .unwrap();
    lock(&s.0.db)
        .execute(
            "INSERT INTO channel_permissions VALUES(?1,'role','member',15)",
            [p.channel_id.as_ref().unwrap()],
        )
        .unwrap();
    assert!(snapshot(&s, &p).desired.is_none());
    cleanup(&s, &http, 12345, Some(12345)).await;
    assert!(record(&s).unwrap().is_none());
    assert_eq!(l.writes("DELETE").len(), 1);
}

#[tokio::test]
async fn no_idle_http_and_activity_bursts_are_debounced_with_a_write_limit() {
    let (_t, s, _app, _admin, p, l) = fixture().await;
    let http = l.http(&s);
    let (_ready, rx) = tokio::sync::watch::channel(Some(12345));
    let cancel = CancellationToken::new();
    let revision = lock(&s.0.discord_credential).revision();
    let status_state = s.clone();
    let status_policy = p.clone();
    let status_cancel = cancel.clone();
    let worker = tokio::spawn(async move {
        run(
            &status_state,
            &http,
            &status_policy,
            revision,
            rx,
            status_cancel,
        )
        .await
    });
    l.wait("POST").await;
    tokio::time::sleep(Duration::from_millis(80)).await;
    let idle = lock(&l.f.events).len();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(lock(&l.f.events).len(), idle);
    for i in 0..25 {
        let mut p = p.clone();
        p.public_label = format!("Burst{i}");
        save(&s, &p);
        s.notify("voice", None, None, None);
    }
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert!(
        l.writes("PATCH").is_empty(),
        "15-second production write limit"
    );
    assert_eq!(l.writes("POST").len(), 1);
    // Privacy cleanup takes priority over the write timer.
    lock(&s.0.db)
        .execute(
            "UPDATE channels SET visibility='private' WHERE id=?1",
            [p.channel_id.as_ref().unwrap()],
        )
        .unwrap();
    s.notify("channels", None, None, None);
    l.wait("DELETE").await;
    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(1), worker)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(s.0.discord_activity.receiver_count(), 0);
}

#[tokio::test]
async fn sdk_gateway_has_no_privileged_intents_and_opt_out_removes_own_status() {
    let (_t, s, app, admin, p, l) = fixture().await;
    let worker = tokio::spawn(crate::discord_bridge::run(
        s.clone(),
        Some("fixture-only-loopback-token".into()),
    ));
    l.wait("POST").await;
    assert!(lock(&l.f.events).iter().any(|e| e["identify_intents"] == 0));
    assert_eq!(l.writes("POST").len(), 1);
    let mut disabled = p;
    disabled.enabled = false;
    assert_eq!(
        request(
            &app,
            "PATCH",
            "/api/discord",
            Some(&admin.token),
            json!(disabled)
        )
        .await
        .0,
        StatusCode::OK
    );
    l.wait("DELETE").await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(record(&s).unwrap().is_none());
    assert_eq!(s.0.discord_activity.receiver_count(), 0);
    s.request_shutdown();
    tokio::time::timeout(Duration::from_secs(6), worker)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(l.writes("POST").len(), 1);
}

#[tokio::test]
async fn real_session_expiry_updates_count_on_timer_without_idle_http_polling() {
    let (_t, s, _app, admin, p, l) = fixture().await;
    let mut voice = crate::discord_bridge::tests::ticket(&admin, p.channel_id.as_ref().unwrap());
    voice.expires = now() + 4;
    lock(&s.0.voice).insert(123, voice);
    let snap = snapshot(&s, &p);
    assert!(snap.expires.is_some());
    assert!(snap.desired.unwrap().2.contains("1 person"));
    let http = l.http(&s);
    let (_ready, rx) = tokio::sync::watch::channel(Some(12345));
    let cancel = CancellationToken::new();
    let revision = lock(&s.0.discord_credential).revision();
    let st = s.clone();
    let expected = p.clone();
    let shutdown = cancel.clone();
    let worker = tokio::spawn(async move {
        run(&st, &http, &expected, revision, rx, shutdown).await;
    });
    l.wait("POST").await;
    assert!(l.writes("POST")[0]["body"]["content"]
        .as_str()
        .unwrap()
        .contains("1 person"));
    tokio::time::timeout(Duration::from_secs(19), async {
        while l.writes("PATCH").is_empty() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    let patch = l.writes("PATCH");
    assert_eq!(patch.len(), 1);
    assert!(patch[0]["body"]["content"]
        .as_str()
        .unwrap()
        .contains("0 people"));
    tokio::time::sleep(Duration::from_millis(150)).await;
    let idle = lock(&l.f.events).len();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(idle, lock(&l.f.events).len());
    cancel.cancel();
    worker.await.unwrap();
}

#[tokio::test]
async fn disabled_status_makes_no_requests_and_private_room_can_wake_without_polling() {
    let (_t, s, _app, _admin, mut p, l) = fixture().await;
    p.status_enabled = false;
    save(&s, &p);
    let http = l.http(&s);
    let (_ready, rx) = tokio::sync::watch::channel(Some(12345));
    let cancel = CancellationToken::new();
    let revision = lock(&s.0.discord_credential).revision();
    let st = s.clone();
    let expected = p.clone();
    let shutdown = cancel.clone();
    let worker = tokio::spawn(async move {
        run(&st, &http, &expected, revision, rx, shutdown).await;
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(lock(&l.f.events).is_empty());
    assert_eq!(s.0.discord_activity.receiver_count(), 0);
    lock(&s.0.db)
        .execute(
            "UPDATE channels SET visibility='private' WHERE id=?1",
            [p.channel_id.as_ref().unwrap()],
        )
        .unwrap();
    p.status_enabled = true;
    save(&s, &p);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(lock(&l.f.events).is_empty());
    assert_eq!(s.0.discord_activity.receiver_count(), 1);
    lock(&s.0.db)
        .execute(
            "UPDATE channels SET visibility='public' WHERE id=?1",
            [p.channel_id.as_ref().unwrap()],
        )
        .unwrap();
    s.notify("channels", None, None, None);
    l.wait("POST").await;
    assert_eq!(l.writes("POST").len(), 1);
    cancel.cancel();
    worker.await.unwrap();
    assert_eq!(s.0.discord_activity.receiver_count(), 0);
}

#[tokio::test]
async fn definitive_denied_creation_can_retry_and_never_claims_a_message_exists() {
    let (_t, s, _app, _admin, p, l) = fixture().await;
    let http = l.http(&s);
    let text = snapshot(&s, &p).desired.unwrap().2;
    lock(&l.f.behavior).post_forbidden = true;
    assert_eq!(
        upsert(&s, &http, &p, 12345, 33333, 67890, &text).await,
        Err("needs_permission")
    );
    let r = record(&s).unwrap().unwrap();
    assert!(!r.uncertain && r.message.is_none());
    lock(&l.f.behavior).post_forbidden = false;
    upsert(&s, &http, &p, 12345, 33333, 67890, &text)
        .await
        .unwrap();
    assert_eq!(record(&s).unwrap().unwrap().message, Some(44444));
}

#[test]
fn settings_defaults_legacy_views_and_bounded_backoff() {
    let old = json!({"enabled":false,"application_id":"","guild_id":"","channel_id":null,"public_label":"","join_instructions":""});
    let p: DiscordBridgeSettings = serde_json::from_value(old.clone()).unwrap();
    assert!(!p.status_enabled);
    assert!(p.status_channel_id.is_none());
    assert!(p.valid());
    let view: DiscordBridgeView =
        serde_json::from_value(json!({"settings":old,"connection":"off","public_preview":null}))
            .unwrap();
    assert!(view.channel_status.is_empty());
    let mut p = DiscordBridgeSettings {
        enabled: true,
        status_enabled: true,
        status_channel_id: Some("not-an-id".into()),
        application_id: "12345".into(),
        guild_id: "67890".into(),
        channel_id: Some("room".into()),
        public_label: "Play".into(),
        join_instructions: "Ask host".into(),
    };
    assert!(!p.valid());
    p.enabled = false;
    assert!(p.valid());
    p.enabled = true;
    p.status_channel_id = Some("33333".into());
    assert!(p.valid());
    assert_eq!(retry_delay(1), Duration::from_secs(30));
    assert_eq!(retry_delay(2), Duration::from_secs(60));
    assert_eq!(retry_delay(u32::MAX), Duration::from_secs(300));
    assert_eq!(INTERVAL, Duration::from_secs(15));
}

#[tokio::test]
async fn moving_channel_removes_only_old_owned_id_before_creating_new_status() {
    let (_t, s, _app, _admin, p, l) = fixture().await;
    let http = l.http(&s);
    let text = snapshot(&s, &p).desired.unwrap().2;
    upsert(&s, &http, &p, 12345, 33333, 67890, &text)
        .await
        .unwrap();
    let mut changed = p;
    changed.status_channel_id = Some("33334".into());
    save(&s, &changed);
    let text = snapshot(&s, &changed).desired.unwrap().2;
    upsert(&s, &http, &changed, 12345, 33334, 67890, &text)
        .await
        .unwrap();
    assert_eq!(l.writes("DELETE").len(), 1);
    assert!(l.writes("DELETE")[0]["path"]
        .as_str()
        .unwrap()
        .ends_with("/33333/messages/44444"));
    assert_eq!(l.writes("POST").len(), 2);
    let r = record(&s).unwrap().unwrap();
    assert_eq!(r.channel, 33334);
    assert_eq!(r.message, Some(44445));
    let events = lock(&l.f.events);
    let removed = events.iter().position(|e| e["method"] == "DELETE").unwrap();
    let added = events.iter().rposition(|e| e["method"] == "POST").unwrap();
    assert!(removed < added);
}

#[tokio::test]
async fn cleanup_in_flight_rechecks_revoked_admin_before_delete() {
    let (_t, s, app, admin, p, l) = fixture().await;
    lock(&s.0.discord_credential).environment(Some("fixture-only-loopback-token".into()));
    let http = l.http(&s);
    let text = snapshot(&s, &p).desired.unwrap().2;
    upsert(&s, &http, &p, 12345, 33333, 67890, &text)
        .await
        .unwrap();
    lock(&l.f.behavior).delay_message = true;
    let token = admin.token.clone();
    let request_app = app.clone();
    let job = tokio::spawn(async move {
        request(
            &request_app,
            "POST",
            "/api/discord/status/cleanup",
            Some(&token),
            Value::Null,
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(3), l.f.entered.notified())
        .await
        .unwrap();
    lock(&s.0.db)
        .execute(
            "DELETE FROM sessions WHERE token_hash=?1",
            [crate::session_hash(&admin.token)],
        )
        .unwrap();
    l.f.release.notify_one();
    assert_eq!(job.await.unwrap().0, StatusCode::BAD_REQUEST);
    assert!(l.writes("DELETE").is_empty());
    assert_eq!(record(&s).unwrap().unwrap().message, Some(44444));
    assert!(!settings(&lock(&s.0.db)).unwrap().status_enabled);
}

#[tokio::test]
async fn real_gateway_key_rotation_and_removal_retire_owned_status_without_orphan_worker() {
    let (_t, s, _app, admin, _p, l) = fixture().await;
    let worker = tokio::spawn(crate::discord_bridge::run(
        s.clone(),
        Some("fixture-only-loopback-token".into()),
    ));
    l.wait("POST").await;
    assert_eq!(
        crate::discord_host::tests::host_request(
            &s,
            &admin.token,
            "PUT",
            json!({"token":"fixture-generation-two-loopback-only"})
        )
        .await
        .0,
        StatusCode::OK
    );
    tokio::time::timeout(Duration::from_secs(12), async {
        while l.writes("POST").len() < 2 {
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(l.writes("DELETE").len(), 1);
    assert_eq!(
        crate::discord_host::tests::host_request(&s, &admin.token, "DELETE", Value::Null)
            .await
            .0,
        StatusCode::OK
    );
    tokio::time::timeout(Duration::from_secs(4), async {
        while l.writes("DELETE").len() < 2 {
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(record(&s).unwrap().is_none());
    assert_eq!(s.0.discord_activity.receiver_count(), 0);
    s.request_shutdown();
    tokio::time::timeout(Duration::from_secs(7), worker)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(l.writes("POST").len(), 2);
}
