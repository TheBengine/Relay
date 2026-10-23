use super::*;
use crate::tests::{new_invite, request, signup};
use serde_json::{json, Value};

#[tokio::test]
async fn notice_channels_preserve_legacy_edits_filter_access_and_record_rejoins() {
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open(temp.path(), "Notices", 0).unwrap();
    let app = router(state.clone());
    let admin = signup(&app, "Admin", "").await;
    let member = signup(&app, "Member", &new_invite(&app, &admin).await).await;
    let (_, bootstrap) = request(
        &app,
        "GET",
        "/api/bootstrap",
        Some(&admin.token),
        Value::Null,
    )
    .await;
    let channel = bootstrap["channels"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "general")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let settings = json!({"name":"Notices","description":"Welcome here","accent":0x5865f2,"welcome_channel_id":channel,"system_channel_id":channel});
    assert_eq!(
        request(
            &app,
            "PATCH",
            "/api/server-settings",
            Some(&member.token),
            settings.clone()
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            &app,
            "PATCH",
            "/api/server-settings",
            Some(&admin.token),
            settings
        )
        .await
        .0,
        StatusCode::OK
    );
    let (status, preserved) = request(
        &app,
        "PATCH",
        "/api/server-settings",
        Some(&admin.token),
        json!({"name":"Edited by older client","description":"Still welcoming","accent":0x5865f2}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(preserved["welcome_channel_id"], channel);
    assert_eq!(preserved["system_channel_id"], channel);
    let (_, events) = request(
        &app,
        "GET",
        "/api/system-events",
        Some(&member.token),
        Value::Null,
    )
    .await;
    assert_eq!(events.as_array().unwrap().len(), 2);
    assert_eq!(
        request(&app, "GET", "/api/system-events", None, Value::Null)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        request(
            &app,
            "POST",
            &format!("/api/blocks/{}", admin.user.id),
            Some(&member.token),
            Value::Null
        )
        .await
        .0,
        StatusCode::OK
    );
    let (_, filtered) = request(
        &app,
        "GET",
        "/api/system-events",
        Some(&member.token),
        Value::Null,
    )
    .await;
    assert!(filtered
        .as_array()
        .unwrap()
        .iter()
        .all(|e| e["user"]["id"] != admin.user.id));
    assert_eq!(request(&app,"PUT",&format!("/api/channels/{channel}/permissions"),Some(&admin.token),json!({"entries":[{"subject_kind":"user","subject_id":member.user.id,"read":false,"send":false,"join":false,"stream":false}]})).await.0,StatusCode::OK);
    assert_eq!(
        request(
            &app,
            "GET",
            "/api/system-events",
            Some(&member.token),
            Value::Null
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    let (_, hidden) = request(
        &app,
        "GET",
        "/api/bootstrap",
        Some(&member.token),
        Value::Null,
    )
    .await;
    assert!(hidden["server_presentation"]["system_channel_id"].is_null());
    assert!(hidden["server_presentation"]["welcome_channel_id"].is_null());
    assert_eq!(
        request(
            &app,
            "PUT",
            &format!("/api/channels/{channel}/permissions"),
            Some(&admin.token),
            json!({"entries":[]})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        request(
            &app,
            "PATCH",
            &format!("/api/members/{}", member.user.id),
            Some(&admin.token),
            json!({"status":"removed"})
        )
        .await
        .0,
        StatusCode::OK
    );
    let invite = new_invite(&app, &admin).await;
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/login",
            None,
            json!({"username":"Member","password":"a-long-test-password","invite":invite})
        )
        .await
        .0,
        StatusCode::OK
    );
    let (_, events) = request(
        &app,
        "GET",
        "/api/system-events",
        Some(&admin.token),
        Value::Null,
    )
    .await;
    let kinds: Vec<_> = events
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["user"]["id"] == member.user.id)
        .map(|e| e["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, vec!["joined", "left", "joined"]);
    // Existing installations migrate without fabricating old join events.
    {
        let db = lock(&state.0.db);
        crate::tests::discard_voice_resume_schema(&db);
        db.execute_batch("DROP TABLE crypto_call_requests;DROP TABLE community_system_events;DELETE FROM schema_migrations WHERE version>=15;PRAGMA user_version=14;").unwrap();
    }
    drop(app);
    drop(state);
    let app = router(AppState::open(temp.path(), "Fallback", 0).unwrap());
    let (_, migrated) = request(
        &app,
        "GET",
        "/api/bootstrap",
        Some(&admin.token),
        Value::Null,
    )
    .await;
    assert_eq!(migrated["server_id"], bootstrap["server_id"]);
    assert_eq!(migrated["server_presentation"], preserved);
    assert!(request(
        &app,
        "GET",
        "/api/system-events",
        Some(&admin.token),
        Value::Null
    )
    .await
    .1
    .as_array()
    .unwrap()
    .is_empty());
    assert_eq!(request(&app,"PATCH","/api/server-settings",Some(&admin.token),json!({"name":"Cleared","description":"","accent":0,"welcome_channel_id":"","system_channel_id":""})).await.0,StatusCode::OK);
    assert_eq!(
        request(
            &app,
            "GET",
            "/api/system-events",
            Some(&admin.token),
            Value::Null
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn invalid_notice_targets_are_rejected_and_deleted_channels_disappear() {
    let temp = tempfile::tempdir().unwrap();
    let app = router(AppState::open(temp.path(), "Notices", 0).unwrap());
    let admin = signup(&app, "Admin", "").await;
    let (_, b) = request(
        &app,
        "GET",
        "/api/bootstrap",
        Some(&admin.token),
        Value::Null,
    )
    .await;
    for id in b["channels"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["kind"] == "voice" || !c["voice_parent_id"].is_null())
        .map(|c| c["id"].as_str().unwrap())
        .chain(["missing-channel"])
    {
        assert_eq!(
            request(
                &app,
                "PATCH",
                "/api/server-settings",
                Some(&admin.token),
                json!({"name":"Notices","description":"","accent":0,"system_channel_id":id})
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    }
    let (_, private) = request(
        &app,
        "POST",
        "/api/channels",
        Some(&admin.token),
        json!({"name":"private","kind":"text","visibility":"private"}),
    )
    .await;
    let id = private["id"].as_str().unwrap();
    assert_eq!(
        request(
            &app,
            "PATCH",
            "/api/server-settings",
            Some(&admin.token),
            json!({"name":"Notices","description":"","accent":0,"system_channel_id":id})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let (_, created) = request(
        &app,
        "POST",
        "/api/channels",
        Some(&admin.token),
        json!({"name":"welcome","kind":"text"}),
    )
    .await;
    let id = created["id"].as_str().unwrap();
    assert_eq!(request(&app,"PATCH","/api/server-settings",Some(&admin.token),json!({"name":"Notices","description":"","accent":0,"system_channel_id":id,"welcome_channel_id":id})).await.0,StatusCode::OK);
    assert_eq!(
        request(
            &app,
            "DELETE",
            &format!("/api/channels/{id}"),
            Some(&admin.token),
            Value::Null
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    let (_, settings) = request(
        &app,
        "GET",
        "/api/server-settings",
        Some(&admin.token),
        Value::Null,
    )
    .await;
    assert!(settings["welcome_channel_id"].is_null() && settings["system_channel_id"].is_null());
    assert_eq!(
        request(
            &app,
            "GET",
            "/api/system-events",
            Some(&admin.token),
            Value::Null
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
}
