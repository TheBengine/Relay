//! Explicit native SDK acceptance over owned, fake userspace TURN/UDP bridges.
//! No account login, physical capture, playback, adapter, or firewall changes.
use super::*;
use livekit::webrtc::native::frame_cryptor::EncryptionState;
use livekit::webrtc::{
    peer_connection_factory::{IceServer, IceTransportsType},
    stats::{IceCandidatePairState, IceCandidateType, IceServerTransportProtocol, RtcStats},
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{collections::HashSet, net::Ipv4Addr, path::PathBuf};

#[derive(Deserialize)]
struct EmbeddedTurn {
    url: String,
    username: String,
    password: String,
}

#[derive(Deserialize)]
struct EmbeddedPeer {
    identity: String,
    virtual_ip: String,
    url: String,
    token: String,
    turn: EmbeddedTurn,
}

#[derive(Deserialize)]
struct EmbeddedFixture {
    transport: String,
    fixture_id: String,
    room: String,
    host_virtual_ip: String,
    media_udp_port: u16,
    evidence_filename: String,
    alice: EmbeddedPeer,
    bob: EmbeddedPeer,
    #[serde(default)]
    open_server: Option<OpenServerFixture>,
    #[serde(default)]
    detail_video: bool,
    #[serde(default)]
    diagnostic_bitrate_mbps: u32,
}

#[derive(Deserialize)]
struct OpenServerFixture {
    url: String,
    channel: String,
    revision: u64,
    alice_session: relay_protocol::Session,
    bob_session: relay_protocol::Session,
}

fn fixture_path() -> Result<PathBuf, String> {
    let path =
        PathBuf::from(std::env::var_os("RELAY_EMBEDDED_FIXTURE_JSON").ok_or(
            "RELAY_EMBEDDED_FIXTURE_JSON must name an owned fixture JSON on local C: or D:",
        )?);
    let mut components = path.components();
    let permitted = matches!(components.next(), Some(std::path::Component::Prefix(prefix))
        if matches!(prefix.kind(), std::path::Prefix::Disk(letter) | std::path::Prefix::VerbatimDisk(letter)
            if letter.eq_ignore_ascii_case(&b'C') || letter.eq_ignore_ascii_case(&b'D')));
    if !permitted || !path.is_absolute() {
        return Err("Embedded fixture must use an absolute local C: or D: path".into());
    }
    for ancestor in path.ancestors() {
        let metadata = std::fs::symlink_metadata(ancestor)
            .map_err(|_| "Cannot verify the owned embedded fixture path")?;
        if metadata.file_type().is_symlink() {
            return Err("Embedded fixture paths cannot follow redirects".into());
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if metadata.file_attributes() & 0x400 != 0 {
                return Err("Embedded fixture paths cannot follow Windows reparse points".into());
            }
        }
    }
    let metadata =
        std::fs::metadata(&path).map_err(|_| "Cannot read the owned embedded fixture")?;
    if !metadata.is_file() || metadata.len() > 64 * 1024 {
        return Err("Embedded fixture must be a JSON file of at most 64 KiB".into());
    }
    Ok(path)
}

fn result_path(path: &std::path::Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".result.json");
    PathBuf::from(name)
}

fn write_result(path: &std::path::Path, evidence: &Value) -> Result<(), String> {
    use std::io::Write;
    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        if metadata.file_type().is_symlink() {
            return Err("Owned result file cannot follow a redirect".into());
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if metadata.file_attributes() & 0x400 != 0 {
                return Err("Owned result file cannot follow a reparse point".into());
            }
        }
    }
    let mut temporary_name = path.as_os_str().to_os_string();
    temporary_name.push(format!(".new-{}", uuid::Uuid::new_v4()));
    let temporary = PathBuf::from(temporary_name);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|_| "Create owned temporary acceptance result")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|_| "Protect owned acceptance result")?;
    }
    let bytes = serde_json::to_vec_pretty(evidence).map_err(|_| "Encode acceptance result")?;
    file.write_all(&bytes)
        .and_then(|_| file.sync_all())
        .map_err(|_| "Write owned temporary acceptance result")?;
    drop(file);
    std::fs::rename(&temporary, path).map_err(|_| "Commit atomic owned acceptance result")?;
    Ok(())
}

fn virtual_ipv4(value: &str) -> Result<Ipv4Addr, String> {
    let ip: Ipv4Addr = value.parse().map_err(|_| "Invalid fake userspace IPv4")?;
    let bytes = ip.octets();
    if bytes[0] != 100 || !(64..=127).contains(&bytes[1]) || ip.to_string() != value {
        return Err("Fixture virtual addresses must be canonical IPv4 in 100.64.0.0/10".into());
    }
    Ok(ip)
}

fn check_owned_port(port: u16) -> Result<(), String> {
    if port == 0 || [41128, 37724, 41688].contains(&port) {
        return Err("Fixture must use its own nonzero ports, separate from live hosts".into());
    }
    Ok(())
}

