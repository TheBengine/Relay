//! R3.2 against the real server: a friend who joins with a verified invitation
//! and the host trust each other without any pasted code, a forged proof
//! approves nothing, and the invitation's secret never reaches the server.
//! Only synthetic accounts on a private localhost server.
use super::*;
use crate::invite_proof::InviteProof;
use crate::network::tests::{MockHttp, Reply};

fn invitation(host: &mut CryptoWorker, request_id: u64) -> (String, InviteProof) {
    let origin = host.origin();
    let updates = host.handle(Command::Invite {
        origin,
        request_id: Some(request_id),
    });
    updates
        .into_iter()
        .find_map(|update| match update {
            Update::Invite {
                code,
                proof: Some(proof),
                ..
            } => Some((code, proof)),
            _ => None,
        })
        .expect("a host with encryption keys issues a verifiable invitation")
}

/// Verified, not merely trusted on first use: the invitation is what verifies.
fn trusts(worker: &CryptoWorker, peer: &relay_crypto::PublicDevice) -> bool {
    worker
        .worker
        .connection
        .as_ref()
        .unwrap()
        .crypto
        .as_ref()
        .unwrap()
        .device
        .is_verified(peer)
}

fn files_under(root: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(root).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            files_under(&path, out);
        } else {
            out.push(path);
        }
    }
}

pub(super) fn run() {
    let mut server = CryptoServer::start();
    let mut host = CryptoWorker::new(&server, register(&server, "invite_host", ""));
    let host_device = host.own();

    // 1. A friend joins with a verified invitation and nobody pastes anything.
    let (code, proof) = invitation(&mut host, 1);
    assert_eq!(proof.host_device_id, host_device.identity.device_id);
    assert_eq!(proof.host_fingerprint, host_device.fingerprint);
    let mut friend = CryptoWorker::new(&server, register(&server, "invite_friend", &code));
    let friend_device = friend.own();
    assert!(!trusts(&friend, &host_device) && !trusts(&host, &friend_device));
    let origin = friend.origin();
    friend
        .worker
        .refresh_pending
        .store(false, Ordering::Relaxed);
    friend.handle(Command::AttestInvitation {
        origin,
        proof: proof.clone(),
    });
    assert!(
        trusts(&friend, &host_device),
        "the friend pinned the host device the link named"
    );
    assert!(friend.worker.refresh_pending.load(Ordering::Relaxed));
    host.worker.refresh_pending.store(false, Ordering::Relaxed);
    host.worker.refresh();
    let updates = host.drain();
    assert!(
        trusts(&host, &friend_device),
        "the host approved the friend because the proof verified"
    );
    assert!(
        host.worker.refresh_pending.load(Ordering::Relaxed),
        "new invitation approval schedules the short conversation-key refresh"
    );
    assert!(updates.iter().any(|update| matches!(
        update,
        Update::Notice(text) if text.contains("verified through your invitation")
    )));
    let host_status = host
        .worker
        .connection
        .as_ref()
        .unwrap()
        .crypto
        .as_ref()
        .unwrap()
        .status
        .clone();
    assert_eq!(
        host_status.via_invitation,
        std::slice::from_ref(&friend_device.identity.device_id)
    );

    // 2. The invitation is single use on the server too: a second account cannot
    // join with it, so it can never carry a second device's proof.
    let again = Client::new()
        .post(format!("{}/api/register", server.url))
        .json(&AuthRequest {
            second_factor: None,
            initial_protection: None,
            username: "invite_late".into(),
            password: "Owned_Crypto_Test_1234!".into(),
            invite: code.clone(),
        })
        .send()
        .unwrap();
    assert_eq!(again.status(), reqwest::StatusCode::FORBIDDEN);

    // 3. A proof made with the wrong secret is stored by the server (it cannot
    // tell) but the host never approves it and says so.
    let (code, real) = invitation(&mut host, 2);
    let forged = InviteProof::new(
        &real.invite_id,
        &real.host_device_id,
        &real.host_fingerprint,
        [7u8; crate::invite_proof::SECRET_BYTES],
    )
    .unwrap();
    let mut impostor = CryptoWorker::new(&server, register(&server, "invite_impostor", &code));
    let origin = impostor.origin();
    impostor.handle(Command::AttestInvitation {
        origin,
        proof: forged,
    });
    host.handle(Command::CryptoRefresh);
    assert!(
        !trusts(&host, &impostor.own()),
        "a forged proof approves nothing"
    );
    let status = host
        .worker
        .connection
        .as_ref()
        .unwrap()
        .crypto
        .as_ref()
        .unwrap()
        .status
        .clone();
    assert_eq!(
        status.invitation_unconfirmed,
        [impostor.own().identity.device_id.clone()]
    );

    // 4. The secrets never reached the server, its logs or any file next to it. The
    // host's own record of them is protected at rest, so it is not readable either.
    // Close everything that holds a file open so that no file is skipped.
    let host_keys = host.keys.clone();
    drop((host, friend, impostor));
    server.stop();
    let mut files = Vec::new();
    files_under(&server.root, &mut files);
    assert!(files.iter().any(|file| file.starts_with(&host_keys)));
    assert!(
        files.len() > 3,
        "the scan covered the server's data and logs"
    );
    for secret in [
        proof.secret().to_vec(),
        proof.secret_for_link().as_bytes().to_vec(),
        real.secret().to_vec(),
        real.secret_for_link().as_bytes().to_vec(),
    ] {
        for file in &files {
            assert_absent(&fs::read(file).unwrap(), &secret);
        }
    }
}

