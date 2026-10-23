use super::*;
use crate::tests::{new_invite, request, signup};
use serde_json::{json, Value};

#[tokio::test]
async fn a_full_open_room_refuses_a_new_seat_without_breaking_existing_calls() {
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open(temp.path(), "Open room capacity", 0).unwrap();
    let app = router(state.clone());
    let owner = signup(&app, "Owner", "").await;
    let member = signup(&app, "Member", &new_invite(&app, &owner).await).await;
    let bootstrap = announced(&app, &owner.token).await;
    let channel = bootstrap["channels"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["kind"] == "voice")
        .unwrap()["id"]
        .as_str()
        .unwrap();
    assert_eq!(
        request(
            &app,
            "PUT",
            "/api/protection",
            Some(&owner.token),
            json!({"level":"open","expected_revision":0})
        )
        .await
        .0,
        StatusCode::OK
    );
    let join = |person: &Session| {
        json!({"channel_id":channel,
        "device_id":open_device(&token_hash(&person.token)),"open_revision":1})
    };
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/voice/join",
            Some(&owner.token),
            join(&owner)
        )
        .await
        .0,
        StatusCode::OK
    );
    // Seed only owned, synthetic runtime seats. Authentication/admission of the
    // real owner and the newcomer still goes through the actual HTTP handlers.
    let expired_id = {
        let mut sessions = lock(&state.0.voice);
        let mut first = None;
        let mut id = 1;
        for index in 0..31 {
            while sessions.contains_key(&id) {
                id += 1;
            }
            first.get_or_insert(id);
            sessions.insert(
                id,
                voice::VoiceSession {
                    private_call_id: None,
                    recovery_deadline: None,
                    user: User {
                        id: format!("owned-peer-{index}"),
                        ..Default::default()
                    },
                    channel_id: channel.into(),
                    device_id: None,
                    token_hash: format!("{index:064x}"),
                    key: [0; 32],
                    state: VoiceState::default(),
                    remote: None,
                    tunnel_sender: None,
                    tunnel_id: None,
                    in_sequence: None,
                    replay: Default::default(),
                    out_sequence: 0,
                    last_seen: now(),
                    expires: now() + 120,
                    direct: Default::default(),
                    can_speak: true,
                    can_stream: true,
                    whisper_channels: Default::default(),
                },
            );
            id += 1;
        }
        first.unwrap()
    };
    let call = json!({"channel_id":channel,"open_revision":1});
    let before = request(
        &app,
        "POST",
        "/api/protection/call",
        Some(&owner.token),
        call.clone(),
    )
    .await;
    assert_eq!(before.0, StatusCode::OK);
    assert_eq!(before.1["senders"].as_array().unwrap().len(), 32);
    let refused = request(
        &app,
        "POST",
        "/api/voice/join",
        Some(&member.token),
        join(&member),
    )
    .await;
    assert_eq!(
        refused.0,
        StatusCode::SERVICE_UNAVAILABLE,
        "a 33rd seat must be refused before it invalidates everyone's call keys"
    );
    assert_eq!(lock(&state.0.voice).len(), 32);
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/protection/call",
            Some(&owner.token),
            call.clone()
        )
        .await,
        before,
        "refusing admission must preserve existing key material and epoch"
    );
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/voice/join",
            Some(&owner.token),
            join(&owner)
        )
        .await
        .0,
        StatusCode::OK,
        "the owner can replace their own seat even when the room is full"
    );
    assert_eq!(lock(&state.0.voice).len(), 32);
    lock(&state.0.voice).get_mut(&expired_id).unwrap().last_seen = 0;
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/voice/join",
            Some(&member.token),
            join(&member)
        )
        .await
        .0,
        StatusCode::OK,
        "expired seats must not prevent a real member from joining"
    );
    let after = request(
        &app,
        "POST",
        "/api/protection/call",
        Some(&member.token),
        call,
    )
    .await;
    assert_eq!(after.0, StatusCode::OK);
    assert_eq!(after.1["senders"].as_array().unwrap().len(), 32);
    assert!(after.1["epoch"].as_u64().unwrap() > before.1["epoch"].as_u64().unwrap());
}