fn validate_peer(peer: &EmbeddedPeer) -> Result<(), String> {
    virtual_ipv4(&peer.virtual_ip)?;
    let signal = reqwest::Url::parse(&peer.url).map_err(|_| "Invalid fixture signaling URL")?;
    if signal.scheme() != "ws"
        || signal.host_str() != Some("127.0.0.1")
        || !signal.username().is_empty()
        || signal.password().is_some()
        || signal.query().is_some()
        || signal.fragment().is_some()
    {
        return Err("Fixture signaling must use its owned plain loopback WebSocket".into());
    }
    check_owned_port(
        signal
            .port()
            .ok_or("Fixture signaling needs an explicit owned port")?,
    )?;
    let port = peer
        .turn
        .url
        .strip_prefix("turn:127.0.0.1:")
        .and_then(|value| value.strip_suffix("?transport=tcp"))
        .and_then(|value| value.parse::<u16>().ok())
        .ok_or("Fixture TURN must use its owned loopback TCP port")?;
    check_owned_port(port)?;
    if peer.identity.is_empty()
        || peer.identity.len() > 128
        || peer.token.is_empty()
        || peer.token.len() > 16 * 1024
        || peer.turn.username.is_empty()
        || peer.turn.username.len() > 256
        || !(16..=256).contains(&peer.turn.password.len())
    {
        return Err("Fixture identity, TURN credentials or isolated SFU token is invalid".into());
    }
    Ok(())
}

fn call_keys(fixture: &EmbeddedFixture, local: &EmbeddedPeer) -> CallKeys {
    CallKeys {
        group_id: format!("owned-embedded-{}", fixture.fixture_id),
        scope: fixture.room.clone(),
        epoch: 9,
        local_user_id: local.identity.clone(),
        local_device_id: format!("owned-embedded-device-{}", local.identity),
        sender_keys: [(&fixture.alice, 72_u8), (&fixture.bob, 84_u8)]
            .into_iter()
            .map(|(peer, byte)| {
                (
                    peer.identity.clone(),
                    crate::e2ee::SenderKeys {
                        device_id: format!("owned-embedded-device-{}", peer.identity),
                        opus: zeroize::Zeroizing::new([byte; 32]),
                        rtc: zeroize::Zeroizing::new([byte + 1; 32]),
                    },
                )
            })
            .collect(),
        voice_replay: Mutex::default(),
    }
}

fn native_options(keys: &CallKeys, peer: &EmbeddedPeer) -> Result<RoomOptions, String> {
    validate_peer(peer)?;
    let mut options = protected_room_options(keys)?;
    options.join_retries = 0;
    options.rtc_config.ice_transport_type = IceTransportsType::Relay;
    options.rtc_config.enable_loopback_turn = true;
    options.rtc_config.ice_servers = vec![IceServer {
        urls: vec![peer.turn.url.clone()],
        username: peer.turn.username.clone(),
        password: peer.turn.password.clone(),
    }];
    if options.encryption.as_ref().map(|e| e.encryption_type) != Some(EncryptionType::Gcm) {
        return Err("Fixture cannot downgrade protected native RTC options".into());
    }
    Ok(options)
}

fn check_crypto_event(event: Option<RoomEvent>) -> Result<(), String> {
    match event {
        Some(RoomEvent::E2eeStateChanged {
            state:
                EncryptionState::EncryptionFailed
                | EncryptionState::DecryptionFailed
                | EncryptionState::MissingKey
                | EncryptionState::InternalError,
            ..
        }) => Err("Native protected frame authentication failed".into()),
        Some(RoomEvent::Disconnected { .. }) | None => Err("Owned native room disconnected".into()),
        _ => Ok(()),
    }
}

async fn selected_path(
    room: &Room,
    local: &str,
    remote: &str,
    turn_url: &str,
) -> Result<Value, String> {
    let reports = tokio::time::timeout(Duration::from_secs(3), room.get_stats())
        .await
        .map_err(|_| "Native candidate statistics timed out")?
        .map_err(|_| "Native candidate statistics unavailable")?;
    let mut diagnostics = Vec::new();
    for stats in [&reports.publisher_stats, &reports.subscriber_stats] {
        let selected: HashSet<_> = stats
            .iter()
            .filter_map(|stat| match stat {
                RtcStats::Transport(transport)
                    if !transport.transport.selected_candidate_pair_id.is_empty() =>
                {
                    Some(transport.transport.selected_candidate_pair_id.as_str())
                }
                _ => None,
            })
            .collect();
        for stat in stats {
            let RtcStats::CandidatePair(pair) = stat else {
                continue;
            };
            let local_candidate = stats.iter().find_map(|stat| match stat {
                RtcStats::LocalCandidate(candidate)
                    if candidate.rtc.id == pair.candidate_pair.local_candidate_id =>
                {
                    Some(&candidate.local_candidate)
                }
                _ => None,
            });
            let remote_candidate = stats.iter().find_map(|stat| match stat {
                RtcStats::RemoteCandidate(candidate)
                    if candidate.rtc.id == pair.candidate_pair.remote_candidate_id =>
                {
                    Some(&candidate.remote_candidate)
                }
                _ => None,
            });
            if let (Some(local_candidate), Some(remote_candidate)) =
                (local_candidate, remote_candidate)
            {
                if diagnostics.len() < 32 {
                    diagnostics.push(json!({
                        "selected":selected.contains(pair.rtc.id.as_str()),
                        "nominated":pair.candidate_pair.nominated,
                        "state":format!("{:?}",pair.candidate_pair.state),
                        "local_type":format!("{:?}",local_candidate.candidate_type),
                        "local_address_matches":local_candidate.address == local,
                        "related_address_matches":local_candidate.related_address == local,
                        "relay_protocol":format!("{:?}",local_candidate.relay_protocol),
                        "owned_turn_url_matches":local_candidate.url == turn_url,
                        "remote_address_matches":remote_candidate.address == remote,
                        "protocol":local_candidate.protocol,
                        "bytes_sent":pair.candidate_pair.bytes_sent,
                        "bytes_received":pair.candidate_pair.bytes_received
                    }));
                }
                // The host's opaque UDP bridge changes the socket tuple seen
                // by the SFU. Its integrity-protected STUN response can produce
                // a peer-reflexive candidate; retain the original virtual relay
                // address and exact TURN origin as mandatory path evidence.
                let virtual_relay = (local_candidate.candidate_type
                    == Some(IceCandidateType::Relay)
                    && local_candidate.address == local)
                    || (local_candidate.candidate_type == Some(IceCandidateType::Prflx)
                        && local_candidate.related_address == local
                        && local_candidate.address == "127.0.0.1");
                if selected.contains(pair.rtc.id.as_str())
                    && pair.candidate_pair.nominated
                    && pair.candidate_pair.state == Some(IceCandidatePairState::Succeeded)
                    && virtual_relay
                    && local_candidate.relay_protocol == Some(IceServerTransportProtocol::Tcp)
                    && local_candidate.url == turn_url
                    && remote_candidate.address == remote
                    && local_candidate.protocol == "udp"
                    && pair.candidate_pair.bytes_sent + pair.candidate_pair.bytes_received > 0
                {
                    return Ok(
                        json!({"local_type":format!("{:?}",local_candidate.candidate_type), "local_virtual_ipv4":local,
                        "remote_virtual_ipv4":remote,"protocol":"udp", "nominated":true,
                        "local_turn_transport":"tcp","owned_turn_origin_confirmed":true,
                        "bytes_sent":pair.candidate_pair.bytes_sent,"bytes_received":pair.candidate_pair.bytes_received}),
                    );
                }
            }
        }
    }
    Err(format!("Native media did not use a nominated TURN relay path through the virtual userspace addresses: {}",json!(diagnostics)))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "Explicit owned Go fake-userspace SFU/TURN fixture; generated native video only"]
