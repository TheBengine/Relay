//! Explicit live Friends-media opt-in. Fresh owned QA accounts only; no devices.
use super::*;
use serde::Deserialize;

const BASE: &str = "https://relay-accounts.tail787aee.ts.net/people";
const SERVER: &str = "096a6cf1-4ef4-4bc3-be10-e736d84145b6";
const PURPOSE: &str = "relay-owned-friends-media-v1";

// No Debug: bearer sessions must not appear in failure diagnostics.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Fixture {
    schema: u32,
    purpose: String,
    run_id: String,
    base: String,
    server_id: String,
    duration_seconds: u64,
    expected_user_ids: [String; 2],
    sessions: [Session; 2],
}

impl Fixture {
    fn validate(&self) -> Result<(), &'static str> {
        if self.schema != 1
            || self.purpose != PURPOSE
            || self.base != BASE
            || self.server_id != SERVER
            || uuid::Uuid::parse_str(&self.run_id).is_err()
            || !(15..=360).contains(&self.duration_seconds)
            || self.expected_user_ids[0] == self.expected_user_ids[1]
        {
            return Err("Not the explicit named-host Friends media fixture");
        }
        if uuid::Uuid::parse_str(&self.run_id)
            .map_err(|_| "Invalid fixture ID")?
            .to_string()
            != self.run_id
        {
            return Err("Use the canonical owned run identifier");
        }
        let prefix = format!(
            "qafm{}",
            self.run_id
                .replace('-', "")
                .get(..12)
                .ok_or("Invalid fixture ID")?
        );
        for (index, session) in self.sessions.iter().enumerate() {
            if session.user.id != self.expected_user_ids[index]
                || uuid::Uuid::parse_str(&session.user.id).is_err()
                || session.user.username
                    != format!("{prefix}{}", if index == 0 { 'a' } else { 'b' })
                || session.user.role != "member"
                || session.token.is_empty()
                || session.token.len() > 16 * 1024
            {
                return Err("Use only this run's two fresh non-admin QA accounts");
            }
        }
        Ok(())
    }
}

fn checked_input(path: &Path, filename: &str) {
    use std::path::{Component, Prefix};
    assert!(
        path.is_absolute()
            && matches!(path.components().next(), Some(Component::Prefix(p))
        if matches!(p.kind(), Prefix::Disk(b'C' | b'D') | Prefix::VerbatimDisk(b'C' | b'D'))),
        "Owned media fixture must be on direct local C: or D:"
    );
    assert!(!path
        .components()
        .any(|part| matches!(part, Component::ParentDir | Component::CurDir)));
    assert_eq!(path.file_name().and_then(|s| s.to_str()), Some(filename));
    for ancestor in path.ancestors().collect::<Vec<_>>().into_iter().rev() {
        let metadata = fs::symlink_metadata(ancestor).expect("Check owned media fixture ancestor");
        assert!(!metadata.file_type().is_symlink());
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            assert_eq!(
                metadata.file_attributes() & 0x400,
                0,
                "Redirected media fixture refused"
            );
        }
    }
    let metadata = fs::metadata(path).unwrap();
    assert!(metadata.is_file() && metadata.len() <= 64 * 1024);
}

struct Pair {
    a: CryptoWorker,
    b: CryptoWorker,
    call: Option<String>,
}
impl Drop for Pair {
    fn drop(&mut self) {
        // Even a failed SDK/crypto probe must retire only these owned voice seats.
        for actor in [&mut self.a, &mut self.b] {
            let _ = actor.worker.handle(Command::LeaveVoice);
            actor.drain();
        }
        if let Some(call) = &self.call {
            let _ = self.a.worker.post::<serde_json::Value, _>(
                &format!("/api/private-calls/{call}"),
                &json!({"action":"cancel"}),
            );
        }
    }
}

fn rtc_grant(
    actor: &mut CryptoWorker,
    channel: &str,
    request_id: u64,
) -> crate::rtc::PeopleMediaGrant {
    let updates = actor.handle(Command::RtcToken {
        channel_id: channel.into(),
        request_id,
    });
    let (ticket, keys) = updates
        .into_iter()
        .find_map(|update| match update {
            Update::RtcToken {
                ticket,
                keys,
                request_id: id,
            } if id == request_id => Some((ticket, keys)),
            _ => None,
        })
        .expect("Production worker must return this request's actual Friends RTC grant");
    assert_eq!(ticket.url, "wss://relay-accounts.tail787aee.ts.net/people");
    assert!(!ticket.token.is_empty() && !ticket.room.is_empty());
    assert_eq!(keys.local_user_id, actor.origin().user_id);
    assert!(!keys.group_id.starts_with("server-managed:"));
    assert_eq!(keys.sender_keys.len(), 2);
    crate::rtc::PeopleMediaGrant { ticket, keys }
}

