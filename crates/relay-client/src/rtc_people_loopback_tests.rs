//! Actual callback/quality check against an explicit disposable local stack.
//! This shares the generated peer and observer with the real Friends opt-in,
//! while keeping local fixture keys/admission distinct from global Friends QA.
use super::*;
use std::{
    io::{Read, Write},
    path::{Component, Path, PathBuf, Prefix},
};

const PURPOSE: &str = "relay-owned-loopback-media-quality-v1";

fn fixture_path() -> PathBuf {
    let path = PathBuf::from(
        std::env::var_os("RELAY_LOCAL_QUALITY_FIXTURE")
            .expect("Explicit owned local quality fixture required"),
    );
    assert!(
        path.is_absolute()
            && matches!(path.components().next(),
        Some(Component::Prefix(p)) if matches!(p.kind(), Prefix::Disk(b'C' | b'D')))
    );
    assert!(!path
        .components()
        .any(|p| matches!(p, Component::ParentDir | Component::CurDir)));
    assert_eq!(
        path.file_name().and_then(|s| s.to_str()),
        Some("private-quality-tickets.json")
    );
    for parent in path.ancestors().collect::<Vec<_>>().into_iter().rev() {
        use std::os::windows::fs::MetadataExt;
        let metadata = std::fs::symlink_metadata(parent).unwrap();
        assert!(!metadata.file_type().is_symlink() && metadata.file_attributes() & 0x400 == 0);
    }
    assert!(std::fs::metadata(&path).unwrap().len() <= 32_768);
    path
}

fn local_url(value: &str, scheme: &str, port: Option<u16>) -> bool {
    url::Url::parse(value).is_ok_and(|url| {
        url.scheme() == scheme
            && url.host_str() == Some("127.0.0.1")
            && url.port().is_some()
            && port.is_none_or(|port| url.port() == Some(port))
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && url.path() == "/"
    })
}

fn owned_turn_tcp_url(fixture: &Value) -> Result<String, String> {
    let turn = &fixture["turn_tcp"];
    let port = turn["port"]
        .as_u64()
        .filter(|port| (1024..=65535).contains(port))
        .ok_or("Owned TURN TCP port invalid")?;
    let address = turn["owned_host_ipv4"]
        .as_str()
        .and_then(|value| value.parse::<std::net::Ipv4Addr>().ok())
        .filter(|address| address.is_loopback() || address.is_private())
        .ok_or("Owned TURN interface must be local IPv4")?;
    let address_text = address.to_string();
    if !fixture["owned_media_addresses"]
        .as_array()
        .is_some_and(|rows| {
            rows.iter()
                .any(|row| row.as_str() == Some(address_text.as_str()))
        })
    {
        return Err("Owned TURN interface missing from controller socket proof".into());
    }
    Ok(format!("turn:{address}:{port}?transport=tcp"))
}

async fn request(
    base: &str,
    path: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> Result<Value, String> {
    let (base, path, token) = (base.to_owned(), path.to_owned(), token.map(str::to_owned));
    tokio::task::spawn_blocking(move || {
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(5))
            .build()
            .map_err(|_| "Owned local HTTP initialization failed")?;
        let mut request = if body.is_some() {
            client.post(format!("{base}{path}"))
        } else {
            client.get(format!("{base}{path}"))
        };
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request
            .send()
            .map_err(|_| "Owned local HTTP connection failed")?;
        if !response.status().is_success() {
            return Err("Owned local HTTP admission rejected".into());
        }
        let mut raw = Vec::new();
        response
            .take(65_537)
            .read_to_end(&mut raw)
            .map_err(|_| "Owned local HTTP read failed")?;
        if raw.len() > 65_536 {
            return Err("Owned local HTTP response exceeded bound".into());
        }
        serde_json::from_slice(&raw).map_err(|_| "Owned local HTTP JSON failed".into())
    })
    .await
    .map_err(|_| "Owned local HTTP worker failed".to_owned())?
}