async fn embedded_userspace_turn_delivers_protected_native_video() {
    run_native_media_fixture("owned-fake-userspace", 0).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "Explicit owned actual iroh/Go TURN/SFU fixture; generated native video only"]
async fn iroh_stream_turn_delivers_protected_native_video() {
    run_native_media_fixture("owned-iroh-stream", 0).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "Explicit owned iroh/SFU fixture; two minutes of generated detailed720p, no physical devices"]
async fn iroh_stream_turn_sustains_detailed_video_for_two_minutes() {
    run_native_media_fixture("owned-iroh-stream", 120).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "Explicit owned iroh/SFU detailed video; thirty-second encoder-pressure diagnostic"]
async fn iroh_stream_turn_reports_video_encoder_pressure() {
    run_native_media_fixture("owned-iroh-stream", 30).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "Owned actual iroh/SFU production source jobs; generated pixels and no physical devices"]
async fn iroh_native_video_production_jobs_retire_and_recover_after_signal_resume() {
    run_native_media_fixture_case("owned-iroh-stream", 120, true).await;
}

fn fixture_token_claims(token: &str) -> Result<Value, String> {
    use base64::Engine as _;
    // Inspection only. Relay's actual authorize endpoint checks the signature.
    let payload = token
        .split('.')
        .nth(1)
        .ok_or("Missing fixture grant claims")?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| "Invalid fixture grant encoding")?;
    serde_json::from_slice(&bytes).map_err(|_| "Invalid fixture grant claims".into())
}

async fn owned_video_inbound(room: &Room) -> Value {
    let mut rows = Vec::new();
    if let Ok(Ok(stats)) = tokio::time::timeout(Duration::from_secs(2), room.get_stats()).await {
        for stat in stats
            .publisher_stats
            .into_iter()
            .chain(stats.subscriber_stats)
        {
            if let RtcStats::InboundRtp(stat) = stat {
                if stat.stream.kind == "video" {
                    rows.push(json!({"bytes":stat.inbound.bytes_received,"packets":stat.received.packets_received,
                        "lost":stat.received.packets_lost,"decoded":stat.inbound.frames_decoded,
                        "received_frames":stat.inbound.frames_received,"dropped":stat.inbound.frames_dropped,
                        "keyframes":stat.inbound.key_frames_decoded,"pli":stat.inbound.pli_count,"nack":stat.inbound.nack_count}));
                }
            }
        }
    }
    json!(rows)
}

async fn run_native_media_fixture(expected_transport: &str, sustained_seconds: u64) {
    run_native_media_fixture_case(expected_transport, sustained_seconds, false).await;
}