#[test]
#[ignore = "Explicit fresh named-host QA sessions; actual public TLS/TURN and native generated media; no physical devices"]
fn owned_live_friends_native_media_uses_real_admission_and_tls_turn() {
    let input = PathBuf::from(
        std::env::var_os("RELAY_LIVE_FRIENDS_MEDIA")
            .expect("Explicit owned Friends media fixture required"),
    );
    checked_input(&input, "live-friends-media.json");
    let fixture: Fixture = serde_json::from_slice(&fs::read(&input).unwrap())
        .expect("Read owned fixture without logging sessions");
    fixture
        .validate()
        .expect("Validate exact disposable media accounts");
    let root = input.parent().unwrap();
    checked_input(&root.join("owner.json"), "owner.json");
    let marker: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("owner.json")).unwrap()).unwrap();
    assert_eq!(
        marker,
        json!({"schema":1,"purpose":PURPOSE,"run_id":fixture.run_id,"user_ids":fixture.expected_user_ids})
    );
    let client = Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(8))
        .build()
        .unwrap();
    let health: serde_json::Value = client
        .get(format!("{BASE}/api/health"))
        .send()
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .unwrap();
    assert!(
        health["ok"] == true
            && health["people_service"] == true
            && health["rtc_enabled"] == true
            && health["server_id"] == SERVER,
        "Activate the named Friends media route before running this opt-in"
    );
    // Read-only identity/freshness gates precede even the first QA device upload.
    for session in &fixture.sessions {
        let boot: Bootstrap = parse(
            client
                .get(format!("{BASE}/api/bootstrap"))
                .bearer_auth(&session.token)
                .send()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(boot.server_id, SERVER);
        assert_eq!(boot.user.id, session.user.id);
        assert_eq!(boot.user.username, session.user.username);
        assert_eq!(boot.user.role, "member");
        assert!(
            boot.channels.is_empty()
                && boot.conversations.is_empty()
                && boot.social.friends.is_empty()
                && boot.social.calls.is_empty(),
            "Do not reuse existing QA history/devices or a personal account"
        );
        assert_eq!(boot.protection.level, ProtectionLevel::Protected);
        assert!(boot.capabilities.iter().any(|c| c == "people-v1"));
        assert!(
            matches!(fs::symlink_metadata(root.join(format!("keys-{}", session.user.username))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound),
            "Preserve existing fixture keys; do not replay this run"
        );
    }
    let [a, b] = fixture.sessions;
    let mut pair = Pair {
        a: CryptoWorker::new_at(BASE, root, a),
        b: CryptoWorker::new_at(BASE, root, b),
        call: None,
    };
    let _: serde_json::Value = pair
        .a
        .worker
        .post(
            "/api/people/requests",
            &json!({"handle":pair.b.worker.connection.as_ref().unwrap().session.user.username}),
        )
        .unwrap();
    let _: serde_json::Value = pair
        .b
        .worker
        .post(
            &format!("/api/friends/{}", pair.a.origin().user_id),
            &json!(null),
        )
        .unwrap();
    // Synthetic operator comparison of both self-generated exact fingerprints;
    // execute the normal verification command, never set freshness/trust flags.
    let a_public = pair.a.own();
    let b_public = pair.b.own();
    assert_ne!(a_public.fingerprint, b_public.fingerprint);
    pair.a.pin(&b_public);
    pair.b.pin(&a_public);
    let call: relay_protocol::PrivateCall = pair
        .a
        .worker
        .post(
            "/api/private-calls",
            &json!({"peer_id":pair.b.origin().user_id}),
        )
        .unwrap();
    pair.call = Some(call.id.clone());
    let accepted: relay_protocol::PrivateCall = pair
        .b
        .worker
        .post(
            &format!("/api/private-calls/{}", call.id),
            &json!({"action":"accept"}),
        )
        .unwrap();
    assert_eq!(accepted.state, "accepted");
    assert_eq!(accepted.channel_id, call.channel_id);
    assert_eq!(accepted.caller_id, pair.a.origin().user_id);
    assert_eq!(accepted.recipient_id, pair.b.origin().user_id);
    let mut au = pair.a.handle(Command::JoinVoiceRequest {
        channel_id: call.channel_id.clone(),
        request_id: 41201,
    });
    let mut bu = pair.b.handle(Command::JoinVoiceRequest {
        channel_id: call.channel_id.clone(),
        request_id: 41202,
    });
    let deadline = Instant::now() + Duration::from_secs(35);
    let mut a_keys = latest_voice_transport(std::mem::take(&mut au));
    let mut b_keys = latest_voice_transport(std::mem::take(&mut bu));
    loop {
        pair.a.worker.refresh_encryption();
        pair.b.worker.refresh_encryption();
        if let Some(value) = latest_voice_transport(pair.a.drain()) {
            a_keys = Some(value);
        }
        if let Some(value) = latest_voice_transport(pair.b.drain()) {
            b_keys = Some(value);
        }
        if a_keys
            .as_ref()
            .zip(b_keys.as_ref())
            .is_some_and(|(a, b)| a.1.epoch == b.1.epoch && a.1.group_id == b.1.group_id)
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "Production encrypted call-key exchange did not settle"
        );
        thread::sleep(Duration::from_millis(250));
    }
    let a = rtc_grant(&mut pair.a, &call.channel_id, 41203);
    let b = rtc_grant(&mut pair.b, &call.channel_id, 41204);
    assert_eq!(a.ticket.room, b.ticket.room);
    assert_eq!(a.keys.epoch, b.keys.epoch);
    assert_eq!(a.keys.group_id, b.keys.group_id);
    for (user, sender) in &a.keys.sender_keys {
        assert!(
            *sender.rtc == *b.keys.sender_keys[user].rtc,
            "MLS RTC exporter mismatch"
        );
    }
    let epoch = a.keys.epoch;
    let stop = Arc::new(AtomicBool::new(false));
    let media_stop = stop.clone();
    let duration = fixture.duration_seconds;
    let (tx, rx) = mpsc::sync_channel(1);
    let media =
        thread::spawn(move || {
            let result = tokio::runtime::Runtime::new().unwrap().block_on(
                crate::rtc::probe_live_people_media([a, b], duration, media_stop),
            );
            let _ = tx.send(result);
        });
    let limit = Instant::now() + Duration::from_secs(duration + 100);
    let mut keepalive = Instant::now() - Duration::from_secs(5);
    let result = loop {
        match rx.try_recv() {
            Ok(result) => break result,
            Err(mpsc::TryRecvError::Disconnected) => {
                break Err("Native media probe owner exited without a result".into())
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }
        if Instant::now() >= limit {
            break Err("Owned media test exceeded its overall deadline".into());
        }
        if keepalive.elapsed() >= Duration::from_secs(3) {
            let mut failed = false;
            for actor in [&mut pair.a, &mut pair.b] {
                if actor
                    .worker
                    .handle(Command::VoiceState(VoiceState {
                        muted: true,
                        deafened: true,
                        sharing: true,
                        camera: true,
                    }))
                    .is_err()
                {
                    failed = true;
                }
                actor.drain();
            }
            if failed {
                break Err("Real Friends voice-seat keepalive failed".into());
            }
            keepalive = Instant::now();
        }
        thread::sleep(Duration::from_millis(100));
    };
    stop.store(true, Ordering::Release);
    media.join().expect("Native media owner terminated cleanly");
    let mut evidence =
        result.expect("Actual public Friends native media must pass before acceptance");
    evidence["real_people_call_admission"] = json!(true);
    evidence["normal_worker_mls_freshness_exchange"] = json!(true);
    evidence["epoch"] = json!(epoch);
    evidence["shared_community"] = json!(false);
    evidence["physical_devices_opened"] = json!(false);
    evidence["two_pc_wan_acceptance"] = json!(false);
    let users = [pair.a.origin().user_id, pair.b.origin().user_id];
    for actor in [&mut pair.a, &mut pair.b] {
        actor.handle(Command::LeaveVoice);
    }
    for actor in [&mut pair.a, &mut pair.b] {
        let boot: Bootstrap = actor.worker.get("/api/bootstrap").unwrap();
        assert!(boot
            .voice_members
            .iter()
            .all(|member| !users.contains(&member.user.id)));
    }
    evidence["real_voice_seats_left_and_read_back"] = json!(true);
    drop(pair);
    let result_path = root.join("friends-native-media-result.json");
    use std::io::Write;
    let mut output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&result_path)
        .unwrap();
    output
        .write_all(&serde_json::to_vec_pretty(&evidence).unwrap())
        .unwrap();
    output.sync_all().unwrap();
    println!(
        "Owned Friends native media result: {}",
        result_path.display()
    );
}

