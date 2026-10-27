use super::*;
use crate::tests::{new_invite, request, signup};
use serde_json::{json, Value};

async fn login(app: &Router, user: &str, password: &str) -> StatusCode {
    request(app, "POST", "/api/login", None, json!({"username":user,"password":password}))
        .await
        .0
}

fn redeem_body(user: &str, code: &str, password: &str) -> Value {
    json!({"username":user,"code":code,"new_password":password})
}

#[tokio::test]
async fn an_administrator_reset_code_works_once_for_one_member_and_ends_old_sessions() {
    let temp = tempfile::tempdir().unwrap();
    let app = router(AppState::open(temp.path(), "Reset", 0).unwrap());
    let anna = signup(&app, "Anna", "").await;
    let bo = signup(&app, "Bo", &new_invite(&app, &anna).await).await;
    let clara = signup(&app, "Clara", &new_invite(&app, &anna).await).await;
    let issue = |token: String, user: String| {
        let app = app.clone();
        async move {
            request(&app, "POST", &format!("/api/members/{user}/password-reset"), Some(&token), Value::Null).await
        }
    };
    // Members cannot create codes, and administrators cannot create them for themselves.
    assert_eq!(issue(bo.token.clone(), clara.user.id.clone()).await.0, StatusCode::FORBIDDEN);
    assert_eq!(issue(anna.token.clone(), anna.user.id.clone()).await.0, StatusCode::BAD_REQUEST);
    let (code, issued) = issue(anna.token.clone(), bo.user.id.clone()).await;
    assert_eq!(code, StatusCode::OK, "{issued}");
    let reset = issued["code"].as_str().unwrap().to_string();
    assert_eq!(reset.len(), 39, "32 hex digits in groups of four");
    assert!(issued["expires"].as_u64().unwrap() > now());

    let redeem = |body: Value| {
        let app = app.clone();
        async move { request(&app, "POST", "/api/password/reset", None, body).await }
    };
    let fresh = "copper meadow lantern 81";
    // The code belongs to Bo only, and a wrong code gives the same answer.
    assert_eq!(redeem(redeem_body("Clara", &reset, fresh)).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(
        redeem(redeem_body("Bo", "0000-0000-0000-0000-0000-0000-0000-0000", fresh)).await.0,
        StatusCode::UNAUTHORIZED
    );
    // Dashes, spaces and case do not matter.
    let typed = reset.replace('-', " ").to_uppercase();
    let (code, session) = redeem(redeem_body("bo", &typed, fresh)).await;
    assert_eq!(code, StatusCode::OK, "{session}");
    assert!(session["token"].as_str().is_some());
    // Old sessions ended; the new password works; the code is spent.
    assert_eq!(
        request(&app, "GET", "/api/bootstrap", Some(&bo.token), Value::Null).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(login(&app, "Bo", fresh).await, StatusCode::OK);
    assert_eq!(redeem(redeem_body("Bo", &reset, "another fine password 7")).await.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn expired_reset_codes_and_weak_passwords_are_refused() {
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open(temp.path(), "Reset", 0).unwrap();
    let app = router(state.clone());
    let anna = signup(&app, "Anna", "").await;
    let bo = signup(&app, "Bo", &new_invite(&app, &anna).await).await;
    let (_, issued) = request(&app, "POST", &format!("/api/members/{}/password-reset", bo.user.id), Some(&anna.token), Value::Null).await;
    let reset = issued["code"].as_str().unwrap().to_string();
    assert_eq!(
        request(&app, "POST", "/api/password/reset", None, redeem_body("Bo", &reset, "short")).await.0,
        StatusCode::BAD_REQUEST
    );
    lock(&state.0.db)
        .execute("UPDATE password_resets SET expires=?1", [now() - 1])
        .unwrap();
    assert_eq!(
        request(&app, "POST", "/api/password/reset", None, redeem_body("Bo", &reset, "copper meadow lantern 81")).await.0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn a_recovery_code_made_in_advance_brings_a_password_member_back_after_a_move() {
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open(&temp.path().join("old"), "Moved", 0).unwrap();
    let app = router(state.clone());
    let anna = signup(&app, "Anna", "").await;
    let bo = signup(&app, "Bo", &new_invite(&app, &anna).await).await;
    let (code, issued) = request(&app, "POST", "/api/password/recovery-code", Some(&bo.token), Value::Null).await;
    assert_eq!(code, StatusCode::OK, "{issued}");
    assert!(issued["expires"].is_null(), "recovery codes do not expire");
    let recovery = issued["code"].as_str().unwrap().to_string();
    // The community moves: a full copy without password hashes or sessions.
    let moved = temp.path().join("new");
    std::fs::create_dir_all(&moved).unwrap();
    state.backup_to(&moved.join("relay.sqlite3")).unwrap();
    drop(app);
    drop(state);
    {
        let copy = Connection::open(moved.join("relay.sqlite3")).unwrap();
        crate::host_copy_secrets::strip(&copy).unwrap();
        // A fresh host with its own authority; the old host's markers stay behind.
        copy.execute(
            "DELETE FROM metadata WHERE key IN('host_authority.v1','host_term.v1')",
            [],
        )
        .unwrap();
        assert!(crate::host_copy_secrets::is_stripped(&copy).unwrap());
    }
    let app = router(AppState::open(&moved, "Moved", 0).unwrap());
    // The old password cannot work: its hash was never copied.
    let (code, refused) = request(&app, "POST", "/api/login", None, json!({"username":"Bo","password":"a-long-test-password"})).await;
    assert_eq!(code, StatusCode::UNAUTHORIZED);
    assert!(refused["error"].as_str().unwrap_or_default().contains("recovery code"), "{refused}");
    let fresh = "copper meadow lantern 81";
    let (code, session) = request(&app, "POST", "/api/password/reset", None, redeem_body("Bo", &recovery, fresh)).await;
    assert_eq!(code, StatusCode::OK, "{session}");
    assert_eq!(session["user"]["id"], bo.user.id.as_str(), "same member, same identity");
    assert_eq!(login(&app, "Bo", fresh).await, StatusCode::OK);
}
