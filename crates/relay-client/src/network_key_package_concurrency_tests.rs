//! Real MLS stock/reservation/receipt races; actors stay on their owning thread.
use super::*;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use std::sync::Barrier;

static BUSY_RETRIES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn crypto_target(target: &Target) -> CryptoTarget {
    let Target::Channel(id) = target else {
        panic!("owned channel expected")
    };
    CryptoTarget {
        channel_id: Some(id.clone()),
        peer_id: None,
    }
}

fn parallel(requests: Vec<reqwest::blocking::RequestBuilder>) -> Vec<reqwest::blocking::Response> {
    let barrier = Arc::new(Barrier::new(requests.len()));
    let jobs: Vec<_> = requests
        .into_iter()
        .map(|request| {
            let barrier = barrier.clone();
            thread::spawn(move || {
                barrier.wait();
                // The real server admits at most 32 blocking jobs. Keep the
                // 128-request burst, then retry only its explicit pre-dispatch
                // 503 rejection with the exact body; do not hide other errors.
                for attempt in 0..8 {
                    let response = request.try_clone().unwrap().send().unwrap();
                    if response.status() != reqwest::StatusCode::SERVICE_UNAVAILABLE {
                        return response;
                    }
                    let failure: serde_json::Value = response.json().unwrap();
                    assert_eq!(failure["error"], "Server is busy. Retry shortly");
                    BUSY_RETRIES.fetch_add(1, Ordering::Relaxed);
                    thread::sleep(Duration::from_millis(100 * (attempt + 1)));
                }
                panic!("owned server did not admit the exact request after bounded backoff")
            })
        })
        .collect();
    jobs.into_iter().map(|job| job.join().unwrap()).collect()
}
fn context_request(actor: &CryptoWorker, target: &Target) -> reqwest::blocking::RequestBuilder {
    let Target::Channel(id) = target else {
        panic!("owned channel expected")
    };
    actor
        .worker
        .request(reqwest::Method::GET, "/api/e2ee/context")
        .unwrap()
        .query(&[
            ("channel_id", id.clone()),
            ("device_id", actor.own().identity.device_id),
        ])
}
fn contexts(actor: &CryptoWorker, targets: &[Target]) -> Vec<CryptoContext> {
    parallel(
        targets
            .iter()
            .map(|target| context_request(actor, target))
            .collect(),
    )
    .into_iter()
    .map(|response| parse(response).unwrap())
    .collect()
}
fn stock(actor: &CryptoWorker) -> CryptoKeyPackageStatus {
    parse(
        actor
            .worker
            .request(reqwest::Method::GET, "/api/e2ee/key-packages")
            .unwrap()
            .query(&[("device_id", actor.own().identity.device_id)])
            .send()
            .unwrap(),
    )
    .unwrap()
}
fn publish_request(actor: &CryptoWorker, packages: &[String]) -> reqwest::blocking::RequestBuilder {
    actor
        .worker
        .request(reqwest::Method::POST, "/api/e2ee/key-packages")
        .unwrap()
        .json(&PublishCryptoKeyPackages {
            device_id: actor.own().identity.device_id,
            packages: packages.to_vec(),
        })
}
fn generated(actor: &mut CryptoWorker, count: usize) -> Vec<String> {
    actor
        .worker
        .crypto()
        .unwrap()
        .key_packages(count)
        .unwrap()
        .into_iter()
        .map(|p| STANDARD.encode(p.key_package))
        .collect()
}
fn public(context: &CryptoContext, actor: &CryptoWorker) -> relay_crypto::PublicDevice {
    let identity = actor.own().identity;
    let device = context
        .devices
        .iter()
        .find(|d| d.device_id == identity.device_id)
        .unwrap();
    relay_crypto::PublicDevice {
        identity,
        fingerprint: device.identity.clone(),
        key_package: STANDARD.decode(&device.key_package).unwrap(),
    }
}
fn reopen(actor: &mut CryptoWorker) {
    let c = actor.worker.connection.as_mut().unwrap();
    drop(c.crypto.take());
    c.crypto = Some(
        CryptoSession::open_for_test(actor.keys.clone(), &c.server_id, &c.session.user.id).unwrap(),
    );
}
fn consume_request(
    actor: &CryptoWorker,
    receipt: &ConsumeCryptoKeyPackage,
) -> reqwest::blocking::RequestBuilder {
    actor
        .worker
        .request(reqwest::Method::POST, "/api/e2ee/key-packages/consume")
        .unwrap()
        .json(receipt)
}

