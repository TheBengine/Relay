//! The access explanation and the access decision come from the same rule.
use super::*;
use crate::tests::{create_test_channel, new_invite, request, signup};
use relay_protocol::channel_access::{EffectiveChannelAccess, Rule};
use serde_json::json;

async fn explain(app: &Router, token: &str, channel: &str, user: &str) -> (StatusCode, Option<EffectiveChannelAccess>) {
    let (status, body) = request(
        app,
        "GET",
        &format!("/api/channels/{channel}/access/{user}"),
        Some(token),
        serde_json::Value::Null,
    )
    .await;
    (status, serde_json::from_value(body).ok())
}

#[tokio::test]
async fn explanations_follow_every_rule_and_match_the_real_decision() {
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open(temp.path(), "access", 0).unwrap();
    let app = router(state.clone());
    let admin = signup(&app, "Admin", "").await;
    let member = signup(&app, "Member", &new_invite(&app, &admin).await).await;
    let public = create_test_channel(&app, &admin, "text", false).await;
    let private = create_test_channel(&app, &admin, "text", true).await;

    let (status, answer) = explain(&app, &admin.token, &public, &member.user.id).await;
    assert_eq!(status, StatusCode::OK);
    let answer = answer.unwrap();
    assert_eq!(answer.access.rule, Rule::PublicDefault);
    assert_eq!(answer.access.effective(), 15);

    let answer = explain(&app, &admin.token, &private, &member.user.id).await.1.unwrap();
    assert_eq!(answer.access.rule, Rule::PrivateDefault);
    assert_eq!(answer.access.effective(), 0);
    let decision = community::channel_access(&lock(&state.0.db), &private, &member.user.id, READ);
    assert!(decision.is_err(), "the real decision must agree with the explanation");

    let answer = explain(&app, &admin.token, &private, &admin.user.id).await.1.unwrap();
    assert_eq!(answer.access.rule, Rule::Administrator);

    // A custom role rule on the private channel.
    let (status, role) = request(&app, "POST", "/api/roles", Some(&admin.token),
        json!({"name":"Raid","color":0x5865f2,"position":3,"permissions":15})).await;
    assert_eq!(status, StatusCode::OK, "{role}");
    let role_id = role["id"].as_str().unwrap().to_owned();
    let (status, _) = request(&app, "PUT", &format!("/api/members/{}/roles", member.user.id), Some(&admin.token),
        json!({"user_id":member.user.id,"role_ids":[role_id]})).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = request(&app, "PUT", &format!("/api/channels/{private}/permissions"), Some(&admin.token),
        json!({"entries":[{"subject_kind":"role","subject_id":role_id,"read":true,"send":false,"join":true,"stream":false}]})).await;
    assert_eq!(status, StatusCode::OK);
    // Role changes sign the member out; ask as the administrator.
    let answer = explain(&app, &admin.token, &private, &member.user.id).await.1.unwrap();
    assert_eq!(answer.access.rule, Rule::Roles { role_ids: vec![role_id.clone()] });
    assert!(answer.access.allows(READ) && answer.access.allows(JOIN) && !answer.access.allows(SEND));
    {
        let db = lock(&state.0.db);
        assert!(community::channel_access(&db, &private, &member.user.id, JOIN).is_ok());
        assert!(community::channel_access(&db, &private, &member.user.id, SEND).is_err());
    }

    // A rule for the person is final, even when it grants less.
    let (status, _) = request(&app, "PUT", &format!("/api/channels/{private}/permissions"), Some(&admin.token),
        json!({"entries":[
            {"subject_kind":"role","subject_id":role_id,"read":true,"send":true,"join":true,"stream":true},
            {"subject_kind":"user","subject_id":member.user.id,"read":false,"send":true,"join":false,"stream":false}]})).await;
    assert_eq!(status, StatusCode::OK);
    let answer = explain(&app, &admin.token, &private, &member.user.id).await.1.unwrap();
    assert_eq!(answer.access.rule, Rule::Person);
    assert_eq!(answer.access.effective(), 0);
    assert!(answer.access.blocked_by_missing_read(SEND));
    assert!(community::channel_access(&lock(&state.0.db), &private, &member.user.id, SEND).is_err());
}

#[tokio::test]
async fn only_channel_managers_may_ask_about_someone_else() {
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open(temp.path(), "access-auth", 0).unwrap();
    let app = router(state);
    let admin = signup(&app, "Admin", "").await;
    let member = signup(&app, "Member", &new_invite(&app, &admin).await).await;
    let channel = create_test_channel(&app, &admin, "text", false).await;
    assert_eq!(explain(&app, &member.token, &channel, &admin.user.id).await.0, StatusCode::FORBIDDEN);
    let (status, answer) = explain(&app, &member.token, &channel, &member.user.id).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(answer.unwrap().user_id, member.user.id);
    assert_eq!(explain(&app, &admin.token, "missing", &member.user.id).await.0, StatusCode::NOT_FOUND);
    let (status, _) = request(&app, "GET", &format!("/api/channels/{channel}/access/{}", member.user.id), None, serde_json::Value::Null).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}
