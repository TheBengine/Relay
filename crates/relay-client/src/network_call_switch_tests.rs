//! Exact call-switch worker fixtures. No media devices, vault or real profiles.
use super::*;
use crate::network::call_switch::Request;
use base64::{engine::general_purpose::STANDARD, Engine as _};

const CALL: &str = "271c581a-d27b-47de-84f8-29e817c9ae36";

fn bootstrap(server: &str, accepted: bool, source: bool) -> Bootstrap {
    let mut b = Bootstrap {
        server_id: server.into(),
        instance_id: format!("{server}-instance"),
        protocol_version: API_PROTOCOL_VERSION,
        user: User {
            id: "fixture-user".into(),
            ..Default::default()
        },
        protection: CommunityProtection {
            level: ProtectionLevel::Open,
            revision: 1,
            changed_by: None,
        },
        capabilities: vec![
            "private-call-switch-v1".into(),
            "protection-calls-v1".into(),
        ],
        ..Default::default()
    };
    if !source {
        b.capabilities.extend(["people-v1".into(), "friends-gateway-broker-v1".into()]);
    }
    let device = open_device();
    if source || accepted {
        b.voice_members.push(VoiceMember {
            user: b.user.clone(),
            channel_id: if source {
                "voice-room"
            } else {
                "incoming-room"
            }
            .into(),
            device_id: Some(device),
            session_id: if source { 11 } else { 22 },
            ..Default::default()
        });
    }
    if !source {
        b.social.server_time = 100;
        b.social.friends.push("caller".into());
        b.members.push(User {
            id: "caller".into(),
            ..Default::default()
        });
        b.channels.push(Channel {
            id: "incoming-room".into(),
            kind: "voice".into(),
            can_join: true,
            ..Default::default()
        });
        b.social.calls.push(PrivateCall {
            id: CALL.into(),
            channel_id: "incoming-room".into(),
            caller_id: "caller".into(),
            recipient_id: "fixture-user".into(),
            state: if accepted { "accepted" } else { "ringing" }.into(),
            expires_at: 130,
        });
    }
    b
}

fn open_device() -> String {
    use sha2::{Digest, Sha256};
    format!(
        "open-{:.32}",
        format!("{:x}", Sha256::digest(b"fixture-token"))
    )
}

fn connection(client: &mut TestWorker, url: &str, server: &str, source: bool) {
    client.connection(url.into());
    let b = bootstrap(server, false, source);
    let c = client.worker.connection.as_mut().unwrap();
    c.server_id = server.into();
    c.chat = extended::ChatSession::new(&b);
    c.chat.set_open_consent(Some(1));
    c.voice_channel = source.then(|| "voice-room".into());
    c.voice_attempt = source.then_some(7);
    if source {
        let ticket: VoiceTicket = serde_json::from_value(json!({
            "session_id":11,"key":STANDARD.encode([1;32]),"udp_port":4581,"wire_version":4,
            "resume":{"instance_id":format!("{server}-instance"),"token":base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([3;32])}
        })).unwrap();
        client.worker.remember_voice_admission(&ticket);
        client.worker.live.epoch.store(9, Ordering::Release);
        *client.worker.live.context.lock().unwrap() = Some(LiveConnection {
            url: url.into(),
            token: "fixture-token".into(),
            channel_id: "voice-room".into(),
            generation: 9,
            sharing: false,
            camera: false,
        });
    }
}

fn request(client: &TestWorker, destination: &str) -> Request {
    Request {
        origin: CommandOrigin {
            url: destination.into(),
            server_id: "destination".into(),
            user_id: "fixture-user".into(),
        },
        previous: client.command_origin(),
        call_id: CALL.into(),
        previous_channel: "voice-room".into(),
        previous_attempt: 7,
        media_generation: 9,
        operation: 101,
        voice_attempt: 8,
        state: VoiceState {
            muted: true,
            deafened: true,
            ..Default::default()
        },
    }
}

fn retain_source(client: &TestWorker, original: &CommandOrigin) {
    assert_eq!(&client.command_origin(), original);
    let c = client.worker.connection.as_ref().unwrap();
    assert_eq!(c.voice_channel.as_deref(), Some("voice-room"));
    assert_eq!(c.voice_attempt, Some(7));
    assert_eq!(c.voice_admission.as_ref().unwrap().session, 11);
    assert_eq!(client.worker.live.epoch.load(Ordering::Acquire), 9);
    let live = client.worker.live.context.lock().unwrap();
    assert_eq!(live.as_ref().unwrap().url, original.url);
    assert_eq!(live.as_ref().unwrap().channel_id, "voice-room");
    assert_eq!(live.as_ref().unwrap().generation, 9);
}

fn save_destination(client: &mut TestWorker, destination: &MockHttp, source: &MockHttp) {
    connection(client, &destination.url, "destination", false);
    let parked = client.worker.connection.take().unwrap();
    client.worker.saved_connections.insert(
        destination.url.clone(),
        SavedConnection {
            connection: parked,
            stop: Arc::new(AtomicBool::new(false)),
            dirty: Arc::new(AtomicBool::new(false)),
        },
    );
    connection(client, &source.url, "source", true);
}

