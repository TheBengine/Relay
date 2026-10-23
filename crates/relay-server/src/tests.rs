use super::*;
use axum::http::Request;
use http_body_util::BodyExt;
use tower::ServiceExt;

/// Strip additions after schema 9 before a fixture deliberately restores an older schema.
/// These are owned test databases, never the migration or recovery path.
pub(super) fn discard_post_profile_additions(db: &Connection) {
    discard_voice_resume_schema(db);
    db.execute_batch("DROP TABLE IF EXISTS community_system_events;")
        .unwrap();
    db.execute_batch("DROP TABLE IF EXISTS presentation_assets;")
        .unwrap();
    db.execute_batch("DROP TABLE IF EXISTS stage_members;DROP TABLE IF EXISTS event_interest;DROP TABLE IF EXISTS scheduled_events;DROP TABLE IF EXISTS conversation_members;DROP TABLE IF EXISTS conversations;").unwrap();
    db.execute_batch("DROP TABLE IF EXISTS friendships;DROP TABLE IF EXISTS user_blocks;DROP TABLE IF EXISTS user_presence;DROP TABLE IF EXISTS user_reports;").unwrap();
    db.execute_batch("DROP TABLE IF EXISTS member_roles;DROP TABLE IF EXISTS community_roles;DROP TABLE IF EXISTS audit_log;DROP INDEX IF EXISTS voice_text_parent;").unwrap();
    let columns = {
        let mut q = db.prepare("PRAGMA table_info(channels)").unwrap();
        let result = q
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        result
    };
    if columns.iter().any(|name| name == "voice_parent_id") {
        db.execute_batch("DELETE FROM messages WHERE channel_id IN(SELECT id FROM channels WHERE voice_parent_id IS NOT NULL);DELETE FROM channels WHERE voice_parent_id IS NOT NULL;ALTER TABLE channels DROP COLUMN voice_parent_id;").unwrap();
    }
}

pub(super) fn discard_voice_resume_schema(db: &Connection) {
    discard_private_call_schema(db);
    db.execute_batch(
        "DROP TABLE IF EXISTS identity_login_challenges;DROP TABLE IF EXISTS identity_link_challenges;DROP TABLE IF EXISTS linked_identities;",
    )
    .unwrap();
    db.execute_batch("DROP TABLE IF EXISTS authenticator_recovery;DROP TABLE IF EXISTS authenticator_setup;DROP TABLE IF EXISTS authenticator_credentials;").unwrap();
    db.execute_batch("DROP TRIGGER IF EXISTS voice_resume_device_revoke;DROP TRIGGER IF EXISTS voice_resume_channel_permission_insert;DROP TRIGGER IF EXISTS voice_resume_channel_permission_update;DROP TRIGGER IF EXISTS voice_resume_channel_permission_delete;DROP TABLE IF EXISTS voice_resume_leases;").unwrap();
}

