use super::*;
use crate::tests::{new_invite, request, signup};
use serde_json::{json, Value};

async fn create(
    app: &Router,
    admin: &Session,
    name: &str,
    position: u32,
    bits: u32,
) -> CommunityRole {
    let (status, body) = request(
        app,
        "POST",
        "/api/roles",
        Some(&admin.token),
        json!({"name":name,"color":0x5865f2,"position":position,"permissions":bits}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    serde_json::from_value(body).unwrap()
}
async fn assign(
    app: &Router,
    actor: &Session,
    user: &Session,
    roles: &[&CommunityRole],
) -> StatusCode {
    request(
        app,
        "PUT",
        &format!("/api/members/{}/roles", user.user.id),
        Some(&actor.token),
        json!({"user_id":user.user.id,"role_ids":roles.iter().map(|r|&r.id).collect::<Vec<_>>()}),
    )
    .await
    .0
}
#[tokio::test]
async fn custom_role_hierarchy_rejects_escalation_self_assignment_and_revoked_manager() {
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open(temp.path(), "roles", 0).unwrap();
    let app = router(state.clone());
    let admin = signup(&app, "Admin", "").await;
    let manager = signup(&app, "Manager", &new_invite(&app, &admin).await).await;
    let member = signup(&app, "Member", &new_invite(&app, &admin).await).await;
    let manage = create(
        &app,
        &admin,
        "Role managers",
        50,
        PERMISSION_ROLES | PERMISSION_INVITES,
    )
    .await;
    let low = create(&app, &admin, "Guests", 10, PERMISSION_READ).await;
    let high = create(&app, &admin, "Above managers", 60, PERMISSION_READ).await;
    assert_eq!(
        assign(&app, &admin, &manager, &[&manage]).await,
        StatusCode::OK
    );
    assert_eq!(
        assign(&app, &manager, &member, &[&low]).await,
        StatusCode::OK
    );
    assert_eq!(
        assign(&app, &manager, &manager, &[&low]).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        assign(&app, &manager, &member, &[&high]).await,
        StatusCode::FORBIDDEN
    );
    let mut promote = low.clone();
    promote.permissions = PERMISSION_ADMIN;
    assert_eq!(
        request(
            &app,
            "PUT",
            &format!("/api/roles/{}", low.id),
            Some(&manager.token),
            json!(promote)
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    promote.permissions = PERMISSION_READ;
    promote.position = 50;
    assert_eq!(
        request(
            &app,
            "PUT",
            &format!("/api/roles/{}", low.id),
            Some(&manager.token),
            json!(promote)
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/invites",
            Some(&manager.token),
            Value::Null
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        request(
            &app,
            "GET",
            "/api/members",
            Some(&manager.token),
            Value::Null
        )
        .await
        .0,
        StatusCode::FORBIDDEN,
        "invite grants do not grant moderation"
    );
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/channels",
            Some(&manager.token),
            json!({"name":"forged","kind":"text"})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(assign(&app, &admin, &manager, &[]).await, StatusCode::OK);
    assert_eq!(
        assign(&app, &manager, &member, &[]).await,
        StatusCode::FORBIDDEN,
        "existing session must see current grants"
    );
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/invites",
            Some(&manager.token),
            Value::Null
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    for bad in [
        json!({"name":" ","color":0,"position":1,"permissions":0}),
        json!({"name":"x","color":0x1000000,"position":1,"permissions":0}),
        json!({"name":"x","color":0,"position":1001,"permissions":0}),
        json!({"name":"x","color":0,"position":1,"permissions":1024}),
    ] {
        assert_eq!(
            request(&app, "POST", "/api/roles", Some(&admin.token), bad)
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
    }
    let (_, audit) = request(
        &app,
        "GET",
        "/api/audit-log",
        Some(&admin.token),
        Value::Null,
    )
    .await;
    assert!(audit
        .as_array()
        .unwrap()
        .iter()
        .any(|entry| entry["action"] == "member.roles.updated"));
    assert_eq!(
        request(
            &app,
            "GET",
            "/api/audit-log",
            Some(&member.token),
            Value::Null
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
}
#[tokio::test]
async fn custom_channel_overrides_member_final_precedence_and_deletion_persist() {
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open(temp.path(), "roles", 0).unwrap();
    let app = router(state.clone());
    let admin = signup(&app, "Admin", "").await;
    let member = signup(&app, "Member", &new_invite(&app, &admin).await).await;
    let role = create(&app, &admin, "Private team", 10, 15).await;
    let (_, channel) = request(
        &app,
        "POST",
        "/api/channels",
        Some(&admin.token),
        json!({"name":"private","kind":"text","visibility":"private"}),
    )
    .await;
    let channel_id = channel["id"].as_str().unwrap().to_string();
    assert!(channel_access(&lock(&state.0.db), &channel_id, &member.user.id, READ).is_err());
    let entry = json!({"subject_kind":"role","subject_id":role.id,"read":true,"send":false,"join":false,"stream":false});
    let path = format!("/api/channels/{channel_id}/permissions");
    assert_eq!(
        request(
            &app,
            "PUT",
            &path,
            Some(&admin.token),
            json!({"entries":[entry]})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        assign(&app, &admin, &member, &[&role]).await,
        StatusCode::OK
    );
    assert!(channel_access(&lock(&state.0.db), &channel_id, &member.user.id, READ).is_ok());
    assert!(channel_access(&lock(&state.0.db), &channel_id, &member.user.id, SEND).is_err());
    let deny = json!({"subject_kind":"user","subject_id":member.user.id,"read":false,"send":false,"join":false,"stream":false});
    assert_eq!(
        request(
            &app,
            "PUT",
            &path,
            Some(&admin.token),
            json!({"entries":[entry,deny]})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert!(channel_access(&lock(&state.0.db), &channel_id, &member.user.id, READ).is_err());
    drop(app);
    drop(state);
    let state = AppState::open(temp.path(), "restart", 0).unwrap();
    let app = router(state.clone());
    let (_, boot) = request(
        &app,
        "GET",
        "/api/bootstrap",
        Some(&admin.token),
        Value::Null,
    )
    .await;
    assert_eq!(boot["roles"][0]["name"], "Private team");
    assert_eq!(boot["member_roles"][0]["role_ids"][0], role.id);
    assert_eq!(
        request(
            &app,
            "DELETE",
            &format!("/api/roles/{}", role.id),
            Some(&admin.token),
            Value::Null
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    let db = lock(&state.0.db);
    assert!(assignments(&db).unwrap().is_empty());
    assert_eq!(
        db.query_row(
            "SELECT COUNT(*) FROM channel_permissions WHERE subject_kind='role' AND subject_id=?1",
            [&role.id],
            |r| r.get::<_, u32>(0)
        )
        .unwrap(),
        0
    );
}
