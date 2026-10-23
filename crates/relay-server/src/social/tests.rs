use super::*;
use crate::tests::{new_invite, request, signup};
use serde_json::json;

#[tokio::test]
async fn full_friend_lists_can_accept_and_retry_existing_requests_but_cannot_grow() {
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open(temp.path(), "friend-capacity", 0).unwrap();
    let app = router(state.clone());
    let alice = signup(&app, "Alice", "").await;
    let bob = signup(&app, "Bob", &new_invite(&app, &alice).await).await;
    let cara = signup(&app, "Cara", &new_invite(&app, &alice).await).await;
    let path = |peer: &str| format!("/api/friends/{peer}");
    assert_eq!(
        request(
            &app,
            "POST",
            &path(&bob.user.id),
            Some(&alice.token),
            Value::Null
        )
        .await
        .0,
        StatusCode::OK
    );
    {
        let mut db = lock(&state.0.db);
        let tx = db.transaction().unwrap();
        // Populate only owned fixtures; no thousand password hashes or accounts on disk.
        for n in 0..999 {
            let peer = format!("owned-capacity-peer-{n}");
            tx.execute("INSERT INTO users(id,username,password_hash,role) VALUES(?1,?1,'fixture-not-a-login','member')", [&peer]).unwrap();
            tx.execute("INSERT INTO membership(user_id) VALUES(?1)", [&peer])
                .unwrap();
            tx.execute(
                "INSERT INTO friendships(sender,recipient) VALUES(?1,?2)",
                params![peer, bob.user.id],
            )
            .unwrap();
        }
        tx.commit().unwrap();
    }
    // Accepting and retrying either direction does not add another relationship.
    for (token, peer) in [
        (&bob.token, &alice.user.id),
        (&bob.token, &alice.user.id),
        (&alice.token, &bob.user.id),
    ] {
        let (status, social) = request(&app, "POST", &path(peer), Some(token), Value::Null).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "existing request must remain usable at capacity: {social}"
        );
        assert!(social["friends"]
            .as_array()
            .unwrap()
            .iter()
            .any(|id| id == peer));
    }
    for (token, peer) in [(&bob.token, &cara.user.id), (&cara.token, &bob.user.id)] {
        assert_eq!(
            request(&app, "POST", &path(peer), Some(token), Value::Null)
                .await
                .0,
            StatusCode::BAD_REQUEST,
            "new requests must respect both people's capacity"
        );
    }
    let (_, social) = request(&app, "GET", "/api/social", Some(&bob.token), Value::Null).await;
    assert_eq!(social["friends"].as_array().unwrap().len(), 1);
    assert_eq!(social["incoming"].as_array().unwrap().len(), 999);
    // Make one place available, then deliver and accept a real new request.
    assert_eq!(
        request(
            &app,
            "DELETE",
            &path("owned-capacity-peer-0"),
            Some(&bob.token),
            Value::Null
        )
        .await
        .0,
        StatusCode::OK
    );
    for _ in 0..2 {
        assert_eq!(
            request(
                &app,
                "POST",
                &path(&bob.user.id),
                Some(&cara.token),
                Value::Null
            )
            .await
            .0,
            StatusCode::OK
        );
    }
    let (status, _) = request(
        &app,
        "POST",
        &path(&cara.user.id),
        Some(&bob.token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let db = lock(&state.0.db);
    let count: u32 = db
        .query_row(
            "SELECT COUNT(*) FROM friendships WHERE sender=?1 OR recipient=?1",
            [&bob.user.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1000);
    let alice_social = relationships(&db, &alice.user.id).unwrap();
    let cara_social = relationships(&db, &cara.user.id).unwrap();
    assert_eq!(alice_social.friends, vec![bob.user.id.clone()]);
    assert_eq!(cara_social.friends, vec![bob.user.id]);
}

#[tokio::test]
async fn relationships_are_private_blocks_gate_new_dm_crypto_and_typing_and_survive_restart() {
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open(temp.path(), "social", 0).unwrap();
    let app = router(state.clone());
    let a = signup(&app, "Alice", "").await;
    let b = signup(&app, "Bob", &new_invite(&app, &a).await).await;
    let c = signup(&app, "Cara", &new_invite(&app, &a).await).await;
    assert_eq!(
        request(
            &app,
            "POST",
            &format!("/api/friends/{}", b.user.id),
            Some(&a.token),
            Value::Null
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        request(&app, "GET", "/api/social", Some(&b.token), Value::Null)
            .await
            .1["incoming"][0],
        a.user.id
    );
    assert!(
        request(&app, "GET", "/api/social", Some(&c.token), Value::Null)
            .await
            .1["incoming"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        request(
            &app,
            "POST",
            &format!("/api/friends/{}", a.user.id),
            Some(&b.token),
            Value::Null
        )
        .await
        .1["friends"][0],
        a.user.id
    );
    let (_, message) = request(
        &app,
        "POST",
        "/api/messages",
        Some(&a.token),
        json!({"recipient_id":b.user.id,"content":"Legacy routing fixture"}),
    )
    .await;
    assert!(message["id"].is_string());
    assert_eq!(
        request(
            &app,
            "POST",
            &format!("/api/blocks/{}", a.user.id),
            Some(&b.token),
            Value::Null
        )
        .await
        .0,
        StatusCode::OK
    );
    for (token, peer) in [(&a.token, &b.user.id), (&b.token, &a.user.id)] {
        assert_eq!(
            request(
                &app,
                "POST",
                "/api/messages",
                Some(token),
                json!({"recipient_id":peer,"content":"Blocked"})
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            request(
                &app,
                "POST",
                "/api/typing",
                Some(token),
                json!({"peer_id":peer,"typing":true})
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(request(&app,"PUT","/api/e2ee/device",Some(&a.token),json!({"device_id":"blocked-context-device","identity":"a".repeat(64),"key_package":STANDARD.encode(b"opaque transport-only test package")})).await.0,StatusCode::OK);
    assert_eq!(
        request(
            &app,
            "GET",
            &format!(
                "/api/e2ee/context?peer_id={}&device_id=blocked-context-device",
                b.user.id
            ),
            Some(&a.token),
            Value::Null
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert!(
        request(&app, "GET", "/api/social", Some(&a.token), Value::Null)
            .await
            .1["friends"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    // Existing history remains available to its participants, even after blocking.
    assert_eq!(
        request(
            &app,
            "GET",
            &format!("/api/history?peer_id={}", b.user.id),
            Some(&a.token),
            Value::Null
        )
        .await
        .0,
        StatusCode::OK
    );
    drop(app);
    drop(state);
    let state = AppState::open(temp.path(), "restart", 0).unwrap();
    let app = router(state);
    assert_eq!(
        request(&app, "GET", "/api/social", Some(&b.token), Value::Null)
            .await
            .1["blocked"][0],
        a.user.id
    );
    assert_eq!(
        request(
            &app,
            "DELETE",
            &format!("/api/blocks/{}", a.user.id),
            Some(&b.token),
            Value::Null
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/messages",
            Some(&a.token),
            json!({"recipient_id":b.user.id,"content":"Unblocked"})
        )
        .await
        .0,
        StatusCode::OK
    );
}
#[tokio::test]
async fn invisible_status_expiry_and_reports_keep_private_data_scoped() {
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open(temp.path(), "privacy", 0).unwrap();
    let app = router(state.clone());
    let a = signup(&app, "Alice", "").await;
    let b = signup(&app, "Bob", &new_invite(&app, &a).await).await;
    assert_eq!(request(&app,"PUT","/api/presence",Some(&a.token),json!({"user_id":b.user.id,"status":"invisible","text":"Private activity","expires_at":now()+3600})).await.0,StatusCode::OK);
    let public = presence(&lock(&state.0.db), &b.user.id).unwrap();
    let own = presence(&lock(&state.0.db), &a.user.id).unwrap();
    assert_eq!(
        public
            .iter()
            .find(|p| p.user_id == a.user.id)
            .unwrap()
            .status,
        "offline"
    );
    assert!(public
        .iter()
        .find(|p| p.user_id == a.user.id)
        .unwrap()
        .text
        .is_empty());
    assert_eq!(
        own.iter().find(|p| p.user_id == a.user.id).unwrap().text,
        "Private activity"
    );
    assert_eq!(
        request(
            &app,
            "PUT",
            "/api/presence",
            Some(&a.token),
            json!({"status":"online","text":"x".repeat(129)})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let evidence = json!({"reported_id":a.user.id,"reason":"Harassment","evidence":"Explicit evidence","share_evidence":false});
    assert_eq!(
        request(&app, "POST", "/api/reports", Some(&b.token), evidence)
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    let (_,result)=request(&app,"POST","/api/reports",Some(&b.token),json!({"reported_id":a.user.id,"reason":"Harassment","evidence":"Explicit evidence","share_evidence":true})).await;
    assert_eq!(
        request(&app, "GET", "/api/reports", Some(&b.token), Value::Null)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(&app, "GET", "/api/reports", Some(&a.token), Value::Null)
            .await
            .1[0]["evidence"],
        "Explicit evidence"
    );
    assert_eq!(
        request(
            &app,
            "PUT",
            &format!("/api/reports/{}", result["id"].as_str().unwrap()),
            Some(&a.token),
            Value::Null
        )
        .await
        .0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn social_and_bootstrap_replies_sample_the_server_private_call_clock() {
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open(temp.path(), "social-clock", 0).unwrap();
    let app = router(state.clone());
    let alice = signup(&app, "Alice", "").await;
    let before = now();
    let (status, bootstrap) = request(
        &app,
        "GET",
        "/api/bootstrap",
        Some(&alice.token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let bootstrap_time = bootstrap["social"]["server_time"].as_u64().unwrap();
    assert!((before..=now()).contains(&bootstrap_time));
    let (status, social) =
        request(&app, "GET", "/api/social", Some(&alice.token), Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    let social_time = social["server_time"].as_u64().unwrap();
    assert!((before..=now()).contains(&social_time));
    assert!(social_time > 0);
}