async fn connect(fixture: &Value, name: &str) -> Result<Peer, String> {
    let base = fixture["base"].as_str().unwrap();
    let token = fixture["users"][name]["session"]["token"].as_str().unwrap();
    let channel = fixture["room_id"].as_str().unwrap();
    request(
        base,
        "/api/voice/join",
        Some(token),
        Some(json!({"channel_id":channel})),
    )
    .await?;
    let ticket: RtcTicket = serde_json::from_value(
        request(
            base,
            "/api/rtc/token",
            Some(token),
            Some(json!({"channel_id":channel})),
        )
        .await?,
    )
    .map_err(|_| "Owned local RTC ticket failed")?;
    if !local_url(&ticket.url, "ws", url::Url::parse(base).unwrap().port()) {
        return Err("Owned local RTC destination changed".into());
    }
    let keys = super::super::integration_tests::fixture_call_keys(fixture, name);
    let mut options = protected_room_options(&keys)?;
    options.join_retries = 0;
    options.rtc_config.ice_transport_type = IceTransportsType::All;
    // A nonempty explicit local ICE list prevents the SDK from adopting the
    // SFU's fallback public STUN list. This is the owned SFU's ICE UDP endpoint;
    // local host candidates provide the route, without external discovery.
    let ice_port = fixture["sfu_udp_port"]
        .as_u64()
        .filter(|port| (1024..=65535).contains(port))
        .ok_or("Owned local ICE endpoint missing")?;
    options.rtc_config.ice_servers = vec![livekit::webrtc::peer_connection_factory::IceServer {
        urls: vec![format!("stun:127.0.0.1:{ice_port}")],
        username: String::new(),
        password: String::new(),
    }];
    if let Some(turn) = fixture.get("turn_tcp") {
        let url = owned_turn_tcp_url(fixture)?;
        let credential = &turn["credentials"][name];
        let username = credential["username"]
            .as_str()
            .filter(|value| !value.is_empty() && value.len() <= 1024)
            .ok_or("Owned TURN username missing")?;
        let password = credential["password"]
            .as_str()
            .filter(|value| !value.is_empty() && value.len() <= 1024)
            .ok_or("Owned TURN password missing")?;
        // This explicit fixture is plain TCP to the isolated SFU's TLS-proxy
        // backend. It changes no system trust and cannot select a direct path.
        options.rtc_config.ice_transport_type = IceTransportsType::Relay;
        options.rtc_config.ice_servers =
            vec![livekit::webrtc::peer_connection_factory::IceServer {
                urls: vec![url],
                username: username.to_owned(),
                password: password.to_owned(),
            }];
    }
    let (room, events) = tokio::time::timeout(
        Duration::from_secs(20),
        Room::connect(&ticket.url, &ticket.token, options),
    )
    .await
    .map_err(|_| "Owned local RTC deadline")?
    .map_err(|error| match error {
        livekit::RoomError::Engine(livekit::rtc_engine::EngineError::IceTimeout(failure)) => {
            // The pinned SDK builds this summary from address-free numeric
            // counters; never format the general SDK error or its peer list.
            format!("Owned local RTC ICE timeout: {}", failure.summary)
        }
        livekit::RoomError::Engine(livekit::rtc_engine::EngineError::Signal(_)) => {
            "Owned local RTC signaling failed".into()
        }
        _ => "Owned local RTC join failed".into(),
    })?;
    if room.local_participant().identity().to_string() != keys.local_user_id {
        let _ = room.close().await;
        return Err("Owned local RTC identity changed".into());
    }
    Ok(Peer::from_connected(room, events, keys))
}