/// The server accepts the proof, then the first directory read fails. The
/// ordinary refresh must finish pinning the exact invited host without a UI visit.
pub(super) fn retry_after_directory_outage() {
    let server = CryptoServer::start();
    let mut host = CryptoWorker::new(&server, register(&server, "retry_host", ""));
    let (code, proof) = invitation(&mut host, 1);
    let host_public = host.own();
    let mut friend = CryptoWorker::new(&server, register(&server, "retry_friend", &code));
    let friend_public = friend.own();
    let fail_directory = Arc::new(AtomicBool::new(false));
    let failures = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let armed = fail_directory.clone();
    let failed = failures.clone();
    let base = server.url.clone();
    assert!(base.starts_with("http://127.0.0.1:"));
    let proxy_http = Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap();
    let proxy = MockHttp::with_request(move |request| {
        if request.method == "GET"
            && request.path == "/api/e2ee/directory"
            && armed.swap(false, Ordering::AcqRel)
        {
            failed.fetch_add(1, Ordering::AcqRel);
            return Reply::json(503, json!({"error":"Owned temporary directory failure"}));
        }
        let mut forward = proxy_http
            .request(
                reqwest::Method::from_bytes(request.method.as_bytes()).unwrap(),
                format!("{base}{}", request.path),
            )
            .header("Content-Type", "application/json")
            .body(request.body.clone());
        if let Some(auth) = &request.authorization {
            forward = forward.header("Authorization", auth);
        }
        let response = forward.send().unwrap();
        let status = response.status().as_u16();
        if request.method == "PUT" && request.path == "/api/e2ee/device/attestation" {
            assert_eq!(
                status, 204,
                "the actual server must have accepted the proof first"
            );
            armed.store(true, Ordering::Release);
        }
        Reply {
            status,
            body: response.bytes().unwrap().to_vec(),
            headers: Vec::new(),
        }
    });
    friend.worker.connection.as_mut().unwrap().url = proxy.url.clone();
    let origin = friend.origin();
    friend.handle(Command::AttestInvitation { origin, proof });
    assert_eq!(failures.load(Ordering::Acquire), 1);
    assert!(!trusts(&friend, &host_public));
    // No manual CryptoRefresh, verification action, GUI or new invitation.
    friend.worker.refresh();
    friend.drain();
    assert!(
        trusts(&friend, &host_public),
        "normal refresh must finish the interrupted host pin"
    );
    host.worker.refresh();
    host.drain();
    assert!(trusts(&host, &friend_public));
    assert_eq!(
        proxy
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.path == "/api/e2ee/device/attestation")
            .count(),
        1,
        "directory recovery must not repost the one-time attestation"
    );
    fs::write(
        server.root.join("invited-host-retry.json"),
        serde_json::to_vec_pretty(&json!({
            "actual_server_accepted_attestation":true,"injected_directory_failures":1,
            "normal_refresh_pinned_exact_host":true,"host_verified_friend":true,
            "attestation_requests":1,"manual_crypto_refresh":false,"physical_devices":false
        }))
        .unwrap(),
    )
    .unwrap();
}

