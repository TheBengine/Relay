//! Logical call membership and selected public-device routing; no audio devices.
use super::*;
use crate::tests::{new_invite, request, signup};
use serde_json::{json, Value};

async fn enroll(app: &Router, session: &Session, device: &str) {
    let (status, value) = request(app, "PUT", "/api/e2ee/device", Some(&session.token), json!({
        "device_id":device,"identity":"ab".repeat(32),"key_package":STANDARD.encode(format!("owned-public-keypackage:{device}"))
    })).await;
    assert_eq!(status, StatusCode::OK, "{value}");
}
async fn join(app: &Router, session: &Session, channel: &str, device: Option<&str>) -> StatusCode {
    request(
        app,
        "POST",
        "/api/voice/join",
        Some(&session.token),
        json!({"channel_id":channel,"device_id":device}),
    )
    .await
    .0
}
async fn context(
    app: &Router,
    session: &Session,
    channel: &str,
    device: &str,
) -> (StatusCode, Value) {
    request(
        app,
        "GET",
        &format!("/api/e2ee/context?channel_id={channel}&device_id={device}"),
        Some(&session.token),
        Value::Null,
    )
    .await
}
fn devices(value: &Value) -> std::collections::BTreeSet<String> {
    value["devices"]
        .as_array()
        .unwrap()
        .iter()
        .map(|device| device["device_id"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn voice_context_selects_one_active_device_and_pending_requester_without_other_devices() {
    let data = tempfile::tempdir().unwrap();
    let state = AppState::open(data.path(), "selected voice devices", 0).unwrap();
    let app = router(state.clone());
    let admin = signup(&app, "Admin", "").await;
    let member = signup(&app, "Member", &new_invite(&app, &admin).await).await;
    let outsider = signup(&app, "Outsider", &new_invite(&app, &admin).await).await;
    for (session, ids) in [
        (&admin, vec!["admin-active", "admin-spare"]),
        (&member, vec!["member-active", "member-spare"]),
        (&outsider, vec!["outsider-device"]),
    ] {
        for device in ids {
            enroll(&app, session, device).await;
        }
    }
    let channel: String = lock(&state.0.db)
        .query_row(
            "SELECT id FROM channels WHERE kind='voice' LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let (status, pending) = context(&app, &admin, &channel, "admin-active").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(devices(&pending), ["admin-active".into()].into());
    assert_eq!(pending["roster"].as_array().unwrap().len(), 1);
    assert_eq!(
        join(&app, &admin, &channel, Some("missing-device")).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        join(&app, &member, &channel, Some("admin-active")).await,
        StatusCode::FORBIDDEN
    );
    assert!(lock(&state.0.voice).is_empty());
    assert_eq!(
        join(&app, &admin, &channel, Some("admin-active")).await,
        StatusCode::OK
    );
    assert_eq!(
        join(&app, &member, &channel, Some("member-active")).await,
        StatusCode::OK
    );
    let (status, current) = context(&app, &admin, &channel, "admin-active").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        devices(&current),
        ["admin-active".into(), "member-active".into()].into()
    );
    assert_eq!(current["roster"].as_array().unwrap().len(), 2);
    assert_eq!(
        context(&app, &admin, &channel, "admin-spare").await.0,
        StatusCode::FORBIDDEN
    );
    let (_, bootstrap) = request(
        &app,
        "GET",
        "/api/bootstrap",
        Some(&admin.token),
        Value::Null,
    )
    .await;
    let advertised: std::collections::BTreeSet<_> = bootstrap["voice_members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|member| member["device_id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        advertised,
        ["admin-active".into(), "member-active".into()].into()
    );
    assert_eq!(
        join(&app, &member, &channel, Some("member-spare")).await,
        StatusCode::OK
    );
    let (_, replaced) = context(&app, &admin, &channel, "admin-active").await;
    assert_eq!(
        devices(&replaced),
        ["admin-active".into(), "member-spare".into()].into()
    );
    assert_eq!(
        lock(&state.0.voice)
            .values()
            .filter(|v| v.user.id == member.user.id)
            .count(),
        1
    );
    let (status, pending_other) = context(&app, &outsider, &channel, "outsider-device").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        devices(&pending_other),
        [
            "admin-active".into(),
            "member-spare".into(),
            "outsider-device".into()
        ]
        .into()
    );
    assert_eq!(pending_other["roster"].as_array().unwrap().len(), 3);
    // A spare device was never reserved just because its account joined a call.
    assert_eq!(lock(&state.0.db).query_row("SELECT COUNT(*) FROM crypto_key_packages WHERE device_id='admin-spare' AND target_kind IS NOT NULL",[],|r|r.get::<_,u32>(0)).unwrap(),0);
    // A legacy caller may still obtain a transport ticket, but cannot retrieve
    // a protected call context under a device it did not select when joining.
    assert_eq!(join(&app, &admin, &channel, None).await, StatusCode::OK);
    assert_eq!(
        context(&app, &admin, &channel, "admin-active").await.0,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn voice_device_claims_reject_revoked_devices_and_restart_retains_directory_not_call_presence(
) {
    let data = tempfile::tempdir().unwrap();
    let state = AppState::open(data.path(), "device restart", 0).unwrap();
    let app = router(state.clone());
    let admin = signup(&app, "Admin", "").await;
    enroll(&app, &admin, "active-device").await;
    enroll(&app, &admin, "revoked-device").await;
    let channel: String = lock(&state.0.db)
        .query_row(
            "SELECT id FROM channels WHERE kind='voice' LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        request(
            &app,
            "DELETE",
            "/api/e2ee/device/revoked-device",
            Some(&admin.token),
            Value::Null
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        join(&app, &admin, &channel, Some("revoked-device")).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        join(&app, &admin, &channel, Some("active-device")).await,
        StatusCode::OK
    );
    drop(app);
    drop(state);
    let reopened = AppState::open(data.path(), "device restart", 0).unwrap();
    assert!(lock(&reopened.0.voice).is_empty());
    let app = router(reopened);
    let (status, pending) = context(&app, &admin, &channel, "active-device").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(devices(&pending), ["active-device".into()].into());
    assert_eq!(
        context(&app, &admin, &channel, "revoked-device").await.0,
        StatusCode::FORBIDDEN
    );
}