async fn run_native_media_fixture_case(
    expected_transport: &str,
    sustained_seconds: u64,
    signal_recovery: bool,
) {
    assert!(matches!(sustained_seconds, 0 | 30 | 120));
    let path = fixture_path().expect("Explicit embedded fixture path is required");
    let evidence_path = result_path(&path);
    let mut rooms = Vec::<Arc<Room>>::new();
    let body = std::panic::AssertUnwindSafe(async {
        let mut fixture: EmbeddedFixture =
            serde_json::from_slice(&std::fs::read(&path).map_err(|_| "Cannot read fixture JSON")?)
                .map_err(|_| "Cannot decode owned embedded fixture JSON")?;
        if fixture.transport != expected_transport
            || fixture.fixture_id.is_empty()
            || fixture.room.is_empty()
            || std::path::Path::new(&fixture.evidence_filename) != evidence_path
            || fixture.alice.identity == fixture.bob.identity
            || fixture.alice.virtual_ip == fixture.bob.virtual_ip
        {
            return Err(
                "Fixture must identify the isolated fake-userspace transport and owned result file"
                    .to_string(),
            );
        }
        virtual_ipv4(&fixture.host_virtual_ip)?;
        check_owned_port(fixture.media_udp_port)?;
        if sustained_seconds > 0 && !fixture.detail_video {
            return Err("Sustained fixture requires the detailed generated720p source".into());
        }
        if !matches!(fixture.diagnostic_bitrate_mbps, 0 | 2 | 4 | 8) {
            return Err("Diagnostic bitrate must be the explicit2,4or8Mbps ceiling".into());
        }
        let diagnostic_bitrate = fixture.diagnostic_bitrate_mbps;
        if diagnostic_bitrate == 4
            && crate::capture::CaptureOptions::default().target_bitrate() != 4_000_000
        {
            return Err(
                "Candidate4Mbps must match the actual production default720p30 ceiling".into(),
            );
        }
        let server_managed = fixture.open_server.is_some();
        if signal_recovery && (!server_managed || sustained_seconds != 120) {
            return Err(
                "Signal recovery requires an actual Open server and the two-minute fixture".into(),
            );
        }
        let authorize_url = fixture
            .open_server
            .as_ref()
            .map(|s| format!("{}/api/rtc/authorize", s.url));
        let (width, height, required_sent, required_received) = if fixture.detail_video {
            (1280, 720, 90, 45)
        } else {
            (320, 180, 45, 12)
        };
        let (alice_keys, bob_keys) = if let Some(server) = fixture.open_server.take() {
            if expected_transport != "owned-iroh-stream"
                || server.alice_session.user.id != fixture.alice.identity
                || server.bob_session.user.id != fixture.bob.identity
            {
                return Err("Open fixture accounts must match the two native participants".into());
            }
            let (alice, bob) = tokio::task::spawn_blocking(move || {
                let alice = crate::network::native_fixture::open_context(
                    &server.url,
                    server.alice_session,
                    &server.channel,
                    server.revision,
                )?;
                let bob = crate::network::native_fixture::open_context(
                    &server.url,
                    server.bob_session,
                    &server.channel,
                    server.revision,
                )?;
                Ok::<_, String>((alice, bob))
            })
            .await
            .map_err(|_| "Owned Open worker failed")??;
            if alice.1.room != fixture.room
                || bob.1.room != fixture.room
                || alice.0.group_id != bob.0.group_id
                || alice.0.epoch != bob.0.epoch
                || alice.0.sender_keys.len() != 2
                || bob.0.sender_keys.len() != 2
                || alice.0.sender_keys.iter().any(|(user, key)| {
                    bob.0.sender_keys.get(user).is_none_or(|other| {
                        key.device_id != other.device_id
                            || key.rtc != other.rtc
                            || key.opus != other.opus
                    })
                })
            {
                return Err("Owned server did not give both native participants the same two-person context".into());
            }
            fixture.alice.token = alice.1.token;
            fixture.bob.token = bob.1.token;
            (alice.0, bob.0)
        } else {
            (
                Arc::new(call_keys(&fixture, &fixture.alice)),
                Arc::new(call_keys(&fixture, &fixture.bob)),
            )
        };
        let bob_options = native_options(&bob_keys, &fixture.bob)?;
        let (bob, mut bob_events) = tokio::time::timeout(
            Duration::from_secs(25),
            Room::connect(&fixture.bob.url, &fixture.bob.token, bob_options),
        )
        .await
        .map_err(|_| "Bob native signaling/ICE connection timed out")?
        .map_err(|_| "Bob native signaling/ICE connection failed")?;
        let bob = Arc::new(bob);
        rooms.push(bob.clone());
        if bob.local_participant().identity().to_string() != fixture.bob.identity {
            return Err("Bob RTC ticket identity does not match the fixture verified key".into());
        }
        let alice_options = native_options(&alice_keys, &fixture.alice)?;
        let (alice, mut alice_events) = tokio::time::timeout(
            Duration::from_secs(25),
            Room::connect(&fixture.alice.url, &fixture.alice.token, alice_options),
        )
        .await
        .map_err(|_| "Alice native signaling/ICE connection timed out")?
        .map_err(|_| "Alice native signaling/ICE connection failed")?;
        let alice = Arc::new(alice);
        rooms.push(alice.clone());
        if alice.local_participant().identity().to_string() != fixture.alice.identity {
            return Err("Alice RTC ticket identity does not match the fixture verified key".into());
        }
        let initial_claims = if signal_recovery {
            Some(fixture_token_claims(&fixture.alice.token)?)
        } else {
            None
        };
        let mut reconnect_started = None::<Instant>;
        let mut reconnect_seen = false;
        let mut reconnect_finished = None::<Instant>;
        let mut decoded_before_reconnect = 0;
        let mut refreshed_expiry = 0;
        let mut recovery_events = Vec::new();
        let mut delivered_frames = 0;
        let mut delivered_before_reconnect = 0;
        let mut content_hashes = HashSet::new();
        let mut content_before_reconnect = 0;
        let mut inbound_before_reconnect = Value::Null;
        let mut replacement_track = None::<LocalVideoTrack>;
        let mut replacement_sid = None::<String>;
        let mut subscribed_sid = None::<String>;
        let mut replacement_decoded = 0;
        let mut source = NativeVideoSource::new(VideoResolution { width, height }, true);
        let mut track = LocalVideoTrack::create_video_track(
            "owned-embedded-generated-pattern",
            RtcVideoSource::Native(source.clone()),
        );
        let publication = tokio::time::timeout(
            Duration::from_secs(10),
            alice.local_participant().publish_track(
                LocalTrack::Video(track.clone()),
                TrackPublishOptions {
                    source: TrackSource::Screenshare,
                    video_codec: VideoCodec::VP8,
                    video_encoding: Some(VideoEncoding {
                        max_bitrate: if diagnostic_bitrate > 0 {
                            u64::from(diagnostic_bitrate) * 1_000_000
                        } else if fixture.detail_video {
                            2_000_000
                        } else {
                            500_000
                        },
                        max_framerate: 30.0,
                    }),
                    simulcast: false,
                    ..Default::default()
                },
            ),
        )
        .await
        .map_err(|_| "Native generated publication timed out")?
        .map_err(|_| "Native generated publication failed")?;
        if publication.encryption_type() != EncryptionType::Gcm {
            return Err("Published synthetic video was not GCM encrypted".into());
        }
        let resume_state = signal_recovery
            .then(|| super::integration_tests::recovery_fixture_state(alice_keys.clone(), 1));
        let mut resume_publications = HashMap::new();
        let mut resume_jobs = SourceJobs::default();
        if let Some(state) = &resume_state {
            state.tracks.lock().unwrap().insert(false, track.clone());
            resume_publications.insert(
                false,
                super::integration_tests::recovery_fixture_publication(
                    source.clone(),
                    publication.sid(),
                    track.rtc_track().id(),
                    CaptureOptions {
                        quality: crate::capture::CaptureQuality::P720,
                        fps: 30,
                        codec: VideoCodecPreference::Software,
                        adaptive_video: false,
                        ..Default::default()
                    },
                    1,
                ),
            );
        }
        let mut stream = None::<NativeVideoStream>;
        let mut received = HashSet::new();
        let mut color = false;
        let mut sent_frames = 0_u64;
        let mut tick = tokio::time::interval(Duration::from_micros(33_333));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let video_started = Instant::now();
        let until = video_started
            + Duration::from_secs(if sustained_seconds > 0 {
                sustained_seconds
            } else {
                15
            });
        let mut last_decoded = None::<Instant>;
        let mut max_decode_gap_ms = 0_u128;
        let mut windows = Vec::new();
        let mut window_started = video_started;
        let (mut window_decoded, mut window_sent) = (0, 0);
        while Instant::now() < until
            && (sustained_seconds > 0
                || received.len() < required_received
                || sent_frames < required_sent)
        {
            if let Some(state) = &resume_state {
                poll_source_jobs(&alice, &mut resume_publications, &mut resume_jobs, state)?;
                if let Some(current) = resume_publications.get(&false) {
                    if current.sid != publication.sid() && replacement_track.is_none() {
                        if alice
                            .local_participant()
                            .track_publications()
                            .contains_key(&publication.sid())
                            || alice.local_participant().track_publications().len() != 1
                            || current.generation != 1
                        {
                            return Err("Production resume must retire the old SID without duplicate publications or capture restart".into());
                        }
                        track = state
                            .tracks
                            .lock()
                            .unwrap()
                            .get(&false)
                            .cloned()
                            .ok_or("Replacement source track missing")?;
                        source = current.source.clone();
                        replacement_sid = Some(current.sid.to_string());
                        replacement_track = Some(track.clone());
                        recovery_events.push(json!({"event":"production-replacement-attached","ms":video_started.elapsed().as_millis()}));
                    }
                }
            }
            if signal_recovery
                && reconnect_started.is_none()
                && video_started.elapsed() >= Duration::from_secs(70)
            {
                let initial = initial_claims.as_ref().unwrap();
                let original_expiry = initial["exp"]
                    .as_u64()
                    .ok_or("Original grant expiry missing")?;
                let at = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_err(|_| "Fixture clock is before Unix epoch")?
                    .as_secs();
                if original_expiry > at {
                    return Err("Initial RTC grant must have expired before recovery".into());
                }
                let token = alice.token();
                let current = fixture_token_claims(&token)?;
                refreshed_expiry = current["exp"]
                    .as_u64()
                    .ok_or("Refreshed grant expiry missing")?;
                if token == fixture.alice.token
                    || refreshed_expiry <= at
                    || current["sub"] != initial["sub"]
                    || current["attributes"] != initial["attributes"]
                    || current["video"]["room"] != initial["video"]["room"]
                {
                    return Err("Refreshed SDK grant must retain the original account/server/login/channel binding".into());
                }
                let url = authorize_url.clone().unwrap();
                let authorized = tokio::task::spawn_blocking(move || {
                    reqwest::blocking::Client::builder()
                        .no_proxy()
                        .timeout(Duration::from_secs(3))
                        .build()
                        .map_err(|_| "Cannot create owned authorization client")?
                        .get(url)
                        .bearer_auth(token)
                        .send()
                        .map(|r| r.status().as_u16())
                        .map_err(|_| "Fresh grant authorization request failed")
                })
                .await
                .map_err(|_| "Owned authorization task failed")??;
                if authorized != 200 {
                    return Err("Actual Relay rejected the refreshed SDK signaling grant".into());
                }
                decoded_before_reconnect = received.len();
                delivered_before_reconnect = delivered_frames;
                content_before_reconnect = content_hashes.len();
                inbound_before_reconnect = owned_video_inbound(&bob).await;
                reconnect_started = Some(Instant::now());
                tokio::time::timeout(
                    Duration::from_secs(2),
                    alice.simulate_scenario(livekit::SimulateScenario::SignalReconnect),
                )
                .await
                .map_err(|_| "Signal reconnect injection timed out")?
                .map_err(|_| "Cannot inject owned signal reconnect")?;
            }
            if reconnect_started.is_some_and(|at| at.elapsed() > Duration::from_secs(10))
                && reconnect_finished.is_none()
            {
                return Err("Owned native signal did not reconnect within ten seconds".into());
            }
            tokio::select! {
                event = alice_events.recv() => {
                    match &event {
                        Some(RoomEvent::Reconnecting) if reconnect_started.is_some()=>{
                            reconnect_seen=true;
                            recovery_events.push(json!({"event":"publisher-reconnecting","ms":video_started.elapsed().as_millis()}));
                        },
                        Some(RoomEvent::Reconnected) if reconnect_started.is_some()=>{
                            reconnect_finished=Some(Instant::now());
                            recovery_events.push(json!({"event":"publisher-reconnected","ms":video_started.elapsed().as_millis()}));
                            if signal_recovery {
                                let current=resume_publications.get(&false).ok_or("Owned source missing")?;
                                let interrupted=HashMap::from([(false,InterruptedVideo{
                                    generation:current.generation,sid:current.sid.clone(),native_id:current.native_id.clone()
                                })]);
                                rebind_resumed_video(&alice,&mut resume_publications,&mut resume_jobs,resume_state.as_ref().unwrap(),interrupted)?;
                            }

                        },
                        _=>{},
                    }
                    check_crypto_event(event)?;
                },
                event = bob_events.recv() => match event {
                    Some(RoomEvent::TrackPublished {publication, participant}) => {
                        if signal_recovery {recovery_events.push(json!({"event":"subscriber-track-published","ms":video_started.elapsed().as_millis()}));}
                        let identity = participant.identity().to_string();
                        publication.set_subscribed(verified_publication(&bob_keys, &identity, &publication));
                    }
                    Some(RoomEvent::TrackSubscribed {track:RemoteTrack::Video(track), publication, participant}) => {
                        if signal_recovery {recovery_events.push(json!({"event":"subscriber-track-subscribed","ms":video_started.elapsed().as_millis()}));}
                        if participant.identity().to_string() != fixture.alice.identity
                            || !verified_publication(&bob_keys, &fixture.alice.identity, &publication) {
                            return Err("Native subscriber received an unverified or unprotected track".into());
                        }
                        stream = Some(NativeVideoStream::new(track.rtc_track()));
                        subscribed_sid = Some(publication.sid().to_string());
                    }
                    event => check_crypto_event(event)?,
                },
                frame = async {
                    match stream.as_mut() {Some(stream)=>stream.next().await, None=>std::future::pending().await}
                } => {
                    let frame = frame.ok_or("Native protected frame stream ended")?;
                    if replacement_sid.is_some() && replacement_sid == subscribed_sid {
                        replacement_decoded += 1;
                    }
                    if (frame.buffer.width(), frame.buffer.height()) != (width, height) {
                        return Err("Decoded synthetic frame dimensions changed".into());
                    }
                    if received.insert(frame.timestamp_us) {
                        let now = Instant::now();
                        if let Some(previous) = last_decoded {
                            max_decode_gap_ms = max_decode_gap_ms.max(now.duration_since(previous).as_millis());
                        }
                        last_decoded = Some(now);
                    }
                    let mut rgba = vec![0;(width*height*4) as usize];
                    frame.buffer.to_argb(VideoFormatType::ABGR, &mut rgba, width*4, width as i32, height as i32);
                    if signal_recovery {
                        delivered_frames += 1;
                        let fingerprint=rgba.iter().step_by(4096).fold(0xcbf29ce484222325_u64,
                            |hash,sample|(hash^u64::from(*sample)).wrapping_mul(0x100000001b3));
                        content_hashes.insert(fingerprint);
                    }
                    color |= rgba.chunks_exact(4).take(20).any(|pixel| pixel[0].abs_diff(pixel[2]) > 40);
                },
                _ = tick.tick() => {
                    let mut buffer = I420Buffer::new(width,height);
                    let (y,u,v) = buffer.data_mut();
                    if fixture.detail_video {
                        // Moving generated tiles stress packet volume without any capture API.
                        for (row_index,row) in y.chunks_exact_mut(width as usize).enumerate() {
                            for (column,tile) in row.chunks_mut(8).enumerate() {
                                let mut seed=(column as u32).wrapping_mul(0x9e3779b9)
                                    ^ ((row_index/8) as u32).wrapping_mul(0x85ebca6b)
                                    ^ (sent_frames as u32).wrapping_mul(0xc2b2ae35);
                                seed^=seed>>16;seed=seed.wrapping_mul(0x7feb352d);seed^=seed>>15;
                                tile.fill(40+(seed%160) as u8);
                            }
                        }
                    } else { y.fill(80+(sent_frames%30) as u8); }
                    u.fill(90);v.fill(230);
                    sent_frames += 1;
                    source.capture_frame(&VideoFrame {rotation:VideoRotation::VideoRotation0,
                        timestamp_us:1_000_000+(sent_frames as i64)*33_333, frame_metadata:None, buffer});
                },
            }
            if sustained_seconds > 0 {
                let now = Instant::now();
                if now.duration_since(video_started) >= Duration::from_secs(10)
                    && last_decoded.is_none_or(|at| now.duration_since(at) > Duration::from_secs(5))
                {
                    if signal_recovery {
                        let mut outbound = Vec::new();
                        if let Ok(Ok(stats)) =
                            tokio::time::timeout(Duration::from_secs(2), track.get_stats()).await
                        {
                            for stat in stats {
                                if let RtcStats::OutboundRtp(stat) = stat {
                                    outbound.push(json!({"bytes":stat.sent.bytes_sent,"encoded":stat.outbound.frames_encoded,
                                        "sent":stat.outbound.frames_sent,"retransmitted_bytes":stat.outbound.retransmitted_bytes_sent}));
                                }
                            }
                        }
                        let diagnostic = json!({"elapsed_ms":now.duration_since(video_started).as_millis(),
                            "last_decode_gap_ms":last_decoded.map(|at|now.duration_since(at).as_millis()),
                            "generated":sent_frames,"decoded":received.len(),"decoded_before_reconnect":decoded_before_reconnect,
                            "publisher_state":format!("{:?}",alice.connection_state()),"subscriber_state":format!("{:?}",bob.connection_state()),
                            "reconnect_started":reconnect_started.is_some(),"reconnecting_event":reconnect_seen,
                            "reconnect_ms":reconnect_started.zip(reconnect_finished).map(|(a,b)|b.duration_since(a).as_millis()),
                            "original_grant_expiry":initial_claims.as_ref().map(|c|&c["exp"]),"refreshed_grant_expiry":refreshed_expiry,
                            "recovery_events":recovery_events,"outbound":outbound,
                            "delivered_frames":delivered_frames,"delivered_before_reconnect":delivered_before_reconnect,
                            "distinct_content":content_hashes.len(),"content_before_reconnect":content_before_reconnect,
                            "inbound_before_reconnect":inbound_before_reconnect,"inbound_after":owned_video_inbound(&bob).await});
                        std::fs::write(
                            evidence_path.with_extension("diagnostic.json"),
                            serde_json::to_vec_pretty(&diagnostic)
                                .map_err(|_| "Cannot serialize owned diagnostics")?,
                        )
                        .map_err(|_| "Cannot save owned recovery diagnostics")?;
                    }
                    return Err(
                        "Sustained native video stopped producing new decoded frames".into(),
                    );
                }
                if now.duration_since(window_started) >= Duration::from_secs(10) {
                    let decoded = received.len() - window_decoded;
                    if decoded == 0 {
                        return Err(
                            "Sustained native video has an empty ten-second decode window".into(),
                        );
                    }
                    windows.push(
                        json!({"elapsed_ms":now.duration_since(video_started).as_millis(),
                        "window_ms":now.duration_since(window_started).as_millis(),
                        "decoded_distinct":decoded,"generated":sent_frames-window_sent}),
                    );
                    window_started = now;
                    window_decoded = received.len();
                    window_sent = sent_frames;
                }
            }
        }
        let signal_recovery_evidence = if signal_recovery {
            let started = reconnect_started.ok_or("Owned signal recovery never started")?;
            let finished = reconnect_finished.ok_or("Owned signal recovery never completed")?;
            if !reconnect_seen
                || finished.duration_since(started) > Duration::from_secs(10)
                || received.len() <= decoded_before_reconnect + 10
            {
                return Err(
                    "Signal recovery must produce SDK events and new protected frames".into(),
                );
            }
            if replacement_decoded < 10 {
                return Err(
                    "Replacement publication must deliver at least ten new protected frames".into(),
                );
            }
            Some(
                json!({"initial_grant_expiry":initial_claims.as_ref().unwrap()["exp"],
                "refreshed_grant_expiry":refreshed_expiry,"original_expired_before_reconnect":true,
                "same_account_server_login_channel_binding":true,"actual_relay_authorize_status":200,
                "reconnecting_event":reconnect_seen,"reconnected_event":true,
                "recovery_ms":finished.duration_since(started).as_millis(),
                "decoded_before_reconnect":decoded_before_reconnect,
                "decoded_after_reconnect":received.len()-decoded_before_reconnect,
                "replacement_frames":replacement_decoded,
                "production_source_jobs":true,
                "old_publication_retired":true,
                "scope":"Production source-job recovery after SDK signal resume; no injected complete media outage; not full app-controller or WAN acceptance"}),
            )
        } else {
            None
        };
        let media_run_ms = video_started.elapsed().as_millis();
        if sustained_seconds > 0 && windows.len() < (sustained_seconds / 10 - 1) as usize {
            return Err("Sustained native video did not complete its measurement windows".into());
        }
        if received.len() < required_received || !color {
            return Err(format!(
                "Native TURN bridge decoded only {} distinct colored frames",
                received.len()
            ));
        }
        let mut rtp_bytes = 0_u64;
        let mut encoded = 0_u32;
        let mut encoder_reports = Vec::new();
        for stat in tokio::time::timeout(Duration::from_secs(3), track.get_stats())
            .await
            .map_err(|_| "Native encoder statistics timed out")?
            .map_err(|_| "Native encoder statistics unavailable")?
        {
            if let RtcStats::OutboundRtp(stat) = stat {
                rtp_bytes = rtp_bytes.saturating_add(stat.sent.bytes_sent);
                encoded = encoded.saturating_add(stat.outbound.frames_encoded);
                encoder_reports.push(json!({
                    "kind":stat.stream.kind,"width":stat.outbound.frame_width,"height":stat.outbound.frame_height,
                    "frames_encoded":stat.outbound.frames_encoded,"frames_sent":stat.outbound.frames_sent,
                    "reported_fps":stat.outbound.frames_per_second,"target_bitrate":stat.outbound.target_bitrate,
                    "quality_limitation_reason":format!("{:?}",stat.outbound.quality_limitation_reason),
                    "quality_limitation_durations":stat.outbound.quality_limitation_durations,
                    "total_encode_seconds":stat.outbound.total_encode_time,"qp_sum":stat.outbound.qp_sum,
                    "encoder_implementation":stat.outbound.encoder_implementation,
                    "power_efficient_encoder":stat.outbound.power_efficient_encoder,
                    "retransmitted_bytes":stat.outbound.retransmitted_bytes_sent
                }));
            }
        }
        if rtp_bytes == 0 || encoded == 0 {
            return Err("Actual native encoded RTP evidence is missing".into());
        }
        if fixture.detail_video && rtp_bytes < 80_000 {
            return Err("Detailed native fixture did not exercise sufficient RTP payload".into());
        }
        let alice_path = selected_path(
            &alice,
            &fixture.alice.virtual_ip,
            &fixture.host_virtual_ip,
            &fixture.alice.turn.url,
        )
        .await?;
        let bob_path = selected_path(
            &bob,
            &fixture.bob.virtual_ip,
            &fixture.host_virtual_ip,
            &fixture.bob.turn.url,
        )
        .await?;
        Ok::<Value, String>(json!({"result":"passed", "fixture_id":fixture.fixture_id,
            "transport":fixture.transport,"encryption":"Gcm per verified participant",
            "key_source":if server_managed {"actual Open server via production Worker; no MLS actor"} else {"manufactured fixture keys"},
            "server_issued_rtc_grants":server_managed,
            "width":width,"height":height,"requested_fps":30,"detailed_generated_tiles":fixture.detail_video,
            "video_loop_ms":video_started.elapsed().as_millis(),
            "sustained_seconds":sustained_seconds,"media_run_ms":media_run_ms,
            "signal_recovery":signal_recovery_evidence,
            "max_decode_gap_ms":max_decode_gap_ms,"ten_second_windows":windows,
            "diagnostic_bitrate_mbps":diagnostic_bitrate,"encoder_reports":encoder_reports,
            "decoded_distinct_frames":received.len(),"generated_frames":sent_frames,
            "encoded_frames":encoded,"native_rtp_bytes":rtp_bytes,"generated_chroma_survived":color,
            "alice_selected_path":alice_path,"bob_selected_path":bob_path,
            "physical_capture_or_playback_opened":false,"network_adapter_or_firewall_changed":false,
            "limitations":if server_managed {"Actual Open server accounts/admission, production Worker key validation and server-issued RTC grants; owned offline iroh QUIC/local TURN/native VP8/GCM. Server can read media keys: this is not E2EE. No physical devices, native GUI, public relay or WAN acceptance."} else if expected_transport=="owned-iroh-stream" {"Owned actual iroh QUIC streams, authenticated local TURN and loopback SFU; generated native GCM/VP8 encode/decode. No physical audio/camera/screen capture, WAN acceptance or Relay account/key-exchange proof."} else {"Owned offline fake SDK sockets and loopback signaling; actual native libwebrtc TURN, GCM, VP8 encode/decode, and virtual UDP bridges. No real tsnet enrollment, WAN/DERP timing, Relay API auth, key exchange, or microphone/camera acceptance."}}))
    });
    let outcome = tokio::time::timeout(
        Duration::from_secs(75 + sustained_seconds),
        body.catch_unwind(),
    )
    .await;
    let mut cleanup_errors = Vec::new();
    for room in rooms {
        match tokio::time::timeout(Duration::from_secs(4), room.close()).await {
            Ok(Ok(())) => {}
            _ => cleanup_errors.push("Owned native room failed normal cleanup"),
        }
    }
    let evidence = match outcome {
        Ok(Ok(Ok(evidence))) if cleanup_errors.is_empty() => evidence,
        Ok(Ok(Ok(_))) => json!({"result":"failed","error":"Native room cleanup failed"}),
        Ok(Ok(Err(error))) => json!({"result":"failed","error":error}),
        Ok(Err(_)) => json!({"result":"failed","error":"Native embedded acceptance panicked"}),
        Err(_) => {
            json!({"result":"failed","error":"Native embedded acceptance exceeded its deadline"})
        }
    };
    // The Go fixture waits on this owned result file to clean up its own SFU,
    // TURN allocations and fake SDK sockets on either success or failure.
    write_result(&evidence_path, &evidence)
        .expect("Write atomic owned native embedded acceptance result");
    assert_eq!(evidence["result"], "passed", "{evidence}");
}
