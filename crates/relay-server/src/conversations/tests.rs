use super::*;
use crate::tests::{new_invite, request, signup};
use serde_json::json;
#[tokio::test]
async fn groups_exclude_nonmember_admins_and_revoke_removed_members_without_losing_history() {
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open(temp.path(), "groups", 0).unwrap();
    let app = router(state.clone());
    let admin = signup(&app, "Admin", "").await;
    let a = signup(&app, "Alice", &new_invite(&app, &admin).await).await;
    let b = signup(&app, "Bob", &new_invite(&app, &admin).await).await;
    let c = signup(&app, "Cara", &new_invite(&app, &admin).await).await;
    for peer in [&b, &c] {
        request(
            &app,
            "POST",
            &format!("/api/friends/{}", peer.user.id),
            Some(&a.token),
            Value::Null,
        )
        .await;
        request(
            &app,
            "POST",
            &format!("/api/friends/{}", a.user.id),
            Some(&peer.token),
            Value::Null,
        )
        .await;
    }
    let (status, group) = request(
        &app,
        "POST",
        "/api/conversations",
        Some(&a.token),
        json!({"name":"Friends","kind":"group","member_ids":[b.user.id,c.user.id]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{group}");
    let channel = group["channel_id"].as_str().unwrap();
    assert!(channel_access(&lock(&state.0.db), channel, &admin.user.id, READ).is_err());
    assert!(channel_access(&lock(&state.0.db), channel, &b.user.id, SEND).is_ok());
    let calls = list(&lock(&state.0.db), &a.user.id).unwrap();
    let voice = &calls
        .iter()
        .find(|i| i.kind == "group_voice")
        .unwrap()
        .channel_id;
    assert!(channel_access(&lock(&state.0.db), voice, &b.user.id, JOIN).is_ok());
    assert!(channel_access(&lock(&state.0.db), voice, &admin.user.id, JOIN).is_err());
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
        StatusCode::BAD_REQUEST
    );
    let (_, message) = request(
        &app,
        "POST",
        "/api/messages",
        Some(&b.token),
        json!({"channel_id":channel,"content":"Legacy routing fixture only"}),
    )
    .await;
    assert!(message["id"].is_string());
    assert_eq!(
        request(
            &app,
            "PATCH",
            &format!("/api/conversations/{channel}"),
            Some(&a.token),
            json!({"member_ids":[a.user.id,c.user.id]})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert!(channel_access(&lock(&state.0.db), channel, &b.user.id, READ).is_err());
    assert!(channel_access(&lock(&state.0.db), voice, &b.user.id, JOIN).is_err());
    assert_eq!(
        request(
            &app,
            "GET",
            &format!("/api/history?channel_id={channel}"),
            Some(&b.token),
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
            &format!("/api/history?channel_id={channel}"),
            Some(&c.token),
            Value::Null
        )
        .await
        .1["messages"][0]["id"],
        message["id"]
    );
    drop(app);
    drop(state);
    let state = AppState::open(temp.path(), "restart", 0).unwrap();
    assert!(channel_access(&lock(&state.0.db), channel, &b.user.id, READ).is_err());
}
#[tokio::test]
async fn thread_parent_acl_archives_forums_announcements_events_and_deletion_are_enforced() {
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open(temp.path(), "types", 0).unwrap();
    let app = router(state.clone());
    let a = signup(&app, "Admin", "").await;
    let b = signup(&app, "Bob", &new_invite(&app, &a).await).await;
    let (_, forum) = request(
        &app,
        "POST",
        "/api/conversations",
        Some(&a.token),
        json!({"name":"Forum","kind":"forum"}),
    )
    .await;
    let parent = forum["channel_id"].as_str().unwrap();
    let (status, thread) = request(
        &app,
        "POST",
        "/api/conversations",
        Some(&b.token),
        json!({"name":"Discussion","kind":"thread","parent_id":parent}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{thread}");
    let channel = thread["channel_id"].as_str().unwrap();
    assert!(channel_access(&lock(&state.0.db), channel, &b.user.id, SEND).is_ok());
    assert_eq!(
        request(
            &app,
            "PATCH",
            &format!("/api/conversations/{channel}"),
            Some(&b.token),
            json!({"archived":true})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert!(channel_access(&lock(&state.0.db), channel, &a.user.id, SEND).is_err());
    assert!(channel_access(&lock(&state.0.db), channel, &b.user.id, READ).is_ok());
    let (_, announcement) = request(
        &app,
        "POST",
        "/api/conversations",
        Some(&a.token),
        json!({"name":"News","kind":"announcement"}),
    )
    .await;
    let news = announcement["channel_id"].as_str().unwrap();
    assert!(channel_access(&lock(&state.0.db), news, &b.user.id, READ).is_ok());
    assert!(channel_access(&lock(&state.0.db), news, &b.user.id, SEND).is_err());
    assert!(channel_access(&lock(&state.0.db), news, &a.user.id, SEND).is_ok());
    let (status,event)=request(&app,"POST","/api/scheduled-events",Some(&a.token),json!({"name":"Meetup","description":"Community event","channel_id":parent,"starts_at":now()+3600,"ends_at":now()+7200})).await;
    assert_eq!(status, StatusCode::OK, "{event}");
    let event_id = event["id"].as_str().unwrap();
    assert_eq!(
        request(
            &app,
            "POST",
            &format!("/api/scheduled-events/{event_id}/interest"),
            Some(&b.token),
            Value::Null
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        events(&lock(&state.0.db), &b.user.id).unwrap()[0].interested,
        vec![b.user.id.clone()]
    );
    assert_eq!(request(&app,"PUT",&format!("/api/channels/{parent}/permissions"),Some(&a.token),json!({"entries":[{"subject_kind":"user","subject_id":b.user.id,"read":false,"send":false,"join":false,"stream":false}]})).await.0,StatusCode::OK);
    assert!(channel_access(&lock(&state.0.db), channel, &b.user.id, READ).is_err());
    assert!(events(&lock(&state.0.db), &b.user.id).unwrap().is_empty());
    assert_eq!(
        request(
            &app,
            "DELETE",
            &format!("/api/channels/{parent}"),
            Some(&a.token),
            Value::Null
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    assert!(get_channel(&lock(&state.0.db), channel).is_err());
    assert!(events(&lock(&state.0.db), &a.user.id).unwrap().is_empty());
}
#[tokio::test]
async fn stage_audience_cannot_unmute_publish_or_promote_itself() {
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open(temp.path(), "stage", 0).unwrap();
    let app = router(state.clone());
    let a = signup(&app, "Admin", "").await;
    let b = signup(&app, "Bob", &new_invite(&app, &a).await).await;
    let (_, stage) = request(
        &app,
        "POST",
        "/api/conversations",
        Some(&a.token),
        json!({"name":"Talk","kind":"stage"}),
    )
    .await;
    let channel = stage["channel_id"].as_str().unwrap();
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/voice/join",
            Some(&b.token),
            json!({"channel_id":channel})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/voice/state",
            Some(&b.token),
            json!({"muted":false,"deafened":false,"sharing":true,"camera":true})
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    let current = voice_members(&state);
    let listener = current.iter().find(|v| v.user.id == b.user.id).unwrap();
    assert!(listener.muted);
    assert!(!listener.sharing);
    assert!(!listener.camera);
    assert_eq!(
        request(
            &app,
            "PUT",
            &format!("/api/stages/{channel}"),
            Some(&b.token),
            json!({"user_id":b.user.id,"speaker":true})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            &app,
            "PUT",
            &format!("/api/stages/{channel}"),
            Some(&b.token),
            json!({"user_id":b.user.id,"raised":true})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        request(
            &app,
            "PUT",
            &format!("/api/stages/{channel}"),
            Some(&a.token),
            json!({"user_id":b.user.id,"speaker":true})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert!(stage_speaker(&lock(&state.0.db), channel, &b.user.id).unwrap());
    assert!(channel_access(&lock(&state.0.db), channel, &b.user.id, STREAM).is_ok());
    request(
        &app,
        "POST",
        "/api/voice/state",
        Some(&b.token),
        json!({"muted":false,"deafened":false,"sharing":false,"camera":false}),
    )
    .await;
    assert!(
        !voice_members(&state)
            .iter()
            .find(|v| v.user.id == b.user.id)
            .unwrap()
            .muted
    );
    request(
        &app,
        "PUT",
        &format!("/api/stages/{channel}"),
        Some(&a.token),
        json!({"user_id":b.user.id,"speaker":false}),
    )
    .await;
    assert!(
        voice_members(&state)
            .iter()
            .find(|v| v.user.id == b.user.id)
            .unwrap()
            .muted
    );
}
