use super::*;
use tower::ServiceExt as _;

#[tokio::test]
async fn people_private_shutdown_requires_current_direct_loopback_owner_proof() {
    const CONTROL: &str = "owned-people-process-control-0123456789";
    let data = tempfile::tempdir().unwrap();
    let state = AppState::open_with_config(
        data.path(),
        "owned Friends",
        0,
        ServerConfig {
            people: true,
            account_provider: Some(
                Provider::new(
                    "https://accounts.example.test/realms/relay-accounts",
                    "relay-desktop",
                    "http://127.0.0.1:4580",
                )
                .unwrap(),
            ),
            local_control_token: Some(CONTROL.into()),
            ..Default::default()
        },
    )
    .unwrap();
    let app = router(state.clone());
    let body =
        json!({"instance_id":state.instance_id(),"confirm_server_id":state.server_id().unwrap()});
    let valid = format!("Bearer {CONTROL}");
    for (peer, headers, input, expected) in [
        (
            "127.0.0.1:7001",
            vec![],
            body.clone(),
            StatusCode::UNAUTHORIZED,
        ),
        (
            "127.0.0.1:7001",
            vec![("authorization", "Bearer wrong-owner-token")],
            body.clone(),
            StatusCode::UNAUTHORIZED,
        ),
        (
            "192.0.2.8:7001",
            vec![("authorization", valid.as_str())],
            body.clone(),
            StatusCode::FORBIDDEN,
        ),
        (
            "127.0.0.1:7001",
            vec![
                ("authorization", valid.as_str()),
                ("forwarded", "for=192.0.2.8"),
            ],
            body.clone(),
            StatusCode::FORBIDDEN,
        ),
        (
            "127.0.0.1:7001",
            vec![
                ("authorization", valid.as_str()),
                ("x-forwarded-for", "192.0.2.8"),
            ],
            body.clone(),
            StatusCode::FORBIDDEN,
        ),
        (
            "127.0.0.1:7001",
            vec![("authorization", valid.as_str())],
            json!({"instance_id":"stale-instance","confirm_server_id":state.server_id().unwrap()}),
            StatusCode::BAD_REQUEST,
        ),
    ] {
        let response = app
            .clone()
            .oneshot(local_request("/api/local/shutdown", peer, &headers, input))
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
        assert!(tokio::time::timeout(
            std::time::Duration::from_millis(10),
            state.shutdown_requested()
        )
        .await
        .is_err());
    }
    // The exception is exact: other local management paths stay outside Friends.
    assert_eq!(
        app.clone()
            .oneshot(local_request(
                "/api/local/account-destination",
                "127.0.0.1:7001",
                &[("authorization", valid.as_str())],
                json!({})
            ))
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        app.oneshot(local_request(
            "/api/local/shutdown",
            "127.0.0.1:7001",
            &[("authorization", valid.as_str())],
            body
        ))
        .await
        .unwrap()
        .status(),
        StatusCode::NO_CONTENT
    );
    tokio::time::timeout(
        std::time::Duration::from_millis(100),
        state.shutdown_requested(),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn people_private_shutdown_stays_unreachable_from_authenticated_network_ingress() {
    const CONTROL: &str = "owned-people-process-control-0123456789";
    const INGRESS: &str = "owned-network-capability-distinct-from-process-control";
    let data = tempfile::tempdir().unwrap();
    let state = AppState::open_with_config(
        data.path(),
        "owned Friends",
        0,
        ServerConfig {
            people: true,
            account_provider: Some(
                Provider::new(
                    "https://accounts.example.test/realms/relay-accounts",
                    "relay-desktop",
                    "http://127.0.0.1:4580",
                )
                .unwrap(),
            ),
            local_control_token: Some(CONTROL.into()),
            ..Default::default()
        },
    )
    .unwrap();
    let app = network_ingress_router(state.clone(), INGRESS.into());
    let valid = format!("Bearer {CONTROL}");
    let response = app.oneshot(local_request("/api/local/shutdown", "127.0.0.1:7001", &[
        ("authorization",valid.as_str()),("x-relay-network-ingress",INGRESS),
        ("x-relay-network-peer","100.64.0.7")],
        json!({"instance_id":state.instance_id(),"confirm_server_id":state.server_id().unwrap()}))).await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(tokio::time::timeout(
        std::time::Duration::from_millis(20),
        state.shutdown_requested()
    )
    .await
    .is_err());
}

#[tokio::test]
async fn people_signed_accounts_enroll_without_invites_never_become_admin_and_bind_one_use_proofs()
{
    let fixture = Fixture::new().await;
    let data = tempfile::tempdir().unwrap();
    let state = AppState::open_with_config(
        data.path(),
        "Relay friends",
        0,
        ServerConfig {
            people: true,
            account_provider: Some(fixture.provider.clone()),
            ..Default::default()
        },
    )
    .unwrap();
    let app = router(state.clone());
    for (handle, subject) in [
        ("alice_shared", "subject-alice"),
        ("bob_shared", "subject-bob"),
    ] {
        let challenge = login_challenge(&app, false, "").await;
        let now = chrono::Utc::now();
        let claims = CoreIdTokenClaims::new(
            IssuerUrl::new(fixture.provider.issuer.to_string()).unwrap(),
            vec![Audience::new("relay-desktop".into())],
            now + chrono::Duration::seconds(300),
            now,
            StandardClaims::new(SubjectIdentifier::new(subject.into())),
            EmptyAdditionalClaims {},
        )
        .set_nonce(Some(Nonce::new(
            challenge["nonce"].as_str().unwrap().into(),
        )))
        .set_preferred_username(Some(EndUserUsername::new(handle.into())));
        let proof = token(claims, 23);
        let (status, session) = login_proof(&app, &challenge, proof.clone()).await;
        assert_eq!(status, StatusCode::OK, "{session}");
        assert_eq!(session["user"]["role"], "member");
        assert!(login_proof(&app, &challenge, proof)
            .await
            .0
            .is_client_error());
    }
    let challenge = login_challenge(&app, true, "").await;
    // A newly issued subject must never claim an already enrolled handle.
    assert_eq!(
        login_proof(
            &app,
            &challenge,
            fixture.token(challenge["nonce"].as_str().unwrap())
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    let challenge = login_challenge(&app, true, "").await;
    assert!(login_proof(
        &app,
        &challenge,
        token(fixture.claims(challenge["nonce"].as_str().unwrap()), 24)
    )
    .await
    .0
    .is_client_error());
    assert_eq!(
        lock(&state.0.db)
            .query_row("SELECT COUNT(*) FROM users", [], |r| r.get::<_, u32>(0))
            .unwrap(),
        2
    );
    assert_eq!(
        lock(&state.0.db)
            .query_row("SELECT COUNT(*) FROM channels", [], |r| r.get::<_, u32>(0))
            .unwrap(),
        0
    );
    assert_eq!(request(&app,"POST","/api/account-login/challenge",None,json!({"destination":"https://community.example.test","register":true,"initial_protection":"open"})).await.0,StatusCode::FORBIDDEN);
}
