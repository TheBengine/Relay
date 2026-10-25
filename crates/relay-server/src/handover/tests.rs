use super::*;
use crate::tests::{new_invite, request, signup};
use ed25519_dalek::{Signer, SigningKey};
use serde_json::{json, Value};

// Synthetic fixture only; real destinations create a random private local key.
fn fixture_key() -> SigningKey {
    SigningKey::from_bytes(&[31; 32])
}
fn fixture_proof(status: &model::Status) -> model::HostProof {
    let p = status.preparation.as_ref().unwrap();
    let signature = fixture_key().sign(&model::acceptance_message(
        &status.server_id,
        &status.source_host_id,
        status.source_identity.as_ref().unwrap(),
        status.term,
        p,
    ));
    model::HostProof {
        identity: crate::handover_target::identity(&fixture_key().verifying_key()),
        signature: URL_SAFE_NO_PAD.encode(signature.to_bytes()),
    }
}

pub(crate) async fn accepted() -> (tempfile::TempDir, AppState, Router, Session, Session, Value) {
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open(temp.path(), "handover", 0).unwrap();
    let app = router(state.clone());
    let owner = signup(&app, "Owner", "").await;
    let target = signup(&app, "Target", &new_invite(&app, &owner).await).await;
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/hosting/offers",
            Some(&owner.token),
            json!({"user_id":target.user.id,"roles":["replica"]})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/hosting/answer",
            Some(&target.token),
            json!({"accepted":["replica"]})
        )
        .await
        .0,
        StatusCode::OK
    );
    let (code, offer) = request(
        &app,
        "POST",
        &format!("{ROOT}/offer"),
        Some(&owner.token),
        json!({"server_id":state.server_id().unwrap(),"target_user_id":target.user.id,
            "target_host_id":crate::handover_target::identity(&fixture_key().verifying_key()).host_id,"target_url":"https://new-host.example/"}),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{offer}");
    let change =
        json!({"server_id":state.server_id().unwrap(),"preparation_id":offer["preparation"]["id"]});
    (temp, state, app, owner, target, change)
}
async fn accept(app: &Router, target: &Session, change: &Value) {
    let mut answer = change.clone();
    answer["accept_full_server_data"] = json!(true);
    let (code, status) = request(app, "GET", ROOT, Some(&target.token), Value::Null).await;
    assert_eq!(code, StatusCode::OK);
    let status: model::Status = serde_json::from_value(status).unwrap();
    answer["target_proof"] = json!(fixture_proof(&status));
    let (code, result) = request(
        app,
        "POST",
        &format!("{ROOT}/answer"),
        Some(&target.token),
        answer,
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{result}");
    assert_eq!(result["preparation"]["phase"], "accepted");
    assert_eq!(result["activation_available"], false);
}

#[tokio::test]
async fn private_destination_setup_is_verified_persisted_and_cleared_on_cancel() {
    let (temp,state,app,owner,target,change)=accepted().await;
    assert_eq!(request(&app,"POST",&format!("{ROOT}/cancel"),Some(&owner.token),change).await.0,StatusCode::OK);
    let destination=tempfile::tempdir().unwrap();
    let key=crate::handover_target::TargetKey::open(destination.path()).unwrap();
    let initial=state.0.handover.status();
    let pin=model::RoutePin {server_id:initial.server_id.clone(),host:initial.source_identity.unwrap(),term:initial.term};
    let endpoint="b".repeat(64);
    let private=model::IrohDestination {endpoint_id:endpoint.clone(),relay:None,token:"N".repeat(43),scope:None};
    let card=key.plan(pin.clone(),target.user.id.clone(),format!("http://{endpoint}.iroh:4580"),Some(private)).unwrap();
    let input=json!({"server_id":pin.server_id,"target_user_id":target.user.id,
        "target_host_id":card.body.target.host_id,"target_url":card.body.target_url,"destination":card});
    for change in 0..5 {
        let mut bad=input.clone();
        match change {
            0=>bad.as_object_mut().unwrap().remove("destination"),
            1=>Some(std::mem::replace(&mut bad["destination"]["signature"],json!("forged"))),
            2=>Some(std::mem::replace(&mut bad["destination"]["body"]["private_route"]["token"],json!("C".repeat(43)))),
            3=>Some(std::mem::replace(&mut bad["destination"]["body"]["source"]["term"],json!(pin.term+1))),
            _=>Some(std::mem::replace(&mut bad["target_url"],json!("https://unrelated.example"))),
        };
        let (code,_)=request(&app,"POST",&format!("{ROOT}/offer"),Some(&owner.token),bad).await;
        assert_eq!(code,StatusCode::BAD_REQUEST,"mutation {change}");
        assert!(state.0.handover.status().preparation.is_none());
    }
    let (code,offered)=request(&app,"POST",&format!("{ROOT}/offer"),Some(&owner.token),input.clone()).await;
    assert_eq!(code,StatusCode::OK,"{offered}");
    assert_eq!(offered["destination"],input["destination"]);
    assert_eq!(offered["writes_allowed"],true);
    drop(app);drop(state);
    let reopened=AppState::open(temp.path(),"handover",0).unwrap();
    let status=reopened.0.handover.status();
    assert_eq!(json!(status.destination),input["destination"]);
    let app=router(reopened.clone());
    let (_,cancelled)=request(&app,"POST",&format!("{ROOT}/cancel"),Some(&owner.token),
        json!({"server_id":pin.server_id,"preparation_id":status.preparation.unwrap().id})).await;
    assert!(cancelled.get("destination").is_none());
    assert!(reopened.0.handover.status().destination.is_none());
    assert!(!destination.path().join("relay.sqlite3").exists(),"planning grants no full data");
}

#[tokio::test]
async fn permission_to_copy_is_not_consent_to_host_and_only_target_can_accept() {
    let (_temp, _state, app, owner, target, change) = accepted().await;
    assert_eq!(
        request(
            &app,
            "POST",
            &format!("{ROOT}/freeze"),
            Some(&owner.token),
            change.clone()
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    let mut answer = change.clone();
    answer["accept_full_server_data"] = json!(true);
    assert_eq!(
        request(
            &app,
            "POST",
            &format!("{ROOT}/answer"),
            Some(&owner.token),
            answer
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    accept(&app, &target, &change).await;
    assert_eq!(
        request(
            &app,
            "POST",
            &format!("{ROOT}/freeze"),
            Some(&target.token),
            change.clone()
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let mut stale = change.clone();
    stale["preparation_id"] = json!(id());
    assert_eq!(
        request(
            &app,
            "POST",
            &format!("{ROOT}/freeze"),
            Some(&owner.token),
            stale
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    request(
        &app,
        "DELETE",
        &format!("/api/hosting/candidates/{}", target.user.id),
        Some(&owner.token),
        Value::Null,
    )
    .await;
    assert_eq!(
        request(
            &app,
            "POST",
            &format!("{ROOT}/freeze"),
            Some(&owner.token),
            change
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn freeze_blocks_http_and_background_writers_and_cancel_keeps_original_data() {
    let (temp, state, app, owner, target, change) = accepted().await;
    accept(&app, &target, &change).await;
    let backup = temp.path().join("before.sqlite3");
    state.backup_to(&backup).unwrap();
    let (code, result) = request(
        &app,
        "POST",
        &format!("{ROOT}/freeze"),
        Some(&owner.token),
        change.clone(),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{result}");
    assert_eq!(result["writes_allowed"], false);
    // The frozen image remains unchanged, but a remembered account can reopen
    // the move panel after an app restart and explicitly cancel the pause.
    let before = lock(&state.0.db).total_changes();
    for session in [&owner, &target] {
        let (code, view) = request(
            &app,
            "GET",
            "/api/bootstrap",
            Some(&session.token),
            Value::Null,
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{view}");
        assert_eq!(view["host_paused"], true);
        assert_eq!(view["user"]["id"], session.user.id);
        assert_eq!(
            view["host_authority"]["host"]["host_id"],
            result["source_identity"]["host_id"]
        );
    }
    assert_eq!(lock(&state.0.db).total_changes(), before);
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/categories",
            Some(&owner.token),
            json!({"name":"must not exist"})
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert!(lock(&state.0.db)
        .execute("INSERT INTO categories VALUES('forged','forged',1)", [])
        .is_err());
    assert!(state.restore_from(&backup).is_err());
    assert_eq!(
        request(&app, "GET", "/health", None, Value::Null).await.0,
        StatusCode::OK
    );
    assert_eq!(
        request(&app, "GET", ROOT, Some(&owner.token), Value::Null)
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        request(
            &app,
            "POST",
            &format!("{ROOT}/cancel"),
            Some(&owner.token),
            change
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/categories",
            Some(&owner.token),
            json!({"name":"after cancel"})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        lock(&state.0.db)
            .query_row("SELECT count(*) FROM users", [], |r| r.get::<_, u32>(0))
            .unwrap(),
        2
    );
}

#[tokio::test]
async fn barrier_survives_restart_and_an_old_database_cannot_remove_it() {
    let (temp, state, app, owner, target, change) = accepted().await;
    accept(&app, &target, &change).await;
    let old = temp.path().join("old.sqlite3");
    state.backup_to(&old).unwrap();
    assert_eq!(
        request(
            &app,
            "POST",
            &format!("{ROOT}/freeze"),
            Some(&owner.token),
            change.clone()
        )
        .await
        .0,
        StatusCode::OK
    );
    let host = state.0.handover.status().source_host_id;
    drop(app);
    drop(state);
    // Simulate an operator putting an older DB back. Its data does not contain
    // the local authority journal, so the preparation still fences this host.
    fs::copy(old, temp.path().join("relay.sqlite3")).unwrap();
    let state = AppState::open(temp.path(), "handover", 0).unwrap();
    assert_eq!(state.0.handover.status().source_host_id, host);
    assert!(!state.0.handover.writes_allowed());
    let app = router(state.clone());
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/categories",
            Some(&owner.token),
            json!({"name":"stale host"})
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        lock(&state.0.db)
            .pragma_query_value(None, "query_only", |r| r.get::<_, u32>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        request(
            &app,
            "POST",
            &format!("{ROOT}/cancel"),
            Some(&owner.token),
            change
        )
        .await
        .0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn queued_http_mutation_checks_barrier_after_waiting_for_maintenance() {
    let (_temp, state, app, owner, target, change) = accepted().await;
    accept(&app, &target, &change).await;
    let guard = state.0.maintenance.clone().read_owned().await;
    let freeze_path = format!("{ROOT}/freeze");
    let freeze = request(
        &app,
        "POST",
        &freeze_path,
        Some(&owner.token),
        change.clone(),
    );
    tokio::pin!(freeze);
    assert!(futures_util::poll!(&mut freeze).is_pending());
    let mutation = request(
        &app,
        "POST",
        "/api/categories",
        Some(&owner.token),
        json!({"name":"queued"}),
    );
    tokio::pin!(mutation);
    assert!(futures_util::poll!(&mut mutation).is_pending());
    drop(guard);
    assert_eq!(freeze.await.0, StatusCode::OK);
    assert_eq!(mutation.await.0, StatusCode::CONFLICT);
    assert_eq!(
        lock(&state.0.db)
            .query_row(
                "SELECT count(*) FROM categories WHERE name='queued'",
                [],
                |r| r.get::<_, u32>(0)
            )
            .unwrap(),
        0
    );
}

#[test]
fn malformed_or_misbound_authority_never_becomes_a_new_active_host() {
    let temp = tempfile::tempdir().unwrap();
    let server = id();
    let authority = Authority::open(temp.path(), server.clone()).unwrap();
    let host = authority.status().source_host_id;
    assert_eq!(
        Authority::open(temp.path(), server.clone())
            .unwrap()
            .status()
            .source_host_id,
        host
    );
    assert!(Authority::open(temp.path(), id()).is_err());
    fs::write(temp.path().join(JOURNAL), b"broken").unwrap();
    assert!(Authority::open(temp.path(), server).is_err());
    assert!(!target_url("http://192.168.1.5/"));
    assert!(!target_url("https://name:secret@example.com/"));
    assert!(!target_url("https://example.com/other"));
}

#[tokio::test]
async fn normal_authorized_restore_updates_binding_and_cancels_unfrozen_offer() {
    let (temp, state, app, _owner, _target, _change) = accepted().await;
    let other_dir = tempfile::tempdir().unwrap();
    let other = AppState::open(other_dir.path(), "other", 0).unwrap();
    let other_id = other.server_id().unwrap();
    let backup = temp.path().join("other.sqlite3");
    other.backup_to(&backup).unwrap();
    state.restore_from(&backup).unwrap();
    assert_eq!(state.0.handover.status().server_id, other_id);
    assert!(state.0.handover.status().preparation.is_none());
    drop(app);
    drop(state);
    assert_eq!(
        AppState::open(temp.path(), "restored", 0)
            .unwrap()
            .server_id()
            .unwrap(),
        other_id
    );
}

#[tokio::test]
async fn failed_offer_storage_does_not_freeze_an_active_community() {
    let (temp, state, app, owner, target, change) = accepted().await;
    assert_eq!(
        request(
            &app,
            "POST",
            &format!("{ROOT}/cancel"),
            Some(&owner.token),
            change
        )
        .await
        .0,
        StatusCode::OK
    );
    let journal = temp.path().join(JOURNAL);
    let original = fs::read(&journal).unwrap();
    fs::remove_file(&journal).unwrap();
    fs::create_dir(&journal).unwrap(); // deterministic failure of atomic replacement
    let (code, _) = request(
        &app,
        "POST",
        &format!("{ROOT}/offer"),
        Some(&owner.token),
        json!({"server_id":state.server_id().unwrap(),"target_user_id":target.user.id,
            "target_host_id":id(),"target_url":"https://new-host.example/"}),
    )
    .await;
    assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
    assert!(state.0.handover.writes_allowed());
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/categories",
            Some(&owner.token),
            json!({"name":"still working"})
        )
        .await
        .0,
        StatusCode::OK
    );
    fs::remove_dir(&journal).unwrap();
    fs::write(journal, original).unwrap();
}

#[tokio::test]
async fn restore_intent_must_be_durable_before_either_restore_entry_changes_data() {
    let (temp, state, app, owner, _target, _change) = accepted().await;
    let original_id = state.server_id().unwrap();
    let other_dir = tempfile::tempdir().unwrap();
    let other = AppState::open(other_dir.path(), "other", 0).unwrap();
    let backup_id = id();
    fs::create_dir_all(temp.path().join("backups")).unwrap();
    let backup = temp
        .path()
        .join("backups")
        .join(format!("{backup_id}.sqlite3"));
    other.backup_to(&backup).unwrap();
    let journal = temp.path().join(JOURNAL);
    let original = fs::read(&journal).unwrap();
    fs::remove_file(&journal).unwrap();
    fs::create_dir(&journal).unwrap();
    assert!(state.restore_from(&backup).is_err());
    assert_eq!(state.server_id().unwrap(), original_id);
    let (code, _) = request(
        &app,
        "POST",
        "/api/admin/restore",
        Some(&owner.token),
        json!({"backup_id":backup_id,"confirm_server_id":original_id}),
    )
    .await;
    assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(state.server_id().unwrap(), original_id);
    assert_eq!(
        lock(&state.0.db)
            .query_row("SELECT count(*) FROM users", [], |r| r.get::<_, u32>(0))
            .unwrap(),
        2
    );
    assert!(state.0.handover.writes_allowed());
    fs::remove_dir(&journal).unwrap();
    fs::write(journal, original).unwrap();
}

#[test]
fn interrupted_restore_reconciles_only_its_two_known_database_identities() {
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open(temp.path(), "old", 0).unwrap();
    let old_id = state.server_id().unwrap();
    let host_id = state.0.handover.status().source_host_id;
    let other_dir = tempfile::tempdir().unwrap();
    let other = AppState::open(other_dir.path(), "new", 0).unwrap();
    let new_id = other.server_id().unwrap();
    let backup = temp.path().join("restore.sqlite3");
    other.backup_to(&backup).unwrap();
    state.0.handover.begin_restore(new_id.clone()).unwrap();
    assert!(!state.0.handover.writes_allowed());
    drop(state); // interruption before DB commit: original identity wins
    let state = AppState::open(temp.path(), "old", 0).unwrap();
    assert_eq!(state.server_id().unwrap(), old_id);
    assert!(state.0.handover.writes_allowed());
    state.0.handover.begin_restore(new_id.clone()).unwrap();
    restore_database(&mut lock(&state.0.db), &backup).unwrap();
    drop(state); // interruption after DB commit, before final journal rename
    let state = AppState::open(temp.path(), "new", 0).unwrap();
    assert_eq!(state.server_id().unwrap(), new_id);
    assert_eq!(state.0.handover.status().source_host_id, host_id);
    assert!(state.0.handover.writes_allowed());
}

#[test]
fn missing_or_foreign_journal_does_not_reactivate_a_marked_database() {
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open(temp.path(), "original", 0).unwrap();
    let copy_dir = tempfile::tempdir().unwrap();
    state
        .backup_to(&copy_dir.path().join("relay.sqlite3"))
        .unwrap();
    assert!(AppState::open(copy_dir.path(), "copy", 0).is_err());
    assert!(!copy_dir.path().join(JOURNAL).exists());

    // An unrelated journal for the same community must not replace its host.
    let foreign = Authority::open(copy_dir.path(), state.server_id().unwrap()).unwrap();
    drop(foreign);
    assert!(AppState::open(copy_dir.path(), "foreign", 0).is_err());

    drop(state);
    fs::remove_file(temp.path().join(JOURNAL)).unwrap();
    assert!(AppState::open(temp.path(), "original", 0).is_err());
    assert!(!temp.path().join(JOURNAL).exists());
}

#[tokio::test]
async fn frozen_host_refuses_schema_upgrade_before_running_migrations() {
    let (temp, state, app, owner, target, change) = accepted().await;
    accept(&app, &target, &change).await;
    let (code, _) = request(
        &app,
        "POST",
        &format!("{ROOT}/freeze"),
        Some(&owner.token),
        change,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    drop(app);
    drop(state);
    let path = temp.path().join("relay.sqlite3");
    let db = Connection::open(&path).unwrap();
    db.pragma_update(None, "user_version", SCHEMA_VERSION - 1)
        .unwrap();
    drop(db);
    assert!(AppState::open(temp.path(), "upgrade", 0).is_err());
    let db = Connection::open(&path).unwrap();
    assert_eq!(
        db.pragma_query_value(None, "user_version", |r| r.get::<_, u32>(0))
            .unwrap(),
        SCHEMA_VERSION - 1
    );
    assert_eq!(
        fs::read_dir(temp.path().join("backups"))
            .map(|d| d.count())
            .unwrap_or(0),
        0
    );
}

#[tokio::test]
async fn freeze_closes_existing_event_socket_even_when_immediately_cancelled() {
    use futures_util::StreamExt;
    use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};
    let (_temp, state, app, owner, target, change) = accepted().await;
    accept(&app, &target, &change).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let served = app.clone();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            served.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });
    let mut handshake = format!("ws://{address}/api/events")
        .into_client_request()
        .unwrap();
    handshake.headers_mut().insert(
        header::AUTHORIZATION,
        format!("Bearer {}", owner.token).parse().unwrap(),
    );
    let (mut socket, _) = tokio_tungstenite::connect_async(handshake).await.unwrap();
    let old_transport = state.0.handover.watch_transports();
    assert_eq!(
        request(
            &app,
            "POST",
            &format!("{ROOT}/freeze"),
            Some(&owner.token),
            change.clone()
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        request(
            &app,
            "POST",
            &format!("{ROOT}/cancel"),
            Some(&owner.token),
            change
        )
        .await
        .0,
        StatusCode::OK
    );
    assert!(state.0.handover.writes_allowed());
    assert!(old_transport.has_changed().unwrap());
    assert!(!state.0.handover.watch_transports().has_changed().unwrap());
    let closed = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match socket.next().await {
                Some(Ok(Message::Close(_))) | None => return true,
                Some(Err(_)) => return true,
                _ => {}
            }
        }
    })
    .await;
    server.abort();
    assert_eq!(closed.unwrap(), true);
}

#[tokio::test]
async fn destination_proof_is_bound_to_the_exact_offer_source_term_and_address() {
    let (temp, state, app, _owner, target, change) = accepted().await;
    let status = state.0.handover.status();
    let mut alternatives = Vec::new();
    let mut other = status.clone();
    other.term += 1;
    alternatives.push(other);
    let mut other = status.clone();
    other.server_id = id();
    alternatives.push(other);
    let mut other = status.clone();
    other.source_host_id = id();
    alternatives.push(other);
    let mut other = status.clone();
    other.preparation.as_mut().unwrap().id = id();
    alternatives.push(other);
    let mut other = status.clone();
    other.preparation.as_mut().unwrap().target_url = "https://wrong-host.example/".into();
    alternatives.push(other);
    let mut other = status.clone();
    other.preparation.as_mut().unwrap().target_user_id = id();
    alternatives.push(other);
    let mut other = status.clone();
    other.preparation.as_mut().unwrap().offered_by = id();
    alternatives.push(other);
    for other in alternatives {
        let mut answer = change.clone();
        answer["accept_full_server_data"] = json!(true);
        answer["target_proof"] = json!(fixture_proof(&other));
        assert_eq!(
            request(
                &app,
                "POST",
                &format!("{ROOT}/answer"),
                Some(&target.token),
                answer
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(
        state.0.handover.status().preparation.unwrap().phase,
        Phase::Offered
    );
    accept(&app, &target, &change).await;
    let expected = crate::handover_target::identity(&fixture_key().verifying_key());
    assert_eq!(
        state.0.handover.status().target_identity,
        Some(expected.clone())
    );
    drop(app);
    drop(state);
    let state = AppState::open(temp.path(), "restart", 0).unwrap();
    assert_eq!(state.0.handover.status().target_identity, Some(expected));
}

#[tokio::test]
async fn full_data_consent_without_destination_key_cannot_freeze_the_source() {
    let (_temp, state, app, owner, target, change) = accepted().await;
    let mut answer = change.clone();
    answer["accept_full_server_data"] = json!(true);
    assert_eq!(
        request(
            &app,
            "POST",
            &format!("{ROOT}/answer"),
            Some(&target.token),
            answer
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        request(
            &app,
            "POST",
            &format!("{ROOT}/freeze"),
            Some(&owner.token),
            change
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert!(state.0.handover.writes_allowed());
    assert!(state.0.handover.status().target_identity.is_none());
}

#[test]
fn an_unknown_newer_database_is_refused_before_journal_key_or_header_changes() {
    let data = tempfile::tempdir().unwrap();
    let path = data.path().join("relay.sqlite3");
    let mut db = Connection::open(&path).unwrap();
    migrate_database(&mut db).unwrap();
    db.pragma_update(None, "user_version", SCHEMA_VERSION + 1).unwrap();
    drop(db);
    let original = std::fs::read(&path).unwrap();
    assert!(AppState::open(data.path(), "newer format", 0).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), original);
    assert!(!data.path().join("host-authority.json").exists());
    assert!(!data.path().join("handover-source").exists());
}

#[tokio::test]
async fn public_route_proof_is_challenge_bound_and_does_not_publish_an_unfinished_offer() {
    let (_temp, state, app, _owner, target, change) = accepted().await;
    let challenge = URL_SAFE_NO_PAD.encode([72; 32]);
    let path = "/api/hosting/handover/route";
    let (code, value) = request(&app, "POST", path, None, json!({"challenge":challenge})).await;
    assert_eq!(code, StatusCode::OK, "{value}");
    let proof: model::RouteProof = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(proof.state.pin, state.0.handover.route_pin());
    assert_eq!(proof.state.challenge, challenge);
    assert!(proof.state.writes_allowed);
    assert!(proof.state.moved.is_none());
    assert!(crate::handover_target::verify(
        &model::HostProof {
            identity: proof.state.pin.host.clone(),
            signature: proof.signature.clone(),
        },
        &model::route_message(&proof.state)
    ));
    assert!(!value.to_string().contains(&target.user.id));
    assert!(!value.to_string().contains("https://new-host.example"));
    let mut replay = proof.state.clone();
    replay.challenge = URL_SAFE_NO_PAD.encode([73; 32]);
    assert!(!crate::handover_target::verify(
        &model::HostProof {
            identity: proof.state.pin.host,
            signature: proof.signature,
        },
        &model::route_message(&replay)
    ));
    assert_eq!(
        request(&app, "POST", path, None, json!({"challenge":"short"}))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    accept(&app, &target, &change).await;
    assert_eq!(
        request(
            &app,
            "POST",
            &format!("{ROOT}/freeze"),
            Some(&_owner.token),
            change
        )
        .await
        .0,
        StatusCode::OK
    );
    let (code, value) = request(&app, "POST", path, None, json!({"challenge":challenge})).await;
    assert_eq!(code, StatusCode::OK, "{value}");
    assert_eq!(value["state"]["writes_allowed"], false);
    assert!(value["state"]["moved"].is_null());
}

#[tokio::test]
async fn an_accepted_data_copy_member_can_read_move_status_before_any_offer() {
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open(temp.path(), "handover", 0).unwrap();
    let app = router(state.clone());
    let owner = signup(&app, "Owner", "").await;
    let member = signup(&app, "Member", &new_invite(&app, &owner).await).await;
    let outsider = signup(&app, "Outsider", &new_invite(&app, &owner).await).await;
    let (code, _) = request(&app, "POST", "/api/hosting/offers", Some(&owner.token),
        json!({"user_id":member.user.id,"roles":["replica"]})).await;
    assert_eq!(code, StatusCode::OK);
    let (code, _) = request(&app, "POST", "/api/hosting/answer", Some(&member.token),
        json!({"accepted":["replica"]})).await;
    assert_eq!(code, StatusCode::OK);
    let (code, status) = request(&app, "GET", ROOT, Some(&member.token), Value::Null).await;
    assert_eq!(code, StatusCode::OK, "{status}");
    assert!(status["preparation"].is_null());
    let (code, _) = request(&app, "GET", ROOT, Some(&outsider.token), Value::Null).await;
    assert_eq!(code, StatusCode::FORBIDDEN);
}
