//! Opaque delivery/auth/storage tests; actual MLS cryptography is client-tested.
use super::*;
use crate::tests::{new_invite, request, signup};

#[tokio::test]
async fn consumed_key_packages_renew_for_removed_member_rejoin_without_reuse_or_stale_ack_loss() {
    let data = tempfile::tempdir().unwrap();
    let state = AppState::open(data.path(), "one-use renewal", 0).unwrap();
    let app = router(state.clone());
    let admin = signup(&app, "Admin", "").await;
    let member = signup(&app, "Member", &new_invite(&app, &admin).await).await;
    enroll(&app, &admin, "admin-device").await;
    let first = enroll(&app, &member, "member-device").await;
    let second = STANDARD.encode(b"a fresh distinct public package for rejoin");
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/e2ee/key-packages",
            Some(&member.token),
            serde_json::json!({"device_id":"member-device","packages":[second]})
        )
        .await
        .0,
        StatusCode::OK
    );
    let target = CryptoTarget {
        channel_id: Some(
            lock(&state.0.db)
                .query_row("SELECT id FROM channels WHERE name='general'", [], |row| {
                    row.get(0)
                })
                .unwrap(),
        ),
        ..Default::default()
    };
    let (_, before) = get_context(&app, &admin, "admin-device", &target, 0).await;
    let selected = before["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["device_id"] == "member-device")
        .unwrap()["key_package"]
        .as_str()
        .unwrap();
    assert_eq!(selected, first.key_package);
    let first_hash = token_hash_bytes(&STANDARD.decode(selected).unwrap());
    let initial = initial(
        target.clone(),
        "admin-device",
        "renewal-group",
        Some("member-device"),
    );
    assert_eq!(append(&app, &admin, &initial).await.0, StatusCode::OK);
    let mut reused = initial.clone();
    reused.events.remove(0);
    reused.expected_sequence = 3;
    reused.epoch = 2;
    reused.operation_id = "attempted-package-reuse".into();
    assert_eq!(
        append(&app, &admin, &reused).await.0,
        StatusCode::CONFLICT,
        "a second opaque Welcome cannot reuse published one-use package bytes"
    );
    assert_eq!(
        lock(&state.0.db)
            .query_row(
                "SELECT sequence FROM crypto_groups WHERE group_id='renewal-group'",
                [],
                |row| row.get::<_, u64>(0)
            )
            .unwrap(),
        3
    );
    let first_ack = ConsumeCryptoKeyPackage {
        target: target.clone(),
        device_id: "member-device".into(),
        package_hash: first_hash.clone(),
        welcome_sequence: 3,
    };
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/e2ee/key-packages/consume",
            None,
            serde_json::to_value(&first_ack).unwrap()
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/e2ee/key-packages/consume",
            Some(&admin.token),
            serde_json::to_value(&first_ack).unwrap()
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/e2ee/key-packages/consume",
            Some(&member.token),
            serde_json::to_value(&first_ack).unwrap()
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    let (_, status) = request(
        &app,
        "GET",
        "/api/e2ee/key-packages?device_id=member-device",
        Some(&member.token),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status["unused"], 1);
    assert_eq!(status["reserved"], 0);
    assert_eq!(status["consumed"], 1);
    // Removal then restoration preserves the same signing device identity.
    assert_eq!(
        request(
            &app,
            "PATCH",
            &format!("/api/members/{}", member.user.id),
            Some(&admin.token),
            serde_json::json!({"status":"banned"})
        )
        .await
        .0,
        StatusCode::OK
    );
    let remove = AppendCryptoEvents {
        target: target.clone(),
        device_id: "admin-device".into(),
        group_id: "renewal-group".into(),
        expected_sequence: 3,
        epoch: 2,
        operation_id: "remove-member".into(),
        events: vec![CryptoEventBody {
            kind: "commit".into(),
            body: STANDARD.encode(b"opaque removal commit"),
            ..Default::default()
        }],
    };
    assert_eq!(append(&app, &admin, &remove).await.0, StatusCode::OK);
    assert_eq!(
        request(
            &app,
            "PATCH",
            &format!("/api/members/{}", member.user.id),
            Some(&admin.token),
            serde_json::json!({"status":"active"})
        )
        .await
        .0,
        StatusCode::OK
    );
    // A ban revoked the member session. Use their original account/password.
    let (status, relogin) = request(
        &app,
        "POST",
        "/api/login",
        None,
        serde_json::json!({"username":"Member","password":"a-long-test-password"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{relogin}");
    let member: Session = serde_json::from_value(relogin).unwrap();
    let (_, renewed) = get_context(&app, &admin, "admin-device", &target, 4).await;
    let renewed = renewed["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["device_id"] == "member-device")
        .unwrap()["key_package"]
        .as_str()
        .unwrap();
    assert_eq!(renewed, second);
    let second_hash = token_hash_bytes(&STANDARD.decode(renewed).unwrap());
    let rejoin = AppendCryptoEvents {
        target: target.clone(),
        device_id: "admin-device".into(),
        group_id: "renewal-group".into(),
        expected_sequence: 4,
        epoch: 3,
        operation_id: "rejoin-member".into(),
        events: vec![
            CryptoEventBody {
                kind: "commit".into(),
                body: STANDARD.encode(b"opaque rejoin commit"),
                ..Default::default()
            },
            CryptoEventBody {
                kind: "welcome".into(),
                recipient_device_id: Some("member-device".into()),
                body: STANDARD.encode(b"opaque fresh Welcome"),
            },
        ],
    };
    assert_eq!(append(&app, &admin, &rejoin).await.0, StatusCode::OK);
    // A lost first ACK response retried after a later Welcome cannot clear it.
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/e2ee/key-packages/consume",
            Some(&member.token),
            serde_json::to_value(&first_ack).unwrap()
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    let wrong = ConsumeCryptoKeyPackage {
        welcome_sequence: 6,
        ..first_ack.clone()
    };
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/e2ee/key-packages/consume",
            Some(&member.token),
            serde_json::to_value(wrong).unwrap()
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    let (_, still_current) = get_context(&app, &admin, "admin-device", &target, 6).await;
    assert_eq!(
        still_current["devices"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["device_id"] == "member-device")
            .unwrap()["key_package"],
        second
    );
    let second_ack = ConsumeCryptoKeyPackage {
        package_hash: second_hash,
        welcome_sequence: 6,
        ..first_ack
    };
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/e2ee/key-packages/consume",
            Some(&member.token),
            serde_json::to_value(second_ack).unwrap()
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    let (_, duplicate) = request(
        &app,
        "POST",
        "/api/e2ee/key-packages",
        Some(&member.token),
        serde_json::json!({"device_id":"member-device","packages":[first.key_package,second]}),
    )
    .await;
    assert_eq!(duplicate["accepted"], 0);
    assert_eq!(
        duplicate["unused"], 0,
        "consumed bytes never return to stock"
    );
    let (_, depleted) = get_context(&app, &admin, "admin-device", &target, 6).await;
    assert_eq!(
        depleted["devices"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["device_id"] == "member-device")
            .unwrap()["key_package"],
        ""
    );
    let saved_server_id = server_id(&lock(&state.0.db)).unwrap();
    drop(app);
    drop(state);
    let restarted = AppState::open(data.path(), "one-use renewal", 0).unwrap();
    assert_eq!(server_id(&lock(&restarted.0.db)).unwrap(), saved_server_id);
    assert_eq!(
        lock(&restarted.0.db)
            .query_row(
                "SELECT COUNT(*) FROM crypto_key_package_consumptions",
                [],
                |row| row.get::<_, u32>(0)
            )
            .unwrap(),
        2
    );
}

#[tokio::test]
async fn consumption_migration_preserves_existing_welcomes_and_failed_migration_rolls_back() {
    let data = tempfile::tempdir().unwrap();
    let state = AppState::open(data.path(), "consumption migration", 0).unwrap();
    let app = router(state.clone());
    let admin = signup(&app, "Admin", "").await;
    let member = signup(&app, "Member", &new_invite(&app, &admin).await).await;
    enroll(&app, &admin, "admin-device").await;
    let public = enroll(&app, &member, "member-device").await;
    let target = CryptoTarget {
        peer_id: Some(member.user.id.clone()),
        ..Default::default()
    };
    assert_eq!(
        append(
            &app,
            &admin,
            &initial(
                target,
                "admin-device",
                "existing-group",
                Some("member-device")
            )
        )
        .await
        .0,
        StatusCode::OK
    );
    let saved = server_id(&lock(&state.0.db)).unwrap();
    drop(app);
    drop(state);
    let path = data.path().join("relay.sqlite3");
    let db = Connection::open(&path).unwrap();
    crate::tests::discard_post_profile_additions(&db);
    db.execute_batch("DROP TABLE crypto_call_requests;DROP TABLE crypto_key_package_consumptions;DROP TABLE crypto_welcome_packages;ALTER TABLE user_profiles DROP COLUMN appearance;DELETE FROM schema_migrations WHERE version>=8;PRAGMA user_version=7;").unwrap();
    drop(db);
    let state = AppState::open(data.path(), "consumption migration", 0).unwrap();
    let app = router(state.clone());
    assert_eq!(server_id(&lock(&state.0.db)).unwrap(), saved);
    assert_eq!(
        lock(&state.0.db)
            .query_row("SELECT COUNT(*) FROM crypto_welcome_packages", [], |row| {
                row.get::<_, u32>(0)
            })
            .unwrap(),
        1
    );
    let acknowledgement = ConsumeCryptoKeyPackage {
        target: CryptoTarget {
            peer_id: Some(admin.user.id.clone()),
            ..Default::default()
        },
        device_id: "member-device".into(),
        package_hash: token_hash_bytes(&STANDARD.decode(public.key_package).unwrap()),
        welcome_sequence: 3,
    };
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/e2ee/key-packages/consume",
            Some(&member.token),
            serde_json::to_value(acknowledgement).unwrap()
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    drop(app);
    drop(state);
    let db = Connection::open(&path).unwrap();
    crate::tests::discard_post_profile_additions(&db);
    db.execute_batch("DROP TABLE crypto_call_requests;DROP TABLE crypto_key_package_consumptions;DROP TABLE crypto_welcome_packages;ALTER TABLE user_profiles DROP COLUMN appearance;DELETE FROM schema_migrations WHERE version>=8;PRAGMA user_version=7;CREATE TABLE crypto_welcome_packages(deliberate_conflict TEXT);").unwrap();
    drop(db);
    assert!(AppState::open(data.path(), "consumption migration", 0).is_err());
    let db = Connection::open(path).unwrap();
    assert_eq!(
        db.pragma_query_value::<u32, _>(None, "user_version", |row| row.get(0))
            .unwrap(),
        7
    );
    assert_eq!(
        db.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name='crypto_key_package_consumptions'",
            [],
            |row| row.get::<_, u32>(0)
        )
        .unwrap(),
        0
    );
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM crypto_events", [], |row| row
            .get::<_, u32>(0))
            .unwrap(),
        3
    );
}

#[tokio::test]
async fn protected_voice_welcomes_and_commits_require_the_selected_device_not_its_spare() {
    let data = tempfile::tempdir().unwrap();
    let state = AppState::open(data.path(), "selected welcome", 0).unwrap();
    let app = router(state.clone());
    let admin = signup(&app, "Admin", "").await;
    let member = signup(&app, "Member", &new_invite(&app, &admin).await).await;
    for (session, devices) in [
        (&admin, ["admin-selected", "admin-spare"]),
        (&member, ["member-selected", "member-spare"]),
    ] {
        for id in devices {
            enroll(&app, session, id).await;
        }
    }
    let channel: String = lock(&state.0.db)
        .query_row(
            "SELECT id FROM channels WHERE kind='voice' LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    for (session, device) in [(&admin, "admin-selected"), (&member, "member-selected")] {
        assert_eq!(
            request(
                &app,
                "POST",
                "/api/voice/join",
                Some(&session.token),
                serde_json::json!({"channel_id":channel,"device_id":device})
            )
            .await
            .0,
            StatusCode::OK
        );
    }
    let target = CryptoTarget {
        channel_id: Some(channel.clone()),
        ..Default::default()
    };
    let spare = initial(
        target.clone(),
        "admin-selected",
        "voice-group",
        Some("member-spare"),
    );
    assert_eq!(append(&app, &admin, &spare).await.0, StatusCode::FORBIDDEN);
    assert_eq!(
        lock(&state.0.db)
            .query_row("SELECT COUNT(*) FROM crypto_groups", [], |row| row
                .get::<_, u32>(0))
            .unwrap(),
        0,
        "rejected Welcome is atomic"
    );
    let author_spare = initial(
        target.clone(),
        "admin-spare",
        "voice-group",
        Some("member-selected"),
    );
    assert_eq!(
        append(&app, &admin, &author_spare).await.0,
        StatusCode::FORBIDDEN
    );
    let active = initial(
        target,
        "admin-selected",
        "voice-group",
        Some("member-selected"),
    );
    assert_eq!(append(&app, &admin, &active).await.0, StatusCode::OK);
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/voice/join",
            Some(&admin.token),
            serde_json::json!({"channel_id":channel})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        append(&app, &admin, &active).await.0,
        StatusCode::FORBIDDEN,
        "legacy unbound call tickets cannot append protected epochs"
    );
}