pub(super) fn run() {
    BUSY_RETRIES.store(0, Ordering::Relaxed);
    let started = Instant::now();
    let mut server = CryptoServer::start();
    let mut alice = CryptoWorker::new(&server, register(&server, "stock_race_alice", ""));
    let invite: serde_json::Value = alice.worker.post("/api/invites", &json!({})).unwrap();
    let mut bob = CryptoWorker::new(
        &server,
        register(&server, "stock_race_bob", invite["code"].as_str().unwrap()),
    );
    let a = alice.own();
    let b = bob.own();
    alice
        .worker
        .crypto()
        .unwrap()
        .with_device(|d| d.approve(&b, &b.fingerprint))
        .unwrap();
    bob.worker
        .crypto()
        .unwrap()
        .with_device(|d| d.approve(&a, &a.fingerprint))
        .unwrap();
    assert_eq!(stock(&alice).unused, 16);
    assert_eq!(stock(&bob).unused, 16);
    // Simultaneous duplicate uploads must insert each actual TLS package once.
    for actor in [&mut alice, &mut bob] {
        let extra = generated(actor, 16);
        let requests = (0..8).map(|_| publish_request(actor, &extra)).collect();
        let inserted: u64 = parallel(requests)
            .into_iter()
            .map(|r| {
                let result: serde_json::Value = parse(r).unwrap();
                result["accepted"].as_u64().unwrap()
            })
            .sum();
        assert_eq!(inserted, 16);
        actor.worker.crypto().unwrap().pool_committed().unwrap();
        assert_eq!(stock(actor).unused, 32);
    }
    let failed_pool = generated(&mut bob, 16);
    assert_eq!(
        publish_request(&bob, &failed_pool).send().unwrap().status(),
        reqwest::StatusCode::CONFLICT
    );
    assert_eq!(
        stock(&bob).unused,
        32,
        "overflow must roll back the whole insert"
    );
    reopen(&mut bob);
    assert_eq!(
        generated(&mut bob, 16),
        failed_pool,
        "reopen must preserve exact unpublished TLS packages"
    );
    let targets: Vec<_> = (0..64)
        .map(|index| {
            let channel: Channel = alice
                .worker
                .post(
                    "/api/channels",
                    &json!({"name":format!("owned-race-{index}"),"kind":"text"}),
                )
                .unwrap();
            Target::Channel(channel.id)
        })
        .collect();
    let reservation_at = Instant::now();
    // 128 requests start together: two identities compete for the same 64 targets.
    let requests = targets
        .iter()
        .map(|t| context_request(&alice, t))
        .chain(targets.iter().map(|t| context_request(&bob, t)))
        .collect();
    let responses: Vec<CryptoContext> = parallel(requests)
        .into_iter()
        .map(|r| parse(r).unwrap())
        .collect();
    let mut admitted = Vec::new();
    let mut exhausted = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let bob_id = bob.own().identity.device_id;
    for (index, target) in targets.iter().enumerate() {
        let left = &responses[index];
        let right = &responses[index + 64];
        let package = public(left, &bob).key_package;
        assert_eq!(
            package,
            public(right, &bob).key_package,
            "same-target reservations must be stable"
        );
        if package.is_empty() {
            exhausted.push(target.clone());
        } else {
            assert!(
                seen.insert(package),
                "different targets cannot share one-use bytes"
            );
            admitted.push((target.clone(), left.clone()));
        }
    }
    assert_eq!(admitted.len(), 32);
    assert_eq!(exhausted.len(), 32);
    assert_eq!((stock(&bob).unused, stock(&bob).reserved), (0, 32));
    let reservation_ms = reservation_at.elapsed().as_millis();
    // Retry the saved failed publication now that space exists; duplicate uploads remain exact.
    let inserted: u64 = parallel(
        (0..8)
            .map(|_| publish_request(&bob, &failed_pool))
            .collect(),
    )
    .into_iter()
    .map(|r| {
        parse::<serde_json::Value>(r).unwrap()["accepted"]
            .as_u64()
            .unwrap()
    })
    .sum();
    assert_eq!(inserted, 16);
    bob.worker.crypto().unwrap().pool_committed().unwrap();
    let refill = generated(&mut bob, 16);
    let _: serde_json::Value = parse(publish_request(&bob, &refill).send().unwrap()).unwrap();
    bob.worker.crypto().unwrap().pool_committed().unwrap();
    assert_eq!(stock(&bob).unused, 32);
    for context in contexts(&bob, &exhausted) {
        assert!(seen.insert(public(&context, &bob).key_package));
    }
    assert_eq!(seen.len(), 64);
    assert_eq!((stock(&bob).unused, stock(&bob).reserved), (0, 64));
    let original_targets: Vec<_> = admitted.iter().map(|(target, _)| target.clone()).collect();
    for (context, (_, old)) in contexts(&bob, &original_targets).iter().zip(&admitted) {
        assert_eq!(
            public(context, &bob).key_package,
            public(old, &bob).key_package
        );
    }
    let membership_at = Instant::now();
    let mut batches = Vec::new();
    for (target, context) in &admitted {
        let scope = alice.worker.crypto_scope(target).unwrap();
        let batch = alice
            .worker
            .crypto()
            .unwrap()
            .stage_membership(
                &scope,
                crypto_target(target),
                uuid::Uuid::new_v4().to_string(),
                0,
                &[public(context, &bob)],
                &[],
                true,
            )
            .unwrap();
        batches.push((scope, batch));
    }
    let requests = batches
        .iter()
        .map(|(_, batch)| {
            alice
                .worker
                .request(reqwest::Method::POST, "/api/e2ee/events")
                .unwrap()
                .json(batch)
        })
        .collect();
    for ((scope, _), response) in batches.iter().zip(parallel(requests)) {
        let ack: CryptoContext = parse(response).unwrap();
        assert_eq!(ack.sequence, 3);
        alice
            .worker
            .crypto()
            .unwrap()
            .committed_control(scope, ack.sequence)
            .unwrap();
    }
    // Apply real Welcomes but deliberately retain all delivery receipts (lost responses).
    for ((target, _), context) in admitted.iter().zip(contexts(&bob, &original_targets)) {
        let scope = bob.worker.crypto_scope(target).unwrap();
        let package_hash = format!("{:x}", Sha256::digest(public(&context, &bob).key_package));
        for event in &context.events {
            let receipt = (event.kind == "welcome").then(|| ConsumeCryptoKeyPackage {
                target: crypto_target(target),
                device_id: bob_id.clone(),
                package_hash: package_hash.clone(),
                welcome_sequence: event.sequence,
            });
            bob.worker
                .crypto()
                .unwrap()
                .apply_control(&scope, event, &context.server_id, receipt)
                .unwrap();
        }
    }
    let receipts = bob.worker.crypto().unwrap().pending_consumptions();
    assert_eq!(receipts.len(), 32);
    assert!(receipts.iter().all(|r| r.welcome_sequence == 3));
    let identity = bob.own().identity;
    reopen(&mut bob);
    assert_eq!(bob.own().identity, identity);
    assert_eq!(
        bob.worker.crypto().unwrap().pending_consumptions().len(),
        32
    );
    assert_eq!(
        consume_request(&bob, &receipts[0]).send().unwrap().status(),
        reqwest::StatusCode::NO_CONTENT
    );
    bob.worker
        .crypto()
        .unwrap()
        .committed_consumption(&receipts[0])
        .unwrap();
    assert_eq!(bob.worker.crypto().unwrap().pending_consumptions().len(), 31,
        "one acknowledged Welcome must not delete receipts from other conversations with sequence 3");
    reopen(&mut bob);
    assert_eq!(
        bob.worker.crypto().unwrap().pending_consumptions().len(),
        31
    );
    // Repeated acknowledgments and refreshes compete; the durable ledger prevents reuse.
    let requests = receipts
        .iter()
        .flat_map(|r| (0..3).map(|_| consume_request(&bob, r)))
        .chain(original_targets.iter().map(|t| context_request(&bob, t)))
        .collect();
    for (index, response) in parallel(requests).into_iter().enumerate() {
        assert_eq!(
            response.status(),
            if index < 96 {
                reqwest::StatusCode::NO_CONTENT
            } else {
                reqwest::StatusCode::OK
            }
        );
    }
    // Use the real worker flush so production acknowledgment handling is covered.
    bob.worker
        .ensure_crypto_conversation(&original_targets[0])
        .unwrap();
    assert!(bob
        .worker
        .crypto()
        .unwrap()
        .pending_consumptions()
        .is_empty());
    assert_eq!(
        (
            stock(&bob).unused,
            stock(&bob).reserved,
            stock(&bob).consumed
        ),
        (0, 32, 32)
    );
    let mut wrong = receipts[0].clone();
    wrong.target = receipts[1].target.clone();
    assert_eq!(
        consume_request(&bob, &wrong).send().unwrap().status(),
        reqwest::StatusCode::CONFLICT
    );
    wrong = receipts[0].clone();
    wrong.welcome_sequence += 1;
    assert_eq!(
        consume_request(&bob, &wrong).send().unwrap().status(),
        reqwest::StatusCode::CONFLICT
    );
    assert_eq!(
        consume_request(&alice, &receipts[0])
            .send()
            .unwrap()
            .status(),
        reqwest::StatusCode::FORBIDDEN
    );
    let consumed_bytes: Vec<_> = admitted
        .iter()
        .map(|(_, c)| STANDARD.encode(public(c, &bob).key_package))
        .collect();
    for chunk in consumed_bytes.chunks(16) {
        let result: serde_json::Value =
            parse(publish_request(&bob, chunk).send().unwrap()).unwrap();
        assert_eq!(result["accepted"], 0);
        assert_eq!(result["unused"], 0);
    }
    let membership_ms = membership_at.elapsed().as_millis();
    server.restart();
    reopen(&mut alice);
    reopen(&mut bob);
    assert_eq!(bob.own().identity, identity);
    assert!(bob
        .worker
        .crypto()
        .unwrap()
        .pending_consumptions()
        .is_empty());
    assert_eq!(stock(&bob).consumed, 32);
    let messages_at = Instant::now();
    for (index, target) in original_targets.iter().enumerate() {
        let message = format!("owned stock race encrypted group {index}");
        alice.send(
            target,
            &message,
            Vec::new(),
            &format!("race-message-{index}"),
            None,
        );
        assert_eq!(bob.history(target).last().unwrap().content, message);
    }
    reopen(&mut bob);
    for (index, target) in original_targets.iter().enumerate() {
        assert_eq!(
            bob.history(target).last().unwrap().content,
            format!("owned stock race encrypted group {index}")
        );
    }
    fs::write(server.root.join("key-package-concurrency.json"), serde_json::to_vec_pretty(&json!({
        "scope":"Actual MLS actors; known fixture fingerprints; real localhost HTTP; no hardware/WAN/onboarding acceptance",
        "groups_with_real_welcomes":32,"concurrent_reservation_requests":128,"distinct_reserved_bob_packages":64,
        "exhausted_targets":32,"duplicate_publication_inserted_once":true,"overflow_rollback":true,
        "unpublished_stock_survives_reopen":true,"unchanged_old_reservations_after_refill":true,
        "exact_receipt_ack_retains_other_31":true,"concurrent_ack_retry_requests":96,
        "refresh_requests_during_ack":32,"consumed":32,"consumed_bytes_cannot_return_to_stock":true,
        "explicit_pre_dispatch_503_retries":BUSY_RETRIES.load(Ordering::Relaxed),
        "wrong_target_sequence_and_owner_denied":true,"server_and_actor_reopen":true,
        "all_32_messages_decrypt_before_and_after_reopen":true,"reservation_ms":reservation_ms,
        "membership_and_receipt_ms":membership_ms,"messages_and_reopen_ms":messages_at.elapsed().as_millis(),
        "total_ms":started.elapsed().as_millis()
    })).unwrap()).unwrap();
}