#[test]
fn owned_live_media_refuses_reused_identities_or_another_service_before_network() {
    let run = uuid::Uuid::new_v4().to_string();
    let prefix = format!("qafm{}", &run.replace('-', "")[..12]);
    let make = |suffix| Session {
        token: "owned-placeholder".into(),
        user: User {
            id: uuid::Uuid::new_v4().to_string(),
            username: format!("{prefix}{suffix}"),
            role: "member".into(),
            ..Default::default()
        },
    };
    let a = make('a');
    let b = make('b');
    let mut f = Fixture {
        schema: 1,
        purpose: PURPOSE.into(),
        run_id: run,
        base: BASE.into(),
        server_id: SERVER.into(),
        duration_seconds: 15,
        expected_user_ids: [a.user.id.clone(), b.user.id.clone()],
        sessions: [a, b],
    };
    assert!(f.validate().is_ok());
    f.sessions[0].user.username = "personal-user".into();
    assert!(f.validate().is_err());
    f.sessions[0].user.username = format!("{prefix}a");
    f.sessions[0].user.role = "admin".into();
    assert!(f.validate().is_err());
    f.sessions[0].user.role = "member".into();
    f.base = "https://elsewhere.invalid/people".into();
    assert!(f.validate().is_err());
    f.base = BASE.into();
    f.duration_seconds = 361;
    assert!(f.validate().is_err());
}