#[test]
fn call_switch_rejects_changed_account_attempt_or_generation_before_http() {
    let source = MockHttp::new(|_| panic!("stale confirmation must not reach HTTP"));
    for changed in ["account", "attempt", "generation"] {
        let mut client = TestWorker::new();
        connection(&mut client, &source.url, "source", true);
        let mut r = request(&client, "https://never-contact.invalid");
        match changed {
            "account" => r.previous.user_id = "another-account".into(),
            "attempt" => r.previous_attempt += 1,
            _ => r.media_generation += 1,
        }
        assert!(client.worker.leave_and_answer(&r).is_err());
        retain_source(&client, &client.command_origin());
    }
}

#[test]
fn call_switch_denied_or_wrong_identity_destination_keeps_live_source() {
    for failure in [
        "denied",
        "wrong-account",
        "expired",
        "blocked",
        "source-device",
    ] {
        let source = MockHttp::new(move |path| {
            assert_eq!(
                path, "/api/bootstrap",
                "failed destination must never leave source"
            );
            let mut b = bootstrap("source", false, true);
            if failure == "source-device" {
                b.voice_members[0].device_id = Some("another-valid-device".into());
            }
            Reply::json(200, serde_json::to_value(b).unwrap())
        });
        let destination = MockHttp::new(move |path| {
            assert_eq!(
                path, "/api/bootstrap",
                "invalid invitation must never be admitted"
            );
            if failure == "denied" {
                return Reply::json(403, json!({"error":"account revoked"}));
            }
            let mut b = bootstrap("destination", false, false);
            match failure {
                "wrong-account" => b.user.id = "another-account".into(),
                "expired" => b.social.calls[0].expires_at = 100,
                _ => b.social.blocked.push("caller".into()),
            }
            Reply::json(200, serde_json::to_value(b).unwrap())
        });
        let mut client = TestWorker::new();
        save_destination(&mut client, &destination, &source);
        let origin = client.command_origin();
        let r = request(&client, &destination.url);
        let result = client.worker.leave_and_answer(&r);
        assert!(result.is_err(), "{failure}");
        assert!(result.err().unwrap().previous_kept, "{failure}");
        retain_source(&client, &origin);
        assert!(source
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|r| r.method == "GET"));
    }
}

fn admission() -> Value {
    json!({"call":bootstrap("destination",true,false).social.calls[0],"ticket":{
        "session_id":22,"key":STANDARD.encode([2;32]),"udp_port":4581,"wire_version":4,
        "resumed_state":{"muted":true,"deafened":true,"sharing":false,"camera":false},
        "resume":{"instance_id":"destination-instance","token":base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([4;32])}
    }})
}

#[test]
fn call_switch_lost_reply_retries_exact_call_before_releasing_source() {
    let admitted = Arc::new(AtomicBool::new(false));
    let keys_ready = Arc::new(AtomicBool::new(false));
    let join_count = Arc::new(AtomicUsize::new(0));
    let (accepted, keys, joins) = (admitted.clone(), keys_ready.clone(), join_count.clone());
    let destination = MockHttp::with_request(move |r| match r.path.as_str() {
        "/api/bootstrap" => Reply::json(
            200,
            serde_json::to_value(bootstrap(
                "destination",
                accepted.load(Ordering::Acquire),
                false,
            ))
            .unwrap(),
        ),
        path if path == format!("/api/private-calls/{CALL}/join") => {
            let input: PrivateCallJoin = serde_json::from_slice(&r.body).unwrap();
            assert_eq!(input.instance_id, "destination-instance");
            assert!(input.replace_session_id.is_none());
            assert_eq!(input.join.device_id, Some(open_device()));
            assert!(input.state.muted && input.state.deafened);
            accepted.store(true, Ordering::Release);
            if joins.fetch_add(1, Ordering::AcqRel) == 0 {
                Reply {
                    status: 0,
                    headers: vec![],
                    body: vec![],
                }
            } else {
                Reply::json(200, admission())
            }
        }
        "/api/protection/call" => {
            assert!(accepted.load(Ordering::Acquire));
            keys.store(true, Ordering::Release);
            Reply::json(
                200,
                json!({"revision":1,"epoch":1,"group_id":"server-managed:destination/incoming-room","senders":[{
                    "user_id":"fixture-user","device_id":open_device(),"opus":STANDARD.encode([5;32]),"rtc":STANDARD.encode([6;32])
                }]}),
            )
        }
        _ => panic!("unexpected destination request {}", r.path),
    });
    let released = Arc::new(AtomicBool::new(false));
    let (accepted, keys, release) = (admitted.clone(), keys_ready.clone(), released.clone());
    let source = MockHttp::with_request(move |r| match r.path.as_str() {
        "/api/bootstrap" => Reply::json(
            200,
            serde_json::to_value(bootstrap("source", false, true)).unwrap(),
        ),
        "/api/voice/leave-exact" => {
            assert!(accepted.load(Ordering::Acquire) && keys.load(Ordering::Acquire));
            let input: VoiceLeaveExact = serde_json::from_slice(&r.body).unwrap();
            assert_eq!(input.instance_id, "source-instance");
            assert_eq!(input.session_id, 11);
            assert_eq!(input.channel_id, "voice-room");
            assert_eq!(input.device_id, open_device());
            assert_eq!(input.open_revision, Some(1));
            release.store(true, Ordering::Release);
            Reply::json(204, Value::Null)
        }
        _ => panic!("unexpected source request {}", r.path),
    });
    let mut client = TestWorker::new();
    save_destination(&mut client, &destination, &source);
    client.worker.live_frames = None; // No runtime media worker is needed by this fixture.
    let r = request(&client, &destination.url);
    let result = client.worker.leave_and_answer(&r);
    let ready = match result {
        Ok(r) => r,
        Err(e) => panic!("{}", e.message),
    };
    assert_eq!(join_count.load(Ordering::Acquire), 2);
    assert!(released.load(Ordering::Acquire));
    assert_eq!(ready.admission.call.id, CALL);
    let gateway = ready.gateway.as_ref().expect("accepted Friends destination prepares its own broker");
    let request = gateway.prepared_list_for_test();
    assert_eq!(request.url().as_str(), format!("{}/api/hosting/voice-gateways", destination.url));
    assert_eq!(request.headers().get("authorization").unwrap(), "Bearer fixture-token");

    assert_eq!(client.command_origin(), ready.request.origin);
    assert_eq!(
        client
            .worker
            .connection
            .as_ref()
            .unwrap()
            .voice_admission
            .as_ref()
            .unwrap()
            .session,
        22
    );
    assert_eq!(
        client.worker.saved_connections[&source.url]
            .connection
            .voice_channel,
        None
    );
}