/// The first proof request is lost either before the server sees it, or after
/// the server commits it. Retry must reuse exactly the same device-bound proof.
pub(super) fn retry_initial_attestation() {
    let mut server = CryptoServer::start();
    let mut host = CryptoWorker::new(&server, register(&server, "post_retry_host", ""));
    let host_public = host.own();
    let mut evidence = Vec::new();
    let mut secrets = Vec::new();
    for (index, scenario) in [
        "before_commit",
        "after_commit",
        "always_unavailable",
        "rejected",
    ]
    .into_iter()
    .enumerate()
    {
        let (code, proof) = invitation(&mut host, index as u64 + 1);
        secrets.push(proof.secret().to_vec());
        secrets.push(proof.secret_for_link().as_bytes().to_vec());
        let mut friend = CryptoWorker::new(
            &server,
            register(&server, &format!("post_retry_friend_{index}"), &code),
        );
        let friend_public = friend.own();
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = attempts.clone();
        let actual_statuses = Arc::new(std::sync::Mutex::new(Vec::new()));
        let statuses = actual_statuses.clone();
        let base = server.url.clone();
        assert!(base.starts_with("http://127.0.0.1:"));
        let http = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(3))
            .build()
            .unwrap();
        let proxy = MockHttp::with_request(move |request| {
            let post = request.method == "PUT" && request.path == "/api/e2ee/device/attestation";
            let attempt = if post {
                count.fetch_add(1, Ordering::AcqRel) + 1
            } else {
                0
            };
            if post && scenario == "rejected" {
                return Reply::json(400, json!({"error":"Owned invalid-request response"}));
            }
            if post
                && (scenario == "always_unavailable"
                    || (scenario == "before_commit" && attempt == 1))
            {
                return Reply::json(503, json!({"error":"Owned temporary proof-request outage"}));
            }
            let mut forward = http
                .request(
                    reqwest::Method::from_bytes(request.method.as_bytes()).unwrap(),
                    format!("{base}{}", request.path),
                )
                .header("Content-Type", "application/json")
                .body(request.body.clone());
            if let Some(auth) = &request.authorization {
                forward = forward.header("Authorization", auth);
            }
            let response = forward.send().unwrap();
            let status = response.status().as_u16();
            if post {
                statuses.lock().unwrap().push(status);
                if scenario == "after_commit" && attempt == 1 {
                    assert_eq!(
                        status, 204,
                        "the actual server committed the original proof"
                    );
                    return Reply::json(503, json!({"error":"Owned response lost after commit"}));
                }
            }
            Reply {
                status,
                body: response.bytes().unwrap().to_vec(),
                headers: Vec::new(),
            }
        });
        friend.worker.connection.as_mut().unwrap().url = proxy.url.clone();
        let origin = friend.origin();
        friend.handle(Command::AttestInvitation { origin, proof });
        let recovered = matches!(scenario, "before_commit" | "after_commit");
        assert_eq!(
            trusts(&friend, &host_public),
            recovered,
            "temporary initial attestation outage must recover using the same proof: {scenario}"
        );
        host.worker.refresh();
        host.drain();
        assert_eq!(trusts(&host, &friend_public), recovered);
        let requests = proxy.requests.lock().unwrap();
        let proofs: Vec<_> = requests
            .iter()
            .filter(|r| r.path == "/api/e2ee/device/attestation")
            .map(|r| r.body.clone())
            .collect();
        let expected_attempts = if scenario == "rejected" { 1 } else { 2 };
        assert_eq!(
            attempts.load(Ordering::Acquire),
            expected_attempts,
            "bounded attempts: {scenario}"
        );
        assert_eq!(proofs.len(), expected_attempts);
        if proofs.len() == 2 {
            assert_eq!(proofs[0], proofs[1], "retry must preserve the exact proof");
        }
        let statuses = actual_statuses.lock().unwrap().clone();
        assert_eq!(
            statuses,
            match scenario {
                "before_commit" => vec![204],
                "after_commit" => vec![204, 409],
                _ => Vec::new(),
            }
        );
        evidence.push(json!({"scenario":scenario,"attempts":expected_attempts,
            "actual_server_statuses":statuses,"recovered":recovered,"new_invitation":false}));
    }
    drop(host);
    server.stop();
    let mut files = Vec::new();
    files_under(&server.root, &mut files);
    assert!(files.len() > 3);
    for secret in secrets {
        for file in &files {
            assert_absent(&fs::read(file).unwrap(), &secret);
        }
    }
    fs::write(
        server.root.join("initial-attestation-retry.json"),
        serde_json::to_vec_pretty(
            &json!({"scenarios":evidence,"secret_absent_from_owned_files":true,
            "physical_devices":false,"native_gui":false,"wan":false}),
        )
        .unwrap(),
    )
    .unwrap();
}
