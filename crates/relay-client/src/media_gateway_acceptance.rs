// Included inside media::tests. Real router, broker, UDP and voice workers;
// disposable account/device records and call keys; no physical audio devices.
#[test]
#[ignore = "native acceptance binds one of this PC's own private IPv4 interfaces"]
fn actual_friends_broker_drives_two_generated_voice_workers() {
    friends_gateway_acceptance(false);
}

#[test]
#[ignore = "needs an own private IPv4 and the loopback-only QA iroh helper"]
fn actual_friends_broker_drives_iroh_and_lan_voice_workers() {
    friends_gateway_acceptance(true);
}

fn friends_gateway_acceptance(with_iroh: bool) {
    use relay_server::{AccountProviderConfig, AppState, ServerConfig};
    use serde_json::{json, Value};
    use sha2::{Digest, Sha256};
    use std::time::{SystemTime, UNIX_EPOCH};
    let ip = crate::invitation_probe::local_subnets()
        .into_iter()
        .find_map(|subnet| {
            let crate::invitation_probe::Subnet(std::net::IpAddr::V4(ip), _) = subnet else {
                return None;
            };
            ip.is_private().then_some(ip)
        })
        .expect("needs an own private IPv4; never weaken broker address rules");
    let folder = tempfile::tempdir().unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let udp = runtime
        .block_on(tokio::net::UdpSocket::bind((
            if with_iroh {
                std::net::Ipv4Addr::UNSPECIFIED
            } else {
                ip
            },
            0,
        )))
        .unwrap();
    let upstream = SocketAddr::from((ip, udp.local_addr().unwrap().port()));
    let tcp = runtime
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let url = format!("http://{}", tcp.local_addr().unwrap());
    const ISSUER: &str = "https://accounts.example.test/realms/relay-accounts";
    let state = AppState::open_with_config(
        folder.path(),
        "Owned Friends acceptance",
        upstream.port(),
        ServerConfig {
            people: true,
            account_provider: Some(
                AccountProviderConfig::new(ISSUER, "relay-desktop", &url).unwrap(),
            ),
            ..Default::default()
        },
    )
    .unwrap();
    // Only seed this disposable database. Real login/MLS enrollment is outside
    // this check; no identity provider is contacted, and no profile is read.
    let tokens: Vec<String> = (0..2)
        .map(|_| {
            let mut bytes = [0; 32];
            getrandom::getrandom(&mut bytes).unwrap();
            STANDARD.encode(bytes)
        })
        .collect();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    {
        let db = rusqlite::Connection::open(folder.path().join("relay.sqlite3")).unwrap();
        for (name, token) in ["local", "remote"].into_iter().zip(&tokens) {
            db.execute("INSERT INTO users(id,username,password_hash,role) VALUES(?1,?1,'account-only','member')", [name]).unwrap();
            db.execute("INSERT INTO membership(user_id) VALUES(?1)", [name])
                .unwrap();
            db.execute("INSERT INTO user_profiles(user_id) VALUES(?1)", [name])
                .unwrap();
            db.execute(
                "INSERT INTO linked_identities VALUES(?1,?2,?1,?1,?3)",
                rusqlite::params![name, ISSUER, now],
            )
            .unwrap();
            db.execute(
                "INSERT INTO sessions(token_hash,user_id,expires,created_at) VALUES(?1,?2,?3,?4)",
                rusqlite::params![
                    format!("{:x}", Sha256::digest(token.as_bytes())),
                    name,
                    now + 3600,
                    now
                ],
            )
            .unwrap();
            db.execute("INSERT INTO crypto_devices(device_id,user_id,identity,key_package,active) VALUES(?1,?2,'owned-fixture','owned-fixture',1)", rusqlite::params![format!("device-{name}"), name]).unwrap();
        }
    }
    struct Tasks(Vec<tokio::task::JoinHandle<()>>);
    impl Drop for Tasks {
        fn drop(&mut self) {
            for task in &self.0 {
                task.abort();
            }
        }
    }
    let routes = relay_server::router(state.clone());
    let ingress_state = state.clone();
    let _tasks = Tasks(vec![
        runtime.spawn(async move {
            axum::serve(
                tcp,
                routes.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        }),
        runtime.spawn(async move {
            relay_server::run_udp(udp, state).await.unwrap();
        }),
    ]);
    let http = reqwest::blocking::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap();
    let post = |path: &str, token: &str, body: Value| -> Value {
        let response = http
            .post(format!("{url}{path}"))
            .bearer_auth(token)
            .json(&body)
            .send()
            .unwrap();
        let status = response.status();
        let value: Value = response.json().unwrap();
        assert!(
            status.is_success(),
            "owned endpoint {path} refused with {status}"
        );
        value
    };
    for (from, to) in [(0, "remote"), (1, "local")] {
        post(&format!("/api/friends/{to}"), &tokens[from], Value::Null);
    }
    let bootstrap: Value = http
        .get(format!("{url}/api/bootstrap"))
        .bearer_auth(&tokens[1])
        .send()
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .unwrap();
    // Only this selected native test uses the global helper. Its own disposable
    // profile outlives all voice workers, and shutdown never touches a user app.
    struct OwnedIroh(Option<std::ffi::OsString>);
    impl Drop for OwnedIroh {
        fn drop(&mut self) {
            crate::iroh_route::shutdown();
            match self.0.take() {
                Some(value) => std::env::set_var("RELAY_PROFILE_DIR", value),
                None => std::env::remove_var("RELAY_PROFILE_DIR"),
            }
        }
    }
    let mut own_iroh = None;
    let mut ingress_task = None;
    let private = if with_iroh {
        own_iroh = Some(OwnedIroh(std::env::var_os("RELAY_PROFILE_DIR")));
        std::env::set_var(
            "RELAY_PROFILE_DIR",
            folder.path().join("owned-iroh-profile"),
        );
        let tcp = runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let peer = tcp.local_addr().unwrap().port();
        let ingress = "owned-native-friends-media-ingress-012345678901234567890";
        // A separate actual ingress router exercises the same forwarding guard
        // as production; ordinary account requests still use the other listener.
        let routes = relay_server::network_ingress_router(ingress_state.clone(), ingress.into());
        ingress_task = Some(Tasks(vec![runtime.spawn(async move {
            axum::serve(
                tcp,
                routes.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        })]));
        Some(
            crate::iroh_route::media_fixture_host(
                peer,
                upstream.port(),
                ingress,
                bootstrap["server_id"].as_str().unwrap(),
            )
            .unwrap(),
        )
    } else {
        None
    };
    assert!(bootstrap["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v == "friends-gateway-broker-v1"));
    let call = post(
        "/api/private-calls",
        &tokens[0],
        json!({"peer_id":"remote"}),
    );
    let call_id = call["id"].as_str().unwrap();
    let channel = call["channel_id"].as_str().unwrap();
    let admitted = post(
        &format!("/api/private-calls/{call_id}/join"),
        &tokens[1],
        json!({
            "instance_id":bootstrap["instance_id"], "join":{"channel_id":channel,"device_id":"device-remote"},
            "replace_session_id":null, "state":{"muted":false,"deafened":false,"sharing":false,"camera":false}
        }),
    );
    let second: VoiceTicket = serde_json::from_value(admitted["ticket"].clone()).unwrap();
    let first: VoiceTicket = serde_json::from_value(post(
        "/api/voice/join",
        &tokens[0],
        json!({"channel_id":channel,"device_id":"device-local"}),
    ))
    .unwrap();
    let mut owners = Vec::new();
    let mut captures = Vec::new();
    let mut playbacks = Vec::new();
    let mut stats = Vec::new();
    let mut toggles = Vec::new();
    let mut errors = Vec::new();
    for (index, ticket) in [first, second].into_iter().enumerate() {
        // The shared fixture initializes a loopback-connected socket. Windows
        // cannot connect that socket to a private NIC; replace it before use.
        let (mut worker, _, counters) =
            probe_fixture(SocketAddr::from(([127, 0, 0, 1], upstream.port())));
        let private_caller = private.as_ref().filter(|_| index == 1);
        let remote = private_caller.map_or(upstream, |(_, _, port)| {
            SocketAddr::from(([127, 0, 0, 1], *port))
        });
        worker.remote = remote;
        // The production caller uses an unconnected wildcard socket so a
        // loopback helper and an approved LAN gateway can share the call port.
        worker.socket = UdpSocket::bind("0.0.0.0:0").unwrap();
        worker.socket.set_nonblocking(true).unwrap();
        worker.session_id = ticket.session_id;
        worker.wire_version = ticket.wire_version;
        worker.cipher =
            ChaCha20Poly1305::new_from_slice(&STANDARD.decode(&ticket.key).unwrap()).unwrap();
        worker.call =
            CallCipher::new(test_call_keys(if index == 0 { "local" } else { "remote" })).unwrap();
        worker
            .controls
            .gateway_enabled
            .store(true, Ordering::Release);
        worker
            .controls
            .transmission
            .store(TransmissionMode::Continuous as u32, Ordering::Release);
        worker.controls.suppression.store(false, Ordering::Relaxed);
        worker.controls.packet_samples.store(480, Ordering::Relaxed);
        worker.muted.store(false, Ordering::Release);
        worker.deafened.store(false, Ordering::Release);
        let control = private_caller.map_or(url.as_str(), |(_, http, _)| http.as_str());
        let config = crate::gateway_control::Config::new(ticket.session_id, |method, path| {
            Ok(http
                .request(method, format!("{control}{path}"))
                .bearer_auth(&tokens[index]))
        })
        .unwrap();
        let config = match private_caller {
            Some((saved, _, _)) => config.with_private_route(
                saved.clone(),
                bootstrap["server_id"].as_str().unwrap().into(),
            ),
            None => config,
        };
        worker.gateway = Some(
            crate::gateway_control::Session::start(
                config,
                remote,
                worker.controls.gateway_enabled.clone(),
                worker.stop.clone(),
            )
            .unwrap(),
        );
        captures.push(worker.capture.clone());
        playbacks.push(worker.playback.clone());
        toggles.push(worker.controls.gateway_enabled.clone());
        errors.push(worker.errors.clone());
        stats.push(counters);
        owners.push(OwnedVoiceWorker::start(worker));
    }
    let tone: Vec<f32> = (0..960).map(|n| (n as f32 * 0.09).sin() * 0.2).collect();
    let mut gateway_seen = None;
    let mut next_tone = Instant::now();
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        // Explicit synthetic slow-route estimate exercises route preference.
        // This is not a measured latency comparison or performance claim.
        for counters in &stats {
            counters.udp_rtt_us.store(100_000, Ordering::Relaxed);
        }
        if Instant::now() >= next_tone {
            for capture in &captures {
                capture.lock().unwrap().extend(tone.iter().copied());
            }
            next_tone = Instant::now() + Duration::from_millis(20);
        }
        if stats
            .iter()
            .all(|s| s.gateway_active.load(Ordering::Acquire))
        {
            let began = gateway_seen.get_or_insert_with(Instant::now);
            if began.elapsed() > Duration::from_millis(500)
                && playbacks
                    .iter()
                    .all(|p| p.lock().unwrap().iter().any(|v| v.abs() > 0.01))
            {
                break;
            }
        }
        thread::sleep(Duration::from_millis(5));
    }
    assert!(
        gateway_seen.is_some()
            && stats
                .iter()
                .all(|s| s.gateway_active.load(Ordering::Acquire)),
        "both broker-managed gateway paths selected; stats={:?}",
        stats.iter().map(|s| s.snapshot()).collect::<Vec<_>>()
    );
    assert!(
        playbacks
            .iter()
            .all(|p| p.lock().unwrap().iter().any(|v| v.abs() > 0.01)),
        "both actual workers decode the other's generated audio"
    );
    // Opt-out releases offered/used routes, preserving the same admitted call.
    let before: Vec<u64> = stats
        .iter()
        .map(|s| s.received.load(Ordering::Relaxed))
        .collect();
    for toggle in &toggles {
        toggle.store(false, Ordering::Release);
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline
        && !stats.iter().enumerate().all(|(i, s)| {
            !s.gateway_active.load(Ordering::Acquire)
                && s.received.load(Ordering::Relaxed) > before[i] + 3
        })
    {
        for capture in &captures {
            capture.lock().unwrap().extend(tone.iter().copied());
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert!(
        stats
            .iter()
            .enumerate()
            .all(|(i, s)| !s.gateway_active.load(Ordering::Acquire)
                && s.received.load(Ordering::Relaxed) > before[i] + 3),
        "opt-out keeps the accepted call audible through its ordinary route"
    );
    post(
        &format!("/api/private-calls/{call_id}"),
        &tokens[0],
        json!({"action":"end"}),
    );
    let response = http
        .get(format!("{url}/api/hosting/voice-gateways"))
        .bearer_auth(&tokens[1])
        .send()
        .unwrap();
    assert_eq!(
        response.status().as_u16(),
        403,
        "ended call has no gateway authority"
    );
    for owner in &mut owners {
        owner.finish();
    }
    assert!(
        errors.iter().all(|e| e.lock().unwrap().is_none()),
        "workers ended without transport errors"
    );
    if let Some((saved, expected_http, expected_voice)) = &private {
        let (http, voice) = crate::iroh_route::new_voice_route(&saved.address()).unwrap();
        assert_eq!(
            (&http, voice),
            (expected_http, *expected_voice),
            "gateway opt-out never replaces the ordinary iroh sockets"
        );
    }
    drop(owners);
    drop(own_iroh);
    drop(ingress_task);
}