async fn selected_owned_paths(room: &Room, fixture: &Value) -> Result<Value, String> {
    if fixture.get("turn_tcp").is_some() {
        return selected_owned_turn_tcp_paths(room, fixture).await;
    }
    // The pinned native client can exclude loopback ICE interfaces. Accept a
    // machine's own interface address only after the controller independently
    // checked that this exact SFU process owns both configured media sockets.
    let values = fixture["owned_media_addresses"]
        .as_array()
        .ok_or("Owned local interface snapshot missing")?;
    if values.is_empty() || values.len() > 128 {
        return Err("Owned local interface snapshot invalid".into());
    }
    let allowed = values
        .iter()
        .map(|value| {
            value
                .as_str()
                .and_then(|v| v.parse::<std::net::IpAddr>().ok())
        })
        .collect::<Option<HashSet<_>>>()
        .ok_or("Owned local interface address invalid")?;
    let stats = tokio::time::timeout(Duration::from_secs(3), room.get_stats())
        .await
        .map_err(|_| "Owned local selected-path deadline")?
        .map_err(|_| "Owned local selected-path statistics missing")?;
    let mut output = serde_json::Map::new();
    for (name, rows) in
        selected_media_transport_scopes(room.is_single_peer_connection_active(), &stats)?
    {
        let ids: HashSet<_> = rows
            .iter()
            .filter_map(|row| match row {
                RtcStats::Transport(t) if !t.transport.selected_candidate_pair_id.is_empty() => {
                    Some(t.transport.selected_candidate_pair_id.as_str())
                }
                _ => None,
            })
            .collect();
        let mut count = 0;
        let mut address_observations = Vec::new();
        for row in rows {
            let RtcStats::CandidatePair(pair) = row else {
                continue;
            };
            if !ids.contains(pair.rtc.id.as_str())
                || !pair.candidate_pair.nominated
                || pair.candidate_pair.state != Some(IceCandidatePairState::Succeeded)
            {
                continue;
            }
            let remote = rows
                .iter()
                .find_map(|row| match row {
                    RtcStats::RemoteCandidate(c)
                        if c.rtc.id == pair.candidate_pair.remote_candidate_id =>
                    {
                        Some(&c.remote_candidate)
                    }
                    _ => None,
                })
                .ok_or("Owned selected remote candidate missing")?;
            // Export only fixed diagnostic labels. Candidate strings, IDs and
            // addresses stay in memory, including when the native SDK withholds IPs.
            use livekit::webrtc::stats::IceCandidateType;
            let candidate_kind = match remote.candidate_type {
                Some(IceCandidateType::Host) => "host",
                Some(IceCandidateType::Srflx) => "server_reflexive",
                Some(IceCandidateType::Prflx) => "peer_reflexive",
                Some(IceCandidateType::Relay) => "relay",
                None => "unknown",
            };
            let transport = match remote.protocol.as_str() {
                "udp" => "udp",
                "tcp" => "tcp",
                _ => "unknown",
            };
            let owned_port_matches = match remote.protocol.as_str() {
                "udp" => Some(i64::from(remote.port)) == fixture["sfu_udp_port"].as_i64(),
                "tcp" => Some(i64::from(remote.port)) == fixture["sfu_tcp_port"].as_i64(),
                _ => false,
            };
            let observed = &remote.address_observation;
            let resolved = observed.resolve_literal().map_err(|error| {
                format!(
                    "Owned candidate address unresolved: kind={}, protocol={}, owned_port={}, modern={}, legacy={}, reason={}",
                    candidate_kind,
                    transport,
                    owned_port_matches,
                    observed.modern_presence().label(),
                    observed.legacy_presence().label(),
                    error.label()
                )
            })?;
            let address = resolved.literal_ip();
            if !allowed.contains(&address) {
                return Err("Owned selected candidate address is not assigned to this host".into());
            }
            if match remote.protocol.as_str() {
                "udp" => Some(i64::from(remote.port)) != fixture["sfu_udp_port"].as_i64(),
                "tcp" => Some(i64::from(remote.port)) != fixture["sfu_tcp_port"].as_i64(),
                _ => true,
            } {
                return Err(
                    "Owned selected candidate transport or port does not match this SFU".into(),
                );
            }
            if pair
                .candidate_pair
                .bytes_sent
                .saturating_add(pair.candidate_pair.bytes_received)
                == 0
            {
                return Err(
                    "Owned native media did not select this SFU's active local socket".into(),
                );
            }
            address_observations.push(json!({"modern_presence":observed.modern_presence().label(),
                "legacy_presence":observed.legacy_presence().label(),"resolved_from":resolved.origin().label()}));
            count += 1;
        }
        if count == 0 {
            return Err("Owned local publisher/subscriber path is unproven".into());
        }
        output.insert(
            name.into(),
            json!({"selected_active_pairs":count,"remote_is_owned_host_interface":true,"remote_port_matches_owned_sfu":true,
                "address_observations":address_observations}),
        );
    }
    Ok(Value::Object(output))
}

