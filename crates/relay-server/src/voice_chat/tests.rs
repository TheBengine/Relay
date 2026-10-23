use super::*;
use crate::tests::{new_invite, request, signup};
use serde_json::{json, Value};
#[tokio::test]
async fn voice_text_has_stable_independent_target_inherits_acl_without_call_and_deletes_with_parent(
) {
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open(temp.path(), "voice text", 0).unwrap();
    let app = router(state.clone());
    let admin = signup(&app, "Admin", "").await;
    let member = signup(&app, "Member", &new_invite(&app, &admin).await).await;
    let (_, voice) = request(
        &app,
        "POST",
        "/api/channels",
        Some(&admin.token),
        json!({"name":"Private voice","kind":"voice","visibility":"private"}),
    )
    .await;
    let voice_id = voice["id"].as_str().unwrap().to_string();
    let chat_id: String = lock(&state.0.db)
        .query_row(
            "SELECT id FROM channels WHERE voice_parent_id=?1",
            [&voice_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_ne!(chat_id, voice_id);
    assert_eq!(
        get_channel(&lock(&state.0.db), &chat_id).unwrap().kind,
        "text"
    );
    assert!(channel_access(&lock(&state.0.db), &chat_id, &member.user.id, READ).is_err());
    let permissions = json!({"entries":[{"subject_kind":"user","subject_id":member.user.id,"read":true,"send":true,"join":false,"stream":false}]});
    assert_eq!(
        request(
            &app,
            "PUT",
            &format!("/api/channels/{voice_id}/permissions"),
            Some(&admin.token),
            permissions
        )
        .await
        .0,
        StatusCode::OK
    );
    assert!(channel_access(&lock(&state.0.db), &chat_id, &member.user.id, SEND).is_ok());
    assert!(channel_access(&lock(&state.0.db), &voice_id, &member.user.id, JOIN).is_err());
    assert!(voice_members(&state).is_empty());
    assert_eq!(request(&app,"PUT","/api/e2ee/device",Some(&member.token),json!({"device_id":"voice-chat-device","identity":"a".repeat(64),"key_package":STANDARD.encode(b"opaque transport-only test package")})).await.0,StatusCode::OK);
    let (status, context) = request(
        &app,
        "GET",
        &format!("/api/e2ee/context?channel_id={chat_id}&device_id=voice-chat-device"),
        Some(&member.token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{context}");
    assert_eq!(context["target"]["channel_id"], chat_id);
    assert!(context["roster"]
        .as_array()
        .unwrap()
        .iter()
        .any(|entry| entry["id"] == member.user.id));
    let (status, message) = request(
        &app,
        "POST",
        "/api/messages",
        Some(&member.token),
        json!({"channel_id":chat_id,"content":"Explicit legacy fixture only"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{message}");
    drop(app);
    drop(state);
    let state = AppState::open(temp.path(), "restart", 0).unwrap();
    let app = router(state.clone());
    let saved: String = lock(&state.0.db)
        .query_row(
            "SELECT id FROM channels WHERE voice_parent_id=?1",
            [&voice_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(saved, chat_id);
    assert_eq!(
        request(
            &app,
            "GET",
            &format!("/api/history?channel_id={chat_id}"),
            Some(&member.token),
            Value::Null
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        request(
            &app,
            "PUT",
            &format!("/api/channels/{voice_id}/permissions"),
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
            "GET",
            &format!("/api/history?channel_id={chat_id}"),
            Some(&member.token),
            Value::Null
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            &app,
            "GET",
            &format!("/api/e2ee/context?channel_id={chat_id}&device_id=voice-chat-device"),
            Some(&member.token),
            Value::Null
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            &app,
            "DELETE",
            &format!("/api/channels/{voice_id}"),
            Some(&admin.token),
            Value::Null
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    assert!(get_channel(&lock(&state.0.db), &chat_id).is_err());
    assert_eq!(
        lock(&state.0.db)
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE id=?1",
                [message["id"].as_str().unwrap()],
                |r| r.get::<_, u32>(0)
            )
            .unwrap(),
        0
    );
}