#[tokio::test]
async fn unfinished_local_setup_stays_visible_after_restart_until_an_owner_exists() {
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open(temp.path(), "Unfinished setup", 0).unwrap();
    assert!(temp.path().join("relay.sqlite3").is_file());
    let app = router(state.clone());
    let (status, first) = request(&app, "GET", "/api/health", None, Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        first["setup_required"], true,
        "an existing database is not an account"
    );
    drop(app);
    drop(state);

    let state = AppState::open(temp.path(), "Unfinished setup", 0).unwrap();
    let app = router(state.clone());
    let (_, restarted) = request(&app, "GET", "/api/health", None, Value::Null).await;
    assert_eq!(restarted["server_id"], first["server_id"]);
    assert_eq!(restarted["setup_required"], true);
    assert_eq!(
        lock(&state.0.db)
            .query_row("SELECT COUNT(*) FROM users", [], |r| r.get::<_, u64>(0))
            .unwrap(),
        0
    );
    let owner = signup(&app, "FirstOwner", "").await;
    assert_eq!(owner.user.role, "admin");
    assert_eq!(
        announced(&app, &owner.token).await["protection"]["level"],
        "protected"
    );
    let (_, ready) = request(&app, "GET", "/api/health", None, Value::Null).await;
    assert_eq!(ready["setup_required"], false);
    drop(app);
    drop(state);

    let state = AppState::open(temp.path(), "Unfinished setup", 0).unwrap();
    let app = router(state.clone());
    let (_, ready) = request(&app, "GET", "/api/health", None, Value::Null).await;
    assert_eq!(ready["setup_required"], false);
    assert_eq!(ready["server_id"], first["server_id"]);
    assert_eq!(
        lock(&state.0.db)
            .query_row("SELECT COUNT(*) FROM users", [], |r| r.get::<_, u64>(0))
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn creation_choice_is_atomic_local_first_owner_only_and_survives_restart() {
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open(temp.path(), "Creation choice", 0).unwrap();
    let app = router(state.clone());
    let (status,body)=request(&app,"POST","/api/register",None,json!({"username":"OpenOwner","password":"synthetic-password-20261018","initial_protection":"open"})).await;
    assert_eq!(status, StatusCode::OK);
    let owner: Session = serde_json::from_value(body).unwrap();
    let before = announced(&app, &owner.token).await;
    assert_eq!(before["protection"]["level"], "open");
    assert_eq!(before["protection"]["revision"], 1);
    let invite = new_invite(&app, &owner).await;
    let (status,_)=request(&app,"POST","/api/register",None,json!({"username":"Attacker","password":"synthetic-password-20261018","invite":invite,"initial_protection":"protected"})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        announced(&app, &owner.token).await["protection"],
        before["protection"]
    );
    let _member = signup(&app, "NormalMember", &invite).await;
    drop(app);
    drop(state);
    let restarted = AppState::open(temp.path(), "Creation choice", 0).unwrap();
    assert_eq!(
        current(&lock(&restarted.0.db)).unwrap().level,
        ProtectionLevel::Open
    );
}

#[tokio::test]
async fn open_calls_need_explicit_revision_live_membership_and_rotate_on_roster_or_mode_change() {
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open(temp.path(), "Open calls", 0).unwrap();
    let app = router(state.clone());
    let owner = signup(&app, "Owner", "").await;
    let member = signup(&app, "Member", &new_invite(&app, &owner).await).await;
    let bootstrap = announced(&app, &owner.token).await;
    let channel = bootstrap["channels"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["kind"] == "voice")
        .unwrap()["id"]
        .as_str()
        .unwrap();
    let call = json!({"channel_id":channel,"open_revision":1});
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/protection/call",
            Some(&owner.token),
            call.clone()
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            &app,
            "PUT",
            "/api/protection",
            Some(&owner.token),
            json!({"level":"open","expected_revision":0})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/voice/join",
            Some(&owner.token),
            json!({"channel_id":channel})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let join = |user: &Session| json!({"channel_id":channel,"device_id":open_device(&token_hash(&user.token)),"open_revision":1});
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/voice/join",
            Some(&owner.token),
            join(&owner)
        )
        .await
        .0,
        StatusCode::OK
    );
    let first = request(
        &app,
        "POST",
        "/api/protection/call",
        Some(&owner.token),
        call.clone(),
    )
    .await;
    assert_eq!(first.0, StatusCode::OK);
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/protection/call",
            Some(&member.token),
            call.clone()
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/voice/join",
            Some(&member.token),
            join(&member)
        )
        .await
        .0,
        StatusCode::OK
    );
    let second = request(
        &app,
        "POST",
        "/api/protection/call",
        Some(&owner.token),
        call.clone(),
    )
    .await;
    let peer = request(
        &app,
        "POST",
        "/api/protection/call",
        Some(&member.token),
        call.clone(),
    )
    .await;
    assert_eq!(second.1, peer.1);
    assert!(second.1["epoch"].as_u64().unwrap() > first.1["epoch"].as_u64().unwrap());
    assert_eq!(second.1["senders"].as_array().unwrap().len(), 2);
    assert_ne!(
        second.1["senders"][0]["opus"],
        second.1["senders"][0]["rtc"]
    );
    assert_eq!(
        request(
            &app,
            "GET",
            "/api/e2ee/device",
            Some(&owner.token),
            Value::Null
        )
        .await
        .1
        .as_array()
        .unwrap()
        .len(),
        0,
        "open calls do not register or approve E2EE devices"
    );
    assert_eq!(
        request(
            &app,
            "PUT",
            "/api/protection",
            Some(&owner.token),
            json!({"level":"protected","expected_revision":1})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert!(
        lock(&state.0.voice).is_empty(),
        "a mode change ends old voice seats"
    );
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/protection/call",
            Some(&owner.token),
            call
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
}

async fn announced(app: &Router, token: &str) -> Value {
    let (status, bootstrap) = request(app, "GET", "/api/bootstrap", Some(token), Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    bootstrap
}

#[tokio::test]
async fn only_an_administrator_can_change_the_mode_and_every_member_sees_it() {
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open(temp.path(), "Protection", 0).unwrap();
    let app = router(state.clone());
    let admin = signup(&app, "Admin", "").await;
    let member = signup(&app, "Member", &new_invite(&app, &admin).await).await;

    // A new community is protected, at revision 0, and says so.
    let before = announced(&app, &member.token).await;
    assert_eq!(
        before["protection"],
        json!({"level":"protected","revision":0})
    );
    assert!(before["capabilities"]
        .as_array()
        .unwrap()
        .contains(&json!("protection-modes-v1")));

    // A member cannot change it, and nothing is stored.
    let open = json!({"level":"open","expected_revision":0});
    assert_eq!(
        request(
            &app,
            "PUT",
            "/api/protection",
            Some(&member.token),
            open.clone()
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        announced(&app, &admin.token).await["protection"]["revision"],
        0
    );

    // Requests without a login, with unknown fields or with an unknown level are refused.
    assert_eq!(
        request(&app, "PUT", "/api/protection", None, open.clone())
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    for bad_body in [
        json!({"level":"open","expected_revision":0,"extra":true}),
        json!({"level":"off","expected_revision":0}),
        json!({"level":"open"}),
    ] {
        let (status, _) =
            request(&app, "PUT", "/api/protection", Some(&admin.token), bad_body).await;
        assert!(status.is_client_error(), "{status}");
    }
    assert_eq!(
        announced(&app, &admin.token).await["protection"]["revision"],
        0
    );

    // The administrator changes it. Everyone then sees the new revision, and who changed it.
    let (status, changed) = request(
        &app,
        "PUT",
        "/api/protection",
        Some(&admin.token),
        open.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{changed}");
    assert_eq!(changed["level"], "open");
    assert_eq!(changed["revision"], 1);
    assert_eq!(changed["changed_by"], admin.user.id);
    let seen = announced(&app, &member.token).await;
    assert_eq!(seen["protection"], changed);

    // A stale request is a conflict and changes nothing.
    let (status, _) = request(
        &app,
        "PUT",
        "/api/protection",
        Some(&admin.token),
        json!({"level":"protected","expected_revision":0}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        announced(&app, &admin.token).await["protection"]["level"],
        "open"
    );

    // Repeating the current level is harmless and does not spend a revision,
    // so nobody is asked for consent again for nothing.
    let (status, same) = request(
        &app,
        "PUT",
        "/api/protection",
        Some(&admin.token),
        json!({"level":"open","expected_revision":1}),
    )
    .await;
    assert_eq!(
        (status, same["revision"].clone()),
        (StatusCode::OK, json!(1))
    );

    // Going back to protected is a new revision. A later "open" is another one,
    // so earlier consent can never apply to it.
    let (_, back) = request(
        &app,
        "PUT",
        "/api/protection",
        Some(&admin.token),
        json!({"level":"protected","expected_revision":1}),
    )
    .await;
    assert_eq!(
        (back["level"].clone(), back["revision"].clone()),
        (json!("protected"), json!(2))
    );
    let (_, again) = request(
        &app,
        "PUT",
        "/api/protection",
        Some(&admin.token),
        json!({"level":"open","expected_revision":2}),
    )
    .await;
    assert_eq!(again["revision"], 3);

    // The change is audited without any message content.
    let db = lock(&state.0.db);
    let audited: Vec<(String, String)> = db
        .prepare("SELECT action,target_id FROM audit_log WHERE action='server.protection' ORDER BY sequence")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        audited
            .iter()
            .map(|(_, level)| level.as_str())
            .collect::<Vec<_>>(),
        ["open", "protected", "open"]
    );
}

#[tokio::test]
async fn the_mode_survives_a_restart_and_an_older_stored_value_is_rejected_safely() {
    let temp = tempfile::tempdir().unwrap();
    {
        let state = AppState::open(temp.path(), "Protection", 0).unwrap();
        let app = router(state);
        let admin = signup(&app, "Admin", "").await;
        let (status, _) = request(
            &app,
            "PUT",
            "/api/protection",
            Some(&admin.token),
            json!({"level":"open","expected_revision":0}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }
    let state = AppState::open(temp.path(), "Protection", 0).unwrap();
    {
        let db = lock(&state.0.db);
        let stored = current(&db).unwrap();
        assert_eq!((stored.level, stored.revision), (ProtectionLevel::Open, 1));
        // Damaged storage must fail closed instead of announcing "protected" by default.
        db.execute(
            "UPDATE metadata SET value='{\"level\":\"strange\"}' WHERE key='protection'",
            [],
        )
        .unwrap();
        assert!(current(&db).is_err());
    }
}