async fn selected_owned_turn_tcp_paths(room: &Room, fixture: &Value) -> Result<Value, String> {
    use livekit::webrtc::stats::IceCandidateType;
    let expected_url = owned_turn_tcp_url(fixture)?;
    let stats = tokio::time::timeout(Duration::from_secs(3), room.get_stats())
        .await
        .map_err(|_| "Owned TURN selected-path deadline")?
        .map_err(|_| "Owned TURN selected-path statistics unavailable")?;
    let mut output = serde_json::Map::new();
    for (name, rows) in
        selected_media_transport_scopes(room.is_single_peer_connection_active(), &stats)?
    {
        let selected: HashSet<_> = rows
            .iter()
            .filter_map(|row| match row {
                RtcStats::Transport(t) if !t.transport.selected_candidate_pair_id.is_empty() => {
                    Some(t.transport.selected_candidate_pair_id.as_str())
                }
                _ => None,
            })
            .collect();
        let mut paths = Vec::new();
        for row in rows {
            let RtcStats::CandidatePair(pair) = row else {
                continue;
            };
            if !selected.contains(pair.rtc.id.as_str())
                || !pair.candidate_pair.nominated
                || pair.candidate_pair.state != Some(IceCandidatePairState::Succeeded)
            {
                continue;
            }
            let candidate = rows
                .iter()
                .find_map(|row| match row {
                    RtcStats::LocalCandidate(c)
                        if c.rtc.id == pair.candidate_pair.local_candidate_id =>
                    {
                        Some(&c.local_candidate)
                    }
                    _ => None,
                })
                .ok_or("Owned selected TURN candidate missing")?;
            // Local relay provenance remains observable even when the native
            // SDK redacts remote IPs. Require the exact own interface URL and
            // actual nominated transport; never infer ownership from a port.
            let protocol = format!("{:?}", candidate.relay_protocol);
            if candidate.candidate_type != Some(IceCandidateType::Relay)
                || candidate.url != expected_url
                || protocol != "Some(Tcp)"
                || pair.candidate_pair.bytes_sent == 0
                || pair.candidate_pair.bytes_received == 0
            {
                return Err("Native media did not prove the exact owned TURN TCP route".into());
            }
            paths.push(
                json!({"selected_nominated_relay":true,"exact_owned_host_url":true,
                "relay_protocol":"tcp","bytes_sent":pair.candidate_pair.bytes_sent,
                "bytes_received":pair.candidate_pair.bytes_received}),
            );
        }
        if paths.is_empty() {
            return Err("Every negotiated media transport requires owned TURN TCP evidence".into());
        }
        output.insert(name.into(), Value::Array(paths));
    }
    Ok(Value::Object(output))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "Explicit owned local stack, generated audio/video and 120/180-second recovery check; no devices or public service"]
