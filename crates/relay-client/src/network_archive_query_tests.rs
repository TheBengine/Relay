//! Opt-in long-history queries through the production Network thread.
//! Run alone in a fresh process: profile paths are configured before actors.
use super::*;

fn copy_owned_storage(source: &Path, destination: &Path, root: &Path) {
    assert!(source.canonicalize().unwrap().starts_with(root));
    assert!(destination.canonicalize().unwrap().starts_with(root));
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let kind = entry.file_type().unwrap();
        assert!(!kind.is_symlink());
        let path = entry.path();
        if path
            .extension()
            .is_some_and(|extension| extension == "lock")
        {
            continue;
        }
        let target = destination.join(entry.file_name());
        if kind.is_dir() {
            fs::create_dir(&target).unwrap();
            copy_owned_storage(&path, &target, root);
        } else {
            assert!(kind.is_file());
            assert!(!target.exists());
            fs::copy(path, target).unwrap();
        }
    }
}

fn receive(network: &Network, mut accepts: impl FnMut(&Update) -> bool) -> Update {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(!remaining.is_zero(), "owned threaded query timed out");
        let update = network.rx.recv_timeout(remaining).unwrap();
        if let Update::Error(error) = &update {
            panic!("owned threaded worker error: {error}");
        }
        if let Update::RequestFailed { request_id, error } = &update {
            panic!("owned threaded request {request_id} failed: {error}");
        }
        if accepts(&update) {
            return update;
        }
    }
}

fn history(network: &Network, target: &Target, request_id: u64) -> Vec<Message> {
    network
        .send(Command::History {
            target: target.clone(),
            before: None,
            request_id,
        })
        .unwrap();
    let Update::History { page, .. } = receive(
        network,
        |update| matches!(update, Update::History { request_id: id, .. } if *id == request_id),
    ) else {
        unreachable!()
    };
    page.messages
}