pub(super) fn discard_private_call_schema(db: &Connection) {
    db.execute_batch("DROP TRIGGER IF EXISTS private_call_budget;
        DROP TRIGGER IF EXISTS private_call_ended;DROP TRIGGER IF EXISTS private_call_no_resurrection;
        DROP TRIGGER IF EXISTS private_call_lease_insert;DROP TRIGGER IF EXISTS private_call_lease_update;
        DROP TRIGGER IF EXISTS private_call_session_revoke;DROP TRIGGER IF EXISTS private_call_device_revoke;
        DROP TRIGGER IF EXISTS private_call_permission_insert;DROP TRIGGER IF EXISTS private_call_permission_update;
        DROP TRIGGER IF EXISTS private_call_permission_delete;DROP TRIGGER IF EXISTS private_call_unfriend;
        DROP TRIGGER IF EXISTS private_call_block;
        DROP TABLE IF EXISTS private_call_recovery;").unwrap();
}

#[tokio::test]
async fn category_edit_is_partial_admin_only_validated_and_persistent() {
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open(temp.path(), "categories", 0).unwrap();
    let app = router(state.clone());
    let admin = signup(&app, "Admin", "").await;
    let member = signup(&app, "Member", &new_invite(&app, &admin).await).await;
    let (status, original) = request(
        &app,
        "POST",
        "/api/categories",
        Some(&admin.token),
        serde_json::json!({"name":"Before","position":7}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let id = original["id"].as_str().unwrap();
    let path = format!("/api/categories/{id}");
    assert_eq!(
        request(
            &app,
            "PATCH",
            &path,
            Some(&member.token),
            serde_json::json!({"name":"Forged"})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    for body in [
        serde_json::json!({"name":"  "}),
        serde_json::json!({"name":"x".repeat(65)}),
    ] {
        assert_eq!(
            request(&app, "PATCH", &path, Some(&admin.token), body)
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        request(
            &app,
            "PATCH",
            &path,
            Some(&admin.token),
            serde_json::json!({"unexpected":true})
        )
        .await
        .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        request(
            &app,
            "PATCH",
            "/api/categories/missing",
            Some(&admin.token),
            serde_json::json!({"name":"Absent"})
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    let (_, renamed) = request(
        &app,
        "PATCH",
        &path,
        Some(&admin.token),
        serde_json::json!({"name":"  After  "}),
    )
    .await;
    assert_eq!(renamed["name"], "After");
    assert_eq!(renamed["position"], 7);
    let (_, moved) = request(
        &app,
        "PATCH",
        &path,
        Some(&admin.token),
        serde_json::json!({"position":-2}),
    )
    .await;
    assert_eq!(moved["name"], "After");
    assert_eq!(moved["position"], -2);
    assert_eq!(
        request(
            &app,
            "GET",
            "/api/categories",
            Some(&member.token),
            serde_json::Value::Null
        )
        .await
        .1,
        serde_json::json!([]),
        "unused categories stay private"
    );
    drop(app);
    drop(state);
    let restarted = AppState::open(temp.path(), "categories", 0).unwrap();
    let row: (String, i64) = lock(&restarted.0.db)
        .query_row(
            "SELECT name,position FROM categories WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(row, ("After".to_owned(), -2));
}

pub(super) async fn request(
    app: &Router,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let (status, _, body) = request_with_headers(app, method, path, token, body).await;
    (status, body)
}
pub(super) async fn request_with_headers(
    app: &Router,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: serde_json::Value,
) -> (StatusCode, HeaderMap, serde_json::Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(token) = token {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        headers,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}
pub(super) async fn signup(app: &Router, name: &str, invite: &str) -> Session {
    let (status, value) = request(
        app,
        "POST",
        "/api/register",
        None,
        serde_json::json!({"username":name,"password":"a-long-test-password","invite":invite}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{value}");
    serde_json::from_value(value).unwrap()
}
pub(super) async fn new_invite(app: &Router, admin: &Session) -> String {
    let (status, value) = request(
        app,
        "POST",
        "/api/invites",
        Some(&admin.token),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    value["code"].as_str().unwrap().into()
}

#[tokio::test]
async fn message_edits_are_author_only_scoped_bounded_and_persistent() {
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open(temp.path(), "test", 0).unwrap();
    let app = router(state.clone());
    let admin = signup(&app, "Admin", "").await;
    let member = signup(&app, "Member", &new_invite(&app, &admin).await).await;
    let outsider = signup(&app, "Outsider", &new_invite(&app, &admin).await).await;
    let (status, original) = request(
        &app,
        "POST",
        "/api/messages",
        Some(&member.token),
        serde_json::json!({"recipient_id":admin.user.id,"content":"before"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(serde_json::from_value::<Message>(original.clone())
        .unwrap()
        .edited_at
        .is_none());
    let message_id = original["id"].as_str().unwrap();
    let path = format!("/api/messages/{message_id}");
    let mut notices = state.0.events.subscribe();
    for token in [&admin.token, &outsider.token] {
        let (status, _) = request(
            &app,
            "PATCH",
            &path,
            Some(token),
            serde_json::json!({"content":"forged"}),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "neither admin nor a DM outsider may rewrite another author"
        );
    }
    let (status, _) = request(
        &app,
        "PATCH",
        &path,
        None,
        serde_json::json!({"content":"anonymous"}),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    for content in [" ".into(), "x".repeat(10_001)] {
        let (status, _) = request(
            &app,
            "PATCH",
            &path,
            Some(&member.token),
            serde_json::json!({"content":content}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
    let (status, _) = request(
        &app,
        "PATCH",
        &path,
        Some(&member.token),
        serde_json::json!({"content":"forged audience","recipient_id":outsider.user.id}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "the edit request is content-only"
    );
    let (status, unchanged) = request(
        &app,
        "PATCH",
        &path,
        Some(&member.token),
        serde_json::json!({"content":"before"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(unchanged, original);
    assert!(matches!(
        notices.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));
    let (status, edited) = request(
        &app,
        "PATCH",
        &path,
        Some(&member.token),
        serde_json::json!({"content":"after"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(edited["content"], "after");
    assert_eq!(edited["created_at"], original["created_at"]);
    assert_eq!(edited["recipient_id"], original["recipient_id"]);
    chrono::DateTime::parse_from_rfc3339(edited["edited_at"].as_str().unwrap()).unwrap();
    let notice = notices.recv().await.unwrap();
    assert_eq!(notice.event.kind, "messages");
    let audience = notice.audience.unwrap();
    assert!(audience.contains(&member.user.id) && audience.contains(&admin.user.id));
    assert!(!audience.contains(&outsider.user.id));
    let (status, _) = request(
        &app,
        "PATCH",
        "/api/messages/absent",
        Some(&member.token),
        serde_json::json!({"content":"after"}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    drop(app);
    drop(state);
    let reopened = router(AppState::open(temp.path(), "test", 0).unwrap());
    let (status, list) = request(
        &reopened,
        "GET",
        &format!("/api/messages?peer_id={}", member.user.id),
        Some(&admin.token),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        list[0], edited,
        "edited text and its timestamp survive restart"
    );
}

#[tokio::test]
async fn message_edits_preserve_attachments_and_reactions_and_allow_file_only_text() {
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open(temp.path(), "test", 0).unwrap();
    let app = router(state.clone());
    let author = signup(&app, "Author", "").await;
    let channel: String = lock(&state.0.db)
        .query_row(
            "SELECT id FROM channels WHERE kind='text' LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let upload_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/uploads?name=edit.txt&mime=text%2Fplain")
                .header(header::AUTHORIZATION, format!("Bearer {}", author.token))
                .body(Body::from("keep this attachment"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(upload_response.status(), StatusCode::OK);
    let upload: Attachment = serde_json::from_slice(
        &upload_response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes(),
    )
    .unwrap();
    let (status, original) = request(
        &app,
        "POST",
        "/api/messages",
        Some(&author.token),
        serde_json::json!({"channel_id":channel,"content":"caption","attachment_ids":[upload.id]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let path = format!("/api/messages/{}", original["id"].as_str().unwrap());
    let (status, reacted) = request(
        &app,
        "POST",
        &format!("{path}/reactions"),
        Some(&author.token),
        serde_json::json!({"emoji":"👍"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let mut notices = state.0.events.subscribe();
    let (status, edited) = request(
        &app,
        "PATCH",
        &path,
        Some(&author.token),
        serde_json::json!({"content":""}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(edited["attachments"], reacted["attachments"]);
    assert_eq!(edited["reactions"], reacted["reactions"]);
    assert_eq!(edited["content"], "");
    let notice = notices.recv().await.unwrap();
    assert_eq!(notice.event.channel_id.as_deref(), Some(channel.as_str()));
    assert!(
        notice.audience.is_none(),
        "channel edits notify all members"
    );
}

#[test]
fn legacy_sqlite_messages_migrate_additively_and_migration_is_idempotent() {
    let temp = tempfile::tempdir().unwrap();
    let legacy = Connection::open(temp.path().join("relay.sqlite3")).unwrap();
    legacy.execute_batch("CREATE TABLE users(id TEXT PRIMARY KEY, username TEXT NOT NULL UNIQUE COLLATE NOCASE, password_hash TEXT NOT NULL, role TEXT NOT NULL);
        CREATE TABLE channels(id TEXT PRIMARY KEY, name TEXT NOT NULL, kind TEXT NOT NULL, topic TEXT NOT NULL);
        CREATE TABLE messages(id TEXT PRIMARY KEY, channel_id TEXT REFERENCES channels(id), recipient_id TEXT REFERENCES users(id), author_id TEXT NOT NULL REFERENCES users(id), content TEXT NOT NULL, created_at TEXT NOT NULL, created INTEGER NOT NULL, CHECK((channel_id IS NULL) != (recipient_id IS NULL)));
        INSERT INTO users VALUES('author','Legacy','unused','admin');
        INSERT INTO channels VALUES('channel','general','text','');
        INSERT INTO messages VALUES('old-message','channel',NULL,'author','keep me','2026-10-03T00:00:00Z',1);").unwrap();
    drop(legacy);
    for _ in 0..2 {
        let state = AppState::open(temp.path(), "test", 0).unwrap();
        let old = get_message(&lock(&state.0.db), "old-message").unwrap();
        assert_eq!(old.content, "keep me");
        assert_eq!(old.created_at, "2026-10-03T00:00:00Z");
        assert!(old.edited_at.is_none());
        assert!(serde_json::to_value(old)
            .unwrap()
            .get("edited_at")
            .is_none());
    }
}

#[tokio::test]
async fn new_usernames_are_plain_so_none_can_pass_for_another() {
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open(temp.path(), "test", 0).unwrap();
    let app = router(state.clone());
    let admin = signup(&app, "Alex", "").await;
    for lookalike in ["\u{410}lex", "\u{c5}lex", "Alex\u{200b}", "\u{ff41}lex"] {
        let invite = new_invite(&app, &admin).await;
        let (status, value) = request(
            &app,
            "POST",
            "/api/register",
            None,
            serde_json::json!({"username":lookalike,"password":"a-long-test-password","invite":invite}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{lookalike}: {value}");
    }
    let invite = new_invite(&app, &admin).await;
    let plain = signup(&app, "mira.k-2_x", &invite).await;
    assert_eq!(plain.user.username, "mira.k-2_x");
    // Signing in keeps the older, wider rule, so no existing account is locked out.
    let older: AuthRequest = serde_json::from_value(
        serde_json::json!({"username":"Bjørn","password":"a-long-test-password"}),
    )
    .unwrap();
    assert!(validate_auth(&older).is_ok());
}

#[tokio::test]
async fn backup_ids_name_files_only_in_their_plain_form() {
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open(temp.path(), "test", 0).unwrap();
    let app = router(state.clone());
    let admin = signup(&app, "Admin", "").await;
    let id = "6d29f75a-57d1-4c41-9da5-19d3ad548ece";
    for variant in [
        format!("urn:uuid:{id}"),
        format!("%7B{id}%7D"),
        id.to_uppercase(),
        id.replace('-', ""),
    ] {
        let (status, value) = request(
            &app,
            "GET",
            &format!("/api/admin/backups/{variant}"),
            Some(&admin.token),
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{variant}: {value}");
    }
    let (status, _) = request(
        &app,
        "GET",
        &format!("/api/admin/backups/{id}"),
        Some(&admin.token),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "the plain form is looked up");
}

#[tokio::test]
async fn accounts_invites_dm_privacy_and_revocation() {
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open(temp.path(), "test", 0).unwrap();
    let app = router(state.clone());
    let admin = signup(&app, "Admin", "").await;
    assert_eq!(admin.user.role, "admin");
    let (status, _) = request(
        &app,
        "POST",
        "/api/register",
        None,
        serde_json::json!({"username":"Other","password":"a-long-test-password"}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let invite = new_invite(&app, &admin).await;
    let member = signup(&app, "Member", &invite).await;
    let (status, _) = request(
        &app,
        "POST",
        "/api/register",
        None,
        serde_json::json!({"username":"Reuse","password":"a-long-test-password","invite":invite}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let outsider = signup(&app, "Outsider", &new_invite(&app, &admin).await).await;
    let (status, _) = request(
        &app,
        "POST",
        "/api/invites",
        Some(&member.token),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let mut notices = state.0.events.subscribe();
    let (status, message) = request(
        &app,
        "POST",
        "/api/messages",
        Some(&admin.token),
        serde_json::json!({"recipient_id":member.user.id,"content":"private"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let notice = notices.recv().await.unwrap();
    assert!(!notice.audience.unwrap().contains(&outsider.user.id));
    let message_id = message["id"].as_str().unwrap();
    let (status, _) = request(
        &app,
        "POST",
        &format!("/api/messages/{message_id}/reactions"),
        Some(&outsider.token),
        serde_json::json!({"emoji":"👍"}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, list) = request(
        &app,
        "GET",
        &format!("/api/messages?peer_id={}", member.user.id),
        Some(&outsider.token),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(list.as_array().unwrap().is_empty());
    let (status, list) = request(
        &app,
        "GET",
        &format!("/api/messages?peer_id={}", admin.user.id),
        Some(&member.token),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list.as_array().unwrap().len(), 1);
    let (status, _) = request(
        &app,
        "POST",
        "/api/logout",
        Some(&member.token),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = request(
        &app,
        "GET",
        "/api/bootstrap",
        Some(&member.token),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    lock(&state.0.db)
        .execute(
            "UPDATE sessions SET expires=?1 WHERE token_hash=?2",
            params![now() - 1, token_hash(&admin.token)],
        )
        .unwrap();
    let (status, _) = request(
        &app,
        "GET",
        "/api/bootstrap",
        Some(&admin.token),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn attachments_are_scoped_and_chat_survives_restart() {
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open(temp.path(), "test", 0).unwrap();
    let app = router(state.clone());
    let admin = signup(&app, "Admin", "").await;
    let member = signup(&app, "Member", &new_invite(&app, &admin).await).await;
    let outsider = signup(&app, "Outsider", &new_invite(&app, &admin).await).await;
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/uploads?name=test.txt&mime=text%2Fplain")
                .header(header::AUTHORIZATION, format!("Bearer {}", admin.token))
                .body(Body::from("secret upload"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let upload: Attachment =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let (status, _) = request(
        &app,
        "GET",
        &upload.url,
        Some(&member.token),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status,message)=request(&app,"POST","/api/messages",Some(&admin.token),serde_json::json!({"recipient_id":member.user.id,"content":"file","attachment_ids":[upload.id]})).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = request(
        &app,
        "GET",
        &upload.url,
        Some(&member.token),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = request(
        &app,
        "GET",
        &upload.url,
        Some(&outsider.token),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status,_)=request(&app,"POST","/api/messages",Some(&outsider.token),serde_json::json!({"recipient_id":admin.user.id,"content":"stolen","attachment_ids":[upload.id]})).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let message_id = message["id"].as_str().unwrap();
    for expected in [1usize, 0] {
        let (status, value) = request(
            &app,
            "POST",
            &format!("/api/messages/{message_id}/reactions"),
            Some(&member.token),
            serde_json::json!({"emoji":"👍"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(value["reactions"].as_array().unwrap().len(), expected);
    }
    drop(app);
    drop(state);
    let reopened = router(AppState::open(temp.path(), "test", 0).unwrap());
    let (status, list) = request(
        &reopened,
        "GET",
        &format!("/api/messages?peer_id={}", member.user.id),
        Some(&admin.token),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list.as_array().unwrap().len(), 1);
    let (status, _) = request(
        &reopened,
        "DELETE",
        &format!("/api/messages/{message_id}"),
        Some(&admin.token),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = request(
        &reopened,
        "GET",
        &upload.url,
        Some(&admin.token),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn media_requires_channel_membership_and_logout_revokes_ticket() {
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open(temp.path(), "test", 0).unwrap();
    let app = router(state.clone());
    let admin = signup(&app, "Admin", "").await;
    let (status, value) = request(
        &app,
        "GET",
        "/api/bootstrap",
        Some(&admin.token),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let data: Bootstrap = serde_json::from_value(value).unwrap();
    let voice = data.channels.iter().find(|v| v.kind == "voice").unwrap();
    let (status, _) = request(
        &app,
        "GET",
        &format!("/api/media/frames?channel_id={}", voice.id),
        Some(&admin.token),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, ticket) = request(
        &app,
        "POST",
        "/api/voice/join",
        Some(&admin.token),
        serde_json::json!({"channel_id":voice.id}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let session = ticket["session_id"].as_u64().unwrap();
    assert!(lock(&state.0.voice).contains_key(&session));
    let second = request(
        &app,
        "POST",
        "/api/voice/join",
        Some(&admin.token),
        serde_json::json!({"channel_id":voice.id}),
    )
    .await
    .1;
    let replacement = second["session_id"].as_u64().unwrap();
    assert_ne!(session, replacement);
    assert!(!lock(&state.0.voice).contains_key(&session));
    assert_eq!(lock(&state.0.voice).len(), 1);
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new(&mut jpeg)
        .encode_image(&image::RgbImage::new(1, 1))
        .unwrap();
    for (width, bytes, expected) in [
        (1, jpeg.clone(), StatusCode::OK),
        (2, jpeg, StatusCode::BAD_REQUEST),
        (1, vec![0xff, 0xd8, 0xff, 0xd9], StatusCode::BAD_REQUEST),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!(
                        "/api/media/frame?kind=screen&width={width}&height=1"
                    ))
                    .header(header::AUTHORIZATION, format!("Bearer {}", admin.token))
                    .body(Body::from(bytes))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
    }
    let live_url = format!("/api/media/live/{}/screen", admin.user.id);
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(&live_url)
                .header(header::AUTHORIZATION, format!("Bearer {}", admin.token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    assert!(response.headers().contains_key("x-relay-frame-id"));
    let latest = lock(&state.0.frames)
        .get(&(admin.user.id.clone(), "screen".into()))
        .unwrap()
        .metadata
        .frame_id
        .clone();
    lock(&state.0.frames)
        .get_mut(&(admin.user.id.clone(), "screen".into()))
        .unwrap()
        .metadata
        .frame_id = "replacement".into();
    let old = request(
        &app,
        "GET",
        &format!("/api/media/frames/{latest}"),
        Some(&admin.token),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(old.0, StatusCode::NOT_FOUND);
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(&live_url)
                .header(header::AUTHORIZATION, format!("Bearer {}", admin.token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-relay-frame-id"], "replacement");
    lock(&state.0.frames)
        .get_mut(&(admin.user.id.clone(), "screen".into()))
        .unwrap()
        .channel_id = "another-room".into();
    assert_eq!(
        request(
            &app,
            "GET",
            &live_url,
            Some(&admin.token),
            serde_json::Value::Null
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let (status, _) = request(
        &app,
        "POST",
        "/api/logout",
        Some(&admin.token),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(lock(&state.0.voice).is_empty());
    assert_eq!(
        request(
            &app,
            "GET",
            &live_url,
            Some(&admin.token),
            serde_json::Value::Null
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
}

pub(super) async fn create_test_channel(
    app: &Router,
    admin: &Session,
    kind: &str,
    private: bool,
) -> String {
    let(status,channel)=request(app,"POST","/api/channels",Some(&admin.token),serde_json::json!({"name":"test-room","kind":kind,"visibility":if private{"private"}else{"public"}})).await;
    assert_eq!(status, StatusCode::OK, "{channel}");
    channel["id"].as_str().unwrap().to_owned()
}
pub(super) async fn send_test_message(
    app: &Router,
    session: &Session,
    body: serde_json::Value,
) -> Message {
    let (status, message) = request(app, "POST", "/api/messages", Some(&session.token), body).await;
    assert_eq!(status, StatusCode::OK, "{message}");
    serde_json::from_value(message).unwrap()
}

#[tokio::test]
async fn history_cursors_search_replies_unread_mentions_and_idempotency_survive_delete_restart() {
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open(temp.path(), "history", 0).unwrap();
    let app = router(state.clone());
    let admin = signup(&app, "Admin", "").await;
    let member = signup(&app, "Member", &new_invite(&app, &admin).await).await;
    let outsider = signup(&app, "Outsider", &new_invite(&app, &admin).await).await;
    let channel = create_test_channel(&app, &admin, "text", false).await;
    let mut sent = vec![];
    for index in 0..7 {
        // Advance only this history seeding fixture beyond the account burst.
        lock(&state.0.db)
            .execute(
                "UPDATE message_send_log SET sent_at_ms=?1 WHERE user_id=?2",
                params![now_ms().saturating_sub(5000), admin.user.id],
            )
            .unwrap();
        sent.push(send_test_message(&app,&admin,serde_json::json!({"channel_id":channel,"content":format!("history needle {index}"),"client_id":format!("send-{index}"),"mentions":[member.user.id]})).await);
    }
    let path = format!("/api/history?channel_id={channel}&limit=3");
    let (status, page) = request(
        &app,
        "GET",
        &path,
        Some(&member.token),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let first: HistoryPage = serde_json::from_value(page).unwrap();
    assert_eq!(first.messages.len(), 3);
    assert_eq!(first.messages[0].id, sent[4].id);
    let (status, page) = request(
        &app,
        "GET",
        &format!("{path}&before={}", first.next_before.unwrap()),
        Some(&member.token),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let second: HistoryPage = serde_json::from_value(page).unwrap();
    assert_eq!(
        second.messages.iter().map(|m| &m.id).collect::<Vec<_>>(),
        sent[1..4].iter().map(|m| &m.id).collect::<Vec<_>>()
    );
    let (_, page) = request(
        &app,
        "GET",
        &format!("/api/history?channel_id={channel}&around={}", sent[0].id),
        Some(&member.token),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(page["messages"].as_array().unwrap().len(), 7);
    let (status, page) = request(
        &app,
        "GET",
        &format!("/api/history?channel_id={channel}&search=needle%204"),
        Some(&member.token),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page["messages"][0]["id"], sent[4].id);
    let body = serde_json::json!({"channel_id":channel,"content":"history needle 6","client_id":"send-6","mentions":[member.user.id]});
    let duplicate = send_test_message(&app, &admin, body.clone()).await;
    assert_eq!(duplicate.id, sent[6].id);
    assert_eq!(duplicate.sequence, sent[6].sequence);
    let (status, _) = request(
        &app,
        "POST",
        "/api/messages",
        Some(&admin.token),
        serde_json::json!({"channel_id":channel,"content":"changed retry","client_id":"send-6"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (_, read) = request(
        &app,
        "PATCH",
        "/api/read-state",
        Some(&member.token),
        serde_json::json!({"channel_id":channel,"last_read_sequence":sent[3].sequence}),
    )
    .await;
    assert_eq!(read["unread_count"], 3);
    assert_eq!(read["mention_count"], 3);
    let (_, read) = request(
        &app,
        "PATCH",
        "/api/read-state",
        Some(&member.token),
        serde_json::json!({"channel_id":channel,"last_read_sequence":0}),
    )
    .await;
    assert_eq!(read["last_read_sequence"], sent[3].sequence);
    let dm = send_test_message(
        &app,
        &admin,
        serde_json::json!({"recipient_id":member.user.id,"content":"private reply parent"}),
    )
    .await;
    let(status,_)=request(&app,"POST","/api/messages",Some(&admin.token),serde_json::json!({"channel_id":channel,"content":"cross-conversation reply","reply_to_id":dm.id})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = request(
        &app,
        "GET",
        &format!("/api/history?channel_id={channel}&around={}", dm.id),
        Some(&outsider.token),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let reply=send_test_message(&app,&member,serde_json::json!({"recipient_id":admin.user.id,"content":"private response","reply_to_id":dm.id})).await;
    assert_eq!(reply.reply_to_id, Some(dm.id));
    let (status, _) = request(
        &app,
        "DELETE",
        &format!("/api/messages/{}", sent[6].id),
        Some(&admin.token),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = request(&app, "POST", "/api/messages", Some(&admin.token), body).await;
    assert_eq!(
        status,
        StatusCode::GONE,
        "a deleted successful retry must never resurrect the send"
    );
    let next = send_test_message(
        &app,
        &admin,
        serde_json::json!({"channel_id":channel,"content":"after delete"}),
    )
    .await;
    assert!(next.sequence > reply.sequence);
    let (_, changes) = request(
        &app,
        "GET",
        "/api/changes?after=0",
        Some(&outsider.token),
        serde_json::Value::Null,
    )
    .await;
    let changes: ChangePage = serde_json::from_value(changes).unwrap();
    assert!(changes.changes.iter().all(|c| c.channel_id.is_some()));
    assert!(changes
        .changes
        .iter()
        .any(|c| c.kind == "deleted" && c.message_id == sent[6].id));
    drop(app);
    drop(state);
    let reopened = AppState::open(temp.path(), "history", 0).unwrap();
    let db = lock(&reopened.0.db);
    assert_eq!(
        all_read_states(&db, &member.user.id)
            .unwrap()
            .into_iter()
            .find(|s| s.channel_id.as_deref() == Some(channel.as_str()))
            .unwrap()
            .last_read_sequence,
        sent[3].sequence
    );
    assert!(get_message(&db, &next.id).unwrap().sequence > sent[6].sequence);
}

#[tokio::test]
async fn opaque_edit_preserves_original_send_receipt_across_lost_ack_and_restart() {
    use base64::Engine as _;
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open(temp.path(), "encrypted edits", 0).unwrap();
    let app = router(state.clone());
    let author = signup(&app, "Author", "").await;
    let other = signup(&app, "Other", &new_invite(&app, &author).await).await;
    let channel = create_test_channel(&app, &author, "text", false).await;
    let original_wire = format!(
        "relay-e2ee:v1:{}",
        base64::engine::general_purpose::STANDARD.encode(b"opaque original MLS")
    );
    let edited_wire = format!(
        "relay-e2ee:v1:{}",
        base64::engine::general_purpose::STANDARD.encode(b"opaque edited MLS")
    );
    let request_body = serde_json::json!({"channel_id":channel,"content":original_wire,"client_id":"original-client-id"});
    let original = send_test_message(&app, &author, request_body.clone()).await;
    let edit_path = format!("/api/messages/{}", original.id);
    assert_eq!(
        request(
            &app,
            "PATCH",
            &edit_path,
            Some(&other.token),
            serde_json::json!({"content":edited_wire})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let (status, value) = request(
        &app,
        "PATCH",
        &edit_path,
        Some(&author.token),
        serde_json::json!({"content":edited_wire}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{value}");
    let edited: Message = serde_json::from_value(value).unwrap();
    assert_eq!(edited.id, original.id);
    assert_eq!(edited.sequence, original.sequence);
    assert_eq!(edited.client_id, original.client_id);
    assert_eq!(edited.content, edited_wire);
    let duplicate = send_test_message(&app, &author, request_body.clone()).await;
    assert_eq!(duplicate.id, original.id);
    assert_eq!(
        duplicate.content, edited_wire,
        "retry of the original send never undoes the edit"
    );
    assert_eq!(request(&app,"POST","/api/messages",Some(&author.token),serde_json::json!({"channel_id":channel,"content":edited_wire,"client_id":"original-client-id"})).await.0,StatusCode::CONFLICT);
    drop(app);
    drop(state);
    let restored = AppState::open(temp.path(), "encrypted edits", 0).unwrap();
    let app = router(restored);
    let duplicate = send_test_message(&app, &author, request_body).await;
    assert_eq!(duplicate.id, original.id);
    assert_eq!(duplicate.content, edited_wire);
    assert_eq!(duplicate.client_id.as_deref(), Some("original-client-id"));
}

#[tokio::test]
async fn private_channel_permissions_cover_history_upload_owner_voice_media_and_change_feed() {
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open(temp.path(), "private", 0).unwrap();
    let app = router(state.clone());
    let admin = signup(&app, "Admin", "").await;
    let member = signup(&app, "Member", &new_invite(&app, &admin).await).await;
    let text = create_test_channel(&app, &admin, "text", true).await;
    let voice = create_test_channel(&app, &admin, "voice", true).await;
    for channel in [&text, &voice] {
        let(status,_)=request(&app,"PUT",&format!("/api/channels/{channel}/permissions"),Some(&admin.token),serde_json::json!({"entries":[{"subject_kind":"user","subject_id":member.user.id,"read":true,"send":true,"join":true,"stream":true}]})).await;
        assert_eq!(status, StatusCode::OK);
    }
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/uploads?name=private.txt")
                .header(header::AUTHORIZATION, format!("Bearer {}", member.token))
                .body(Body::from("private bytes"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let upload: Attachment =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let message = send_test_message(
        &app,
        &member,
        serde_json::json!({"channel_id":text,"content":"private","attachment_ids":[upload.id]}),
    )
    .await;
    let (status, _) = request(
        &app,
        "POST",
        "/api/voice/join",
        Some(&member.token),
        serde_json::json!({"channel_id":voice}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(lock(&state.0.voice).len(), 1);
    for channel in [&text, &voice] {
        let (status, _) = request(
            &app,
            "PUT",
            &format!("/api/channels/{channel}/permissions"),
            Some(&admin.token),
            serde_json::json!({"entries":[]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }
    assert!(lock(&state.0.voice).is_empty());
    for path in [
        format!("/api/history?channel_id={text}"),
        format!("/api/messages?channel_id={text}"),
        upload.url,
        format!("/api/media/frames?channel_id={voice}"),
    ] {
        let (status, _) = request(
            &app,
            "GET",
            &path,
            Some(&member.token),
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{path}");
    }
    for (method, path, body) in [
        (
            "PATCH",
            format!("/api/messages/{}", message.id),
            serde_json::json!({"content":"rewrite"}),
        ),
        (
            "POST",
            "/api/voice/join".into(),
            serde_json::json!({"channel_id":voice}),
        ),
        (
            "POST",
            "/api/messages".into(),
            serde_json::json!({"channel_id":text,"content":"unauthorized"}),
        ),
    ] {
        assert_eq!(
            request(&app, method, &path, Some(&member.token), body)
                .await
                .0,
            StatusCode::FORBIDDEN
        );
    }
    let (_, bootstrap) = request(
        &app,
        "GET",
        "/api/bootstrap",
        Some(&member.token),
        serde_json::Value::Null,
    )
    .await;
    assert!(bootstrap["channels"]
        .as_array()
        .unwrap()
        .iter()
        .all(|c| c["id"] != text && c["id"] != voice));
    let (_, changes) = request(
        &app,
        "GET",
        "/api/changes?after=0",
        Some(&member.token),
        serde_json::Value::Null,
    )
    .await;
    assert!(changes["changes"].as_array().unwrap().is_empty());
    assert!(
        changes["next_cursor"].as_u64().unwrap() > 0,
        "privacy filtering must still advance the replay cursor"
    );
}

#[tokio::test]
async fn backups_stream_attachment_data_restore_safely_and_revoke_sessions() {
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open(temp.path(), "backups", 0).unwrap();
    let app = router(state.clone());
    let admin = signup(&app, "Admin", "").await;
    let member = signup(&app, "Member", &new_invite(&app, &admin).await).await;
    let channel = create_test_channel(&app, &admin, "text", false).await;
    let uploaded = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/uploads?name=backed-up.txt")
                .header(header::AUTHORIZATION, format!("Bearer {}", admin.token))
                .body(Body::from("persisted attachment bytes"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(uploaded.status(), StatusCode::OK);
    let upload: Attachment =
        serde_json::from_slice(&uploaded.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let first=send_test_message(&app,&admin,serde_json::json!({"channel_id":channel,"content":"before snapshot","attachment_ids":[upload.id]})).await;
    assert_eq!(
        request(
            &app,
            "GET",
            "/api/admin/backup",
            Some(&member.token),
            serde_json::Value::Null
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let snapshot = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/admin/backup")
                .header(header::AUTHORIZATION, format!("Bearer {}", admin.token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(snapshot.status(), StatusCode::OK);
    assert_eq!(
        snapshot.headers()[header::CONTENT_TYPE],
        "application/vnd.sqlite3"
    );
    let bytes = snapshot.into_body().collect().await.unwrap().to_bytes();
    assert!(bytes.starts_with(b"SQLite format 3"));
    let (_, backups) = request(
        &app,
        "GET",
        "/api/admin/backups",
        Some(&admin.token),
        serde_json::Value::Null,
    )
    .await;
    let backup = backups[0]["id"].as_str().unwrap();
    let late = send_test_message(
        &app,
        &admin,
        serde_json::json!({"channel_id":channel,"content":"after snapshot"}),
    )
    .await;
    let (status, _) = request(
        &app,
        "POST",
        "/api/admin/restore",
        Some(&admin.token),
        serde_json::json!({"backup_id":backup,"confirm_server_id":"wrong"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(get_message(&lock(&state.0.db), &late.id).is_ok());
    let (status, value) = request(
        &app,
        "POST",
        "/api/admin/restore",
        Some(&admin.token),
        serde_json::json!({"backup_id":backup,"confirm_server_id":state.server_id().unwrap()}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{value}");
    assert_eq!(
        get_message(&lock(&state.0.db), &first.id)
            .unwrap()
            .attachments[0]
            .id,
        upload.id
    );
    assert_eq!(
        lock(&state.0.db)
            .query_row("SELECT data FROM uploads WHERE id=?1", [&upload.id], |r| {
                r.get::<_, Vec<u8>>(0)
            })
            .unwrap(),
        b"persisted attachment bytes"
    );
    assert!(get_message(&lock(&state.0.db), &late.id).is_err());
    assert_eq!(
        request(
            &app,
            "GET",
            "/api/bootstrap",
            Some(&admin.token),
            serde_json::Value::Null
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    assert!(
        std::fs::read_dir(temp.path().join("backups"))
            .unwrap()
            .count()
            >= 2,
        "keep pre-restore safety copy"
    );
    let bad = temp.path().join("corrupt.sqlite3");
    std::fs::write(&bad, b"not sqlite").unwrap();
    assert!(state.restore_from(&bad).is_err());
    assert!(get_message(&lock(&state.0.db), &first.id).is_ok());
}

#[tokio::test]
async fn roles_bans_last_admin_session_revoke_password_and_host_identity_are_enforced() {
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open(temp.path(), "admin", 0).unwrap();
    let app = router(state.clone());
    let admin = signup(&app, "Admin", "").await;
    let member = signup(&app, "Member", &new_invite(&app, &admin).await).await;
    assert_eq!(
        request(
            &app,
            "PATCH",
            &format!("/api/members/{}", admin.user.id),
            Some(&admin.token),
            serde_json::json!({"role":"member"})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        request(
            &app,
            "PATCH",
            &format!("/api/members/{}", member.user.id),
            Some(&admin.token),
            serde_json::json!({"status":"banned"})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        request(
            &app,
            "GET",
            "/api/bootstrap",
            Some(&member.token),
            serde_json::Value::Null
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    let (_, members) = request(
        &app,
        "GET",
        "/api/members",
        Some(&admin.token),
        serde_json::Value::Null,
    )
    .await;
    assert!(members
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["id"] == member.user.id && m["status"] == "banned"));
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/login",
            None,
            serde_json::json!({"username":"Member","password":"a-long-test-password"})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let (_, metadata) = request(
        &app,
        "GET",
        "/api/admin/metadata",
        Some(&admin.token),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(metadata["server_id"], state.server_id().unwrap());
    assert_eq!(metadata["instance_id"], state.instance_id());
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/admin/shutdown",
            Some(&admin.token),
            serde_json::json!({"instance_id":"stale","confirm_server_id":metadata["server_id"]})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(request(&app,"POST","/api/admin/shutdown",Some(&admin.token),serde_json::json!({"instance_id":metadata["instance_id"],"confirm_server_id":metadata["server_id"]})).await.0,StatusCode::NO_CONTENT);
    tokio::time::timeout(Duration::from_millis(20), state.shutdown_requested())
        .await
        .expect("shutdown notification retained");
    assert_eq!(request(&app,"PATCH","/api/password",Some(&admin.token),serde_json::json!({"current_password":"a-long-test-password","new_password":"new-long-test-password"})).await.0,StatusCode::NO_CONTENT);
    assert!(state.authenticate_token(&admin.token).is_err());
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/login",
            None,
            serde_json::json!({"username":"Admin","password":"new-long-test-password"})
        )
        .await
        .0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn storage_quota_fails_without_partial_blob_and_invites_have_finite_reusable_policy() {
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open_with_config(
        temp.path(),
        "quota",
        0,
        ServerConfig {
            storage_limit_bytes: 5,
            user_storage_limit_bytes: 4,
            ..Default::default()
        },
    )
    .unwrap();
    let app = router(state.clone());
    let admin = signup(&app, "Admin", "").await;
    let (_, invite) = request(
        &app,
        "POST",
        "/api/invites",
        Some(&admin.token),
        serde_json::json!({"max_uses":2,"expires_in":60,"label":"friends"}),
    )
    .await;
    let code = invite["code"].as_str().unwrap();
    signup(&app, "Member", code).await;
    signup(&app, "Other", code).await;
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/register",
            None,
            serde_json::json!({"username":"NoSlot","password":"a-long-test-password","invite":code})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/uploads?name=too-big.txt")
                .header(header::AUTHORIZATION, format!("Bearer {}", admin.token))
                .body(Body::from("12345"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::INSUFFICIENT_STORAGE);
    assert_eq!(
        lock(&state.0.db)
            .query_row("SELECT COUNT(*) FROM uploads", [], |r| r.get::<_, u32>(0))
            .unwrap(),
        0
    );
}

#[test]
fn future_schema_is_refused_and_failed_migration_does_not_commit_partial_columns() {
    let mut future = Connection::open_in_memory().unwrap();
    future
        .pragma_update(None, "user_version", SCHEMA_VERSION + 1)
        .unwrap();
    assert!(migrate_database(&mut future).is_err());
    let mut broken = Connection::open_in_memory().unwrap();
    broken.execute_batch("CREATE TABLE users(id TEXT PRIMARY KEY);CREATE TABLE channels(id TEXT PRIMARY KEY);CREATE TABLE messages(id TEXT PRIMARY KEY,author_id TEXT);CREATE TABLE invites(id TEXT);CREATE TABLE sessions(id TEXT);CREATE TABLE message_order(deliberate_conflict TEXT);CREATE TABLE schema_migrations(version INTEGER PRIMARY KEY,applied_at INTEGER);PRAGMA user_version=1;").unwrap();
    assert!(migrate_database(&mut broken).is_err());
    assert_eq!(
        broken
            .pragma_query_value(None, "user_version", |r| r.get::<_, u32>(0))
            .unwrap(),
        1
    );
    assert!(!broken
        .prepare("PRAGMA table_info(channels)")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(1))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
        .contains(&"visibility".into()));
}

#[tokio::test]
async fn rtc_tokens_gateway_and_actual_room_admin_revocation_follow_current_session_acl() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let removals = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
    let fail = Arc::new(AtomicBool::new(false));
    let received = removals.clone();
    let failing = fail.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let config = RtcConfig::new(
        format!("ws://{address}"),
        format!("http://{address}"),
        "test-key".into(),
        "test-secret-long-enough".into(),
    )
    .unwrap();
    let verify = config.clone();
    let mock = Router::new().route(
        "/twirp/livekit.RoomService/RemoveParticipant",
        post(
            move |headers: HeaderMap, Json(body): Json<serde_json::Value>| {
                let received = received.clone();
                let failing = failing.clone();
                let verify = verify.clone();
                async move {
                    if failing.load(Ordering::SeqCst) {
                        return StatusCode::SERVICE_UNAVAILABLE;
                    }
                    let token = headers[header::AUTHORIZATION]
                        .to_str()
                        .unwrap()
                        .strip_prefix("Bearer ")
                        .unwrap();
                    let claims = verify.verify(token).unwrap();
                    assert_eq!(claims["video"]["roomAdmin"], true);
                    assert_eq!(claims["video"]["room"], body["room"]);
                    lock(&received).push(body);
                    StatusCode::OK
                }
            },
        ),
    );
    let task = tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open_with_config(
        temp.path(),
        "rtc",
        0,
        ServerConfig {
            rtc: Some(config.clone()),
            ..Default::default()
        },
    )
    .unwrap();
    let app = router(state.clone());
    let admin = signup(&app, "Admin", "").await;
    let member = signup(&app, "Member", &new_invite(&app, &admin).await).await;
    let channel = create_test_channel(&app, &admin, "voice", true).await;
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/rtc/token",
            Some(&member.token),
            serde_json::json!({"channel_id":channel})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    request(&app,"PUT",&format!("/api/channels/{channel}/permissions"),Some(&admin.token),serde_json::json!({"entries":[{"subject_kind":"user","subject_id":member.user.id,"read":true,"send":false,"join":true,"stream":true}]})).await;
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/rtc/token",
            Some(&member.token),
            serde_json::json!({"channel_id":channel})
        )
        .await
        .0,
        StatusCode::FORBIDDEN,
        "requires current voice join"
    );
    request(
        &app,
        "POST",
        "/api/voice/join",
        Some(&member.token),
        serde_json::json!({"channel_id":channel}),
    )
    .await;
    let (status, ticket) = request(
        &app,
        "POST",
        "/api/rtc/token",
        Some(&member.token),
        serde_json::json!({"channel_id":channel}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let ticket: RtcTicket = serde_json::from_value(ticket).unwrap();
    let claims = config.verify(&ticket.token).unwrap();
    assert_eq!(claims["sub"], member.user.id);
    assert_eq!(claims["video"]["canPublishData"], false);
    assert_eq!(claims["video"]["canUpdateOwnMetadata"], false);
    assert!(claims["exp"].as_u64().unwrap() <= now() + 60);
    assert_eq!(
        claims["video"]["canPublishSources"],
        serde_json::json!(["camera", "screen_share", "screen_share_audio"])
    );
    assert_eq!(
        request(
            &app,
            "GET",
            "/api/rtc/authorize",
            Some(&ticket.token),
            serde_json::Value::Null
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        request(
            &app,
            "GET",
            "/api/rtc/authorize",
            Some(&format!("{}x", ticket.token)),
            serde_json::Value::Null
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    // Actual Twirp removal is unavailable, but revocation and gateway denial must
    // still commit. A later reachable SFU drains the bounded retry queue.
    fail.store(true, Ordering::SeqCst);
    let (status, _) = request(
        &app,
        "POST",
        "/api/logout",
        Some(&member.token),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        request(
            &app,
            "GET",
            "/api/rtc/authorize",
            Some(&ticket.token),
            serde_json::Value::Null
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    assert!(lock(&state.0.voice).is_empty());
    assert!(!lock(&state.0.rtc_removals).is_empty());
    fail.store(false, Ordering::SeqCst);
    state.flush_rtc_removals().await.unwrap();
    assert!(lock(&state.0.rtc_removals).is_empty());
    assert_eq!(
        lock(&removals)[0],
        serde_json::json!({"room":ticket.room,"identity":member.user.id})
    );
    task.abort();
}

#[tokio::test]
async fn kicked_accounts_require_a_fresh_invite_and_readonly_channels_disable_mutations() {
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open(temp.path(), "kick", 0).unwrap();
    let app = router(state.clone());
    let admin = signup(&app, "Admin", "").await;
    let member = signup(&app, "Member", &new_invite(&app, &admin).await).await;
    let channel = create_test_channel(&app, &admin, "text", false).await;
    let original = send_test_message(
        &app,
        &member,
        serde_json::json!({"channel_id":channel,"content":"before readonly"}),
    )
    .await;
    assert_eq!(request(&app,"PUT",&format!("/api/channels/{channel}/permissions"),Some(&admin.token),serde_json::json!({"entries":[{"subject_kind":"user","subject_id":member.user.id,"read":true,"send":false,"join":false,"stream":false}]})).await.0,StatusCode::OK);
    let (_, bootstrap) = request(
        &app,
        "GET",
        "/api/bootstrap",
        Some(&member.token),
        serde_json::Value::Null,
    )
    .await;
    let read = bootstrap["channels"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == channel)
        .unwrap();
    assert_eq!(read["can_send"], false);
    assert_eq!(read["can_join"], false);
    assert_eq!(read["can_stream"], false);
    assert_eq!(
        request(
            &app,
            "PATCH",
            &format!("/api/messages/{}", original.id),
            Some(&member.token),
            serde_json::json!({"content":"edit after readonly"})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            &app,
            "POST",
            &format!("/api/messages/{}/reactions", original.id),
            Some(&member.token),
            serde_json::json!({"emoji":"x"})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            &app,
            "PATCH",
            &format!("/api/members/{}", member.user.id),
            Some(&admin.token),
            serde_json::json!({"status":"removed"})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/login",
            None,
            serde_json::json!({"username":"Member","password":"a-long-test-password"})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let invite = new_invite(&app, &admin).await;
    let (status, rejoined) = request(
        &app,
        "POST",
        "/api/login",
        None,
        serde_json::json!({"username":"Member","password":"a-long-test-password","invite":invite}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        rejoined["user"]["id"], member.user.id,
        "rejoining retains history and stable identity"
    );
    assert_eq!(
        request(
            &app,
            "PATCH",
            &format!("/api/members/{}", member.user.id),
            Some(&admin.token),
            serde_json::json!({"status":"banned"})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(request(&app,"POST","/api/login",None,serde_json::json!({"username":"Member","password":"a-long-test-password","invite":new_invite(&app,&admin).await})).await.0,StatusCode::FORBIDDEN);
}

#[test]
fn validation_messages_use_a_real_en_dash() {
    let auth = |username: &str, password: &str| {
        validate_auth(&AuthRequest {
            second_factor: None,
            initial_protection: None,
            username: username.into(),
            password: password.into(),
            invite: String::new(),
        })
        .unwrap_err()
        .message
    };
    assert_eq!(
        auth("x", "a-long-test-password"),
        "Username must be 2\u{2013}32 letters, numbers, dots, dashes or underscores"
    );
    assert_eq!(
        auth("Member", "short"),
        "Password must be 10\u{2013}256 characters"
    );
}

#[test]
fn slowmode_messages_in_the_fixed_sources_use_a_real_en_dash() {
    // UTF-8 text decoded as Windows-1252 and saved again: "\u{2013}" becomes
    // "\u{e2}\u{20ac}\u{201c}", and a second pass starts with "\u{c3}\u{a2}".
    // Only the two files that carried the broken dashes are checked, as
    // compile-time copies; nothing is read from disk at run time.
    let markers = ["\u{e2}\u{20ac}", "\u{c3}\u{a2}", "\u{c3}\u{83}"];
    let slowmode = "Slowmode must be 0\u{2013}21600 seconds on a text channel";
    for (name, text) in [
        ("lib.rs", include_str!("lib.rs")),
        ("community.rs", include_str!("community.rs")),
    ] {
        assert!(text.contains(slowmode), "{name}: slowmode message");
        for (number, line) in text.lines().enumerate() {
            assert!(
                !markers.iter().any(|marker| line.contains(marker)),
                "{name}:{}: double-encoded text",
                number + 1
            );
        }
    }
}
