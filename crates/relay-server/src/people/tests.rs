use super::*;
use crate::tests::request;
use serde_json::{json, Value};

#[tokio::test]
async fn people_encrypted_messages_survive_offline_recipient_and_restart_without_plaintext_storage()
{
    use relay_crypto::{Device, Identity, Received};
    let data = tempfile::tempdir().unwrap();
    let state = AppState::open_with_config(data.path(), "Relay friends", 0, config()).unwrap();
    let a = account(&state, "alice");
    let b = account(&state, "bob");
    let c = account(&state, "cara");
    let server = state.server_id().unwrap();
    let mut alice = Device::new(Identity {
        server_id: server.clone(),
        user_id: a.user.id.clone(),
        device_id: "alice-owned-device".into(),
    })
    .unwrap();
    let mut bob = Device::new(Identity {
        server_id: server,
        user_id: b.user.id.clone(),
        device_id: "bob-owned-device".into(),
    })
    .unwrap();
    let a_path = data.path().join("alice-private-state");
    let b_path = data.path().join("bob-private-state");
    let a_public = alice
        .transact(
            |d| d.key_package(),
            |v| std::fs::write(&a_path, v).map_err(|_| "own checkpoint".into()),
        )
        .unwrap();
    let b_public = bob
        .transact(
            |d| d.key_package(),
            |v| std::fs::write(&b_path, v).map_err(|_| "own checkpoint".into()),
        )
        .unwrap();
    // Explicit fingerprint comparison in this synthetic test is not an automatic
    // trust policy. Production users retain the normal device-verification flow.
    alice
        .transact(
            |d| d.approve(&b_public, &b_public.fingerprint),
            |v| std::fs::write(&a_path, v).map_err(|_| "own checkpoint".into()),
        )
        .unwrap();
    bob.transact(
        |d| d.approve(&a_public, &a_public.fingerprint),
        |v| std::fs::write(&b_path, v).map_err(|_| "own checkpoint".into()),
    )
    .unwrap();
    let scope = "owned-private-dm";
    alice
        .transact(
            |d| d.create_group(scope, "owned-private-group"),
            |v| std::fs::write(&a_path, v).map_err(|_| "own checkpoint".into()),
        )
        .unwrap();
    let membership = alice
        .transact(
            |d| d.add_members(scope, std::slice::from_ref(&b_public)),
            |v| std::fs::write(&a_path, v).map_err(|_| "own checkpoint".into()),
        )
        .unwrap();
    bob.transact(
        |d| {
            d.join(
                scope,
                "owned-private-group",
                a_public.identity.clone(),
                membership.welcome.as_ref().unwrap(),
            )
        },
        |v| std::fs::write(&b_path, v).map_err(|_| "own checkpoint".into()),
    )
    .unwrap();
    let app = router(state.clone());
    for (session, device) in [(&a, &a_public), (&b, &b_public)] {
        assert_eq!(request(&app, "PUT", "/api/e2ee/device", Some(&session.token), json!({
            "device_id":device.identity.device_id,"identity":device.fingerprint,"key_package":STANDARD.encode(&device.key_package)
        })).await.0, StatusCode::OK);
    }
    assert_eq!(
        request(
            &app,
            "GET",
            "/api/e2ee/directory",
            Some(&a.token),
            Value::Null
        )
        .await
        .1
        .as_array()
        .unwrap()
        .len(),
        1
    );
    assert!(request(
        &app,
        "GET",
        "/api/e2ee/directory",
        Some(&c.token),
        Value::Null
    )
    .await
    .1
    .as_array()
    .unwrap()
    .is_empty());
    let text = b"Private message while my friend is offline";
    let wire = alice
        .transact(
            |d| d.encrypt(scope, text),
            |v| std::fs::write(&a_path, v).map_err(|_| "own checkpoint".into()),
        )
        .unwrap();
    let envelope = format!("relay-e2ee:v1:{}", STANDARD.encode(&wire));
    let body = json!({"recipient_id":b.user.id,"content":envelope});
    assert_eq!(
        request(&app, "POST", "/api/messages", Some(&a.token), body.clone())
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    for (sender, peer) in [(&a, &b), (&b, &a)] {
        assert_eq!(
            request(
                &app,
                "POST",
                &format!("/api/friends/{}", peer.user.id),
                Some(&sender.token),
                Value::Null
            )
            .await
            .0,
            StatusCode::OK
        );
    }
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/messages",
            Some(&a.token),
            json!({"recipient_id":b.user.id,"content":"plain text is forbidden"})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let (status, sent) = request(&app, "POST", "/api/messages", Some(&a.token), body).await;
    assert_eq!(status, StatusCode::OK, "{sent}");
    let stored: String = lock(&state.0.db)
        .query_row("SELECT content FROM messages", [], |r| r.get(0))
        .unwrap();
    assert_eq!(stored, envelope);
    assert!(!stored.contains("Private message"));
    assert!(lock(&state.0.voice).is_empty());
    drop(app);
    drop(state);
    let restarted = AppState::open_with_config(data.path(), "Relay friends", 0, config()).unwrap();
    let app = router(restarted.clone());
    let (status, history) = request(
        &app,
        "GET",
        &format!("/api/history?peer_id={}", a.user.id),
        Some(&b.token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{history}");
    let rows = history["messages"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    let encoded = rows[0]["content"]
        .as_str()
        .unwrap()
        .strip_prefix("relay-e2ee:v1:")
        .unwrap();
    let wire = STANDARD.decode(encoded).unwrap();
    bob = Device::restore(&std::fs::read(&b_path).unwrap(), &b_public.identity).unwrap();
    let read = bob
        .transact(
            |d| d.receive(scope, &wire),
            |v| std::fs::write(&b_path, v).map_err(|_| "own checkpoint".into()),
        )
        .unwrap();
    assert!(matches!(read,Received::Application{bytes,..} if bytes==text));
    assert_eq!(
        request(
            &app,
            "GET",
            "/api/e2ee/directory",
            Some(&b.token),
            Value::Null
        )
        .await
        .1
        .as_array()
        .unwrap()
        .len(),
        2
    );
    let (status, call) = request(
        &app,
        "POST",
        "/api/private-calls",
        Some(&a.token),
        json!({"peer_id":b.user.id}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        request(
            &app,
            "POST",
            &format!("/api/private-calls/{}", call["id"].as_str().unwrap()),
            Some(&b.token),
            json!({"action":"accept"})
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
            Some(&a.token),
            json!({"channel_id":call["channel_id"]})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    for (session, device) in [(&a, &a_public), (&b, &b_public)] {
        assert_eq!(
            request(
                &app,
                "POST",
                "/api/voice/join",
                Some(&session.token),
                json!({"channel_id":call["channel_id"],"device_id":device.identity.device_id})
            )
            .await
            .0,
            StatusCode::OK
        );
    }
    assert_eq!(
        lock(&restarted.0.voice).len(),
        2,
        "API admission only; no microphone is opened by the server"
    );
    assert_eq!(
        request(
            &app,
            "GET",
            &format!("/api/history?peer_id={}", a.user.id),
            Some(&c.token),
            Value::Null
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
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
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/messages",
            Some(&a.token),
            json!({"recipient_id":b.user.id,"content":envelope})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            &app,
            "PATCH",
            &format!("/api/messages/{}", sent["id"].as_str().unwrap()),
            Some(&a.token),
            json!({"content":envelope})
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
            Some(&a.token),
            json!({"channel_id":call["channel_id"],"device_id":a_public.identity.device_id})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
}

fn config() -> ServerConfig {
    ServerConfig {
        people: true,
        account_provider: Some(
            account_identity::Provider::new(
                "https://accounts.example.test/realms/relay-accounts",
                "relay-desktop",
                "https://people.example.test",
            )
            .unwrap(),
        ),
        ..Default::default()
    }
}

// Owned accounts are inserted without password hashing. Signed OIDC enrollment
// has its own real-signature test in account_identity/tests.rs.
fn account(state: &AppState, handle: &str) -> Session {
    let db = lock(&state.0.db);
    let user = User {
        id: id(),
        username: handle.into(),
        role: "member".into(),
        ..Default::default()
    };
    db.execute(
        "INSERT INTO users VALUES(?1,?2,?3,'member')",
        params![user.id, handle, account_identity::login::ACCOUNT_ONLY],
    )
    .unwrap();
    db.execute("INSERT INTO membership(user_id) VALUES(?1)", [&user.id])
        .unwrap();
    db.execute("INSERT INTO user_profiles(user_id) VALUES(?1)", [&user.id])
        .unwrap();
    db.execute(
        "INSERT INTO linked_identities VALUES(?1,?2,?3,?4,?5)",
        params![
            user.id,
            config().account_provider.unwrap().public().issuer,
            format!("subject-{handle}"),
            handle,
            now()
        ],
    )
    .unwrap();
    new_session(&db, user).unwrap()
}

#[test]
fn people_data_cannot_convert_a_community_or_be_opened_as_one() {
    let data = tempfile::tempdir().unwrap();
    let state = AppState::open_with_config(data.path(), "Relay friends", 0, config()).unwrap();
    assert!(!state.has_users().unwrap());
    assert!(state
        .initialize_admin("admin", "long-unique-password")
        .is_err());
    drop(state);
    assert!(AppState::open(data.path(), "community", 0).is_err());
    assert!(AppState::open_with_config(data.path(), "Relay friends", 0, config()).is_ok());
    let community = tempfile::tempdir().unwrap();
    drop(AppState::open(community.path(), "existing", 0).unwrap());
    assert!(AppState::open_with_config(community.path(), "friends", 0, config()).is_err());
    let unconfigured = tempfile::tempdir().unwrap();
    assert!(AppState::open_with_config(
        unconfigured.path(),
        "friends",
        0,
        ServerConfig {
            people: true,
            ..Default::default()
        }
    )
    .is_err());
}

#[tokio::test]
async fn people_friend_request_private_call_block_and_restart_without_a_community() {
    let data = tempfile::tempdir().unwrap();
    let state = AppState::open_with_config(data.path(), "Relay friends", 0, config()).unwrap();
    let a = account(&state, "alice");
    let b = account(&state, "bob");
    let c = account(&state, "cara");
    let app = router(state.clone());
    let (_, boot) = request(&app, "GET", "/api/bootstrap", Some(&a.token), Value::Null).await;
    assert_eq!(boot["members"].as_array().unwrap().len(), 1);
    assert!(boot["channels"].as_array().unwrap().is_empty());
    assert!(boot["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c == CAPABILITY));
    // No community password, invitation, admin privileges, roster or open mode.
    for (method, path) in [
        ("POST", "/api/register"),
        ("POST", "/api/login"),
        ("POST", "/api/invites"),
        ("GET", "/api/members"),
        ("PUT", "/api/protection"),
        ("POST", "/api/conversations"),
        ("GET", "/api/admin/backup"),
    ] {
        assert_eq!(
            request(&app, method, path, Some(&a.token), json!({}))
                .await
                .0,
            StatusCode::FORBIDDEN,
            "{path}"
        );
    }
    assert_eq!(
        request(
            &app,
            "GET",
            &format!("/api/profiles/{}", b.user.id),
            Some(&a.token),
            Value::Null
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let call_input = json!({"peer_id":b.user.id});
    assert!(request(
        &app,
        "POST",
        "/api/private-calls",
        Some(&a.token),
        call_input.clone()
    )
    .await
    .0
    .is_client_error());
    let (status, _) = request(
        &app,
        "POST",
        "/api/people/requests",
        Some(&a.token),
        json!({"handle":"@BoB"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, boot) = request(&app, "GET", "/api/bootstrap", Some(&b.token), Value::Null).await;
    assert_eq!(boot["social"]["incoming"][0], a.user.id);
    assert_eq!(boot["members"].as_array().unwrap().len(), 2);
    assert_eq!(boot["linked_identities"].as_array().unwrap().len(), 2);
    assert!(boot["presence"]
        .as_array()
        .unwrap()
        .iter()
        .all(|p| p["user_id"] == b.user.id));
    assert_eq!(
        request(
            &app,
            "POST",
            &format!("/api/friends/{}", a.user.id),
            Some(&b.token),
            Value::Null
        )
        .await
        .0,
        StatusCode::OK
    );
    let (status, call) = request(
        &app,
        "POST",
        "/api/private-calls",
        Some(&a.token),
        call_input.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{call}");
    assert_eq!(call["state"], "ringing");
    let endpoint = format!("/api/private-calls/{}", call["id"].as_str().unwrap());
    assert_eq!(
        request(
            &app,
            "POST",
            &endpoint,
            Some(&c.token),
            json!({"action":"accept"})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            &app,
            "POST",
            &endpoint,
            Some(&b.token),
            json!({"action":"accept"})
        )
        .await
        .1["state"],
        "accepted"
    );
    assert!(
        lock(&state.0.voice).is_empty(),
        "ring/accept is not permission to capture media"
    );
    let channel = call["channel_id"].as_str().unwrap();
    assert!(channel_access(&lock(&state.0.db), channel, &b.user.id, JOIN).is_ok());
    assert!(channel_access(&lock(&state.0.db), channel, &c.user.id, JOIN).is_err());
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
    assert!(channel_access(&lock(&state.0.db), channel, &a.user.id, JOIN).is_err());
    assert!(request(
        &app,
        "POST",
        "/api/private-calls",
        Some(&a.token),
        call_input
    )
    .await
    .0
    .is_client_error());
    let (_, outsider) = request(&app, "GET", "/api/bootstrap", Some(&c.token), Value::Null).await;
    assert_eq!(outsider["members"].as_array().unwrap().len(), 1);
    assert!(outsider["social"].get("calls").is_none());
    drop(app);
    drop(state);
    let restarted = AppState::open_with_config(data.path(), "Relay friends", 0, config()).unwrap();
    assert!(social::blocked(&lock(&restarted.0.db), &a.user.id, &b.user.id).unwrap());
    assert!(lock(&restarted.0.private_calls).is_empty());
}

#[tokio::test]
async fn people_exact_name_probe_budget_is_shared_with_uuid_requests() {
    let data = tempfile::tempdir().unwrap();
    let state = AppState::open_with_config(data.path(), "Relay friends", 0, config()).unwrap();
    let a = account(&state, "alice");
    let b = account(&state, "bob");
    let app = router(state);
    for _ in 0..20 {
        assert_eq!(
            request(
                &app,
                "POST",
                "/api/people/requests",
                Some(&a.token),
                json!({"handle":"absent"})
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    }
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
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        request(&app, "GET", "/api/bootstrap", Some(&a.token), Value::Null)
            .await
            .0,
        StatusCode::OK
    );
}

#[path = "security_review_tests.rs"]
mod security_review;