async fn actual_loopback_native_quality_observations_and_recovery() {
    let _serial = super::super::integration_tests::SFU_TEST_LOCK.lock().await;
    crate::tls_crypto::initialize();
    let path = fixture_path();
    let fixture: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(fixture["schema"], 1);
    assert_eq!(fixture["purpose"], PURPOSE);
    let transport_cut = match fixture.get("recovery_mode").and_then(Value::as_str) {
        None => false,
        Some("owned_turn_tcp_close") => true,
        _ => panic!("Unknown owned local recovery mode"),
    };
    assert!(!transport_cut || fixture.get("turn_tcp").is_some());
    for field in ["run_id", "server_id", "room_id", "other_id"] {
        assert!(uuid::Uuid::parse_str(fixture[field].as_str().unwrap()).is_ok());
    }
    let base = fixture["base"].as_str().unwrap();
    assert!(local_url(base, "http", None));
    for name in ["alice", "bob", "eve"] {
        assert_eq!(fixture["users"][name]["session"]["user"]["username"], name);
        assert!(uuid::Uuid::parse_str(
            fixture["users"][name]["session"]["user"]["id"]
                .as_str()
                .unwrap()
        )
        .is_ok());
        let token = fixture["users"][name]["session"]["token"].as_str().unwrap();
        assert!(!token.is_empty() && token.len() < 16_384);
    }
    let health = request(base, "/api/health", None, None).await.unwrap();
    assert_eq!(health["server_id"], fixture["server_id"]);
    let mut peers = Vec::new();
    let mut quality = quality::QualityProbe::new(Instant::now());
    let mut paths = Vec::new();
    let mut cut_ack = None;
    let mut cut_before = Vec::new();
    let mut recovery_elapsed_ms = None;
    let result: Result<(), String> = async {
        peers.push(connect(&fixture, "alice").await?);
        peers.push(connect(&fixture, "bob").await?);
        let [a, b] = peers.as_mut_slice() else {
            return Err("Owned local peer count changed".into());
        };
        a.publish_generated().await?;
        b.publish_generated().await?;
        let started = Instant::now();
        quality = quality::QualityProbe::new(started);
        let mut index = 0_u64;
        let mut recovery = false;
        let mut initial_paths_checked = false;
        let duration = Duration::from_secs(if transport_cut { 180 } else { 120 });
        let mut cut_counts = None;
        let mut recovery_began = None;
        while started.elapsed() < duration {
            a.poll()?;
            b.poll()?;
            quality.tick(a, b);
            a.feed(index, index.is_multiple_of(10)).await?;
            b.feed(index, index.is_multiple_of(10)).await?;
            if !initial_paths_checked && started.elapsed() >= Duration::from_secs(8) {
                for peer in [&*a, &*b] {
                    paths.push(selected_owned_paths(&peer.room, &fixture).await?);
                }
                initial_paths_checked = true;
            }
            if started.elapsed() >= Duration::from_secs(32) && !recovery {
                let summary = quality.summary();
                let impossible =
                    summary["directions"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|direction| {
                            direction["before_recovery"]
                                .as_array()
                                .unwrap()
                                .iter()
                                .any(|window| {
                                    window["assessment"] == "unknown"
                                        && window["unknown_reason"].as_str().is_some_and(|reason| {
                                            reason != "insufficient_sustained_window"
                                        })
                                })
                        });
                if impossible {
                    return Err("Owned actual native observation contract remains unknown".into());
                }
            }
            if !recovery && started.elapsed() >= Duration::from_secs(70) {
                quality.recovery_started();
                recovery = true;
                recovery_began = Some(Instant::now());
                if transport_cut {
                    cut_counts = Some([(a.frames, a.audio_samples), (b.frames, b.audio_samples)]);
                    cut_before = [&*a, &*b].into_iter().map(Peer::diagnostic_counts).collect();
                    let request = json!({"schema":1,"run_id":fixture["run_id"],"operation":"close_owned_turn_tcp"});
                    let target = path.parent().unwrap().join("turn-cut-request.json");
                    std::fs::OpenOptions::new().write(true).create_new(true).open(target)
                        .map_err(|_| "Owned TCP cut request could not be created")?
                        .write_all(&serde_json::to_vec(&request).unwrap())
                        .map_err(|_| "Owned TCP cut request could not be written")?;
                } else {
                    for peer in [&*a, &*b] {
                        tokio::time::timeout(
                            Duration::from_secs(3),
                            peer.room
                                .simulate_scenario(livekit::SimulateScenario::SignalReconnect),
                        )
                        .await
                        .map_err(|_| "Owned local reconnect deadline")?
                        .map_err(|_| "Owned local reconnect failed")?;
                    }
                }
            }
            if transport_cut && recovery {
                let since = recovery_began.unwrap().elapsed();
                if cut_ack.is_none() && index.is_multiple_of(100) {
                    let ack_path = path.parent().unwrap().join("turn-cut-applied.json");
                    match std::fs::symlink_metadata(&ack_path) {
                        Ok(metadata) => {
                            use std::os::windows::fs::MetadataExt;
                            if !metadata.is_file() || metadata.file_attributes() & 0x400 != 0 || metadata.len() > 16_384 {
                                return Err("Owned TCP cut acknowledgement storage invalid".into());
                            }
                            let value: Value = serde_json::from_slice(&std::fs::read(ack_path)
                                .map_err(|_| "Owned TCP cut acknowledgement unreadable")?)
                                .map_err(|_| "Owned TCP cut acknowledgement invalid")?;
                            if value["schema"] != 1 || value["run_id"] != fixture["run_id"]
                                || value["operation"] != "close_owned_turn_tcp"
                                || value["owned_connections_closed"].as_u64().unwrap_or(0) < 2
                            {
                                return Err("Owned TCP cut did not close both peers' streams".into());
                            }
                            cut_ack = Some(value);
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(_) => return Err("Owned TCP cut acknowledgement metadata failed".into()),
                    }
                }
                if since > Duration::from_secs(5) && cut_ack.is_none() {
                    return Err("Owned TCP cut acknowledgement deadline".into());
                }
                let before = cut_counts.as_ref().unwrap();
                if cut_ack.is_some() && recovery_elapsed_ms.is_none()
                    && [&*a, &*b].into_iter().zip(before).all(|(peer, (frames, samples))| {
                        peer.reconnecting && peer.reconnected
                            && peer.frames.iter().zip(frames).all(|(now, old)| *now >= *old + 10)
                            && peer.audio_samples >= *samples + 48_000
                    })
                {
                    recovery_elapsed_ms = Some(since.as_millis());
                }
                if since > Duration::from_secs(60) && recovery_elapsed_ms.is_none() {
                    return Err("Owned TCP media did not recover within the local observation deadline".into());
                }
            }
            index += 1;
            tokio::time::sleep_until((started + Duration::from_millis(index * 10)).into()).await;
        }
        for peer in [&*a, &*b] {
            if !peer.reconnecting
                || !peer.reconnected
                || peer.frames.iter().any(|n| *n < 100)
                || peer.audio_samples < 48_000
            {
                return Err("Owned local generated media/recovery insufficient".into());
            }
            paths.push(selected_owned_paths(&peer.room, &fixture).await?);
        }
        Ok(())
    }
    .await;
    let summary = quality.summary();
    drop(quality);
    let counts = peers
        .iter()
        .map(Peer::diagnostic_counts)
        .collect::<Vec<_>>();
    let mut cleanup = Vec::new();
    for peer in &mut peers {
        let unpublish = peer.unpublish().await.is_ok();
        let close = matches!(
            tokio::time::timeout(Duration::from_secs(15), peer.room.close()).await,
            Ok(Ok(()))
        );
        cleanup.push(unpublish && close);
    }
    let accepted = result.is_ok()
        && cleanup.len() == 2
        && cleanup.iter().all(|v| *v)
        && summary["scenario_assessment"] == "pass";
    let route = if fixture.get("turn_tcp").is_some() {
        "owned_same_host_turn_tcp_without_tls"
    } else {
        "direct_owned_media_sockets"
    };
    let record = json!({"schema":1,"purpose":PURPOSE,"route":route,"scope":"one Windows PC; isolated community/SFU with generated fixture keys, not global Friends admission or public TURN/TLS",
        "recovery_mode":if transport_cut {"actual_owned_turn_tcp_close"} else {"sdk_signal_reconnect"},
        "tcp_cut_acknowledgement":cut_ack,"decoded_before_tcp_cut":cut_before,"tcp_media_recovery_observed_ms":recovery_elapsed_ms,
        "primary_probe_passed":result.is_ok(),"primary_error":result.as_ref().err(),"cleanup":cleanup,
        "local_generated_scenario_passed":accepted,"per_source_quality_observations":summary,"decoded_counts":counts,"selected_media_paths":paths,
        "physical_devices_opened":false,"public_service_accessed":false,"two_pc_wan_or_listening_accepted":false});
    // Keep the same fixed report bound as the shared observation writer.
    // Whitespace must not hide a failed native measurement's actual reason.
    let raw = serde_json::to_vec(&record).unwrap();
    assert!(raw.len() <= 65_536);
    let output = Path::new(&path)
        .parent()
        .unwrap()
        .join("native-quality-loopback.json");
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)
        .unwrap()
        .write_all(&raw)
        .unwrap();
    assert!(accepted,"Owned local native quality scenario was not accepted; inspect the bounded redacted receipt");
}