pub(super) fn run() {
    if cfg!(debug_assertions) {
        panic!("Run threaded archive queries with --release in a fresh process");
    }
    let server = CryptoServer::start();
    let base = PathBuf::from(std::env::var_os("RELAY_TEST_WORK_ROOT").unwrap())
        .canonicalize()
        .unwrap();
    let root = server.root.canonicalize().unwrap();
    assert!(root.starts_with(base));
    let profile = server.root.join("threaded-profile");
    crate::profile_paths::configure(&[
        "owned-query-fixture".into(),
        "--profile-dir".into(),
        profile.to_str().unwrap().into(),
    ])
    .unwrap();
    let (mut alice, mut bob, _eve, target) = workers(&server);
    alice.pin(&bob.own());
    bob.pin(&alice.own());
    let private = "owned archive needle 41";
    alice.send(&target, private, Vec::new(), "threaded-first", None);
    assert_eq!(bob.history(&target)[0].content, private);
    let raw = bob.raw_messages(&target);
    assert_eq!(raw.len(), 1);
    assert!(raw[0].content.starts_with(MESSAGE_PREFIX));
    let own = bob.own();
    let origin = bob.origin();
    let scope = bob.worker.crypto_scope(&target).unwrap();
    let seeded = Instant::now();
    eprintln!("owned threaded fixture: seed archive");
    bob.worker
        .crypto()
        .unwrap()
        .seed_owned_history_measurement(&scope, 64000);
    let seed_us = seeded.elapsed().as_micros();
    eprintln!("owned threaded fixture: archive seeded");
    let keys = bob.keys.clone();
    drop(bob.worker.connection.as_mut().unwrap().crypto.take());
    drop(bob);
    let storage = profile.join("encryption");
    copy_owned_storage(&keys, &storage, &root);

    let network = Network::new(eframe::egui::Context::default());
    eprintln!("owned threaded fixture: normal login");
    network
        .send(Command::Auth {
            authority: None,
            url: server.url.clone(),
            expected_server_id: Some(origin.server_id.clone()),
            request: AuthRequest {
                second_factor: None,
                initial_protection: None,
                username: "crypto_bob".into(),
                password: "Owned_Crypto_Test_1234!".into(),
                invite: String::new(),
            },
            register: false,
            request_id: 0,
        })
        .unwrap();
    let Update::Connected {
        session, bootstrap, ..
    } = receive(&network, |update| {
        matches!(update, Update::Connected { .. })
    })
    else {
        unreachable!()
    };
    assert_eq!(session.user.id, own.identity.user_id);
    assert_eq!(bootstrap.server_id, own.identity.server_id);
    eprintln!("owned threaded fixture: connected; first history");
    network
        .send(Command::SelectConversation(target.clone()))
        .unwrap();
    let first = history(&network, &target, 400);
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].content, private);
    // CryptoStatus snapshots are intentionally deduplicated. The page wait
    // consumed the initial snapshot; do not wait for an unchanged re-emission.
    // Verify exact identity/archive count after the normal worker releases it.

    let mut search_us = Vec::new();
    let mut queued_check_us = Vec::new();
    let mut history_us = Vec::new();
    let mut reaction_us = Vec::new();
    for round in 0..5u64 {
        eprintln!("owned threaded fixture: query round {round}");
        let request_id = 500 + round;
        let started = Instant::now();
        network
            .send(Command::CommunitySearch {
                origin: origin.clone(),
                query: private.into(),
                filters: Default::default(),
                remaining: None,
                request_id,
            })
            .unwrap();
        network
            .send(Command::CheckServer {
                locator_invite: None,
                cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                authority: None,
                url: server.url.clone(),
                request_id,
            })
            .unwrap();
        let Update::CommunitySearch {
            messages,
            remaining,
            errors,
            query,
            ..
        } = receive(
            &network,
            |update| matches!(update, Update::CommunitySearch { request_id: id, .. } if *id == request_id),
        )
        else {
            unreachable!()
        };
        search_us.push(started.elapsed().as_micros());
        assert_eq!(query, private);
        assert!(remaining.is_empty() && errors.is_empty(), "{errors:?}");
        assert_eq!(
            messages.len(),
            1,
            "synthetic cache records are not host-delivered search results"
        );
        assert_eq!(messages[0].0, target);
        assert_eq!(messages[0].1.id, raw[0].id);
        assert_eq!(messages[0].1.content, private);
        let Update::ServerChecked { result, .. } = receive(
            &network,
            |update| matches!(update, Update::ServerChecked { request_id: id, .. } if *id == request_id),
        ) else {
            unreachable!()
        };
        assert!(result.is_ok());
        queued_check_us.push(started.elapsed().as_micros());

        let started = Instant::now();
        let page = history(&network, &target, 600 + round);
        history_us.push(started.elapsed().as_micros());
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].content, private);

        // Unique authenticated intentions; never retry a server rate limit.
        let emoji = format!("owned-{round}");
        let started = Instant::now();
        network
            .send(Command::React {
                origin: origin.clone(),
                target: target.clone(),
                message_id: raw[0].id.clone(),
                target_client_id: Some("threaded-first".into()),
                emoji: emoji.clone(),
                active: true,
            })
            .unwrap();
        let Update::ReactionSnapshot { rows, .. } = receive(&network, |update| {
            matches!(update, Update::ReactionSnapshot { message_id, rows, .. }
                if message_id == &raw[0].id && rows.iter().any(|row| row.emoji == emoji))
        }) else {
            unreachable!()
        };
        reaction_us.push(started.elapsed().as_micros());
        assert!(rows
            .iter()
            .any(|row| row.emoji == emoji && row.user_ids == vec![own.identity.user_id.clone()]));
    }
    // Continue the original real MLS session after archive copying/normal login.
    alice.send(
        &target,
        "actual MLS after threaded queries",
        Vec::new(),
        "threaded-second",
        None,
    );
    let final_page = history(&network, &target, 700);
    assert_eq!(final_page.len(), 2);
    assert!(final_page
        .iter()
        .any(|message| message.content == "actual MLS after threaded queries"));
    let first = final_page
        .iter()
        .find(|message| message.id == raw[0].id)
        .unwrap();
    assert_eq!(first.reactions.len(), 5);
    let alice_page = alice.history(&target);
    assert_eq!(alice_page.len(), 2);
    assert_eq!(alice_page[0].reactions.len(), 5);
    drop(network);
    let deadline = Instant::now() + Duration::from_secs(15);
    let reopened = loop {
        match CryptoSession::open_for_test(storage.clone(), &origin.server_id, &origin.user_id) {
            Ok(crypto) => break crypto,
            Err(error) => {
                assert!(
                    Instant::now() < deadline,
                    "normal worker did not release owned checkpoint: {error}"
                );
                thread::sleep(Duration::from_millis(10));
            }
        }
    };
    assert_eq!(reopened.advertised().identity, own.identity);
    assert_eq!(reopened.advertised().fingerprint, own.fingerprint);
    assert!(reopened.history_usage().records >= 64007);
    assert!(reopened
        .cached_envelopes(&scope)
        .any(|entry| entry.unwrap().envelope.client_id == "threaded-second"));
    let evidence = json!({
        "process_id": std::process::id(), "synthetic_bulk_records": 64000,
        "seed_us": seed_us, "search_us": search_us, "queued_check_us": queued_check_us,
        "history_us": history_us, "reaction_us": reaction_us,
        "normal_Network_thread": true, "same_device_after_normal_shutdown": true,
        "real_MLS_continuity": true, "real_host_messages": 2, "real_reaction_actions": 5,
        "synthetic_records_returned_as_host_matches": 0,
        "scope": "Localhost actual production command loop and protected synthetic bulk archive. No UI, physical media, WAN or universal timing guarantee. Search transport secrecy requires separate source/transport review."
    });
    fs::write(
        server.root.join("threaded-query-measurement.json"),
        serde_json::to_vec_pretty(&evidence).unwrap(),
    )
    .unwrap();
    reopened.remove_test_checkpoint();
}