#[test]
fn cancelled_call_switch_after_readback_releases_only_the_confirmed_new_admission() {
    let mut client = TestWorker::new();
    let epoch = client.worker.live.epoch.clone();
    let accepted = Arc::new(AtomicBool::new(false));
    let cleaned = Arc::new(AtomicBool::new(false));
    let (admitted, cleanup) = (accepted.clone(), cleaned.clone());
    let destination = MockHttp::with_request(move |r| match r.path.as_str() {
        "/api/bootstrap" => {
            let accepted = admitted.load(Ordering::Acquire);
            if accepted {
                epoch.fetch_add(1, Ordering::AcqRel);
            }
            Reply::json(
                200,
                serde_json::to_value(bootstrap("destination", accepted, false)).unwrap(),
            )
        }
        path if path == format!("/api/private-calls/{CALL}/join") => {
            admitted.store(true, Ordering::Release);
            Reply::json(200, admission())
        }
        "/api/protection/call" => Reply::json(
            200,
            json!({"revision":1,"epoch":1,"group_id":"server-managed:destination/incoming-room","senders":[{
                "user_id":"fixture-user","device_id":open_device(),"opus":STANDARD.encode([5;32]),"rtc":STANDARD.encode([6;32])
            }]}),
        ),
        "/api/voice/leave-exact" => {
            let input: VoiceLeaveExact = serde_json::from_slice(&r.body).unwrap();
            assert_eq!(input.instance_id, "destination-instance");
            assert_eq!(input.session_id, 22);
            assert_eq!(input.channel_id, "incoming-room");
            assert_eq!(input.device_id, open_device());
            assert_eq!(input.open_revision, Some(1));
            cleanup.store(true, Ordering::Release);
            Reply::json(204, Value::Null)
        }
        _ => panic!("unexpected destination request {}", r.path),
    });
    let source = MockHttp::new(|path| {
        assert_eq!(
            path, "/api/bootstrap",
            "cancelled switch must not release the source"
        );
        Reply::json(
            200,
            serde_json::to_value(bootstrap("source", false, true)).unwrap(),
        )
    });
    save_destination(&mut client, &destination, &source);
    let original = client.command_origin();
    let r = request(&client, &destination.url);
    let failure = match client.worker.leave_and_answer(&r) {
        Ok(_) => panic!("cancelled handoff must fail"),
        Err(e) => e,
    };
    assert!(failure.previous_kept);
    assert!(accepted.load(Ordering::Acquire) && cleaned.load(Ordering::Acquire));
    assert_eq!(client.command_origin(), original);
    assert_eq!(
        client
            .worker
            .connection
            .as_ref()
            .unwrap()
            .voice_admission
            .as_ref()
            .unwrap()
            .session,
        11
    );
    assert_eq!(
        client.worker.live.epoch.load(Ordering::Acquire),
        10,
        "only the simulated user cancellation changes the epoch"
    );
    assert_eq!(
        client
            .worker
            .live
            .context
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .generation,
        9
    );
    assert!(source
        .requests
        .lock()
        .unwrap()
        .iter()
        .all(|r| r.method == "GET"));
}