#[tokio::test]
async fn revoking_selected_device_removes_voice_frames_and_rtc_but_spare_device_does_not() {
    let data = tempfile::tempdir().unwrap();
    let state = AppState::open(data.path(), "device revocation", 0).unwrap();
    let app = router(state.clone());
    let admin = signup(&app, "Admin", "").await;
    enroll(&app, &admin, "selected-device").await;
    enroll(&app, &admin, "spare-device").await;
    let channel: String = lock(&state.0.db)
        .query_row(
            "SELECT id FROM channels WHERE kind='voice' LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let (status, ticket) = request(
        &app,
        "POST",
        "/api/voice/join",
        Some(&admin.token),
        serde_json::json!({"channel_id":channel,"device_id":"selected-device"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{ticket}");
    let token_hash = lock(&state.0.voice)
        .values()
        .next()
        .unwrap()
        .token_hash
        .clone();
    lock(&state.0.rtc_members).insert(
        admin.user.id.clone(),
        crate::rtc::RtcMembership {
            channel_id: channel.clone(),
            room: "synthetic-room".into(),
            token_hash,
            can_publish: true,
        },
    );
    lock(&state.0.frames).insert(
        (admin.user.id.clone(), "screen".into()),
        StoredFrame {
            metadata: MediaFrame {
                user: admin.user.clone(),
                kind: "screen".into(),
                frame_id: "synthetic-frame".into(),
                url: "/api/media/live/user/screen".into(),
                width: 1,
                height: 1,
            },
            channel_id: channel.clone(),
            bytes: Bytes::from_static(b"synthetic cached bytes"),
            updated: now(),
        },
    );
    assert_eq!(
        request(
            &app,
            "DELETE",
            "/api/e2ee/device/spare-device",
            Some(&admin.token),
            serde_json::Value::Null
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(lock(&state.0.voice).len(), 1);
    assert_eq!(lock(&state.0.rtc_members).len(), 1);
    assert_eq!(lock(&state.0.frames).len(), 1);
    let mut notices = state.0.events.subscribe();
    assert_eq!(
        request(
            &app,
            "DELETE",
            "/api/e2ee/device/selected-device",
            Some(&admin.token),
            serde_json::Value::Null
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    assert!(lock(&state.0.voice).is_empty());
    assert!(lock(&state.0.frames).is_empty());
    assert!(lock(&state.0.rtc_members).is_empty());
    assert!(
        lock(&state.0.rtc_removals).contains_key(&("synthetic-room".into(), admin.user.id.clone()))
    );
    let notices: Vec<_> = std::iter::from_fn(|| notices.try_recv().ok()).collect();
    assert!(notices.iter().any(|notice| notice.event.kind == "voice"));
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/voice/join",
            Some(&admin.token),
            serde_json::json!({"channel_id":channel,"device_id":"selected-device"})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn public_directory_requires_auth_omits_revoked_members_and_never_reserves_packages() {
    let data = tempfile::tempdir().unwrap();
    let state = AppState::open(data.path(), "directory", 0).unwrap();
    let app = router(state.clone());
    let admin = signup(&app, "Admin", "").await;
    let member = signup(&app, "Member", &new_invite(&app, &admin).await).await;
    enroll(&app, &admin, "a-device").await;
    enroll(&app, &member, "m-device").await;
    assert_eq!(
        request(
            &app,
            "GET",
            "/api/e2ee/directory",
            None,
            serde_json::Value::Null
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    let (_, directory) = request(
        &app,
        "GET",
        "/api/e2ee/directory",
        Some(&admin.token),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(directory.as_array().unwrap().len(), 2);
    assert_eq!(
        lock(&state.0.db)
            .query_row(
                "SELECT COUNT(*) FROM crypto_key_packages WHERE target_kind IS NOT NULL",
                [],
                |r| r.get::<_, u32>(0)
            )
            .unwrap(),
        0
    );
    request(
        &app,
        "PATCH",
        &format!("/api/members/{}", member.user.id),
        Some(&admin.token),
        serde_json::json!({"status":"banned"}),
    )
    .await;
    let (_, directory) = request(
        &app,
        "GET",
        "/api/e2ee/directory",
        Some(&admin.token),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(directory.as_array().unwrap().len(), 1);
    assert_eq!(directory[0]["device_id"], "a-device");
    assert!(directory[0].get("group_id").is_none());
}

#[tokio::test]
async fn simultaneous_group_claims_commit_once_and_context_reservations_never_cross_targets() {
    let data = tempfile::tempdir().unwrap();
    let state = AppState::open(data.path(), "races", 0).unwrap();
    let app = router(state.clone());
    let admin = signup(&app, "Admin", "").await;
    enroll(&app, &admin, "device").await;
    request(&app,"POST","/api/e2ee/key-packages",Some(&admin.token),serde_json::json!({"device_id":"device","packages":[STANDARD.encode(b"second public package")]})).await;
    let first = CryptoTarget {
        channel_id: Some(
            lock(&state.0.db)
                .query_row("SELECT id FROM channels WHERE name='general'", [], |r| {
                    r.get(0)
                })
                .unwrap(),
        ),
        ..Default::default()
    };
    let second = CryptoTarget {
        channel_id: Some(
            lock(&state.0.db)
                .query_row("SELECT id FROM channels WHERE name='clips'", [], |r| {
                    r.get(0)
                })
                .unwrap(),
        ),
        ..Default::default()
    };
    let (one, two) = tokio::join!(
        get_context(&app, &admin, "device", &first, 0),
        get_context(&app, &admin, "device", &second, 0)
    );
    assert_eq!(one.0, StatusCode::OK);
    assert_eq!(two.0, StatusCode::OK);
    assert_ne!(
        one.1["devices"][0]["key_package"],
        two.1["devices"][0]["key_package"]
    );
    let a = initial(first.clone(), "device", "first-claim", None);
    let b = initial(first, "device", "second-claim", None);
    let (left, right) = tokio::join!(append(&app, &admin, &a), append(&app, &admin, &b));
    assert!(matches!(
        (left.0, right.0),
        (StatusCode::OK, StatusCode::CONFLICT) | (StatusCode::CONFLICT, StatusCode::OK)
    ));
    assert_eq!(
        lock(&state.0.db)
            .query_row("SELECT COUNT(*) FROM crypto_groups", [], |r| r
                .get::<_, u32>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        lock(&state.0.db)
            .query_row("SELECT COUNT(*) FROM crypto_events", [], |r| r
                .get::<_, u32>(0))
            .unwrap(),
        2
    );
}

async fn enroll(app: &Router, session: &Session, id: &str) -> CryptoDevice {
    let (status, value)=request(app,"PUT","/api/e2ee/device",Some(&session.token),serde_json::json!({
        "device_id":id,"identity":"ab".repeat(32),"key_package":STANDARD.encode(format!("public-keypackage:{id}"))})).await;
    assert_eq!(status, StatusCode::OK, "{value}");
    serde_json::from_value(value).unwrap()
}

#[tokio::test]
async fn call_request_delivery_is_exact_bounded_selected_device_scoped_and_not_chat() {
    let data = tempfile::tempdir().unwrap();
    let state = AppState::open(data.path(), "call control", 0).unwrap();
    let app = router(state.clone());
    let admin = signup(&app, "Admin", "").await;
    let member = signup(&app, "Member", &new_invite(&app, &admin).await).await;
    for (session, id) in [
        (&admin, "call-a"),
        (&member, "call-b"),
        (&member, "call-spare"),
    ] {
        enroll(&app, session, id).await;
    }
    let channel: String = lock(&state.0.db)
        .query_row(
            "SELECT id FROM channels WHERE kind='voice' LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    for (session, id) in [(&admin, "call-a"), (&member, "call-b")] {
        assert_eq!(
            request(
                &app,
                "POST",
                "/api/voice/join",
                Some(&session.token),
                serde_json::json!({"channel_id":channel,"device_id":id})
            )
            .await
            .0,
            StatusCode::OK
        );
    }
    let target = CryptoTarget {
        channel_id: Some(channel.clone()),
        peer_id: None,
    };
    assert_eq!(
        append(
            &app,
            &admin,
            &initial(target.clone(), "call-a", "call-group", Some("call-b"))
        )
        .await
        .0,
        StatusCode::OK
    );
    let input = CallKeyRequest {
        target: target.clone(),
        device_id: "call-b".into(),
        group_id: "call-group".into(),
        epoch: 1,
        operation_id: "call-request-one".into(),
        body: STANDARD.encode(b"opaque authenticated call challenge"),
    };
    let post = serde_json::to_value(&input).unwrap();
    assert_eq!(
        request(&app, "POST", "/api/e2ee/call-requests", None, post.clone())
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    let mut spare = input.clone();
    spare.device_id = "call-spare".into();
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/e2ee/call-requests",
            Some(&member.token),
            serde_json::to_value(spare).unwrap()
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let usage_before = extra_storage_usage(&lock(&state.0.db), &member.user.id).unwrap();
    let (status, delivery) = request(
        &app,
        "POST",
        "/api/e2ee/call-requests",
        Some(&member.token),
        post.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(delivery["request"], post);
    assert_eq!(delivery["author_user_id"], member.user.id);
    assert!(
        extra_storage_usage(&lock(&state.0.db), &member.user.id)
            .unwrap()
            .1
            > usage_before.1
    );
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/e2ee/call-requests",
            Some(&member.token),
            post.clone()
        )
        .await,
        (StatusCode::OK, delivery)
    );
    let mut changed = input.clone();
    changed.body = STANDARD.encode(b"changed opaque body");
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/e2ee/call-requests",
            Some(&member.token),
            serde_json::to_value(changed).unwrap()
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    let mut next = input.clone();
    next.operation_id = "call-request-two".into();
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/e2ee/call-requests",
            Some(&member.token),
            serde_json::to_value(&next).unwrap()
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    let url = format!("/api/e2ee/call-requests?channel_id={channel}&device_id=call-a");
    assert_eq!(
        request(
            &app,
            "GET",
            &url,
            Some(&admin.token),
            serde_json::Value::Null
        )
        .await
        .1
        .as_array()
        .unwrap()
        .len(),
        1
    );
    let ack = AckCallKeyRequest {
        target,
        device_id: "call-a".into(),
        group_id: "call-group".into(),
        operation_id: input.operation_id,
    };
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/e2ee/call-requests/ack",
            Some(&admin.token),
            serde_json::to_value(&ack).unwrap()
        )
        .await,
        (StatusCode::OK, serde_json::to_value(&ack).unwrap())
    );
    assert_eq!(
        request(
            &app,
            "GET",
            &url,
            Some(&admin.token),
            serde_json::Value::Null
        )
        .await
        .1
        .as_array()
        .unwrap()
        .len(),
        0
    );
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/e2ee/call-requests",
            Some(&member.token),
            serde_json::to_value(&next).unwrap()
        )
        .await
        .0,
        StatusCode::OK
    );
    let mut oversized = next;
    oversized.operation_id = "call-big".into();
    oversized.body = STANDARD.encode(vec![7; 8 * 1024 + 1]);
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/e2ee/call-requests",
            Some(&member.token),
            serde_json::to_value(oversized).unwrap()
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    {
        let db = lock(&state.0.db);
        assert_eq!(
            db.query_row("SELECT COUNT(*) FROM crypto_call_requests", [], |r| r
                .get::<_, u32>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            db.query_row(
                "SELECT sequence FROM crypto_groups WHERE group_id='call-group'",
                [],
                |r| r.get::<_, u32>(0)
            )
            .unwrap(),
            3
        );
        assert_eq!(
            db.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get::<_, u32>(0))
                .unwrap(),
            0
        );
    }
    request(
        &app,
        "POST",
        "/api/voice/leave",
        Some(&member.token),
        serde_json::json!({}),
    )
    .await;
    assert!(request(
        &app,
        "GET",
        &url,
        Some(&admin.token),
        serde_json::Value::Null
    )
    .await
    .1
    .as_array()
    .unwrap()
    .is_empty());
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/e2ee/call-requests",
            Some(&member.token),
            post
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    for _ in 0..2 {
        assert_eq!(
            request(
                &app,
                "DELETE",
                "/api/e2ee/device/call-b",
                Some(&member.token),
                serde_json::Value::Null
            )
            .await
            .0,
            StatusCode::NO_CONTENT
        );
    }
    assert_eq!(
        lock(&state.0.db)
            .query_row("SELECT COUNT(*) FROM crypto_call_requests", [], |r| r
                .get::<_, u32>(0))
            .unwrap(),
        0
    );
    assert!(!lock(&state.0.db)
        .query_row(
            "SELECT active FROM crypto_devices WHERE device_id='call-b'",
            [],
            |r| r.get::<_, bool>(0)
        )
        .unwrap());
}

#[tokio::test]
async fn call_request_direct_inbox_preserves_relative_targets_and_pair_privacy() {
    let data = tempfile::tempdir().unwrap();
    let state = AppState::open(data.path(), "private call control", 0).unwrap();
    let app = router(state.clone());
    let admin = signup(&app, "Admin", "").await;
    let member = signup(&app, "Member", &new_invite(&app, &admin).await).await;
    let other = signup(&app, "Other", &new_invite(&app, &admin).await).await;
    for (session, id) in [
        (&admin, "direct-a"),
        (&member, "direct-b"),
        (&other, "direct-e"),
    ] {
        enroll(&app, session, id).await;
    }
    request(
        &app,
        "POST",
        &format!("/api/friends/{}", member.user.id),
        Some(&admin.token),
        serde_json::json!({}),
    )
    .await;
    request(
        &app,
        "POST",
        &format!("/api/friends/{}", admin.user.id),
        Some(&member.token),
        serde_json::json!({}),
    )
    .await;
    let a_target = CryptoTarget {
        channel_id: None,
        peer_id: Some(member.user.id.clone()),
    };
    let b_target = CryptoTarget {
        channel_id: None,
        peer_id: Some(admin.user.id.clone()),
    };
    assert_eq!(
        append(
            &app,
            &admin,
            &initial(
                a_target.clone(),
                "direct-a",
                "direct-call-group",
                Some("direct-b")
            )
        )
        .await
        .0,
        StatusCode::OK
    );
    let input = CallKeyRequest {
        target: b_target.clone(),
        device_id: "direct-b".into(),
        group_id: "direct-call-group".into(),
        epoch: 1,
        operation_id: "direct-call-one".into(),
        body: STANDARD.encode(b"opaque private call challenge"),
    };
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/e2ee/call-requests",
            Some(&member.token),
            serde_json::to_value(&input).unwrap()
        )
        .await
        .0,
        StatusCode::OK
    );
    let (status, hints) = request(
        &app,
        "GET",
        "/api/e2ee/call-inbox?device_id=direct-a",
        Some(&admin.token),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(hints.as_array().unwrap().len(), 1);
    assert_eq!(hints[0]["target"], serde_json::to_value(&a_target).unwrap());
    assert_eq!(
        request(
            &app,
            "GET",
            "/api/e2ee/call-inbox?device_id=direct-e",
            Some(&other.token),
            serde_json::Value::Null
        )
        .await
        .1,
        serde_json::json!([])
    );
    let url = format!(
        "/api/e2ee/call-requests?peer_id={}&device_id=direct-a",
        member.user.id
    );
    let (status, deliveries) = request(
        &app,
        "GET",
        &url,
        Some(&admin.token),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        deliveries[0]["target"],
        serde_json::to_value(a_target).unwrap()
    );
    assert_eq!(
        deliveries[0]["request"],
        serde_json::to_value(input).unwrap()
    );
    request(
        &app,
        "POST",
        &format!("/api/blocks/{}", member.user.id),
        Some(&admin.token),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(
        request(
            &app,
            "GET",
            &url,
            Some(&admin.token),
            serde_json::Value::Null
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            &app,
            "GET",
            "/api/e2ee/call-inbox?device_id=direct-a",
            Some(&admin.token),
            serde_json::Value::Null
        )
        .await
        .1,
        serde_json::json!([])
    );
}

fn initial(
    target: CryptoTarget,
    device: &str,
    group: &str,
    recipient: Option<&str>,
) -> AppendCryptoEvents {
    let mut events = vec![
        CryptoEventBody {
            kind: "create".into(),
            ..Default::default()
        },
        CryptoEventBody {
            kind: "commit".into(),
            body: STANDARD.encode(b"opaque signed commit"),
            ..Default::default()
        },
    ];
    if let Some(recipient) = recipient {
        events.push(CryptoEventBody {
            kind: "welcome".into(),
            recipient_device_id: Some(recipient.into()),
            body: STANDARD.encode(b"opaque encrypted welcome"),
        });
    }
    AppendCryptoEvents {
        target,
        device_id: device.into(),
        group_id: group.into(),
        expected_sequence: 0,
        epoch: 1,
        operation_id: format!("initial-{group}"),
        events,
    }
}

#[tokio::test]
async fn call_request_quota_failure_rolls_back_opaque_delivery_and_migration_conflicts_preserve_schema(
) {
    let data = tempfile::tempdir().unwrap();
    let state = AppState::open_with_config(
        data.path(),
        "call quota",
        0,
        ServerConfig {
            user_storage_limit_bytes: 2048,
            ..Default::default()
        },
    )
    .unwrap();
    let app = router(state.clone());
    let admin = signup(&app, "Admin", "").await;
    let member = signup(&app, "Member", &new_invite(&app, &admin).await).await;
    enroll(&app, &admin, "quota-a").await;
    enroll(&app, &member, "quota-b").await;
    request(
        &app,
        "POST",
        &format!("/api/friends/{}", member.user.id),
        Some(&admin.token),
        serde_json::json!({}),
    )
    .await;
    request(
        &app,
        "POST",
        &format!("/api/friends/{}", admin.user.id),
        Some(&member.token),
        serde_json::json!({}),
    )
    .await;
    let target = CryptoTarget {
        channel_id: None,
        peer_id: Some(member.user.id.clone()),
    };
    assert_eq!(
        append(
            &app,
            &admin,
            &initial(target, "quota-a", "quota-call-group", Some("quota-b"))
        )
        .await
        .0,
        StatusCode::OK
    );
    let input = CallKeyRequest {
        target: CryptoTarget {
            channel_id: None,
            peer_id: Some(admin.user.id.clone()),
        },
        device_id: "quota-b".into(),
        group_id: "quota-call-group".into(),
        epoch: 1,
        operation_id: "quota-call-one".into(),
        body: STANDARD.encode(vec![7; 8 * 1024]),
    };
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/e2ee/call-requests",
            Some(&member.token),
            serde_json::to_value(input).unwrap()
        )
        .await
        .0,
        StatusCode::INSUFFICIENT_STORAGE
    );
    let mut db = lock(&state.0.db);
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM crypto_call_requests", [], |r| r
            .get::<_, u32>(0))
            .unwrap(),
        0
    );
    let identity = server_id(&db).unwrap();
    crate::tests::discard_voice_resume_schema(&db);
    db.execute_batch("DROP TABLE crypto_call_requests;DELETE FROM schema_migrations WHERE version>=16;PRAGMA user_version=15;CREATE TABLE crypto_call_requests(deliberate_conflict TEXT);").unwrap();
    assert!(community::migrate_database(&mut db).is_err());
    assert_eq!(
        db.pragma_query_value::<u32, _>(None, "user_version", |r| r.get(0))
            .unwrap(),
        15
    );
    assert_eq!(
        db.query_row(
            "SELECT COUNT(*) FROM schema_migrations WHERE version=16",
            [],
            |r| r.get::<_, u32>(0)
        )
        .unwrap(),
        0
    );
    assert_eq!(server_id(&db).unwrap(), identity);
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM users", [], |r| r.get::<_, u32>(0))
            .unwrap(),
        2
    );
}

async fn append(
    app: &Router,
    session: &Session,
    input: &AppendCryptoEvents,
) -> (StatusCode, serde_json::Value) {
    request(
        app,
        "POST",
        "/api/e2ee/events",
        Some(&session.token),
        serde_json::to_value(input).unwrap(),
    )
    .await
}

async fn get_context(
    app: &Router,
    session: &Session,
    device: &str,
    target: &CryptoTarget,
    after: u64,
) -> (StatusCode, serde_json::Value) {
    let target = match (&target.channel_id, &target.peer_id) {
        (Some(id), None) => format!("channel_id={id}"),
        (None, Some(id)) => format!("peer_id={id}"),
        _ => "".into(),
    };
    request(
        app,
        "GET",
        &format!("/api/e2ee/context?{target}&device_id={device}&after={after}"),
        Some(&session.token),
        serde_json::Value::Null,
    )
    .await
}

#[tokio::test]
async fn devices_are_owned_bounded_immutable_and_revoked_without_identity_reuse() {
    let data = tempfile::tempdir().unwrap();
    let state = AppState::open(data.path(), "crypto", 0).unwrap();
    let app = router(state.clone());
    let admin = signup(&app, "Admin", "").await;
    let other = signup(&app, "Other", &new_invite(&app, &admin).await).await;
    enroll(&app, &admin, "a-device").await;
    let valid = serde_json::json!({"device_id":"a-device","identity":"ab".repeat(32),"key_package":STANDARD.encode(b"refreshed public package")});
    assert_eq!(
        request(&app, "PUT", "/api/e2ee/device", None, valid.clone())
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        request(
            &app,
            "PUT",
            "/api/e2ee/device",
            Some(&other.token),
            valid.clone()
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let mut changed = valid.clone();
    changed["identity"] = serde_json::json!("cd".repeat(32));
    assert_eq!(
        request(
            &app,
            "PUT",
            "/api/e2ee/device",
            Some(&admin.token),
            changed.clone()
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        request(
            &app,
            "PUT",
            "/api/e2ee/device",
            Some(&admin.token),
            valid.clone()
        )
        .await
        .0,
        StatusCode::OK
    );
    for id in ["a-two", "a-three", "a-four"] {
        enroll(&app, &admin, id).await;
    }
    let mut fifth = valid.clone();
    fifth["device_id"] = serde_json::json!("a-five");
    assert_eq!(
        request(
            &app,
            "PUT",
            "/api/e2ee/device",
            Some(&admin.token),
            fifth.clone()
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        request(
            &app,
            "DELETE",
            "/api/e2ee/device/a-device",
            Some(&other.token),
            serde_json::Value::Null
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            &app,
            "DELETE",
            "/api/e2ee/device/a-device",
            Some(&admin.token),
            serde_json::Value::Null
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        request(&app, "PUT", "/api/e2ee/device", Some(&admin.token), changed)
            .await
            .0,
        StatusCode::CONFLICT
    );
    // Simulate a lost revocation acknowledgement. Durable retries of the
    // owner's tombstone succeed; neither a foreign nor unknown ID does.
    assert_eq!(
        request(
            &app,
            "DELETE",
            "/api/e2ee/device/a-device",
            Some(&admin.token),
            serde_json::Value::Null
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        request(
            &app,
            "DELETE",
            "/api/e2ee/device/a-device",
            Some(&other.token),
            serde_json::Value::Null
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            &app,
            "DELETE",
            "/api/e2ee/device/unknown-device",
            Some(&admin.token),
            serde_json::Value::Null
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert!(!lock(&state.0.db)
        .query_row(
            "SELECT active FROM crypto_devices WHERE device_id='a-device'",
            [],
            |r| r.get::<_, bool>(0)
        )
        .unwrap());
    assert_eq!(
        request(&app, "PUT", "/api/e2ee/device", Some(&admin.token), fifth)
            .await
            .0,
        StatusCode::OK
    );
    let own = request(
        &app,
        "GET",
        "/api/e2ee/device",
        Some(&other.token),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(own.1, serde_json::json!([]));
    let target = CryptoTarget {
        peer_id: Some(other.user.id),
        ..Default::default()
    };
    assert_eq!(
        get_context(&app, &admin, "a-device", &target, 0).await.0,
        StatusCode::FORBIDDEN
    );
    for invalid in [
        serde_json::json!({"device_id":"bad/id","identity":"ab".repeat(32),"key_package":STANDARD.encode(b"x")}),
        serde_json::json!({"device_id":"new","identity":"bad","key_package":STANDARD.encode(b"x")}),
        serde_json::json!({"device_id":"new","identity":"ab".repeat(32),"key_package":"not base64"}),
        serde_json::json!({"device_id":"new","identity":"ab".repeat(32),"key_package":STANDARD.encode(vec![0;MAX_KEY_PACKAGE+1])}),
    ] {
        assert_eq!(
            request(&app, "PUT", "/api/e2ee/device", Some(&other.token), invalid)
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
    }
}

#[tokio::test]
async fn direct_events_are_atomic_idempotent_scoped_to_pair_and_welcome_device() {
    let data = tempfile::tempdir().unwrap();
    let state = AppState::open(data.path(), "crypto", 0).unwrap();
    let app = router(state.clone());
    let admin = signup(&app, "Admin", "").await;
    let member = signup(&app, "Member", &new_invite(&app, &admin).await).await;
    let outsider = signup(&app, "Outsider", &new_invite(&app, &admin).await).await;
    enroll(&app, &admin, "a-device").await;
    enroll(&app, &member, "m-device").await;
    enroll(&app, &member, "m-second").await;
    enroll(&app, &outsider, "o-device").await;
    let target = CryptoTarget {
        peer_id: Some(member.user.id.clone()),
        ..Default::default()
    };
    let input = initial(target.clone(), "a-device", "direct-group", Some("m-device"));
    let (status, response) = append(&app, &admin, &input).await;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert_eq!(response["sequence"], 3);
    assert_eq!(response["events"].as_array().unwrap().len(), 2);
    assert_eq!(
        append(&app, &admin, &input).await.0,
        StatusCode::OK,
        "initial operation retry is idempotent even after group exists"
    );
    assert_eq!(
        lock(&state.0.db)
            .query_row("SELECT COUNT(*) FROM crypto_events", [], |r| r
                .get::<_, u32>(0))
            .unwrap(),
        3
    );
    let reverse = CryptoTarget {
        peer_id: Some(admin.user.id.clone()),
        ..Default::default()
    };
    let (_, recipient) = get_context(&app, &member, "m-device", &reverse, 0).await;
    assert_eq!(recipient["group_id"], "direct-group");
    assert_eq!(recipient["events"].as_array().unwrap().len(), 3);
    let (_, second) = get_context(&app, &member, "m-second", &reverse, 0).await;
    assert_eq!(
        second["events"].as_array().unwrap().len(),
        2,
        "another device cannot retrieve a sibling's Welcome"
    );
    let (_, outside) = get_context(&app, &outsider, "o-device", &reverse, 0).await;
    assert!(
        outside["group_id"].is_null(),
        "outsider's own DM is a distinct scope"
    );
    let mut forged = input.clone();
    forged.device_id = "m-device".into();
    assert_eq!(append(&app, &admin, &forged).await.0, StatusCode::FORBIDDEN);
    let mut altered = input.clone();
    altered.events[1].body = STANDARD.encode(b"different commit");
    assert_eq!(append(&app, &admin, &altered).await.0, StatusCode::CONFLICT);
    let mut next = input.clone();
    next.operation_id = "second-op".into();
    next.events.remove(0);
    next.epoch = 2;
    assert_eq!(
        append(&app, &admin, &next).await.0,
        StatusCode::CONFLICT,
        "stale sequence never commits"
    );
    next.expected_sequence = 3;
    next.events.retain(|event| event.kind == "commit");
    assert_eq!(append(&app, &admin, &next).await.0, StatusCode::OK);
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/logout",
            Some(&member.token),
            serde_json::Value::Null
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        get_context(&app, &member, "m-device", &reverse, 0).await.0,
        StatusCode::UNAUTHORIZED
    );
    let backup = data.path().join("crypto.sqlite3");
    state.backup_to(&backup).unwrap();
    drop(app);
    drop(state);
    let restarted = AppState::open(data.path(), "crypto", 0).unwrap();
    let app = router(restarted.clone());
    let (_, restored) = get_context(&app, &admin, "a-device", &target, 3).await;
    assert_eq!(restored["sequence"], 4);
    assert_eq!(restored["events"].as_array().unwrap().len(), 1);
    let snapshot = Connection::open(backup).unwrap();
    assert_eq!(
        snapshot
            .query_row("SELECT COUNT(*) FROM crypto_events", [], |r| r
                .get::<_, u32>(0))
            .unwrap(),
        4
    );
}

#[tokio::test]
async fn key_packages_are_reserved_per_target_never_reused_and_pool_bounds_rollback() {
    let data = tempfile::tempdir().unwrap();
    let state = AppState::open(data.path(), "pool", 0).unwrap();
    let app = router(state.clone());
    let admin = signup(&app, "Admin", "").await;
    enroll(&app, &admin, "pool-device").await;
    let first = CryptoTarget {
        channel_id: Some(
            lock(&state.0.db)
                .query_row("SELECT id FROM channels WHERE name='general'", [], |r| {
                    r.get(0)
                })
                .unwrap(),
        ),
        ..Default::default()
    };
    let second = CryptoTarget {
        channel_id: Some(
            lock(&state.0.db)
                .query_row("SELECT id FROM channels WHERE name='clips'", [], |r| {
                    r.get(0)
                })
                .unwrap(),
        ),
        ..Default::default()
    };
    let (_, first_context) = get_context(&app, &admin, "pool-device", &first, 0).await;
    let first_package = first_context["devices"][0]["key_package"].clone();
    assert!(!first_package.as_str().unwrap().is_empty());
    assert_eq!(
        get_context(&app, &admin, "pool-device", &first, 0).await.1["devices"][0]["key_package"],
        first_package
    );
    assert_eq!(
        get_context(&app, &admin, "pool-device", &second, 0).await.1["devices"][0]["key_package"],
        ""
    );
    let packages: Vec<String> = (0..16)
        .map(|i| STANDARD.encode(format!("fresh keypackage {i}")))
        .collect();
    let batch = serde_json::json!({"device_id":"pool-device","packages":packages});
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/e2ee/key-packages",
            Some(&admin.token),
            batch.clone()
        )
        .await
        .1["accepted"],
        16
    );
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/e2ee/key-packages",
            Some(&admin.token),
            batch
        )
        .await
        .1["accepted"],
        0
    );
    let (_, second_context) = get_context(&app, &admin, "pool-device", &second, 0).await;
    let second_package = second_context["devices"][0]["key_package"].clone();
    assert_ne!(second_package, first_package);
    let owned = lock(&state.0.db)
        .query_row(
            "SELECT COUNT(*) FROM crypto_key_packages WHERE target_kind IS NOT NULL",
            [],
            |r| r.get::<_, u32>(0),
        )
        .unwrap();
    assert_eq!(owned, 2);
    for offset in [16, 32] {
        let packages: Vec<String> = (offset..offset + 16)
            .map(|i| STANDARD.encode(format!("fresh keypackage {i}")))
            .collect();
        let status = request(
            &app,
            "POST",
            "/api/e2ee/key-packages",
            Some(&admin.token),
            serde_json::json!({"device_id":"pool-device","packages":packages}),
        )
        .await
        .0;
        assert_eq!(
            status,
            if offset == 16 {
                StatusCode::OK
            } else {
                StatusCode::CONFLICT
            }
        );
    }
    assert_eq!(
        unused_packages(&lock(&state.0.db), "pool-device").unwrap(),
        31
    );
    drop(app);
    drop(state);
    let reopened = AppState::open(data.path(), "pool", 0).unwrap();
    let app = router(reopened);
    assert_eq!(
        get_context(&app, &admin, "pool-device", &first, 0).await.1["devices"][0]["key_package"],
        first_package
    );
    assert_eq!(
        get_context(&app, &admin, "pool-device", &second, 0).await.1["devices"][0]["key_package"],
        second_package
    );
}

#[tokio::test]
async fn private_channel_and_call_rosters_recheck_current_permissions_and_presence() {
    let data = tempfile::tempdir().unwrap();
    let state = AppState::open(data.path(), "privacy", 0).unwrap();
    let app = router(state.clone());
    let admin = signup(&app, "Admin", "").await;
    let member = signup(&app, "Member", &new_invite(&app, &admin).await).await;
    let other = signup(&app, "Other", &new_invite(&app, &admin).await).await;
    enroll(&app, &admin, "a-device").await;
    enroll(&app, &member, "m-device").await;
    enroll(&app, &other, "o-device").await;
    let (_, channel) = request(
        &app,
        "POST",
        "/api/channels",
        Some(&admin.token),
        serde_json::json!({"name":"private","kind":"text","visibility":"private"}),
    )
    .await;
    let channel = channel["id"].as_str().unwrap();
    let target = CryptoTarget {
        channel_id: Some(channel.into()),
        ..Default::default()
    };
    assert_eq!(
        get_context(&app, &member, "m-device", &target, 0).await.0,
        StatusCode::FORBIDDEN
    );
    let mut failed = initial(
        target.clone(),
        "a-device",
        "private-group",
        Some("m-device"),
    );
    assert_eq!(append(&app, &admin, &failed).await.0, StatusCode::FORBIDDEN);
    assert_eq!(
        lock(&state.0.db)
            .query_row("SELECT COUNT(*) FROM crypto_groups", [], |r| r
                .get::<_, u32>(0))
            .unwrap(),
        0,
        "no partial group after invalid Welcome"
    );
    let path = format!("/api/channels/{channel}/permissions");
    let grant = serde_json::json!({"entries":[{"subject_kind":"user","subject_id":member.user.id,"read":true,"send":false,"join":false,"stream":false}]});
    assert_eq!(
        request(&app, "PUT", &path, Some(&admin.token), grant)
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(append(&app, &admin, &failed).await.0, StatusCode::OK);
    let (_, visible) = get_context(&app, &member, "m-device", &target, 0).await;
    assert_eq!(visible["roster"].as_array().unwrap().len(), 2);
    assert!(!visible["devices"]
        .as_array()
        .unwrap()
        .iter()
        .any(|d| d["device_id"] == "o-device"));
    request(
        &app,
        "PUT",
        &path,
        Some(&admin.token),
        serde_json::json!({"entries":[]}),
    )
    .await;
    assert_eq!(
        get_context(&app, &member, "m-device", &target, 0).await.0,
        StatusCode::FORBIDDEN
    );
    failed.events.remove(0);
    failed.expected_sequence = 3;
    failed.epoch = 2;
    failed.operation_id = "revoked-welcome".into();
    assert_eq!(append(&app, &admin, &failed).await.0, StatusCode::FORBIDDEN);
    let voice: String = lock(&state.0.db)
        .query_row(
            "SELECT id FROM channels WHERE kind='voice' LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let call = CryptoTarget {
        channel_id: Some(voice.clone()),
        ..Default::default()
    };
    let (_, before) = get_context(&app, &admin, "a-device", &call, 0).await;
    assert_eq!(
        before["roster"].as_array().unwrap().len(),
        1,
        "pending requester only, not every join-capable member"
    );
    request(
        &app,
        "POST",
        "/api/voice/join",
        Some(&member.token),
        serde_json::json!({"channel_id":voice}),
    )
    .await;
    assert_eq!(
        get_context(&app, &admin, "a-device", &call, 0).await.1["roster"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    request(
        &app,
        "POST",
        "/api/voice/leave",
        Some(&member.token),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(
        get_context(&app, &admin, "a-device", &call, 0).await.1["roster"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn crypto_quota_and_malformed_batches_fail_without_partial_state() {
    let data = tempfile::tempdir().unwrap();
    let state = AppState::open_with_config(
        data.path(),
        "quota",
        0,
        ServerConfig {
            storage_limit_bytes: 600,
            user_storage_limit_bytes: 600,
            ..Default::default()
        },
    )
    .unwrap();
    let app = router(state.clone());
    let admin = signup(&app, "Admin", "").await;
    enroll(&app, &admin, "device").await;
    let channel: String = lock(&state.0.db)
        .query_row("SELECT id FROM channels WHERE name='general'", [], |r| {
            r.get(0)
        })
        .unwrap();
    let mut input = initial(
        CryptoTarget {
            channel_id: Some(channel),
            ..Default::default()
        },
        "device",
        "quota-group",
        None,
    );
    input.events[1].body = STANDARD.encode(vec![7; 512]);
    assert_eq!(
        append(&app, &admin, &input).await.0,
        StatusCode::INSUFFICIENT_STORAGE
    );
    assert_eq!(
        lock(&state.0.db)
            .query_row("SELECT COUNT(*) FROM crypto_groups", [], |r| r
                .get::<_, u32>(0))
            .unwrap(),
        0
    );
    input.events[1].body = STANDARD.encode(vec![7; MAX_EVENT_BODY + 1]);
    assert_eq!(
        append(&app, &admin, &input).await.0,
        StatusCode::BAD_REQUEST
    );
    input.events[1].body = "invalid base64".into();
    assert_eq!(
        append(&app, &admin, &input).await.0,
        StatusCode::BAD_REQUEST
    );
    input.events[1].body = STANDARD.encode(b"bounded");
    input.events[0].body = "not empty".into();
    assert_eq!(
        append(&app, &admin, &input).await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        lock(&state.0.db)
            .query_row("SELECT COUNT(*) FROM crypto_events", [], |r| r
                .get::<_, u32>(0))
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn ciphertext_messages_have_only_tagged_bounded_budget_and_no_server_full_text_index() {
    let data = tempfile::tempdir().unwrap();
    let state = AppState::open(data.path(), "ciphertext", 0).unwrap();
    let app = router(state.clone());
    let admin = signup(&app, "Admin", "").await;
    let channel: String = lock(&state.0.db)
        .query_row("SELECT id FROM channels WHERE name='general'", [], |r| {
            r.get(0)
        })
        .unwrap();
    let content = format!("relay-e2ee:v1:{}", STANDARD.encode(vec![7; 48 * 1024]));
    let (status, message) = request(
        &app,
        "POST",
        "/api/messages",
        Some(&admin.token),
        serde_json::json!({"channel_id":channel,"content":content}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{message}");
    assert_eq!(
        lock(&state.0.db)
            .query_row("SELECT COUNT(*) FROM message_search", [], |r| r
                .get::<_, u32>(0))
            .unwrap(),
        0
    );
    let path = format!("/api/messages/{}", message["id"].as_str().unwrap());
    assert_eq!(
        request(
            &app,
            "PATCH",
            &path,
            Some(&admin.token),
            serde_json::json!({"content":"plain older-client edit"})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        lock(&state.0.db)
            .query_row("SELECT COUNT(*) FROM message_search", [], |r| r
                .get::<_, u32>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        request(
            &app,
            "PATCH",
            &path,
            Some(&admin.token),
            serde_json::json!({"content":content})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        lock(&state.0.db)
            .query_row("SELECT COUNT(*) FROM message_search", [], |r| r
                .get::<_, u32>(0))
            .unwrap(),
        0
    );
    for invalid in [
        "p".repeat(10001),
        "relay-e2ee:v1:not-base64".into(),
        format!("relay-e2ee:v1:{}", STANDARD.encode(vec![7; 48 * 1024 + 1])),
    ] {
        assert_eq!(
            request(
                &app,
                "POST",
                "/api/messages",
                Some(&admin.token),
                serde_json::json!({"channel_id":channel,"content":invalid})
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    }
}

fn attestation(device: &str, invite: &str, mac: &str) -> serde_json::Value {
    serde_json::json!({"device_id":device,"invite_id":invite,"mac":mac})
}

#[tokio::test]
async fn invitation_attestation_is_accepted_once_only_for_the_members_own_device_and_invitation() {
    let data = tempfile::tempdir().unwrap();
    let state = AppState::open(data.path(), "invitation attestation", 0).unwrap();
    let app = router(state.clone());
    let admin = signup(&app, "Admin", "").await;
    let code = new_invite(&app, &admin).await;
    let invite_id = token_hash(&code);
    let other_code = new_invite(&app, &admin).await;
    let member = signup(&app, "Member", &code).await;
    enroll(&app, &admin, "admin-device").await;
    enroll(&app, &member, "member-device").await;
    let mac = "cd".repeat(32);
    let put = |session: &Session, body: serde_json::Value| {
        let app = app.clone();
        let token = session.token.clone();
        async move {
            request(
                &app,
                "PUT",
                "/api/e2ee/device/attestation",
                Some(&token),
                body,
            )
            .await
            .0
        }
    };
    // Nobody can attach a proof to another account's device.
    assert_eq!(
        put(&member, attestation("admin-device", &invite_id, &mac)).await,
        StatusCode::FORBIDDEN
    );
    // The first account joined without an invitation, so it has nothing to attest.
    assert_eq!(
        put(&admin, attestation("admin-device", &invite_id, &mac)).await,
        StatusCode::FORBIDDEN
    );
    // Only the invitation the member really joined with counts, not another valid one.
    assert_eq!(
        put(
            &member,
            attestation("member-device", &token_hash(&other_code), &mac)
        )
        .await,
        StatusCode::FORBIDDEN
    );
    // Exact sizes and lowercase hexadecimal only.
    for (invite, bad_mac) in [
        (invite_id.clone(), "cd".repeat(31)),
        (invite_id.clone(), "cd".repeat(33)),
        (invite_id.clone(), "CD".repeat(32)),
        (invite_id.clone(), "zz".repeat(32)),
        (invite_id[..63].to_string(), mac.clone()),
        (invite_id.to_uppercase(), mac.clone()),
    ] {
        assert_eq!(
            put(&member, attestation("member-device", &invite, &bad_mac)).await,
            StatusCode::BAD_REQUEST,
            "{invite} {bad_mac}"
        );
    }
    // Unknown fields and an oversized body are refused before anything is stored.
    assert_eq!(
        put(&member, serde_json::json!({"device_id":"member-device","invite_id":invite_id,"mac":mac,"extra":1})).await,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        put(&member, serde_json::json!({"device_id":"member-device","invite_id":invite_id,"mac":mac,"pad":"x".repeat(2048)})).await,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    assert_eq!(
        lock(&state.0.db)
            .query_row("SELECT COUNT(*) FROM device_attestations", [], |r| r
                .get::<_, u32>(0))
            .unwrap(),
        0,
        "no refused request stored anything"
    );
    // The proof is stored once and then appears in the community directory.
    assert_eq!(
        put(&member, attestation("member-device", &invite_id, &mac)).await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        put(&member, attestation("member-device", &invite_id, &mac)).await,
        StatusCode::CONFLICT
    );
    let (status, directory) = request(
        &app,
        "GET",
        "/api/e2ee/directory",
        Some(&admin.token),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let devices = directory.as_array().unwrap();
    let find = |id: &str| devices.iter().find(|d| d["device_id"] == id).unwrap();
    assert_eq!(
        find("member-device")["invite_attestation"],
        serde_json::json!({"invite_id":invite_id,"mac":mac})
    );
    assert!(
        find("admin-device").get("invite_attestation").is_none(),
        "no proof, no field"
    );
    // The proof never appears in the member's own-device list or the server's bootstrap.
    let (_, own) = request(
        &app,
        "GET",
        "/api/e2ee/device",
        Some(&member.token),
        serde_json::Value::Null,
    )
    .await;
    assert!(own
        .as_array()
        .unwrap()
        .iter()
        .all(|d| d.get("invite_attestation").is_none()));
    // Only a hash of the code is kept; the secret fragment never reaches the server at all.
    let joined: Option<String> = lock(&state.0.db)
        .query_row(
            "SELECT invite_id FROM member_invitations m JOIN users u ON u.id=m.user_id WHERE u.username='Member'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(joined.as_deref(), Some(invite_id.as_str()));
    assert_ne!(joined.as_deref(), Some(code.as_str()));
}
