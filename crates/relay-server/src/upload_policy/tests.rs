use super::*;
use crate::tests::{new_invite, request, signup};
use axum::http::Request;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

async fn upload_bytes(app: &Router, token: Option<&str>, bytes: Vec<u8>) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method("POST")
        .uri("/api/uploads?name=owned.bin&mime=application/octet-stream");
    if let Some(token) = token {
        req = req.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let reply = app
        .clone()
        .oneshot(req.body(Body::from(bytes)).unwrap())
        .await
        .unwrap();
    let status = reply.status();
    let bytes = reply.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}
async fn save(app: &Router, owner: &Session, policy: &UploadPolicy) -> UploadPolicy {
    let (status, body) = request(
        app,
        "PUT",
        "/api/settings/uploads",
        Some(&owner.token),
        json!(policy),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    serde_json::from_value(body).unwrap()
}
fn install_role(state: &AppState, user: &str, bits: u32) -> String {
    let role = id();
    let db = lock(&state.0.db);
    db.execute(
        "INSERT INTO community_roles VALUES(?1,'Owned role',0,1,?2)",
        params![role, bits],
    )
    .unwrap();
    db.execute(
        "INSERT INTO member_roles VALUES(?1,?2)",
        params![user, role],
    )
    .unwrap();
    role
}

#[tokio::test]
async fn policy_requires_a_real_server_administrator_and_detects_stale_or_deleted_roles() {
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open(temp.path(), "uploads", 0).unwrap();
    let app = router(state.clone());
    let owner = signup(&app, "Owner", "").await;
    let member = signup(&app, "Member", &new_invite(&app, &owner).await).await;
    let role = install_role(&state, &member.user.id, PERMISSION_ADMIN);
    let mut policy = UploadPolicy {
        max_file_bytes: MAX_SUPPORTED_UPLOAD_BYTES as u64,
        ..Default::default()
    };
    policy.role_limits.insert(role.clone(), 0);
    // A delegated administrator may manage existing permissions, but cannot
    // silently rewrite the server administrator's file resource policy.
    assert_eq!(
        request(
            &app,
            "PUT",
            "/api/settings/uploads",
            Some(&member.token),
            json!(policy)
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            &app,
            "GET",
            "/api/settings/uploads",
            Some(&member.token),
            Value::Null
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let saved = save(&app, &owner, &policy).await;
    assert_eq!(saved.revision, 1);
    assert_eq!(
        request(
            &app,
            "PUT",
            "/api/settings/uploads",
            Some(&owner.token),
            json!(policy)
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    let (_, member_boot) = request(
        &app,
        "GET",
        "/api/bootstrap",
        Some(&member.token),
        Value::Null,
    )
    .await;
    assert_eq!(member_boot["upload_limits"]["effective_file_bytes"], 0);
    let (_, owner_boot) = request(
        &app,
        "GET",
        "/api/bootstrap",
        Some(&owner.token),
        Value::Null,
    )
    .await;
    assert_eq!(
        owner_boot["upload_limits"]["effective_file_bytes"],
        MAX_SUPPORTED_UPLOAD_BYTES as u64
    );
    assert!(owner_boot["capabilities"]
        .as_array()
        .unwrap()
        .contains(&json!("upload-policy-v1")));
    lock(&state.0.db)
        .execute("DELETE FROM community_roles WHERE id=?1", [&role])
        .unwrap();
    assert_eq!(
        request(
            &app,
            "PUT",
            "/api/settings/uploads",
            Some(&owner.token),
            json!(saved)
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let (_, cleaned) = request(
        &app,
        "GET",
        "/api/settings/uploads",
        Some(&owner.token),
        Value::Null,
    )
    .await;
    assert!(cleaned["role_limits"].as_object().unwrap().is_empty());
    assert_eq!(current(&lock(&state.0.db)).unwrap().revision, 1);
    let stored: String = lock(&state.0.db)
        .query_row(
            "SELECT value FROM metadata WHERE key='upload_policy'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<UploadPolicy>(&stored).unwrap(),
        saved
    );
}

#[tokio::test]
async fn upload_uses_exact_current_wire_allowance_and_does_not_remove_larger_old_files() {
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open(temp.path(), "uploads", 0).unwrap();
    let app = router(state.clone());
    let owner = signup(&app, "Owner", "").await;
    let member = signup(&app, "Member", &new_invite(&app, &owner).await).await;
    let inherited = UploadPolicy {
        max_file_bytes: 128,
        default_file_bytes: 64,
        ..Default::default()
    };
    let mut policy = save(&app, &owner, &inherited).await;
    assert_eq!(
        upload_bytes(&app, Some(&member.token), vec![7; 65]).await.0,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    let (status, file) = upload_bytes(&app, Some(&member.token), vec![7; 64]).await;
    assert_eq!(status, StatusCode::OK);
    policy.default_file_bytes = 8;
    policy = save(&app, &owner, &policy).await;
    assert_eq!(
        upload_bytes(&app, Some(&member.token), vec![7; 9]).await.0,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    let reply = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(file["url"].as_str().unwrap())
                .header(header::AUTHORIZATION, format!("Bearer {}", member.token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(reply.status(), StatusCode::OK);
    assert_eq!(
        reply.into_body().collect().await.unwrap().to_bytes().len(),
        64
    );
    policy.default_file_bytes = 0;
    save(&app, &owner, &policy).await;
    assert_eq!(
        upload_bytes(&app, Some(&member.token), vec![1]).await.0,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn unauthenticated_and_disabled_uploads_never_poll_a_body() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open(temp.path(), "uploads", 0).unwrap();
    let app = router(state.clone());
    let owner = signup(&app, "Owner", "").await;
    let member = signup(&app, "Member", &new_invite(&app, &owner).await).await;
    save(
        &app,
        &owner,
        &UploadPolicy {
            default_file_bytes: 0,
            ..Default::default()
        },
    )
    .await;
    for token in [None, Some(member.token.clone())] {
        let polls = Arc::new(AtomicUsize::new(0));
        let observed = polls.clone();
        let stream = futures_util::stream::once(async move {
            observed.fetch_add(1, Ordering::SeqCst);
            Ok::<_, std::io::Error>(Bytes::from_static(b"never read"))
        });
        let mut req = Request::builder()
            .method("POST")
            .uri("/api/uploads?name=owned.bin");
        if let Some(token) = &token {
            req = req.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        let reply = app
            .clone()
            .oneshot(req.body(Body::from_stream(stream)).unwrap())
            .await
            .unwrap();
        assert_eq!(
            reply.status(),
            if token.is_none() {
                StatusCode::UNAUTHORIZED
            } else {
                StatusCode::FORBIDDEN
            }
        );
        assert_eq!(polls.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn policy_or_role_changes_during_body_read_are_rechecked_before_commit() {
    for change in ["policy", "role", "membership"] {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::open(temp.path(), "uploads", 0).unwrap();
        let app = router(state.clone());
        let owner = signup(&app, "Owner", "").await;
        let member = signup(&app, "Member", &new_invite(&app, &owner).await).await;
        let role = install_role(&state, &member.user.id, PERMISSION_READ);
        let mut policy = UploadPolicy {
            max_file_bytes: 128,
            default_file_bytes: 64,
            ..Default::default()
        };
        policy.role_limits.insert(role.clone(), 64);
        let mut policy = save(&app, &owner, &policy).await;
        let began = Arc::new(tokio::sync::Notify::new());
        let released = Arc::new(tokio::sync::Notify::new());
        let reader = began.clone();
        let release = released.clone();
        let body = Body::from_stream(futures_util::stream::once(async move {
            reader.notify_one();
            release.notified().await;
            Ok::<_, std::io::Error>(Bytes::from(vec![7; 32]))
        }));
        let request = Request::builder()
            .method("POST")
            .uri("/api/uploads?name=owned.bin")
            .header(header::AUTHORIZATION, format!("Bearer {}", member.token))
            .body(body)
            .unwrap();
        let pending = tokio::spawn(app.clone().oneshot(request));
        tokio::time::timeout(std::time::Duration::from_secs(5), began.notified())
            .await
            .unwrap();
        if change == "policy" {
            policy.max_file_bytes = 16;
            policy.default_file_bytes = 16;
            save(&app, &owner, &policy).await;
        } else if change == "role" {
            policy.role_limits.insert(role, 8);
            save(&app, &owner, &policy).await;
        } else {
            lock(&state.0.db)
                .execute(
                    "UPDATE membership SET status='removed' WHERE user_id=?1",
                    [&member.user.id],
                )
                .unwrap();
        }
        released.notify_one();
        let reply = pending.await.unwrap().unwrap();
        assert_eq!(
            reply.status(),
            if change == "membership" {
                StatusCode::FORBIDDEN
            } else {
                StatusCode::PAYLOAD_TOO_LARGE
            },
            "{change}"
        );
        assert_eq!(
            lock(&state.0.db)
                .query_row("SELECT COUNT(*) FROM uploads", [], |r| r.get::<_, u64>(0))
                .unwrap(),
            0
        );
    }
}

#[tokio::test]
async fn concurrent_uploads_cannot_overrun_the_pending_quota() {
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open(temp.path(), "uploads", 0).unwrap();
    let app = router(state.clone());
    let owner = signup(&app, "Owner", "").await;
    let initial = MAX_PENDING_UPLOAD_BYTES - 4;
    lock(&state.0.db).execute("INSERT INTO uploads VALUES(?1,?2,NULL,'pending.bin','application/octet-stream',?3,zeroblob(?3),?4)", params![id(), owner.user.id, initial, now()]).unwrap();
    let (a, b) = tokio::join!(
        upload_bytes(&app, Some(&owner.token), vec![1; 3]),
        upload_bytes(&app, Some(&owner.token), vec![2; 3])
    );
    assert_eq!(
        [a.0, b.0]
            .iter()
            .filter(|status| **status == StatusCode::OK)
            .count(),
        1
    );
    assert_eq!(
        [a.0, b.0]
            .iter()
            .filter(|status| **status == StatusCode::PAYLOAD_TOO_LARGE)
            .count(),
        1
    );
    let pending: u64 = lock(&state.0.db)
        .query_row(
            "SELECT SUM(size) FROM uploads WHERE message_id IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(pending, initial + 3);
}
