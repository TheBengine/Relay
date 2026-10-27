use super::*;
use crate::tests::request;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

struct Fixture {
    source: tempfile::TempDir,
    destination: tempfile::TempDir,
    state: AppState,
    app: Router,
    owner: Session,
    member: Session,
    status: model::Status,
    input: model::ExportRequest,
}

async fn prepare() -> Fixture {
    prepare_with_rtc(None).await
}

async fn prepare_with_rtc(rtc: Option<crate::rtc::RtcConfig>) -> Fixture {
    let (source, state, app, owner, member, old) = crate::handover::tests::accepted().await;
    let (state, app) = if let Some(rtc) = rtc {
        drop(app);
        drop(state);
        let state = AppState::open_with_config(
            source.path(),
            "handover",
            0,
            ServerConfig {
                rtc: Some(rtc),
                ..Default::default()
            },
        )
        .unwrap();
        (state.clone(), router(state))
    } else {
        (state, app)
    };
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/hosting/handover/cancel",
            Some(&owner.token),
            old
        )
        .await
        .0,
        StatusCode::OK
    );
    let destination = tempfile::tempdir().unwrap();
    let key = TargetKey::open(destination.path()).unwrap();
    let (code,value)=request(&app,"POST","/api/hosting/handover/offer",Some(&owner.token),json!({"server_id":state.server_id().unwrap(),"target_user_id":member.user.id,"target_host_id":key.identity().host_id,"target_url":"https://destination.example/"})).await;
    assert_eq!(code, StatusCode::OK);
    let offered: model::Status = serde_json::from_value(value).unwrap();
    let change = model::Change {
        server_id: offered.server_id.clone(),
        preparation_id: offered.preparation.as_ref().unwrap().id.clone(),
    };
    let answer = model::Answer {
        server_id: change.server_id.clone(),
        preparation_id: change.preparation_id.clone(),
        accept_full_server_data: true,
        target_proof: Some(key.accept(&offered).unwrap()),
    };
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/hosting/handover/answer",
            Some(&member.token),
            json!(answer)
        )
        .await
        .0,
        StatusCode::OK
    );
    // State deliberately outside the member-copy allowlist, spanning >1 part.
    let db = lock(&state.0.db);
    db.execute("CREATE TABLE full_handover_only(secret BLOB)", [])
        .unwrap();
    db.execute(
        "INSERT INTO full_handover_only(secret) VALUES(?1)",
        [vec![0x31u8; 2 * PART + 37]],
    )
    .unwrap();
    db.execute(
        "INSERT INTO metadata(key,value) VALUES('private-handover-fixture','complete state')",
        [],
    )
    .unwrap();
    drop(db);
    let (code, value) = request(
        &app,
        "POST",
        "/api/hosting/handover/freeze",
        Some(&owner.token),
        json!(change),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{value}");
    let status: model::Status = serde_json::from_value(value).unwrap();
    let input = model::ExportRequest {
        change,
        recipient: key.recipient(&status).unwrap(),
        retry: false,
    };
    Fixture {
        source,
        destination,
        state,
        app,
        owner,
        member,
        status,
        input,
    }
}

async fn ready(f: &Fixture) -> TransferManifest {
    for _ in 0..100 {
        let (code, value) = request(
            &f.app,
            "POST",
            "/api/hosting/handover/export",
            Some(&f.member.token),
            json!(f.input),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{value}");
        match serde_json::from_value::<ExportStatus>(value).unwrap() {
            ExportStatus::Ready { manifest } => return manifest,
            ExportStatus::Failed => panic!("Encrypted transfer build failed"),
            ExportStatus::Building => tokio::time::sleep(Duration::from_millis(20)).await,
        }
    }
    panic!("Transfer did not finish")
}
async fn download(
    app: &Router,
    token: &str,
    input: &model::ExportRequest,
    manifest: &TransferManifest,
    index: usize,
) -> (StatusCode, Vec<u8>) {
    let body = json!(model::PartRequest {
        request: input.clone(),
        export_id: manifest.context.export_id.clone(),
        index
    });
    let response = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/api/hosting/handover/part")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let code = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    (code, bytes)
}

#[tokio::test]
async fn complete_image_is_encrypted_resumable_after_source_restart_and_received_only_as_standby() {
    let f = prepare().await;
    let manifest = ready(&f).await;
    assert!(manifest.parts.len() >= 3);
    let original_hash: String = lock(&f.state.0.db)
        .query_row(
            "SELECT password_hash FROM users WHERE id=?1",
            [&f.owner.user.id],
            |r| r.get(0),
        )
        .unwrap();
    let cache = tempfile::tempdir().unwrap();
    fs::write(
        cache.path().join("manifest.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    let (code, bytes) = download(&f.app, &f.member.token, &f.input, &manifest, 0).await;
    assert_eq!(code, StatusCode::OK);
    fs::write(cache.path().join("0.hpke"), &bytes).unwrap();
    let other = tempfile::tempdir().unwrap();
    let wrong = TargetKey::open(other.path()).unwrap();
    let wrong_key = wrong.encryption_keys().unwrap();
    assert!(hpke()
        .open(
            &bytes[..32],
            wrong_key.private_key(),
            INFO,
            &aad(&manifest.context, 0),
            &bytes[32..],
            None,
            None,
            None
        )
        .is_err());
    let key = TargetKey::open(f.destination.path()).unwrap();
    assert!(receive(f.destination.path(), cache.path(), &f.status, &key).is_err()); // unfinished download
                                                                                    // Restart must keep the complete current ciphertext but erase plaintext
                                                                                    // remnants and a bound partial export belonging to an earlier preparation.
    fs::write(
        folder(f.source.path(), &f.input.change.preparation_id).join("snapshot.sqlite3"),
        b"interrupted plaintext",
    )
    .unwrap();
    let obsolete_id = id();
    let obsolete = folder(f.source.path(), &obsolete_id);
    fs::create_dir(&obsolete).unwrap();
    fs::write(
        obsolete.join("binding.json"),
        serde_json::to_vec(&(obsolete_id, &f.input.recipient.public_key)).unwrap(),
    )
    .unwrap();
    fs::write(obsolete.join("snapshot.sqlite3"), b"obsolete plaintext").unwrap();
    let foreign = root(f.source.path()).join("foreign-content");
    fs::create_dir(&foreign).unwrap();
    fs::write(foreign.join("keep"), b"not an export").unwrap();
    let Fixture {
        source,
        destination,
        state,
        app,
        member,
        input,
        status,
        ..
    } = f;
    drop(app);
    drop(state);
    let restarted = AppState::open(source.path(), "restarted", 0).unwrap();
    assert!(!obsolete.exists());
    assert_eq!(fs::read(foreign.join("keep")).unwrap(), b"not an export");
    let app = router(restarted);
    let (code, value) = request(
        &app,
        "POST",
        "/api/hosting/handover/export",
        Some(&member.token),
        json!(input),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let ExportStatus::Ready { manifest: again } = serde_json::from_value(value).unwrap() else {
        panic!("Export lost on restart")
    };
    assert_eq!(again.context.export_id, manifest.context.export_id);
    for i in (1..manifest.parts.len()).rev() {
        let (code, bytes) = download(&app, &member.token, &input, &manifest, i).await;
        assert_eq!(code, StatusCode::OK);
        fs::write(cache.path().join(format!("{i}.hpke")), bytes).unwrap();
    }
    let report = receive(destination.path(), cache.path(), &status, &key).unwrap();
    assert!(!report.activation_allowed);
    let dir = destination
        .path()
        .join("handover-received")
        .join(&report.preparation_id);
    let db = Connection::open_with_flags(
        dir.join("relay.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    assert_eq!(
        db.query_row("SELECT length(secret) FROM full_handover_only", [], |r| {
            r.get::<_, usize>(0)
        })
        .unwrap(),
        2 * PART + 37
    );
    assert_eq!(
        db.query_row(
            "SELECT value FROM metadata WHERE key='private-handover-fixture'",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "complete state"
    );
    assert_eq!(
        db.query_row(
            "SELECT password_hash FROM users WHERE id=?1",
            [&status.preparation.as_ref().unwrap().offered_by],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        crate::host_copy_secrets::NOT_COPIED
    );
    assert_ne!(original_hash, crate::host_copy_secrets::NOT_COPIED);
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert!(crate::host_copy_secrets::is_stripped(&db).unwrap());
    drop(db);
    assert!(AppState::open(&dir, "must not activate", 0).is_err());
    assert_eq!(
        receive(destination.path(), cache.path(), &status, &key).unwrap(),
        report
    );
    // Recover the publication/receipt crash checkpoint without overwriting DB.
    fs::remove_file(dir.join("received.json")).unwrap();
    let expected = serde_json::to_vec(&report).unwrap();
    fs::write(dir.join("received.json.pending"), &expected[..13]).unwrap();
    assert_eq!(
        receive(destination.path(), cache.path(), &status, &key).unwrap(),
        report
    );
    // A cached retry may not change the received runtime/account configuration.
    let mut altered = manifest.clone();
    altered.context.runtime.name = "changed on retry".into();
    fs::write(
        cache.path().join("manifest.json"),
        serde_json::to_vec(&altered).unwrap(),
    )
    .unwrap();
    assert!(receive(destination.path(), cache.path(), &status, &key).is_err());
    assert!(!folder(source.path(), &input.change.preparation_id)
        .join("snapshot.sqlite3")
        .exists());
}

#[tokio::test]
async fn export_refuses_other_members_changed_encryption_key_and_cancelled_preparation() {
    let f = prepare().await;
    assert_eq!(
        request(
            &f.app,
            "POST",
            "/api/hosting/handover/export",
            Some(&f.owner.token),
            json!(f.input)
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let mut bad = f.input.clone();
    bad.recipient.public_key = URL_SAFE_NO_PAD.encode([9; 32]);
    assert_eq!(
        request(
            &f.app,
            "POST",
            "/api/hosting/handover/export",
            Some(&f.member.token),
            json!(bad)
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let manifest = ready(&f).await;
    let cache = tempfile::tempdir().unwrap();
    fs::write(
        cache.path().join("manifest.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    for index in 0..manifest.parts.len() {
        let (code, mut bytes) = download(&f.app, &f.member.token, &f.input, &manifest, index).await;
        assert_eq!(code, StatusCode::OK);
        if index == 1 {
            *bytes.last_mut().unwrap() ^= 1;
        }
        fs::write(cache.path().join(format!("{index}.hpke")), bytes).unwrap();
    }
    let key = TargetKey::open(f.destination.path()).unwrap();
    assert!(receive(f.destination.path(), cache.path(), &f.status, &key).is_err());
    // HPKE Base alone allows anybody to encrypt to the public recipient key.
    // Even an otherwise internally consistent forged image must carry the
    // source key pinned in this destination's original explicit consent.
    let mut forged = manifest.clone();
    forged.context.runtime.name = "forged source image".into();
    let attacker = ed25519_dalek::SigningKey::from_bytes(&[93; 32]);
    forged.context.source_identity = crate::handover_target::identity(&attacker.verifying_key());
    use ed25519_dalek::Signer;
    forged.signature = URL_SAFE_NO_PAD.encode(
        attacker
            .sign(&manifest_message(&forged.context, &forged.parts))
            .to_bytes(),
    );
    let mut forged_status = f.status.clone();
    forged_status.source_identity = Some(forged.context.source_identity.clone());
    fs::write(
        cache.path().join("manifest.json"),
        serde_json::to_vec(&forged).unwrap(),
    )
    .unwrap();
    assert!(receive(f.destination.path(), cache.path(), &forged_status, &key).is_err());
    // Reusing the genuine source identity/signature cannot sign changed data.
    forged.context.source_identity = manifest.context.source_identity.clone();
    forged.signature = manifest.signature.clone();
    fs::write(
        cache.path().join("manifest.json"),
        serde_json::to_vec(&forged).unwrap(),
    )
    .unwrap();
    assert!(receive(f.destination.path(), cache.path(), &f.status, &key).is_err());
    let output = f
        .destination
        .path()
        .join("handover-received")
        .join(&f.input.change.preparation_id);
    assert!(!output.join("relay.sqlite3").exists());
    assert_eq!(
        request(
            &f.app,
            "POST",
            "/api/hosting/handover/cancel",
            Some(&f.owner.token),
            json!(f.input.change)
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_ne!(
        download(&f.app, &f.member.token, &f.input, &manifest, 0)
            .await
            .0,
        StatusCode::OK
    );
    assert!(!folder(f.source.path(), &f.input.change.preparation_id).exists());
}

#[tokio::test]
async fn cancellation_in_final_publish_window_cleans_a_successful_build() {
    let f = prepare().await;
    let owner = f.owner.token.clone();
    let change = f.input.change.clone();
    *lock(&f.state.0.handover_exports.before_publish) = Some(Box::new(move |state| {
        let (code, _) = tokio::runtime::Handle::current().block_on(request(
            &router(state.clone()),
            "POST",
            "/api/hosting/handover/cancel",
            Some(&owner),
            json!(change),
        ));
        assert_eq!(code, StatusCode::OK);
    }));
    assert_eq!(
        request(
            &f.app,
            "POST",
            "/api/hosting/handover/export",
            Some(&f.member.token),
            json!(f.input)
        )
        .await
        .0,
        StatusCode::OK
    );
    for _ in 0..100 {
        if lock(&f.state.0.handover_exports.job).is_none() {
            assert!(!folder(f.source.path(), &f.input.change.preparation_id).exists());
            assert!(f.state.0.handover.status().writes_allowed);
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("Cancelled worker did not stop");
}

#[tokio::test]
async fn missing_pinned_source_key_does_not_generate_a_new_identity() {
    let f = prepare().await;
    let path = f.source.path().join("handover-source/signing-key.v1");
    let original = fs::read(&path).unwrap();
    let Fixture {
        source, state, app, ..
    } = f;
    drop(app);
    drop(state);
    fs::remove_file(&path).unwrap();
    assert!(AppState::open(source.path(), "missing key", 0).is_err());
    assert!(!path.exists());
    fs::write(&path, &original).unwrap();
    #[cfg(unix)] {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    assert!(AppState::open(source.path(), "restored key", 0).is_ok());
}

async fn receive_and_attest(f: &Fixture) -> model::Receipt {
    let m = ready(f).await;
    let cache = tempfile::tempdir().unwrap();
    fs::write(
        cache.path().join("manifest.json"),
        serde_json::to_vec(&m).unwrap(),
    )
    .unwrap();
    for index in 0..m.parts.len() {
        let (code, bytes) = download(&f.app, &f.member.token, &f.input, &m, index).await;
        assert_eq!(code, StatusCode::OK);
        fs::write(cache.path().join(format!("{index}.hpke")), bytes).unwrap();
    }
    let key = TargetKey::open(f.destination.path()).unwrap();
    receive(f.destination.path(), cache.path(), &f.status, &key).unwrap();
    crate::handover::activation::attest(f.destination.path(), &f.status, &key).unwrap()
}
async fn checked_destination(
    f: &Fixture,
) -> (crate::handover_readiness::Prepared, model::ReadyProof) {
    let (code, challenge) = request(
        &f.app,
        "POST",
        "/api/hosting/handover/readiness",
        Some(&f.owner.token),
        json!(f.input.change),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{challenge}");
    let waiting = crate::handover_readiness::Prepared::bind(
        f.destination.path(),
        f.state.0.handover.status(),
        "127.0.0.1".parse().unwrap(),
        0,
        0,
        None,
        zeroize::Zeroizing::new("owned-readiness-control".repeat(3)),
    )
    .await
    .unwrap();
    let (code, proof) = request(
        &waiting.router(),
        "POST",
        "/api/hosting/handover/target",
        None,
        challenge,
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{proof}");
    (waiting, serde_json::from_value(proof).unwrap())
}

#[tokio::test]
async fn readiness_keeps_ports_and_checks_fresh_source_consent_before_irreversible_retirement() {
    let f = prepare().await;
    let receipt = receive_and_attest(&f).await;
    assert_eq!(
        request(
            &f.app,
            "POST",
            "/api/hosting/handover/receipt",
            Some(&f.member.token),
            json!(receipt)
        )
        .await
        .0,
        StatusCode::OK
    );
    f.state.0.handover.elapse_media_leases();
    let mut retire = json!(model::Retire {
        change: f.input.change.clone(),
        confirm_stop_source: true,
        readiness: None
    });
    assert_eq!(
        request(
            &f.app,
            "POST",
            "/api/hosting/handover/readiness",
            Some(&f.member.token),
            json!(f.input.change)
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            &f.app,
            "POST",
            "/api/hosting/handover/retire",
            Some(&f.owner.token),
            retire.clone()
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    let (waiting, proof) = checked_destination(&f).await;
    let peer_secret="standby-private-ingress-01234567890123456789";
    let peer=crate::handover_readiness::Peer::bind(0,zeroize::Zeroizing::new(peer_secret.into())).await.unwrap();
    let peer_port=peer.http.local_addr().unwrap().port();
    let waiting=waiting.with_peer(Some(peer));
    assert!(tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST,peer_port)).await.is_err());
    let ingress=waiting.peer_router().unwrap();
    for (secret,peer,path,expected) in [
        ("wrong","100.64.0.2","/health",StatusCode::UNAUTHORIZED),
        (peer_secret,"127.0.0.1","/health",StatusCode::UNAUTHORIZED),
        (peer_secret,"100.64.0.2","/api/local/handover/activate",StatusCode::FORBIDDEN),
        (peer_secret,"100.64.0.2","/health",StatusCode::OK),
    ] {
        use tower::ServiceExt;
        let req=axum::http::Request::builder().uri(path)
            .header("x-relay-network-ingress",secret).header("x-relay-network-peer",peer)
            .extension(axum::extract::ConnectInfo("127.0.0.1:31000".parse::<std::net::SocketAddr>().unwrap()))
            .body(axum::body::Body::empty()).unwrap();
        assert_eq!(ingress.clone().oneshot(req).await.unwrap().status(),expected);
    }
    let report = waiting.report();
    assert!(tokio::net::TcpListener::bind((
        std::net::Ipv4Addr::LOCALHOST,
        report["http_port"].as_u64().unwrap() as u16
    ))
    .await
    .is_err());
    assert!(tokio::net::UdpSocket::bind((
        std::net::Ipv4Addr::LOCALHOST,
        report["voice_port"].as_u64().unwrap() as u16
    ))
    .await
    .is_err());
    let routes = waiting.router();
    // Knowing the public preparation fields never authorizes a new check.
    // Even a source-signed request must have a short, unexpired deadline.
    for change in 0..6 {
        let mut forged = proof.body.request.clone();
        match change {
            0 => forged.signature.clear(),
            1 => forged.challenge = URL_SAFE_NO_PAD.encode([42_u8; 32]),
            2 => forged.receipt_sha256 = "a".repeat(64),
            3 => forged.expires_at = now().saturating_sub(1),
            4 => forged.expires_at = now().saturating_add(3600),
            _ => forged.signature = TargetKey::open(f.destination.path()).unwrap()
                .sign(&model::readiness_challenge_message(&forged)),
        }
        if matches!(change, 3 | 4) {
            forged.signature = f.state.0.handover.sign(&model::readiness_challenge_message(&forged));
        }
        assert_eq!(request(&routes, "POST", "/api/hosting/handover/target", None, json!(forged)).await.0,
            StatusCode::CONFLICT, "source request mutation {change}");
    }
    assert_eq!(
        request(
            &routes,
            "GET",
            "/api/bootstrap",
            Some(&f.owner.token),
            serde_json::Value::Null
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        request(&routes, "GET", "/health", None, serde_json::Value::Null)
            .await
            .1["writable"],
        false
    );
    let mut changed = proof.clone();
    changed.body.rtc_ready = true;
    retire["readiness"] = json!(changed);
    assert_eq!(
        request(
            &f.app,
            "POST",
            "/api/hosting/handover/retire",
            Some(&f.owner.token),
            retire.clone()
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    // A newer challenge invalidates the older proof without clock assumptions.
    let (_, challenge) = request(
        &f.app,
        "POST",
        "/api/hosting/handover/readiness",
        Some(&f.owner.token),
        json!(f.input.change),
    )
    .await;
    retire["readiness"] = json!(proof);
    assert_eq!(
        request(
            &f.app,
            "POST",
            "/api/hosting/handover/retire",
            Some(&f.owner.token),
            retire.clone()
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    let (_, fresh) = request(
        &routes,
        "POST",
        "/api/hosting/handover/target",
        None,
        challenge,
    )
    .await;
    retire["readiness"] = fresh;
    let (code, grant) = request(
        &f.app,
        "POST",
        "/api/hosting/handover/retire",
        Some(&f.owner.token),
        retire,
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{grant}");
    assert_eq!(
        request(
            &routes,
            "POST",
            "/api/local/handover/activate",
            None,
            grant.clone()
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    let mut forged=grant.clone();forged["signature"]=json!("forged-source-grant");
    assert_eq!(request(&routes,"POST","/api/hosting/handover/grant",None,forged).await.0,StatusCode::CONFLICT);
    let (code, value) = request(
        &routes,
        "POST",
        "/api/hosting/handover/grant",
        None,
        grant,
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{value}");
    let running = waiting.wait().await.unwrap();
    assert_eq!(running.peer.as_ref().unwrap().http.local_addr().unwrap().port(),peer_port);
    assert_eq!(running.peer.as_ref().unwrap().token.as_str(),peer_secret);
    assert!(tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST,peer_port)).await.is_err());
    assert_eq!(
        running.http.local_addr().unwrap().port(),
        report["http_port"].as_u64().unwrap() as u16
    );
    assert_eq!(
        running.udp.local_addr().unwrap().port(),
        report["voice_port"].as_u64().unwrap() as u16
    );
    assert!(!f.state.0.handover.writes_allowed());
    assert_eq!(
        crate::handover::activation::active_runtime(&running.activation.data)
            .unwrap()
            .unwrap()
            .name,
        "handover"
    );
    let data = running.activation.data.clone();
    let term = running.activation.term;
    let db = Connection::open(data.join("relay.sqlite3")).unwrap();
    db.execute(
        "INSERT INTO metadata(key,value) VALUES('after-ready-runtime','preserved')",
        [],
    )
    .unwrap();
    drop(db);
    drop(running);
    let resumed = crate::handover_readiness::ReadyRuntime::resume(
        f.destination.path(),
        f.state.0.handover.status(),
        "127.0.0.1".parse().unwrap(),
        report["http_port"].as_u64().unwrap() as u16,
        report["voice_port"].as_u64().unwrap() as u16,
        None,
    )
    .await
    .unwrap();
    assert_eq!(resumed.activation.term, term);
    let db = Connection::open(resumed.activation.data.join("relay.sqlite3")).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT value FROM metadata WHERE key='after-ready-runtime'",
            [],
            |row| row.get::<_, String>(0)
        )
        .unwrap(),
        "preserved"
    );
}

#[tokio::test]
async fn readiness_requires_the_destinations_own_live_authenticated_video_service() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let udp = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let media_port = udp.local_addr().unwrap().port();
    let udp_echo = tokio::spawn(async move {
        let mut buffer = [0u8; 1601];
        loop {
            let (n, peer) = udp.recv_from(&mut buffer).await.unwrap();
            udp.send_to(&buffer[..n], peer).await.unwrap();
        }
    });
    let config = crate::RtcConfig::new(
        "wss://destination.example/".into(),
        format!("http://{address}"),
        "destination-only-key".into(),
        "destination-only-owned-fixture-secret".into(),
    )
    .unwrap()
    .with_media_bridge(media_port)
    .unwrap();
    let source_rtc = crate::RtcConfig::new(
        format!("ws://{address}"),
        format!("http://{address}"),
        "source-key-stays-source".into(),
        "source-secret-is-never-transferred".into(),
    )
    .unwrap();
    let live = Arc::new(AtomicBool::new(true));
    let state = (config.clone(), source_rtc.clone(), live.clone());
    let service = Router::new()
        .route(
            "/twirp/livekit.RoomService/ListRooms",
            axum::routing::post(
                |State((config, source, live)): State<(
                    crate::RtcConfig,
                    crate::RtcConfig,
                    Arc<AtomicBool>,
                )>,
                 headers: axum::http::HeaderMap| async move {
                    if !live.load(Ordering::Acquire) {
                        return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({})));
                    }
                    let token = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.strip_prefix("Bearer "))
                        .unwrap_or_default();
                    if !config
                        .verify(token)
                        .or_else(|_| source.verify(token))
                        .is_ok_and(|v| v["video"]["roomList"] == true)
                    {
                        return (StatusCode::UNAUTHORIZED, Json(json!({})));
                    }
                    (StatusCode::OK, Json(json!({"rooms":[]})))
                },
            ),
        )
        .route(
            "/rtc/v1",
            axum::routing::get(
                |State((config, _, _)): State<(
                    crate::RtcConfig,
                    crate::RtcConfig,
                    Arc<AtomicBool>,
                )>,
                 headers: axum::http::HeaderMap,
                 ws: axum::extract::WebSocketUpgrade| async move {
                    let token = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.strip_prefix("Bearer "))
                        .unwrap_or_default();
                    if !config.verify(token).is_ok_and(|v| {
                        v["purpose"] == "relay-handover-probe-signaling-v1"
                            && v["video"]["roomJoin"] == true
                    }) {
                        return Err(StatusCode::FORBIDDEN);
                    }
                    Ok::<_, StatusCode>(ws.on_upgrade(|mut ws| async move {
                        while let Some(Ok(message)) = ws.recv().await {
                            if ws.send(message).await.is_err() {
                                break;
                            }
                        }
                    }))
                },
            ),
        )
        .with_state(state);
    let task = tokio::spawn(async move { axum::serve(listener, service).await.unwrap() });
    let f = prepare_with_rtc(Some(source_rtc)).await;
    let receipt = receive_and_attest(&f).await;
    assert_eq!(
        request(
            &f.app,
            "POST",
            "/api/hosting/handover/receipt",
            Some(&f.member.token),
            json!(receipt)
        )
        .await
        .0,
        StatusCode::OK
    );
    let status = f.state.0.handover.status();
    let bind = "127.0.0.1".parse().unwrap();
    let token = || zeroize::Zeroizing::new("owned-video-ready-control".repeat(3));
    assert!(crate::handover_readiness::Prepared::bind(
        f.destination.path(),
        status.clone(),
        bind,
        0,
        0,
        None,
        token()
    )
    .await
    .is_err());
    let wrong = crate::RtcConfig::new(
        config.url.clone(),
        config.api_url.clone(),
        config.api_key.clone(),
        "wrong-owned-destination-secret".into(),
    )
    .unwrap();
    assert!(crate::handover_readiness::Prepared::bind(
        f.destination.path(),
        status.clone(),
        bind,
        0,
        0,
        Some(wrong),
        token()
    )
    .await
    .is_err());
    let waiting = crate::handover_readiness::Prepared::bind(
        f.destination.path(),
        status,
        bind,
        0,
        0,
        Some(config),
        token(),
    )
    .await
    .unwrap();
    let (_, challenge) = request(
        &f.app,
        "POST",
        "/api/hosting/handover/readiness",
        Some(&f.owner.token),
        json!(f.input.change),
    )
    .await;
    let (code, value) = request(
        &waiting.router(),
        "POST",
        "/api/hosting/handover/target",
        None,
        challenge.clone(),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{value}");
    assert_eq!(value["body"]["rtc_ready"], true);
    assert!(
        value["body"]["rtc_media_ready"].is_null(),
        "administrative health is not media readiness"
    );
    f.state.0.handover.elapse_media_leases();
    assert_eq!(
        request(
            &f.app,
            "POST",
            "/api/hosting/handover/retire",
            Some(&f.owner.token),
            json!({"change":f.input.change,"confirm_stop_source":true,"readiness":value})
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    exercise_standby_media_routes(&f, &waiting, challenge.clone()).await;
    live.store(false, Ordering::Release);
    assert_eq!(
        request(
            &waiting.router(),
            "POST",
            "/api/hosting/handover/target",
            None,
            challenge
        )
        .await
        .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert!(f.state.0.handover.status().retired.is_none());
    task.abort();
    udp_echo.abort();
    let _ = task.await;
    let _ = udp_echo.await;
}

// Component test only: a synthetic SFU signaling/UDP peer behind the real
// standby router. No native source/target processes or physical devices.
async fn exercise_standby_media_routes(
    f: &Fixture,
    waiting: &crate::handover_readiness::Prepared,
    challenge: Value,
) {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};
    let app = waiting.router();
    let (code, value) = request(
        &app,
        "POST",
        "/api/hosting/handover/rtc-ticket",
        None,
        challenge.clone(),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let ticket: model::RtcProbeTicket = serde_json::from_value(value.clone()).unwrap();
    assert!(crate::handover_target::verify(
        &model::HostProof {
            identity: ticket.body.target.clone(),
            signature: ticket.signature.clone()
        },
        &model::rtc_probe_ticket_message(&ticket.body)
    ));
    assert_eq!(
        ticket.body.request,
        serde_json::from_value::<model::ReadinessChallenge>(challenge.clone()).unwrap()
    );
    assert!(
        ticket.body.expires_at <= now() + 30
            && ticket.body.expires_at <= ticket.body.request.expires_at
    );
    assert!(ticket.body.rtc[0].room.starts_with("relay-preflight-"));
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/hosting/handover/rtc-ticket",
            None,
            challenge.clone()
        )
        .await
        .1,
        value,
        "a retry retains its exact ticket and deadline"
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let serving = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let connect = |path: &str, token: &str| {
        let mut request = format!("ws://{address}{path}")
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("authorization", format!("Bearer {token}").parse().unwrap());
        tokio_tungstenite::connect_async(request)
    };
    let signal_token = ticket.body.rtc[0].token.as_str();
    let media_token = ticket.body.rtc[0]
        .media_bridge
        .as_ref()
        .unwrap()
        .token
        .as_str();
    for (path, token) in [
        ("/rtc/v1", f.owner.token.as_str()),
        ("/rtc/v1", media_token),
        ("/api/rtc/media", signal_token),
    ] {
        let error = connect(path, token)
            .await
            .err()
            .expect("capability must not cross routes");
        assert!(
            matches!(error,tokio_tungstenite::tungstenite::Error::Http(r) if r.status()==StatusCode::FORBIDDEN)
        );
    }
    let (mut signal, _) = connect("/rtc/v1", signal_token).await.unwrap();
    signal
        .send(Message::Binary(vec![1, 2, 3].into()))
        .await
        .unwrap();
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(3), signal.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        Message::Binary(_)
    ));
    let (mut media, _) = connect("/api/rtc/media", media_token).await.unwrap();
    media
        .send(Message::Binary(vec![0, 3, 4, 5, 6].into()))
        .await
        .unwrap();
    let echoed = tokio::time::timeout(Duration::from_secs(3), media.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(matches!(echoed,Message::Binary(ref b) if b.as_ref()==[0,3,4,5,6]));
    let second = &ticket.body.rtc[1];
    let (mut second_signal, _) = connect("/rtc/v1", &second.token).await.unwrap();
    second_signal
        .send(Message::Binary(vec![7].into()))
        .await
        .unwrap();
    let _ = second_signal.next().await.unwrap().unwrap();
    let (mut second_media, _) = connect(
        "/api/rtc/media",
        &second.media_bridge.as_ref().unwrap().token,
    )
    .await
    .unwrap();
    second_media
        .send(Message::Binary(vec![0, 1, 8].into()))
        .await
        .unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(3), second_media.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let (code, proof) = request(
        &waiting.router(),
        "POST",
        "/api/hosting/handover/target",
        None,
        challenge,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(proof["body"]["rtc_media_ready"], true);
    let signed: model::ReadyProof = serde_json::from_value(proof).unwrap();
    assert!(crate::handover_target::verify(
        &model::HostProof {
            identity: signed.body.target.clone(),
            signature: signed.signature.clone()
        },
        &model::ready_message(&signed.body)
    ));
    // A new authorized check cancels both old paths and their admission tokens.
    let (_, fresh) = request(
        &f.app,
        "POST",
        "/api/hosting/handover/readiness",
        Some(&f.owner.token),
        json!(f.input.change),
    )
    .await;
    assert_eq!(
        request(
            &waiting.router(),
            "POST",
            "/api/hosting/handover/rtc-ticket",
            None,
            fresh.clone()
        )
        .await
        .0,
        StatusCode::OK
    );
    for socket in [
        &mut signal,
        &mut media,
        &mut second_signal,
        &mut second_media,
    ] {
        let closed = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                match socket.next().await {
                    None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                    _ => {}
                }
            }
        })
        .await;
        assert!(closed.is_ok(), "replaced probe streams must close");
    }
    assert!(connect("/api/rtc/media", media_token).await.is_err());
    assert!(request(
        &waiting.router(),
        "POST",
        "/api/hosting/handover/target",
        None,
        fresh
    )
    .await
    .1["body"]["rtc_media_ready"]
        .is_null());
    assert_eq!(
        request(
            &waiting.router(),
            "GET",
            "/api/bootstrap",
            Some(&f.owner.token),
            Value::Null
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    serving.abort();
    let _ = serving.await;
}

async fn grant(f: &Fixture) -> model::ActivationGrant {
    let proof = receive_and_attest(f).await;
    assert_eq!(
        request(
            &f.app,
            "POST",
            "/api/hosting/handover/receipt",
            Some(&f.member.token),
            json!(proof)
        )
        .await
        .0,
        StatusCode::OK
    );
    f.state.0.handover.elapse_media_leases();
    let (_waiting, readiness) = checked_destination(f).await;
    let (code, value) = request(
        &f.app,
        "POST",
        "/api/hosting/handover/retire",
        Some(&f.owner.token),
        json!(model::Retire {
            change: f.input.change.clone(),
            confirm_stop_source: true,
            readiness: Some(readiness),
        }),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{value}");
    serde_json::from_value(value).unwrap()
}

#[tokio::test]
async fn signed_receipt_and_irreversible_retirement_allow_exactly_the_chosen_host_to_write() {
    use crate::handover::activation;
    let f = prepare().await;
    let mut retire = json!(model::Retire {
        change: f.input.change.clone(),
        confirm_stop_source: true,
        readiness: None,
    });
    assert_eq!(
        request(
            &f.app,
            "POST",
            "/api/hosting/handover/retire",
            Some(&f.owner.token),
            retire.clone()
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    let proof = receive_and_attest(&f).await;
    assert_eq!(
        request(
            &f.app,
            "POST",
            "/api/hosting/handover/receipt",
            Some(&f.owner.token),
            json!(proof)
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let mut wrong = proof.clone();
    wrong.body.database_sha256.replace_range(..1, "!");
    assert_eq!(
        request(
            &f.app,
            "POST",
            "/api/hosting/handover/receipt",
            Some(&f.member.token),
            json!(wrong)
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            &f.app,
            "POST",
            "/api/hosting/handover/receipt",
            Some(&f.member.token),
            json!(proof)
        )
        .await
        .0,
        StatusCode::OK
    );
    let no_confirm = json!(model::Retire {
        change: f.input.change.clone(),
        confirm_stop_source: false,
        readiness: None,
    });
    assert_eq!(
        request(
            &f.app,
            "POST",
            "/api/hosting/handover/retire",
            Some(&f.owner.token),
            no_confirm
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        request(
            &f.app,
            "POST",
            "/api/hosting/handover/retire",
            Some(&f.member.token),
            retire.clone()
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let (_waiting, readiness) = checked_destination(&f).await;
    retire["readiness"] = json!(readiness);
    // Exercise the real monotonic lease barrier before the fixture elapses it.
    f.state.0.handover.restart_media_wait_for_test();
    assert_eq!(
        request(
            &f.app,
            "POST",
            "/api/hosting/handover/retire",
            Some(&f.owner.token),
            retire.clone()
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    f.state.0.handover.elapse_media_leases();
    let (code, value) = request(
        &f.app,
        "POST",
        "/api/hosting/handover/retire",
        Some(&f.owner.token),
        retire.clone(),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{value}");
    let g: model::ActivationGrant = serde_json::from_value(value).unwrap();
    assert!(activation::valid_grant(&g));
    let challenge = URL_SAFE_NO_PAD.encode([81; 32]);
    let (code, route) = request(
        &f.app,
        "POST",
        "/api/hosting/handover/route",
        None,
        json!({"challenge":challenge}),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{route}");
    let route: model::RouteProof = serde_json::from_value(route).unwrap();
    assert!(!route.state.writes_allowed);
    assert_eq!(route.state.moved, Some(g.clone()));
    assert!(crate::handover_target::verify(
        &model::HostProof {
            identity: route.state.pin.host.clone(),
            signature: route.signature
        },
        &model::route_message(&route.state)
    ));

    assert!(f.state.0.handover.status().activation_available);
    assert_eq!(request(&f.app,"GET","/api/bootstrap",Some(&f.owner.token),serde_json::Value::Null).await.0,StatusCode::CONFLICT);
    assert!(!f.state.0.handover.writes_allowed());
    assert_eq!(
        request(
            &f.app,
            "POST",
            "/api/hosting/handover/cancel",
            Some(&f.owner.token),
            json!(f.input.change)
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert!(f
        .state
        .restore_from(&f.source.path().join("relay.sqlite3"))
        .is_err());
    let (_, again) = request(
        &f.app,
        "POST",
        "/api/hosting/handover/retire",
        Some(&f.owner.token),
        retire,
    )
    .await;
    assert_eq!(
        serde_json::from_value::<model::ActivationGrant>(again).unwrap(),
        g
    );
    let key = TargetKey::open(f.destination.path()).unwrap();
    let mut forged = g.clone();
    forged.next_term += 1;
    assert!(activation::activate(f.destination.path(), &f.status, &forged, &key).is_err());
    let mut missing_history=g.clone();
    missing_history.history_sha256=Some("a".repeat(64));
    missing_history.signature=f.state.0.handover.sign(&model::grant_message(&missing_history));
    assert!(activation::valid_grant(&missing_history));
    assert!(activation::activate(f.destination.path(),&f.status,&missing_history,&key).is_err(),
        "even a source-signed grant must have its exact committed ancestry");
    let other = tempfile::tempdir().unwrap();
    let wrong = TargetKey::open(other.path()).unwrap();
    assert!(activation::activate(f.destination.path(), &f.status, &g, &wrong).is_err());
    let active = activation::activate(f.destination.path(), &f.status, &g, &key).unwrap();
    assert_eq!(active.term, 2);
    let target = AppState::open(&active.data, "new host", 0).unwrap();
    assert!(target.0.handover.writes_allowed());
    assert_eq!(
        target.0.handover.status().source_host_id,
        key.identity().host_id
    );
    assert_eq!(target.server_id().unwrap(), f.status.server_id);
    assert_ne!(
        target.0.handover.media_room("channel"),
        f.state.0.handover.media_room("channel")
    );
    lock(&target.0.db)
        .execute(
            "INSERT INTO metadata(key,value) VALUES('after-handover','new writer')",
            [],
        )
        .unwrap();
    assert!(lock(&f.state.0.db)
        .execute(
            "INSERT INTO metadata(key,value) VALUES('after-handover','old writer')",
            []
        )
        .is_err());
    assert_eq!(
        lock(&target.0.db)
            .query_row("SELECT length(secret) FROM full_handover_only", [], |r| {
                r.get::<_, usize>(0)
            })
            .unwrap(),
        2 * PART + 37
    );
    assert!(crate::handover_target::active_runtime(&active.data)
        .unwrap()
        .is_some());
    // Idempotency never replaces the new writer with the old received bytes.
    activation::activate(f.destination.path(), &f.status, &g, &key).unwrap();
    assert_eq!(
        lock(&target.0.db)
            .query_row(
                "SELECT value FROM metadata WHERE key='after-handover'",
                [],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
        "new writer"
    );
    assert!(crate::host_copy_secrets::is_stripped(&lock(&target.0.db)).unwrap());
    let target_app = router(target);
    let challenge=URL_SAFE_NO_PAD.encode([82;32]);
    let (code,route)=request(&target_app,"POST","/api/hosting/handover/route",None,
        json!({"challenge":challenge})).await;
    assert_eq!(code,StatusCode::OK);
    let route:model::RouteProof=serde_json::from_value(route).unwrap();
    assert_eq!(route.state.history,vec![g.clone()]);
    assert!(route.state.moved.is_none());
    assert_eq!(route.state.challenge,challenge);
    assert!(crate::handover_target::verify(&model::HostProof{identity:route.state.pin.host.clone(),signature:route.signature},
        &model::route_message(&route.state)));
    // Sessions and password hashes stay with the old host: the copy carries the
    // same members, but each signs in again on the new host.
    assert_eq!(
        request(
            &target_app,
            "GET",
            "/api/bootstrap",
            Some(&f.owner.token),
            serde_json::Value::Null
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    drop(target_app);
    let restarted=AppState::open(&active.data,"new host restarted",0).unwrap();
    assert_eq!(restarted.0.handover.status().history,vec![g.clone()]);
    drop(restarted);
    let journal=active.data.join("host-authority.json");
    let mut legacy:serde_json::Value=serde_json::from_slice(&fs::read(&journal).unwrap()).unwrap();
    legacy.as_object_mut().unwrap().remove("history");
    fs::write(&journal,serde_json::to_vec(&legacy).unwrap()).unwrap();
    let old_format=AppState::open(&active.data,"legacy activated journal",0).unwrap();
    assert_eq!(old_format.0.handover.status().history,vec![g.clone()]);
    drop(old_format);
    legacy["history"]=json!([g.clone()]);
    legacy["history"][0]["signature"]=json!("forged");
    let damaged=serde_json::to_vec(&legacy).unwrap();
    fs::write(&journal,&damaged).unwrap();
    assert!(AppState::open(&active.data,"damaged history",0).is_err());
    assert_eq!(fs::read(&journal).unwrap(),damaged,"invalid history is retained unchanged");
    let Fixture {
        source, state, app, ..
    } = f;
    drop(app);
    drop(state);
    let old = AppState::open(source.path(), "retired after restart", 0).unwrap();
    assert!(!old.0.handover.writes_allowed());
    assert_eq!(old.0.handover.status().retired.unwrap(), g);
}

#[tokio::test]
async fn activation_resumes_both_durable_checkpoints_and_refuses_unknown_replacement() {
    use crate::handover::activation;
    for point in ["prepared", "replaced"] {
        let f = prepare().await;
        let g = grant(&f).await;
        let key = TargetKey::open(f.destination.path()).unwrap();
        activation::stop_once_at(point);
        assert!(activation::activate(f.destination.path(), &f.status, &g, &key).is_err());
        let dir = f
            .destination
            .path()
            .join("handover-received")
            .join(&f.input.change.preparation_id);
        assert!(crate::handover_target::active_runtime(&dir).is_err());
        let active = activation::activate(f.destination.path(), &f.status, &g, &key).unwrap();
        assert!(AppState::open(&active.data, "recovered writer", 0)
            .unwrap()
            .0
            .handover
            .writes_allowed());
    }
    let f = prepare().await;
    let g = grant(&f).await;
    let key = TargetKey::open(f.destination.path()).unwrap();
    activation::stop_once_at("prepared");
    assert!(activation::activate(f.destination.path(), &f.status, &g, &key).is_err());
    let dir = f
        .destination
        .path()
        .join("handover-received")
        .join(&f.input.change.preparation_id);
    let path = dir.join("relay.sqlite3");
    let unknown = b"unknown replacement retained";
    fs::write(&path, unknown).unwrap();
    assert!(activation::activate(f.destination.path(), &f.status, &g, &key).is_err());
    assert_eq!(fs::read(path).unwrap(), unknown);
    assert!(!f.state.0.handover.writes_allowed());
}

#[tokio::test]
async fn a_failed_retirement_write_never_returns_a_grant_or_allows_uncertain_cancellation() {
    let f = prepare().await;
    let proof = receive_and_attest(&f).await;
    assert_eq!(
        request(
            &f.app,
            "POST",
            "/api/hosting/handover/receipt",
            Some(&f.member.token),
            json!(proof)
        )
        .await
        .0,
        StatusCode::OK
    );
    f.state.0.handover.elapse_media_leases();
    let path = f.source.path().join("host-authority.json");
    let (_waiting, readiness) = checked_destination(&f).await;
    let saved = f.source.path().join("saved-authority.json");
    fs::rename(&path, &saved).unwrap();
    fs::create_dir(&path).unwrap();
    let retire = json!(model::Retire {
        change: f.input.change.clone(),
        confirm_stop_source: true,
        readiness: Some(readiness),
    });
    let (code, value) = request(
        &f.app,
        "POST",
        "/api/hosting/handover/retire",
        Some(&f.owner.token),
        retire,
    )
    .await;
    assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
    assert!(value.get("signature").is_none());
    assert!(f.state.0.handover.status().retired.is_none());
    assert_eq!(
        request(
            &f.app,
            "POST",
            "/api/hosting/handover/cancel",
            Some(&f.owner.token),
            json!(f.input.change)
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    fs::remove_dir(&path).unwrap();
    fs::rename(&saved, &path).unwrap();
    let Fixture {
        source,
        state,
        app,
        owner,
        input,
        ..
    } = f;
    drop(app);
    drop(state);
    let state = AppState::open(source.path(), "reconciled", 0).unwrap();
    assert!(state.0.handover.status().retired.is_none());
    let app = router(state.clone());
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/hosting/handover/cancel",
            Some(&owner.token),
            json!(input.change)
        )
        .await
        .0,
        StatusCode::OK
    );
    assert!(state.0.handover.writes_allowed());
}

#[tokio::test]
async fn video_namespace_cleanup_survives_cancellation_and_restart_and_never_deletes_new_or_foreign_rooms(
) {
    let rooms = Arc::new(Mutex::new(Vec::<String>::new()));
    let handler = |State(rooms): State<Arc<Mutex<Vec<String>>>>,
                   axum::extract::Path(method): axum::extract::Path<String>,
                   Json(input): Json<serde_json::Value>| async move {
        let mut rooms = lock(&rooms);
        match method.as_str() {
            "ListRooms" => Json(
                json!({"rooms":rooms.iter().map(|name| json!({"name":name})).collect::<Vec<_>>() }),
            ),
            "DeleteRoom" => {
                rooms.retain(|name| Some(name.as_str()) != input["room"].as_str());
                Json(json!({}))
            }
            _ => panic!("Unexpected synthetic SFU route"),
        }
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let service = Router::new()
        .route(
            "/twirp/livekit.RoomService/{method}",
            axum::routing::post(handler),
        )
        .with_state(rooms.clone());
    let task = tokio::spawn(async move { axum::serve(listener, service).await.unwrap() });
    let rtc = crate::rtc::RtcConfig::new(
        "wss://owned-fixture.invalid".into(),
        format!("http://{address}"),
        "fixture-key".into(),
        "owned-synthetic-video-secret".into(),
    )
    .unwrap();
    let f = prepare_with_rtc(Some(rtc.clone())).await;
    let journal: serde_json::Value =
        serde_json::from_slice(&fs::read(f.source.path().join("host-authority.json")).unwrap())
            .unwrap();
    let prefix = journal["rtc_cleanup_prefix"].as_str().unwrap();
    let channel = id();
    let legacy = format!("relay-{}-{channel}", f.status.server_id);
    let old = format!("{prefix}{channel}");
    let new = f.state.0.handover.media_room(&channel);
    let foreign = format!("relay-{}-{}-{channel}", id(), id());
    *lock(&rooms) = vec![
        legacy,
        old,
        new.clone(),
        foreign.clone(),
        "another-app".into(),
    ];
    assert_eq!(
        request(
            &f.app,
            "POST",
            "/api/hosting/handover/cancel",
            Some(&f.owner.token),
            json!(f.input.change)
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(f.state.0.handover.media_room(&channel), new);
    let Fixture {
        source, state, app, ..
    } = f;
    drop(app);
    drop(state);
    let without_config = AppState::open(source.path(), "missing video config", 0).unwrap();
    assert!(without_config.flush_handover_media().await.is_err());
    assert_eq!(lock(&rooms).len(), 5);
    drop(without_config);
    let changed = crate::rtc::RtcConfig::new(
        "wss://owned-fixture.invalid".into(),
        format!("http://{address}"),
        "fixture-key".into(),
        "different-owned-video-secret".into(),
    )
    .unwrap();
    let wrong_service = AppState::open_with_config(
        source.path(),
        "wrong video config",
        0,
        ServerConfig {
            rtc: Some(changed),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(wrong_service.flush_handover_media().await.is_err());
    assert_eq!(lock(&rooms).len(), 5);
    drop(wrong_service);
    let restarted = AppState::open_with_config(
        source.path(),
        "restarted",
        0,
        ServerConfig {
            rtc: Some(rtc),
            ..Default::default()
        },
    )
    .unwrap();
    restarted.flush_handover_media().await.unwrap();
    assert_eq!(*lock(&rooms), vec![new, foreign, "another-app".to_owned()]);
    let journal: serde_json::Value =
        serde_json::from_slice(&fs::read(source.path().join("host-authority.json")).unwrap())
            .unwrap();
    assert!(journal["rtc_cleanup_prefix"].is_null() && journal["rtc_cleanup_binding"].is_null());
    task.abort();
}
