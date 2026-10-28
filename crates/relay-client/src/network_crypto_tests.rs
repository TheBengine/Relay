//! Opt-in real-server MLS delivery regressions. Only synthetic accounts and files.
use super::*;
use crate::e2ee::{CryptoSession, MessageProtection, MESSAGE_PREFIX};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    fs,
    net::{TcpListener, TcpStream},
    path::Path,
    process::{Child, Stdio},
    thread,
};

#[path = "network_membership_capacity_tests.rs"]
mod membership_capacity_tests;

#[path = "network_key_package_concurrency_tests.rs"]
mod key_package_concurrency_tests;

#[path = "network_archive_query_tests.rs"]
mod archive_query_tests;

#[path = "network_invite_attestation_tests.rs"]
mod invite_attestation_tests;

#[path = "network_protection_tests.rs"]
mod protection_tests;

#[path = "network_open_membership_tests.rs"]
mod open_membership_tests;

#[path = "network_default_microphone_tests.rs"]
mod default_microphone_tests;

#[cfg(feature = "native-rtc")]
#[path = "network_live_people_media.rs"]
mod live_people_media;

#[test]
#[ignore = "owned server;70seconds muted/closedPTT, actual UDP worker and resumed Opus; no devices"]
fn actual_server_silent_open_call_keeps_presence_and_resumes_without_rejoin() {
    open_membership_tests::silent_presence();
}

#[test]
#[ignore = "owned server; Open membership changes, actual UDP/Opus, no physical devices"]
fn actual_server_open_call_membership_refreshes_once_and_resumes_generated_voice() {
    open_membership_tests::run();
}

#[test]
#[ignore = "requires RELAY_NETWORK_TEST_SERVER; two isolated accounts and actual MLS actors"]
fn actual_server_open_protection_needs_the_owner_and_each_persons_agreement() {
    protection_tests::run();
}

#[test]
#[ignore = "requires RELAY_NETWORK_TEST_SERVER; owned accounts, actual MLS and localhost HTTP"]
fn actual_server_open_mode_keeps_reactions_to_old_messages_protected() {
    protection_tests::reactions();
}

#[test]
#[ignore = "requires RELAY_NETWORK_TEST_SERVER; isolated Open calls and consent withdrawal"]
fn actual_server_withdrawing_open_consent_ends_the_call_without_losing_history() {
    protection_tests::call_consent();
}

#[test]
#[ignore = "requires RELAY_NETWORK_TEST_SERVER; two isolated accounts and actual MLS actors"]
fn actual_server_invitation_verification_approves_without_pasting_and_rejects_forged_proofs() {
    invite_attestation_tests::run();
}

#[test]
#[ignore = "requires owned server; transient initial attestation and committed-response failure"]
fn actual_server_initial_invitation_attestation_retries_once_without_changing_proof() {
    invite_attestation_tests::retry_initial_attestation();
}

#[test]
#[ignore = "requires owned server; actual invitation attestation then one injected directory failure"]
fn actual_server_invited_host_pin_recovers_on_normal_refresh_after_directory_outage() {
    invite_attestation_tests::retry_after_directory_outage();
}

#[test]
#[ignore = "requires owned server; run alone in a fresh optimized process with isolated profile"]
fn threaded_worker_archived_queries_and_latency() {
    archive_query_tests::run();
}

#[test]
#[ignore = "requires RELAY_NETWORK_TEST_SERVER; actual MLS and concurrent localhost HTTP"]
fn actual_server_one_use_key_package_concurrency_and_exact_receipts() {
    key_package_concurrency_tests::run();
}

#[test]
#[ignore = "requires RELAY_NETWORK_TEST_SERVER; 64 isolated accounts and actual MLS actors"]
fn actual_server_bounded_membership_sixty_four_devices() {
    membership_capacity_tests::run();
}

struct CryptoServer {
    root: PathBuf,
    executable: PathBuf,
    child: Option<Child>,
    url: String,
    udp_port: u16,
}

impl CryptoServer {
    fn start() -> Self {
        Self::start_on_udp(0)
    }

    fn start_with_udp() -> Self {
        let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = socket.local_addr().unwrap().port();
        drop(socket);
        Self::start_on_udp(port)
    }

    fn start_on_udp(udp_port: u16) -> Self {
        let original = PathBuf::from(
            std::env::var_os("RELAY_NETWORK_TEST_SERVER")
                .expect("RELAY_NETWORK_TEST_SERVER must identify the built server"),
        );
        let base = std::env::var_os("RELAY_TEST_WORK_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("relay-tests"));
        let root = base.join(format!("mls-worker-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let executable = root.join(if cfg!(windows) {
            "relay-server.exe"
        } else {
            "relay-server"
        });
        fs::copy(&original, &executable).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let mut server = Self {
            root,
            executable,
            child: None,
            url: format!("http://{address}"),
            udp_port,
        };
        fs::write(server.root.join("server-identity.json"), serde_json::to_vec_pretty(&json!({
            "original":original,"sha256":format!("{:x}",Sha256::digest(fs::read(&server.executable).unwrap())),
            "scope":"localhost synthetic accounts/files; no audio/camera/screen devices"
        })).unwrap()).unwrap();
        server.spawn();
        server
    }

    fn spawn(&mut self) {
        assert!(self.child.is_none());
        let port = url::Url::parse(&self.url)
            .unwrap()
            .port()
            .unwrap()
            .to_string();
        let mut command = std::process::Command::new(&self.executable);
        let udp_port = self.udp_port.to_string();
        command
            .args([
                "--bind",
                "127.0.0.1",
                "--port",
                &port,
                "--udp-port",
                &udp_port,
                "--data",
            ])
            .arg(self.root.join("data"))
            .args(["--name", "Owned MLS worker regression"])
            .stdout(Stdio::from(
                fs::File::create(self.root.join("server.log")).unwrap(),
            ))
            .stderr(Stdio::from(
                fs::File::create(self.root.join("server-errors.log")).unwrap(),
            ));
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000);
        }
        self.child = Some(command.spawn().unwrap());
        let address = self.url.strip_prefix("http://").unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while TcpStream::connect(address).is_err() {
            assert!(Instant::now() < deadline, "owned server startup timed out");
            assert!(
                self.child.as_mut().unwrap().try_wait().unwrap().is_none(),
                "owned server exited"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            child.wait().unwrap();
        }
    }

    fn restart(&mut self) {
        self.stop();
        self.spawn();
    }
}

impl Drop for CryptoServer {
    fn drop(&mut self) {
        self.stop();
    }
}

struct CryptoWorker {
    worker: Worker,
    updates: mpsc::Receiver<Update>,
    keys: PathBuf,
    successful_sends: std::collections::VecDeque<Instant>,
}

impl CryptoWorker {
    fn new(server: &CryptoServer, session: Session) -> Self {
        Self::new_at(&server.url, &server.root, session)
    }

    fn new_at(server_url: &str, root: &Path, session: Session) -> Self {
        let (tx, updates) = mpsc::sync_channel(256);
        let mut worker = Worker::new(
            tx,
            eframe::egui::Context::default(),
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        let bootstrap: Bootstrap = parse(
            worker
                .http
                .get(format!("{server_url}/api/bootstrap"))
                .bearer_auth(&session.token)
                .send()
                .unwrap(),
        )
        .unwrap();
        assert!(bootstrap.capabilities.iter().any(|c| c == "e2ee_mls_v1"));
        let keys = root.join(format!("keys-{}", session.user.username));
        let crypto =
            CryptoSession::open_for_test(keys.clone(), &bootstrap.server_id, &session.user.id)
                .unwrap();
        worker.connection = Some(Connection {
            url: server_url.into(),
            session,
            current: None,
            voice_channel: None,
            voice_attempt: None,
            voice_admission: None,
            voice_recovery: None,
            frame_ids: HashMap::new(),
            sharing: false,
            camera: false,
            server_id: bootstrap.server_id.clone(),
            name: bootstrap.server_name.clone(),
            change_cursor: bootstrap.latest_change_sequence,
            protocol_version: bootstrap.protocol_version,
            remembered: false,
            chat: extended::ChatSession::new(&bootstrap),
            crypto: Some(crypto),
            crypto_registered: false,
            crypto_call_epoch: None,
            open_call_context: None,
            crypto_pending_join: None,
        });
        worker.ensure_crypto().unwrap();
        Self {
            worker,
            updates,
            keys,
            successful_sends: std::collections::VecDeque::new(),
        }
    }

    fn own(&self) -> relay_crypto::PublicDevice {
        self.worker
            .connection
            .as_ref()
            .unwrap()
            .crypto
            .as_ref()
            .unwrap()
            .advertised()
    }

    fn origin(&self) -> CommandOrigin {
        CommandOrigin::from(self.worker.connection.as_ref().unwrap())
    }

    fn handle(&mut self, command: Command) -> Vec<Update> {
        self.worker.handle(command).unwrap();
        self.drain()
    }

    fn drain(&self) -> Vec<Update> {
        self.updates.try_iter().collect()
    }

    fn drain_checked_community_owners(&self) -> Vec<Update> {
        let updates = self.drain();
        for update in &updates {
            let origin = match update {
                Update::Changes { origin, .. } | Update::MessageProtections { origin, .. } => {
                    origin.clone()
                }
                Update::CommunityBootstrap { url, bootstrap } => CommandOrigin {
                    url: url.clone(),
                    server_id: bootstrap.server_id.clone(),
                    user_id: bootstrap.user.id.clone(),
                },
                _ => continue,
            };
            let connection = self
                .worker
                .connection
                .as_ref()
                .filter(|connection| connection.url == origin.url)
                .or_else(|| {
                    self.worker
                        .saved_connections
                        .get(&origin.url)
                        .map(|saved| &saved.connection)
                })
                .expect("Emitted update belongs to a live active or parked connection");
            assert_eq!(
                origin,
                CommandOrigin::from(connection),
                "A real server update lost its verified account/server owner"
            );
        }
        updates
    }

    fn search_all(&mut self, query: &str) -> Vec<(Target, Message)> {
        let mut remaining = None;
        let mut result = Vec::new();
        for request_id in 101..229 {
            let updates = self.handle(Command::CommunitySearch {
                origin: self.origin(),
                query: query.into(),
                filters: Default::default(),
                remaining,
                request_id,
            });
            let (messages, next) = updates
                .into_iter()
                .find_map(|update| match update {
                    Update::CommunitySearch {
                        request_id: id,
                        query: returned,
                        messages,
                        remaining,
                        errors,
                    } if id == request_id => {
                        assert_eq!(returned, query);
                        assert!(errors.is_empty(), "{errors:?}");
                        Some((messages, remaining))
                    }
                    _ => None,
                })
                .expect("search must return an account-bound result");
            result.extend(messages);
            if next.is_empty() {
                return result;
            }
            remaining = Some(next);
        }
        panic!("owned search did not finish within its bounded pages");
    }

    fn pin(&mut self, peer: &relay_crypto::PublicDevice) {
        self.handle(Command::CryptoRefresh);
        self.handle(Command::CryptoVerify {
            origin: self.origin(),
            user_id: peer.identity.user_id.clone(),
            device_id: peer.identity.device_id.clone(),
            fingerprint: peer.fingerprint.clone(),
        });
    }

    fn select(&mut self, target: &Target) {
        self.handle(Command::SelectConversation(target.clone()));
    }

    fn history(&mut self, target: &Target) -> Vec<Message> {
        let updates = self.handle(Command::History {
            target: target.clone(),
            before: None,
            request_id: 1,
        });
        updates
            .into_iter()
            .find_map(|u| match u {
                Update::History { page, .. } => Some(page.messages),
                _ => None,
            })
            .expect("history emits a page")
    }

    fn raw_messages(&self, target: &Target) -> Vec<Message> {
        let request = self
            .worker
            .request(reqwest::Method::GET, "/api/messages")
            .unwrap();
        let request = match target {
            Target::Channel(id) => request.query(&[("channel_id", id)]),
            Target::Direct(id) => request.query(&[("peer_id", id)]),
        };
        parse(request.send().unwrap()).unwrap()
    }

    fn send(
        &mut self,
        target: &Target,
        content: &str,
        files: Vec<String>,
        client_id: &str,
        reply: Option<String>,
    ) {
        // Pace only these intentional new fixture sends, never retry server 429s.
        while self.successful_sends.len() >= 5 {
            let oldest = *self.successful_sends.front().unwrap();
            if let Some(remaining) = Duration::from_millis(5100).checked_sub(oldest.elapsed()) {
                thread::sleep(remaining);
            }
            self.successful_sends.pop_front();
        }
        self.select(target);
        let updates = self.handle(Command::SendRich {
            binding: None,
            origin: CommandOrigin::from(self.worker.connection.as_ref().unwrap()),
            target: target.clone(),
            content: content.into(),
            attachment_ids: files,
            client_id: client_id.into(),
            reply_to_id: reply,
            mentions: Vec::new(),
        });
        assert!(
            updates
                .iter()
                .any(|u| matches!(u,Update::SentRich {client_id: sent,..} if sent == client_id)),
            "protected send must commit"
        );
        self.successful_sends.push_back(Instant::now());
    }

    fn upload(&mut self, target: &Target, path: &Path) -> Attachment {
        self.handle(Command::StartUpload {
            origin: self.origin(),
            target: target.clone(),
            path: path.to_path_buf(),
            operation_id: "owned-upload".into(),
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            self.worker.poll_transfers();
            for update in self.drain() {
                match update {
                    Update::UploadFinished { attachment, .. } => return attachment,
                    Update::UploadFailed { error, .. } => {
                        panic!("encrypted upload failed: {error}")
                    }
                    _ => {}
                }
            }
            assert!(Instant::now() < deadline, "encrypted upload timed out");
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn download(&mut self, attachment: &Attachment, path: &Path) {
        let origin = self.origin();
        self.handle(Command::StartDownload {
            origin,
            attachment: attachment.clone(),
            path: path.to_path_buf(),
            operation_id: "owned-download".into(),
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            self.worker.poll_transfers();
            for update in self.drain() {
                match update {
                    Update::DownloadFinished { .. } => return,
                    Update::DownloadFailed { error, .. } => {
                        panic!("encrypted download failed: {error}")
                    }
                    _ => {}
                }
            }
            assert!(Instant::now() < deadline, "encrypted download timed out");
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn reopen_keys(&mut self) {
        let c = self.worker.connection.as_mut().unwrap();
        drop(c.crypto.take());
        c.crypto = Some(
            CryptoSession::open_for_test(self.keys.clone(), &c.server_id, &c.session.user.id)
                .unwrap(),
        );
        self.handle(Command::CryptoRefresh);
    }
}

impl Drop for CryptoWorker {
    fn drop(&mut self) {
        self.worker.socket_stop.store(true, Ordering::Release);
        #[cfg(windows)]
        if let Some(crypto) = self
            .worker
            .connection
            .as_ref()
            .and_then(|c| c.crypto.as_ref())
        {
            crypto.remove_test_checkpoint();
        }
    }
}

fn register(server: &CryptoServer, username: &str, invite: &str) -> Session {
    parse(
        Client::new()
            .post(format!("{}/api/register", server.url))
            .json(&AuthRequest {
                second_factor: None,
                initial_protection: None,
                username: username.into(),
                password: "Owned_Crypto_Test_1234!".into(),
                invite: invite.into(),
            })
            .send()
            .unwrap(),
    )
    .unwrap()
}

/// A member who joins through an invitation of `inviter` at this point.
fn late_member(server: &CryptoServer, inviter: &mut CryptoWorker, name: &str) -> CryptoWorker {
    let invite: serde_json::Value = inviter.worker.post("/api/invites", &json!({})).unwrap();
    CryptoWorker::new(server, register(server, name, invite["code"].as_str().unwrap()))
}

fn workers(server: &CryptoServer) -> (CryptoWorker, CryptoWorker, CryptoWorker, Target) {
    let session = register(server, "crypto_alice", "");
    let mut alice = CryptoWorker::new(server, session);
    let invite: serde_json::Value = alice.worker.post("/api/invites", &json!({})).unwrap();
    let bob = CryptoWorker::new(
        server,
        register(server, "crypto_bob", invite["code"].as_str().unwrap()),
    );
    let invite: serde_json::Value = alice.worker.post("/api/invites", &json!({})).unwrap();
    let eve = CryptoWorker::new(
        server,
        register(server, "crypto_eve", invite["code"].as_str().unwrap()),
    );
    let bootstrap: Bootstrap = alice.worker.get("/api/bootstrap").unwrap();
    let target = Target::Channel(
        bootstrap
            .channels
            .iter()
            .find(|c| c.kind == "text")
            .unwrap()
            .id
            .clone(),
    );
    alice.handle(Command::CryptoRefresh);
    (alice, bob, eve, target)
}

#[test]
#[ignore = "requires RELAY_NETWORK_TEST_SERVER; isolated server-managed calls without MLS devices"]
fn open_server_calls_work_without_e2ee_devices_and_require_each_person_to_accept() {
    let mut server = CryptoServer::start();
    let (mut alice, mut bob, _eve, _channel) = workers(&server);
    let a = alice.origin();
    alice
        .worker
        .handle(Command::SetProtection {
            origin: a,
            level: ProtectionLevel::Open,
            expected_revision: 0,
        })
        .unwrap();
    alice.worker.refresh();
    alice.drain();
    bob.worker.refresh();
    bob.drain();
    alice.worker.connection.as_mut().unwrap().crypto = None;
    bob.worker.connection.as_mut().unwrap().crypto = None;
    let bootstrap: Bootstrap = alice.worker.get("/api/bootstrap").unwrap();
    let channel = bootstrap
        .channels
        .iter()
        .find(|c| c.kind == "voice")
        .unwrap()
        .id
        .clone();
    assert!(bob
        .worker
        .handle(Command::JoinVoiceRequest {
            channel_id: channel.clone(),
            request_id: 71
        })
        .is_err());
    let a = alice.origin();
    let b = bob.origin();
    alice
        .worker
        .handle(Command::OpenConsent {
            origin: a,
            revision: Some(1),
        })
        .unwrap();
    bob.worker
        .handle(Command::OpenConsent {
            origin: b,
            revision: Some(1),
        })
        .unwrap();
    // Normal channel selection, history, refresh, edits, reactions and files
    // must work without creating or approving an MLS device in accepted open mode.
    let text_channel = bootstrap
        .channels
        .iter()
        .find(|c| c.kind == "text")
        .unwrap()
        .id
        .clone();
    let target = Target::Channel(text_channel);
    alice.select(&target);
    bob.select(&target);
    alice.send(
        &target,
        "open speech test",
        Vec::new(),
        "open-message",
        None,
    );
    let message = bob
        .history(&target)
        .into_iter()
        .find(|m| m.content == "open speech test")
        .unwrap();
    alice.handle(Command::EditMessage {
        origin: alice.origin(),
        message_id: message.id.clone(),
        content: "open edited test".into(),
    });
    assert!(bob
        .history(&target)
        .iter()
        .any(|m| m.id == message.id && m.content == "open edited test"));
    for active in [true, true, false, false] {
        bob.handle(Command::React {
            origin: bob.origin(),
            target: target.clone(),
            message_id: message.id.clone(),
            // Match the UI's Legacy/Open classification: an outer client ID
            // is not an authenticated protected-original identity.
            target_client_id: None,
            emoji: "👍".into(),
            active,
        });
        let history = alice.history(&target);
        let reacted = history.iter().find(|m| m.id == message.id).unwrap();
        assert_eq!(reacted.reactions.len(), usize::from(active));
    }
    let upload = server.root.join("open-fixture.txt");
    fs::write(&upload, b"owned plain attachment").unwrap();
    let attachment = alice.upload(&target, &upload);
    alice.send(
        &target,
        "open file",
        vec![attachment.id.clone()],
        "open-file",
        None,
    );
    let received = bob
        .history(&target)
        .into_iter()
        .find(|m| m.content == "open file")
        .unwrap();
    let download = server.root.join("open-downloaded.txt");
    bob.download(&received.attachments[0], &download);
    assert_eq!(fs::read(download).unwrap(), b"owned plain attachment");
    alice.worker.refresh();
    bob.worker.refresh();
    alice.drain();
    bob.drain();
    assert!(alice.worker.connection.as_ref().unwrap().crypto.is_none());
    assert!(bob.worker.connection.as_ref().unwrap().crypto.is_none());
    alice
        .worker
        .handle(Command::JoinVoiceRequest {
            channel_id: channel.clone(),
            request_id: 72,
        })
        .unwrap();
    bob.worker
        .handle(Command::JoinVoiceRequest {
            channel_id: channel.clone(),
            request_id: 73,
        })
        .unwrap();
    let ak = alice.worker.protected_call_keys(&channel).unwrap();
    let bk = bob.worker.protected_call_keys(&channel).unwrap();
    assert_eq!(ak.epoch, bk.epoch);
    assert!(ak.group_id.starts_with("server-managed:"));
    assert_eq!(ak.sender_keys.len(), 2);
    for (user, key) in &ak.sender_keys {
        assert_eq!(key.opus, bk.sender_keys[user].opus);
        assert_eq!(key.rtc, bk.sender_keys[user].rtc);
    }
    assert!(alice.worker.connection.as_ref().unwrap().crypto.is_none());
    assert!(bob.worker.connection.as_ref().unwrap().crypto.is_none());
    assert!(
        alice
            .worker
            .connection
            .as_ref()
            .unwrap()
            .voice_admission
            .is_some(),
        "open calls retain normal restart admission"
    );
    server.restart();
    alice.worker.refresh();
    bob.worker.refresh();
    let alice_updates = alice.drain();
    let bob_updates = bob.drain();
    for updates in [&alice_updates, &bob_updates] {
        assert!(updates.iter().any(|update| matches!(update,
            Update::VoiceEncryptionPaused { reconnecting: true, .. })),
            "a known server restart must explicitly report connection recovery, not device approval");
    }
    assert_eq!(
        alice
            .worker
            .connection
            .as_ref()
            .unwrap()
            .voice_channel
            .as_deref(),
        Some(channel.as_str())
    );
    assert_eq!(
        bob.worker
            .connection
            .as_ref()
            .unwrap()
            .voice_channel
            .as_deref(),
        Some(channel.as_str())
    );
    let restored_alice = alice.worker.protected_call_keys(&channel).unwrap();
    let restored_bob = bob.worker.protected_call_keys(&channel).unwrap();
    assert!(restored_alice.epoch > ak.epoch);
    assert_eq!(restored_alice.epoch, restored_bob.epoch);
    assert!(alice.worker.connection.as_ref().unwrap().crypto.is_none());
    assert!(bob.worker.connection.as_ref().unwrap().crypto.is_none());
    bob.worker.pause_voice_for_outage();
    bob.worker
        .connection
        .as_mut()
        .unwrap()
        .voice_recovery
        .as_mut()
        .unwrap()
        .deadline = Instant::now() - Duration::from_secs(1);
    assert!(bob.worker.protected_call_keys(&channel).is_err());
    assert!(bob
        .worker
        .connection
        .as_ref()
        .unwrap()
        .voice_channel
        .is_none());
    let a = alice.origin();
    alice
        .worker
        .handle(Command::SetProtection {
            origin: a,
            level: ProtectionLevel::Protected,
            expected_revision: 1,
        })
        .unwrap();
    bob.worker.refresh();
    bob.drain();
    assert!(!bob.worker.connection.as_ref().unwrap().chat.plain_allowed());
    assert!(
        bob.worker.protected_call_keys(&channel).is_err(),
        "protected mode never falls back to server-managed keys"
    );
}

#[test]
#[ignore = "requires owned RELAY_NETWORK_TEST_SERVER; nonhost private Open call, actual UDP/Opus and synthetic audio only"]
fn actual_server_two_nonhost_friends_exchange_open_private_voice_without_mls_approval() {
    let server = CryptoServer::start_with_udp();
    let (mut owner, mut one, mut two, _text) = workers(&server);
    let owner_origin = owner.origin();
    owner.handle(Command::SetProtection {
        origin: owner_origin.clone(),
        level: ProtectionLevel::Open,
        expected_revision: 0,
    });
    for actor in [&mut owner, &mut one, &mut two] {
        actor.worker.refresh();
        actor.drain();
    }
    assert_ne!(
        one.worker.connection.as_ref().unwrap().session.user.role,
        "admin"
    );
    assert_ne!(
        two.worker.connection.as_ref().unwrap().session.user.role,
        "admin"
    );
    // These Open callers do not depend on a local MLS actor or manual approval.
    one.worker.connection.as_mut().unwrap().crypto = None;
    two.worker.connection.as_mut().unwrap().crypto = None;
    let _: serde_json::Value = one
        .worker
        .post(
            &format!("/api/friends/{}", two.origin().user_id),
            &json!(null),
        )
        .unwrap();
    let social: relay_protocol::SocialState = two
        .worker
        .post(
            &format!("/api/friends/{}", one.origin().user_id),
            &json!(null),
        )
        .unwrap();
    assert_eq!(social.friends, vec![one.origin().user_id.clone()]);
    let ringing: relay_protocol::PrivateCall = one
        .worker
        .post(
            "/api/private-calls",
            &json!({"peer_id":two.origin().user_id}),
        )
        .unwrap();
    assert_eq!(ringing.state, "ringing");
    let incoming: relay_protocol::SocialState = two.worker.get("/api/social").unwrap();
    assert!(incoming.calls.iter().any(|call| call.id == ringing.id));
    for actor in [&one, &two] {
        assert!(actor
            .worker
            .connection
            .as_ref()
            .unwrap()
            .voice_channel
            .is_none());
    }
    let accepted: relay_protocol::PrivateCall = two
        .worker
        .post(
            &format!("/api/private-calls/{}", ringing.id),
            &json!({"action":"accept"}),
        )
        .unwrap();
    assert_eq!(accepted.state, "accepted");
    assert_eq!(accepted.channel_id, ringing.channel_id);
    let boot: Bootstrap = one.worker.get("/api/bootstrap").unwrap();
    let channel = boot
        .conversations
        .iter()
        .find(|c| c.kind == "group_voice" && c.channel_id == accepted.channel_id)
        .unwrap()
        .channel_id
        .clone();
    // Missing consent never admits a seat or exposes call keys.
    assert!(one
        .worker
        .handle(Command::JoinVoiceRequest {
            channel_id: channel.clone(),
            request_id: 9250
        })
        .is_err());
    assert!(one
        .worker
        .connection
        .as_ref()
        .unwrap()
        .voice_channel
        .is_none());
    assert!(!one
        .drain()
        .iter()
        .any(|u| matches!(u, Update::VoiceJoined { .. })));
    for actor in [&mut owner, &mut one, &mut two] {
        let origin = actor.origin();
        actor.handle(Command::OpenConsent {
            origin,
            revision: Some(1),
        });
    }
    assert!(
        owner
            .worker
            .handle(Command::JoinVoiceRequest {
                channel_id: channel.clone(),
                request_id: 9251
            })
            .is_err(),
        "even the nonmember admin cannot join this private call through the API"
    );
    assert!(owner.worker.protected_call_keys(&channel).is_err());
    let mut one_updates = one.handle(Command::JoinVoiceRequest {
        channel_id: channel.clone(),
        request_id: 9252,
    });
    let two_updates = two.handle(Command::JoinVoiceRequest {
        channel_id: channel.clone(),
        request_id: 9253,
    });
    one.worker.refresh_encryption();
    one_updates.extend(one.drain());
    let (one_ticket, one_keys) = latest_voice_transport(one_updates).unwrap();
    let (two_ticket, two_keys) = latest_voice_transport(two_updates).unwrap();
    assert_eq!(one_keys.epoch, two_keys.epoch);
    assert_eq!(one_keys.group_id, two_keys.group_id);
    assert!(one_keys.group_id.starts_with("server-managed:"));
    assert_eq!(one_keys.sender_keys.len(), 2);
    assert!(!one_keys.sender_keys.contains_key(&owner_origin.user_id));
    let mut one_audio = crate::media::LivingVoiceProbe::start(&one_ticket, one_keys);
    let mut two_audio = crate::media::LivingVoiceProbe::start(&two_ticket, two_keys);
    let deadline = Instant::now() + Duration::from_secs(4);
    while !one_audio.played() || !two_audio.played() {
        one_audio.feed();
        two_audio.feed();
        assert!(
            Instant::now() < deadline,
            "both nonhost private calls must decode actual generated 10 ms Opus"
        );
        thread::sleep(Duration::from_millis(10));
    }
    one_audio.assert_healthy();
    two_audio.assert_healthy();
    one_audio.finish();
    two_audio.finish();
    one.handle(Command::LeaveVoice);
    assert!(one.worker.protected_call_keys(&channel).is_err());
    let after_leave = two.worker.protected_call_keys(&channel).unwrap();
    assert_eq!(after_leave.sender_keys.len(), 1);
    assert!(!after_leave.sender_keys.contains_key(&one.origin().user_id));
    two.handle(Command::LeaveVoice);
    let ended: relay_protocol::SocialState = two.worker.get("/api/social").unwrap();
    assert!(ended
        .calls
        .iter()
        .any(|call| call.id == ringing.id && call.state == "ended"));
    for actor in [&one, &two] {
        assert!(actor.worker.connection.as_ref().unwrap().crypto.is_none());
    }
    fs::write(
        server.root.join("nonhost-private-open-voice.json"),
        serde_json::to_vec_pretty(&json!({
            "two_nonhost_friend_callers":true, "private_call_ring_and_accept":true,
            "no_voice_admission_from_ring_alone":true, "leave_ends_call_signalling":true,
            "actual_udp_and_opus_both_directions":true,
            "synthetic_tone_virtual_playback_only":true, "packet_samples":480,
            "explicit_revision_before_admission":true, "nonmember_admin_denied":true,
            "leave_revokes_future_keys":true, "local_mls_actor_not_recreated":true,
            "physical_devices_opened":false, "wan_or_gui_accepted":false
        }))
        .unwrap(),
    )
    .unwrap();
    println!(
        "Nonhost private Open voice evidence: {}",
        server.root.display()
    );
}

#[test]
#[ignore = "requires RELAY_NETWORK_TEST_SERVER; owned first-contact messages, no comparison"]
fn actual_server_first_contact_messages_work_both_ways_without_comparing_codes() {
    let server = CryptoServer::start();
    let (mut alice, mut bob, _eve, channel) = workers(&server);
    let to_bob = Target::Direct(bob.origin().user_id);
    let to_alice = Target::Direct(alice.origin().user_id);
    // Nobody pins or compares anything: the very first message is delivered.
    alice.send(&to_bob, "first contact 4c1e", vec![], "first-contact", None);
    bob.select(&to_alice);
    assert!(bob
        .history(&to_alice)
        .iter()
        .any(|m| m.content == "first contact 4c1e"));
    bob.send(&to_alice, "first reply 9d2f", vec![], "first-reply", None);
    alice.select(&to_bob);
    assert!(alice
        .history(&to_bob)
        .iter()
        .any(|m| m.content == "first reply 9d2f"));
    // The host stored ciphertext only.
    assert!(alice
        .raw_messages(&to_bob)
        .iter()
        .all(|m| !m.content.contains("4c1e") && !m.content.contains("9d2f")));
    // A protected channel works the same way, and nobody is shown as verified.
    alice.send(&channel, "channel first contact 71aa", vec![], "channel-first", None);
    bob.select(&channel);
    assert!(bob
        .history(&channel)
        .iter()
        .any(|m| m.content == "channel first contact 71aa"));
    let status = alice
        .worker
        .connection
        .as_ref()
        .unwrap()
        .crypto
        .as_ref()
        .unwrap()
        .status
        .clone();
    let bob_device = status
        .peers
        .iter()
        .find(|p| p.device_id == bob.own().identity.device_id)
        .unwrap();
    assert!(bob_device.trusted && !bob_device.verified && !bob_device.needs_review);
    assert!(status.notices.is_empty(), "first contact shows no warning");
}

#[test]
#[ignore = "requires RELAY_NETWORK_TEST_SERVER; owned encrypted unread pages and reopen only"]
fn actual_server_unread_history_after_keeps_read_floor_and_decrypts_channel_and_direct() {
    let server = CryptoServer::start();
    let (mut alice, mut bob, _eve, channel) = workers(&server);
    alice.pin(&bob.own());
    bob.pin(&alice.own());
    alice.send(
        &channel,
        "own message before unread reply",
        vec![],
        "unread-own",
        None,
    );
    bob.history(&channel);
    bob.send(
        &channel,
        "first authenticated unread reply",
        vec![],
        "unread-reply",
        None,
    );
    let direct_alice = Target::Direct(bob.origin().user_id);
    let direct_bob = Target::Direct(alice.origin().user_id);
    bob.send(
        &direct_bob,
        "first authenticated private unread",
        vec![],
        "unread-private",
        None,
    );
    let before: Bootstrap = alice.worker.get("/api/bootstrap").unwrap();
    let read_floor = |bootstrap: &Bootstrap, target: &Target| {
        bootstrap
            .read_states
            .iter()
            .find(|read| match target {
                Target::Channel(id) => read.channel_id.as_ref() == Some(id),
                Target::Direct(id) => read.peer_id.as_ref() == Some(id),
            })
            .map_or(0, |read| read.last_read_sequence)
    };
    for (index, target, expected) in [
        (0, channel.clone(), "first authenticated unread reply"),
        (
            1,
            direct_alice.clone(),
            "first authenticated private unread",
        ),
    ] {
        let origin = alice.origin();
        let floor = read_floor(&before, &target);
        let mut navigation =
            crate::unread_navigation::UnreadNavigation::new(origin.clone(), target.clone(), floor);
        navigation.pending = Some(440 + index);
        let updates = alice.handle(Command::HistoryAfter {
            origin: origin.clone(),
            target: target.clone(),
            after: floor,
            request_id: 440 + index,
        });
        let page = updates
            .into_iter()
            .find_map(|update| match update {
                Update::UnreadHistory {
                    origin: received,
                    target: returned,
                    page,
                    request_id,
                    ..
                } => {
                    assert!(navigation.accepts(
                        &received,
                        &returned,
                        request_id,
                        Some(&origin),
                        Some(&target)
                    ));
                    Some(page)
                }
                _ => None,
            })
            .expect("actual encrypted history-after page");
        let first = navigation
            .first_incoming(&page.messages)
            .expect("authenticated incoming row");
        assert_eq!(first.content, expected);
        assert!(first.sequence > floor);
        assert!(!first.content.starts_with(MESSAGE_PREFIX));
        let raw = alice.raw_messages(&target);
        assert!(raw
            .iter()
            .any(|row| row.id == first.id && row.content.starts_with(MESSAGE_PREFIX)));
        for row in raw {
            assert_absent(row.content.as_bytes(), expected.as_bytes());
        }
    }
    alice.reopen_keys();
    let repeated = alice.handle(Command::HistoryAfter {
        origin: alice.origin(),
        target: channel.clone(),
        after: read_floor(&before, &channel),
        request_id: 450,
    });
    assert!(repeated.iter().any(|update| matches!(update, Update::UnreadHistory { page, request_id: 450, .. } if page.messages.iter().any(|message| message.content == "first authenticated unread reply"))));
    let after: Bootstrap = alice.worker.get("/api/bootstrap").unwrap();
    assert_eq!(read_floor(&before, &channel), read_floor(&after, &channel));
    assert_eq!(
        read_floor(&before, &direct_alice),
        read_floor(&after, &direct_alice)
    );
    let mut stale = alice.origin();
    stale.server_id = "different-server".into();
    assert!(alice
        .worker
        .handle(Command::HistoryAfter {
            origin: stale,
            target: channel,
            after: 0,
            request_id: 451
        })
        .is_err());
    assert!(!alice.drain().iter().any(|update| matches!(
        update,
        Update::UnreadHistory {
            request_id: 451,
            ..
        }
    )));
}

fn assert_absent(bytes: &[u8], private: &[u8]) {
    assert!(
        !bytes.windows(private.len()).any(|part| part == private),
        "private bytes leaked to untrusted storage"
    );
}

#[test]
#[ignore = "requires RELAY_NETWORK_TEST_SERVER; two isolated communities and background MLS catch-up"]
fn actual_server_two_communities_background_epoch_catch_up_keeps_active_actor() {
    let mut server_a = CryptoServer::start();
    let server_b = CryptoServer::start();
    let (mut alice_a, mut bob_a, eve_a, target_a) = workers(&server_a);
    let (mut alice_b, mut bob_b, _eve_b, target_b) = workers(&server_b);
    alice_a.pin(&bob_a.own());
    bob_a.pin(&alice_a.own());
    bob_a.pin(&eve_a.own());
    alice_b.pin(&bob_b.own());
    bob_b.pin(&alice_b.own());
    alice_a.send(
        &target_a,
        "A before rotation",
        Vec::new(),
        "bg-a-before",
        None,
    );
    assert_eq!(bob_a.history(&target_a)[0].content, "A before rotation");
    alice_b.send(
        &target_b,
        "B active message",
        Vec::new(),
        "bg-b-active",
        None,
    );
    assert_eq!(bob_b.history(&target_b)[0].content, "B active message");
    bob_b.select(&target_b);
    let bootstrap: Bootstrap = bob_a.worker.get("/api/bootstrap").unwrap();
    let mut parked = bob_a.worker.connection.take().unwrap();
    parked.change_cursor = bootstrap.latest_change_sequence;
    let parked_device = parked.crypto.as_ref().unwrap().device.identity().clone();
    let active_device = bob_b.own().identity;
    let active_scope = bob_b.worker.crypto_scope(&target_b).unwrap();
    let active_cursor = bob_b.worker.crypto().unwrap().cursor(&active_scope);
    bob_b.worker.saved_connections.insert(
        server_a.url.clone(),
        SavedConnection {
            connection: parked,
            stop: Arc::new(AtomicBool::new(false)),
            dirty: Arc::new(AtomicBool::new(true)),
        },
    );
    // Both old-epoch and new-epoch applications must survive background catch-up.
    alice_a.send(
        &target_a,
        "A while parked old epoch",
        Vec::new(),
        "bg-a-old",
        None,
    );
    // Trust on first use added Eve with the first message; a member who joins
    // while Bob is parked brings the membership epoch under test.
    let late = late_member(&server_a, &mut alice_a, "crypto_parked_late");
    alice_a.pin(&late.own());
    alice_a
        .worker
        .ensure_crypto_conversation(&target_a)
        .unwrap();
    alice_a.send(
        &target_a,
        "A while parked new epoch",
        Vec::new(),
        "bg-a-new",
        None,
    );
    let began = Instant::now();
    let mut observed = Vec::new();
    let mut poll_samples = Vec::new();
    let mut maximum_poll_ms = 0;
    let mut interleaved_commands = 0;
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        bob_b.worker.background_at = Instant::now() - Duration::from_secs(1);
        let phase_before = bob_b
            .worker
            .background_pending
            .front()
            .map(|p| p.diagnostic_phase())
            .unwrap_or("waiting_public_io");
        let before_revision = bob_b.worker.saved_connections[&server_a.url]
            .connection
            .crypto
            .as_ref()
            .unwrap()
            .checkpoint_revision_for_test();
        let poll = Instant::now();
        bob_b.worker.refresh_background();
        let elapsed = poll.elapsed().as_millis();
        maximum_poll_ms = maximum_poll_ms.max(elapsed);
        let checkpoint_delta = bob_b.worker.saved_connections[&server_a.url]
            .connection
            .crypto
            .as_ref()
            .unwrap()
            .checkpoint_revision_for_test()
            - before_revision;
        assert!(
            checkpoint_delta <= 1,
            "background round saved more than one checkpoint: {checkpoint_delta}"
        );
        poll_samples.push(json!({"before":phase_before,"after":bob_b.worker.background_pending.front().map(|p| p.diagnostic_phase()).unwrap_or("no_pending"),"elapsed_ms":elapsed,"checkpoints":checkpoint_delta}));
        if !bob_b.worker.background_pending.is_empty() {
            // An actual queued UI command can execute before the next crypto step.
            bob_b.worker.handle(Command::ClearSelection).unwrap();
            assert!(bob_b.worker.connection.as_ref().unwrap().current.is_none());
            interleaved_commands += 1;
        }
        for update in bob_b.drain() {
            if let Update::Changes { origin, changes } = update {
                assert_eq!(
                    origin,
                    CommandOrigin::from(&bob_b.worker.saved_connections[&server_a.url].connection)
                );
                observed.extend(
                    changes
                        .into_iter()
                        .filter_map(|c| c.message)
                        .map(|m| m.content),
                );
            }
        }
        if bob_b.worker.background_jobs.is_empty() {
            break;
        }
        assert!(Instant::now() < deadline, "background catch-up timed out");
        thread::sleep(Duration::from_millis(10));
    }
    let saved = &bob_b.worker.saved_connections[&server_a.url];
    let parked_cursor = saved.connection.change_cursor;
    assert_eq!(
        saved.connection.crypto.as_ref().unwrap().device.identity(),
        &parked_device
    );
    assert_eq!(bob_b.own().identity, active_device);
    assert_eq!(
        bob_b.worker.crypto().unwrap().cursor(&active_scope),
        active_cursor
    );
    fs::write(
        server_a.root.join("two-community-diagnostic.json"),
        serde_json::to_vec_pretty(&json!({
            "background_contents": observed, "elapsed_ms": began.elapsed().as_millis(),
            "maximum_command_poll_ms": maximum_poll_ms, "command_poll_samples":poll_samples,
            "commands_interleaved_before_background_delivery": interleaved_commands,
            "active_identity_and_control_cursor_unchanged": true, "physical_devices_opened": false
        }))
        .unwrap(),
    )
    .unwrap();
    println!("Two-community evidence: {}", server_a.root.display());
    // The same public polling event must not deliver notifications twice.
    bob_b
        .worker
        .saved_connections
        .get(&server_a.url)
        .unwrap()
        .dirty
        .store(true, Ordering::Relaxed);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        bob_b.worker.background_at = Instant::now() - Duration::from_secs(1);
        bob_b.worker.refresh_background();
        assert!(!bob_b
            .drain()
            .iter()
            .any(|u| matches!(u, Update::Changes { .. })));
        if bob_b.worker.background_jobs.is_empty() {
            break;
        }
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        bob_b.worker.saved_connections[&server_a.url]
            .connection
            .change_cursor,
        parked_cursor
    );
    server_a.stop();
    bob_b
        .worker
        .saved_connections
        .get(&server_a.url)
        .unwrap()
        .dirty
        .store(true, Ordering::Relaxed);
    bob_b.worker.background_at = Instant::now() - Duration::from_secs(1);
    let schedule = Instant::now();
    bob_b.worker.refresh_background();
    assert!(
        schedule.elapsed() < Duration::from_millis(200),
        "background HTTP scheduled synchronously"
    );
    alice_b.send(
        &target_b,
        "B survives A outage",
        Vec::new(),
        "bg-b-outage",
        None,
    );
    assert!(bob_b
        .history(&target_b)
        .iter()
        .any(|m| m.content == "B survives A outage"));
    // Recover the owned actor before assertions so fixture cleanup always owns it.
    bob_a.worker.connection = Some(
        bob_b
            .worker
            .saved_connections
            .remove(&server_a.url)
            .unwrap()
            .connection,
    );
    assert!(observed.iter().any(|m| m == "A while parked old epoch"));
    assert!(
        observed.iter().any(|m| m == "A while parked new epoch"),
        "background MLS rotation lost new authenticated message: {observed:?}"
    );
    assert!(!observed
        .iter()
        .any(|m| m == "Encrypted message unavailable"));
    assert!(
        interleaved_commands >= 6,
        "catch-up must yield between saved key/history steps"
    );
    assert!(
        poll_samples
            .iter()
            .any(|s| s["before"] == "bind_applications"),
        "binding must yield after authenticated receive"
    );
    assert!(
        maximum_poll_ms < 500,
        "small owned catch-up still monopolized the command loop: {maximum_poll_ms} ms"
    );
}

#[test]
#[ignore = "requires an owned server; cancel/reopen at the saved receive before binding boundary"]
fn actual_server_background_cancel_between_received_and_bound_history_preserves_prefix() {
    let server_a = CryptoServer::start();
    let server_b = CryptoServer::start();
    let (mut alice_a, mut bob_a, eve_a, target_a) = workers(&server_a);
    let (mut alice_b, mut bob_b, _eve_b, target_b) = workers(&server_b);
    alice_a.pin(&bob_a.own());
    bob_a.pin(&alice_a.own());
    bob_a.pin(&eve_a.own());
    alice_b.pin(&bob_b.own());
    bob_b.pin(&alice_b.own());
    alice_a.send(
        &target_a,
        "A before rotation",
        Vec::new(),
        "bg-a-before",
        None,
    );
    assert_eq!(bob_a.history(&target_a)[0].content, "A before rotation");
    alice_b.send(
        &target_b,
        "B active message",
        Vec::new(),
        "bg-b-active",
        None,
    );
    assert_eq!(bob_b.history(&target_b)[0].content, "B active message");
    bob_b.select(&target_b);
    let bootstrap: Bootstrap = bob_a.worker.get("/api/bootstrap").unwrap();
    let parked_scope = bob_a.worker.crypto_scope(&target_a).unwrap();
    let mut parked = bob_a.worker.connection.take().unwrap();
    parked.change_cursor = bootstrap.latest_change_sequence;
    let parked_device = parked.crypto.as_ref().unwrap().device.identity().clone();
    let active_device = bob_b.own().identity;
    let active_scope = bob_b.worker.crypto_scope(&target_b).unwrap();
    let active_cursor = bob_b.worker.crypto().unwrap().cursor(&active_scope);
    bob_b.worker.saved_connections.insert(
        server_a.url.clone(),
        SavedConnection {
            connection: parked,
            stop: Arc::new(AtomicBool::new(false)),
            dirty: Arc::new(AtomicBool::new(true)),
        },
    );
    // Both old-epoch and new-epoch applications must survive background catch-up.
    alice_a.send(
        &target_a,
        "A while parked old epoch",
        Vec::new(),
        "bg-a-old",
        None,
    );
    // Trust on first use added Eve with the first message; a member who joins
    // while Bob is parked brings the membership epoch under test.
    let late = late_member(&server_a, &mut alice_a, "crypto_parked_late");
    alice_a.pin(&late.own());
    alice_a
        .worker
        .ensure_crypto_conversation(&target_a)
        .unwrap();
    alice_a.send(
        &target_a,
        "A while parked new epoch",
        Vec::new(),
        "bg-a-new",
        None,
    );

    let original_cursor = bob_b.worker.saved_connections[&server_a.url]
        .connection
        .change_cursor;
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        bob_b.worker.background_at = Instant::now() - Duration::from_secs(1);
        let before = bob_b.worker.saved_connections[&server_a.url]
            .connection
            .crypto
            .as_ref()
            .unwrap()
            .checkpoint_revision_for_test();
        bob_b.worker.refresh_background();
        assert!(
            !bob_b
                .drain()
                .iter()
                .any(|u| matches!(u, Update::Changes { .. })),
            "incomplete page reached the UI"
        );
        if bob_b
            .worker
            .background_pending
            .front()
            .is_some_and(|p| p.diagnostic_phase() == "bind_applications")
        {
            let crypto = bob_b.worker.saved_connections[&server_a.url]
                .connection
                .crypto
                .as_ref()
                .unwrap();
            assert_eq!(crypto.checkpoint_revision_for_test() - before, 1);

            assert!(
                !crypto.cached_envelopes(&parked_scope).any(|entry| entry
                    .unwrap()
                    .envelope
                    .content
                    == "A while parked old epoch"),
                "unbound receive appeared as delivered history"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "binding boundary was not reached"
        );
        thread::sleep(Duration::from_millis(10));
    }
    let mut removed = bob_b
        .worker
        .saved_connections
        .remove(&server_a.url)
        .unwrap()
        .connection;
    assert_eq!(removed.change_cursor, original_cursor);
    drop(removed.crypto.take());
    bob_b.worker.refresh_background();
    assert!(bob_b.worker.background_pending.is_empty());
    assert!(!bob_b
        .drain()
        .iter()
        .any(|u| matches!(u, Update::Changes { .. })));
    removed.crypto = Some(
        CryptoSession::open_for_test(
            bob_a.keys.clone(),
            &removed.server_id,
            &removed.session.user.id,
        )
        .unwrap(),
    );
    assert_eq!(
        removed.crypto.as_ref().unwrap().device.identity(),
        &parked_device
    );
    bob_a.worker.connection = Some(removed);
    let recovered = bob_a.history(&target_a);
    assert!(recovered
        .iter()
        .any(|m| m.content == "A while parked old epoch"));
    assert!(recovered
        .iter()
        .any(|m| m.content == "A while parked new epoch"));
    assert!(!recovered
        .iter()
        .any(|m| m.content == "Encrypted message unavailable"));
    assert_eq!(bob_b.own().identity, active_device);
    assert_eq!(
        bob_b.worker.crypto().unwrap().cursor(&active_scope),
        active_cursor
    );
    fs::write(
        server_a.root.join("background-binding-cancel.json"),
        serde_json::to_vec_pretty(&json!({
            "received_prefix_saved_once":true, "unbound_receive_hidden_from_delivered_history":true,
            "no_partial_changes_to_ui":true, "cancel_keeps_changes_cursor":true,
            "reopened_actor_same_identity":true, "old_and_new_epoch_history_recovered":true,
            "active_community_unchanged":true, "physical_devices_opened":false
        }))
        .unwrap(),
    )
    .unwrap();
    println!(
        "Background binding cancellation evidence: {}",
        server_a.root.display()
    );
}

#[test]
#[ignore = "requires RELAY_NETWORK_TEST_SERVER; authenticated changes must retain their message routing"]
fn actual_server_change_routing_rejected_before_private_actor_mutation() {
    let server = CryptoServer::start();
    let (mut alice, mut bob, _eve, target) = workers(&server);
    alice.pin(&bob.own());
    bob.pin(&alice.own());
    alice.send(
        &target,
        "establish owned group",
        Vec::new(),
        "routing-warmup",
        None,
    );
    assert_eq!(bob.history(&target)[0].content, "establish owned group");
    alice.send(
        &target,
        "private correctly routed body",
        Vec::new(),
        "routing-private",
        None,
    );
    let raw = bob
        .raw_messages(&target)
        .into_iter()
        .find(|m| m.client_id.as_deref() == Some("routing-private"))
        .unwrap();
    assert!(raw.content.starts_with(MESSAGE_PREFIX));
    let original = MessageChange {
        sequence: 1,
        kind: "created".into(),
        message_id: raw.id.clone(),
        channel_id: raw.channel_id.clone(),
        recipient_id: raw.recipient_id.clone(),
        user_id: raw.author.id.clone(),
        message: Some(raw),
        ..Default::default()
    };
    let actor_path = protected_device_path(&bob.keys);
    let saved_actor = fs::read(&actor_path).unwrap();
    bob.drain();
    for field in 0..4 {
        let mut altered = original.clone();
        match field {
            0 => altered.message_id = "another-message".into(),
            1 => altered.channel_id = Some("another-room".into()),
            2 => altered.recipient_id = Some("another-recipient".into()),
            _ => altered.user_id = "another-author".into(),
        }
        let mut changes = vec![altered.clone()];
        let protections = encrypted::decrypt_changes_in_connection(
            bob.worker.connection.as_mut().unwrap(),
            &mut changes,
        );
        assert!(
            changes.is_empty() && protections.is_empty(),
            "mismatched field {field} reached the conversation"
        );
        let mut changes = vec![altered.clone()];
        bob.worker.decrypt_changes(&mut changes);
        assert!(changes.is_empty());
        assert!(!bob
            .drain()
            .iter()
            .any(|u| matches!(u, Update::ReactionSnapshot { .. })));
        let mut pending = background_crypto::Pending::new(
            BackgroundResult {
                url: server.url.clone(),
                token: bob
                    .worker
                    .connection
                    .as_ref()
                    .unwrap()
                    .session
                    .token
                    .clone(),
                bootstrap: Ok(Bootstrap::default()),
                changes: Some(Ok(ChangePage {
                    changes: vec![altered],
                    next_cursor: 1,
                    has_more: false,
                })),
                keys: Ok(Vec::new()),
                protections: Vec::new(),
            },
            &bob.own().identity.user_id,
        );
        assert!(!pending
            .advance(bob.worker.connection.as_mut().unwrap())
            .unwrap());
        assert!(pending
            .advance(bob.worker.connection.as_mut().unwrap())
            .unwrap());
        assert!(pending
            .result
            .changes
            .as_ref()
            .unwrap()
            .as_ref()
            .unwrap()
            .changes
            .is_empty());
        assert!(pending.result.protections.is_empty());
        assert_eq!(
            fs::read(&actor_path).unwrap(),
            saved_actor,
            "rejected routing ratcheted or bound private state"
        );
    }
    // Rejection cannot consume the legitimate application or poison its cache.
    let mut changes = vec![original];
    let protections = encrypted::decrypt_changes_in_connection(
        bob.worker.connection.as_mut().unwrap(),
        &mut changes,
    );
    assert_eq!(changes.len(), 1);
    assert_eq!(
        changes[0].message.as_ref().unwrap().content,
        "private correctly routed body"
    );
    assert!(protections
        .iter()
        .all(|(_, p)| matches!(p, MessageProtection::Protected)));
    fs::write(
        server.root.join("change-routing-diagnostic.json"),
        serde_json::to_vec_pretty(&json!({
            "rejected_outer_fields": ["message_id", "channel_id", "recipient_id", "user_id"],
            "paths_checked": ["active_worker", "connection_decoder", "background_pending"],
            "rejected_changes_exposed": 0, "private_checkpoint_unchanged_after_rejection": true,
            "legitimate_application_still_decrypts": true, "physical_devices_opened": false
        }))
        .unwrap(),
    )
    .unwrap();
    println!("Change-routing evidence: {}", server.root.display());
}

#[test]
#[ignore = "requires RELAY_NETWORK_TEST_SERVER; owned Discord conversation fixtures only"]
fn actual_server_mls_worker_groups_voice_text_threads_polls_inbox_and_revocation() {
    let mut server = CryptoServer::start();
    let (mut alice, mut bob, mut eve, general) = workers(&server);
    let a = alice.own();
    let b = bob.own();
    let e = eve.own();
    alice.pin(&b);
    alice.pin(&e);
    bob.pin(&a);
    bob.pin(&e);
    eve.pin(&a);
    eve.pin(&b);
    let a_id = a.identity.user_id.clone();
    let b_id = b.identity.user_id.clone();
    let _: serde_json::Value = alice
        .worker
        .post(&format!("/api/friends/{b_id}"), &json!({}))
        .unwrap();
    let _: serde_json::Value = bob
        .worker
        .post(&format!("/api/friends/{a_id}"), &json!({}))
        .unwrap();
    let e_id = e.identity.user_id.clone();
    let _: serde_json::Value = bob
        .worker
        .post(&format!("/api/friends/{e_id}"), &json!({}))
        .unwrap();
    let _: serde_json::Value = eve
        .worker
        .post(&format!("/api/friends/{b_id}"), &json!({}))
        .unwrap();
    // A group created by a member must exclude an unrelated community owner.
    let group: ConversationInfo = bob
        .worker
        .post(
            "/api/conversations",
            &json!({"name":"Owned private squad","kind":"group","member_ids":[e_id]}),
        )
        .unwrap();
    assert_eq!(
        alice
            .worker
            .request(reqwest::Method::GET, "/api/history")
            .unwrap()
            .query(&[("channel_id", &group.channel_id)])
            .send()
            .unwrap()
            .status(),
        reqwest::StatusCode::FORBIDDEN
    );
    let target = Target::Channel(group.channel_id.clone());
    bob.send(
        &target,
        "owned private group secret 37fb30",
        vec![],
        "group-first",
        None,
    );
    assert_eq!(
        eve.history(&target).last().unwrap().content,
        "owned private group secret 37fb30"
    );
    let poll = "relay-poll:v1:{\"question\":\"Owned encrypted poll 37fb30?\",\"options\":[\"Friday\",\"Saturday\"],\"multiple\":false}";
    let poll_id = uuid::Uuid::new_v4().to_string();
    let updates = bob.handle(Command::SendRich {
        binding: None,
        origin: bob.origin(),
        target: target.clone(),
        content: poll.into(),
        attachment_ids: vec![],
        client_id: poll_id.clone(),
        reply_to_id: None,
        mentions: vec![e_id.clone()],
    });
    assert!(updates
        .iter()
        .any(|u| matches!(u, Update::SentRich { client_id, .. } if client_id == &poll_id)));
    let received = eve
        .history(&target)
        .into_iter()
        .find(|m| m.client_id.as_deref() == Some(&poll_id))
        .unwrap();
    assert_eq!(received.content, poll);
    assert_eq!(received.mentions, vec![e_id.clone()]);
    eve.handle(Command::React {
        origin: eve.origin(),
        target: target.clone(),
        message_id: received.id.clone(),
        target_client_id: Some(poll_id),
        emoji: "relay-poll-vote:1".into(),
        active: true,
    });
    let voted = bob
        .history(&target)
        .into_iter()
        .find(|m| m.id == received.id)
        .unwrap();
    assert_eq!(voted.reactions.len(), 1);
    assert_eq!(voted.reactions[0].emoji, "relay-poll-vote:1");
    assert_eq!(voted.reactions[0].user_ids, vec![e_id.clone()]);
    let inbox = eve.handle(Command::MentionInbox {
        origin: eve.origin(),
        remaining: None,
        request_id: 91,
    });
    assert!(inbox.iter().any(|u| matches!(u, Update::MentionInbox { messages, errors, .. } if errors.is_empty() && messages.iter().any(|(t,m)| *t == target && m.id == received.id && m.content == poll))));
    for raw in bob.raw_messages(&target) {
        assert!(raw.content.starts_with(MESSAGE_PREFIX));
        assert!(!raw.content.contains("37fb30"));
        assert!(raw.mentions.is_empty());
        assert!(
            raw.reactions.is_empty(),
            "poll votes must stay inside protected control messages"
        );
    }
    let bootstrap: Bootstrap = alice.worker.get("/api/bootstrap").unwrap();
    let voice = bootstrap
        .channels
        .iter()
        .find(|c| c.kind == "voice")
        .unwrap();
    let companion = bootstrap
        .channels
        .iter()
        .find(|c| c.voice_parent_id.as_deref() == Some(&voice.id))
        .unwrap();
    let voice_text = Target::Channel(companion.id.clone());
    alice.send(
        &voice_text,
        "owned durable voice text 8ef391",
        vec![],
        "voice-text",
        None,
    );
    assert_eq!(
        bob.history(&voice_text).last().unwrap().content,
        "owned durable voice text 8ef391"
    );
    assert!(
        alice
            .worker
            .get::<Bootstrap>("/api/bootstrap")
            .unwrap()
            .voice_members
            .is_empty(),
        "text must not open media or join a call"
    );
    let Target::Channel(parent) = general else {
        panic!("owned general channel required")
    };
    let thread: ConversationInfo = bob
        .worker
        .post(
            "/api/conversations",
            &json!({"name":"Owned encrypted thread","kind":"thread","parent_id":parent}),
        )
        .unwrap();
    let thread_target = Target::Channel(thread.channel_id);
    alice.send(
        &thread_target,
        "owned separate thread secret 75f6af",
        vec![],
        "thread-first",
        None,
    );
    assert_eq!(
        bob.history(&thread_target).last().unwrap().content,
        "owned separate thread secret 75f6af"
    );
    assert_ne!(
        bob.worker.crypto_scope(&thread_target).unwrap(),
        bob.worker.crypto_scope(&voice_text).unwrap()
    );
    assert_ne!(
        bob.worker.crypto_scope(&target).unwrap(),
        bob.worker.crypto_scope(&voice_text).unwrap()
    );
    let _: ConversationInfo = parse(
        bob.worker
            .request(
                reqwest::Method::PATCH,
                &format!("/api/conversations/{}", group.channel_id),
            )
            .unwrap()
            .json(&json!({"member_ids":[b_id,a_id]}))
            .send()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        eve.worker
            .request(reqwest::Method::GET, "/api/history")
            .unwrap()
            .query(&[("channel_id", &group.channel_id)])
            .send()
            .unwrap()
            .status(),
        reqwest::StatusCode::FORBIDDEN
    );
    bob.send(
        &target,
        "owned future group secret 73ef21",
        vec![],
        "group-after-revoke",
        None,
    );
    assert_eq!(
        alice.history(&target).last().unwrap().content,
        "owned future group secret 73ef21"
    );
    let future = bob.raw_messages(&target).last().unwrap().content.clone();
    let old_scope = eve.worker.crypto_scope(&target).unwrap();
    assert!(eve
        .worker
        .connection
        .as_mut()
        .unwrap()
        .crypto
        .as_mut()
        .unwrap()
        .decrypt_content(&old_scope, &future)
        .is_err());
    bob.reopen_keys();
    alice.reopen_keys();
    server.restart();
    assert!(bob
        .history(&target)
        .iter()
        .any(|m| m.content == poll && m.reactions.iter().any(|r| r.emoji == "relay-poll-vote:1")));
    assert_eq!(
        alice.history(&voice_text).last().unwrap().content,
        "owned durable voice text 8ef391"
    );
    let empty: Channel = alice
        .worker
        .post(
            "/api/channels",
            &json!({"name":"owned-empty-search","kind":"text"}),
        )
        .unwrap();
    let context_url = format!(
        "/api/e2ee/context?channel_id={}&device_id={}",
        empty.id, b.identity.device_id
    );
    let before_empty: CryptoContext = bob.worker.get(&context_url).unwrap();
    assert!(before_empty.group_id.is_none());
    let matches = bob.search_all("owned");
    assert!(matches
        .iter()
        .any(|(t, m)| *t == target && m.content == poll));
    assert!(matches
        .iter()
        .any(|(t, m)| *t == voice_text && m.content.contains("8ef391")));
    assert!(matches
        .iter()
        .any(|(t, m)| *t == thread_target && m.content.contains("75f6af")));
    let after_empty: CryptoContext = bob.worker.get(&context_url).unwrap();
    assert!(
        after_empty.group_id.is_none(),
        "reading empty search history must not create an MLS group"
    );
    let revoked = eve.handle(Command::CommunitySearch {
        origin: eve.origin(),
        query: "73ef21".into(),
        filters: Default::default(),
        remaining: Some(vec![(target.clone(), None)]),
        request_id: 99,
    });
    assert!(revoked.iter().any(|u| matches!(u,Update::CommunitySearch { messages,remaining,errors,.. } if messages.is_empty() && remaining.is_empty() && errors.is_empty())));
    // Crossing the initial 16-package stock must not strand verified peers.
    // The manager is explicitly brought online again to admit replenished peers.
    for index in 0..18 {
        let new: Channel = alice
            .worker
            .post(
                "/api/channels",
                &json!({"name":format!("owned-stock-{index}"),"kind":"text"}),
            )
            .unwrap();
        let next = Target::Channel(new.id);
        alice.worker.ensure_crypto_conversation(&next).unwrap();
        let first = bob.worker.ensure_crypto_conversation(&next);
        assert!(first.is_ok() || first.unwrap_err() == "Waiting for this conversation's key manager to verify and approve your device.");
        alice.worker.ensure_crypto_conversation(&next).unwrap();
        bob.worker.ensure_crypto_conversation(&next).unwrap();
        alice.send(
            &next,
            "owned replenished stock secret 5421f3",
            vec![],
            &format!("stock-{index}"),
            None,
        );
        assert_eq!(
            bob.history(&next).last().unwrap().content,
            "owned replenished stock secret 5421f3"
        );
    }
    assert_eq!(
        bob.search_all("5421f3").len(),
        18,
        "search must resume across more than ten target pages"
    );
    let _: serde_json::Value = bob
        .worker
        .post(&format!("/api/blocks/{a_id}"), &json!({}))
        .unwrap();
    assert!(
        bob.search_all("5421f3").is_empty(),
        "blocked authors must stay hidden after local decryption"
    );
    fs::write(server.root.join("discord-conversations-evidence.json"), serde_json::to_vec_pretty(&json!({"group_nonmember_owner_denied":true,"group_roster_revocation":true,"future_epoch_rejects_removed_member":true,"voice_text_without_call":true,"separate_thread_voice_group_scopes":true,"encrypted_poll_and_votes":true,"client_decrypted_mentions_inbox":true,"client_decrypted_search":true,"search_pagination_and_revocation":true,"empty_search_does_not_create_crypto_group":true,"search_hides_blocked_authors":true,"client_and_server_restart":true,"physical_devices_opened":false})).unwrap()).unwrap();
}

fn protected_device_path(root: &Path) -> PathBuf {
    fs::read_dir(root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "device")
        })
        .unwrap()
}

fn own_device_directory(worker: &CryptoWorker) -> Vec<CryptoDevice> {
    worker.worker.get("/api/e2ee/device").unwrap()
}

#[test]
#[ignore = "requires RELAY_NETWORK_TEST_SERVER; owned device reset/revocation only"]
fn actual_server_mls_worker_recovery_guards_and_reset_revocation_outbox_restart() {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    let server = CryptoServer::start();
    let (mut alice, mut bob, _eve, target) = workers(&server);
    let original = alice.own();
    let bob_public = bob.own();
    alice.pin(&bob_public);
    bob.pin(&original);
    alice.send(
        &target,
        "owned reset historical secret 902957a3",
        Vec::new(),
        "reset-before",
        None,
    );
    assert_eq!(
        bob.history(&target).last().unwrap().content,
        "owned reset historical secret 902957a3"
    );
    let historical_wire = alice.raw_messages(&target).last().unwrap().content.clone();
    let old_path = protected_device_path(&alice.keys);
    let old_blob = fs::read(&old_path).unwrap();
    let user_id = original.identity.user_id.clone();
    let device_id = original.identity.device_id.clone();
    for (url, user, expected, confirmation) in [
        (
            "http://127.0.0.1:1".to_string(),
            user_id.clone(),
            device_id.clone(),
            "RESET DEVICE",
        ),
        (
            server.url.clone(),
            bob_public.identity.user_id.clone(),
            device_id.clone(),
            "RESET DEVICE",
        ),
        (
            server.url.clone(),
            user_id.clone(),
            bob_public.identity.device_id.clone(),
            "RESET DEVICE",
        ),
        (
            server.url.clone(),
            user_id.clone(),
            device_id.clone(),
            "RESET",
        ),
    ] {
        assert!(alice
            .worker
            .handle(Command::CryptoResetDevice {
                confirmation: confirmation.into(),
                url,
                user_id: user,
                expected_device_id: expected,
            })
            .is_err());
        assert_eq!(alice.own().identity, original.identity);
        assert_eq!(
            fs::read(&old_path).unwrap(),
            old_blob,
            "misbound reset may not rotate or save keys"
        );
        assert!(!alice
            .drain()
            .iter()
            .any(|update| matches!(update, Update::VoiceLeft)));
    }
    for (url, user, revoked) in [
        (
            "http://127.0.0.1:1".to_string(),
            user_id.clone(),
            bob_public.identity.device_id.clone(),
        ),
        (
            server.url.clone(),
            bob_public.identity.user_id.clone(),
            bob_public.identity.device_id.clone(),
        ),
        (server.url.clone(), user_id.clone(), device_id.clone()),
        (
            server.url.clone(),
            user_id.clone(),
            bob_public.identity.device_id.clone(),
        ),
    ] {
        assert!(alice
            .worker
            .handle(Command::CryptoRevokeOwnDevice {
                device_id: revoked,
                url,
                user_id: user
            })
            .is_err());
        assert_eq!(fs::read(&old_path).unwrap(), old_blob);
        alice.drain();
        assert!(own_device_directory(&alice)
            .iter()
            .any(|d| d.device_id == device_id));
        assert!(own_device_directory(&bob)
            .iter()
            .any(|d| d.device_id == bob_public.identity.device_id));
    }
    assert!(!fs::read_dir(&alice.keys).unwrap().any(|entry| entry
        .unwrap()
        .path()
        .extension()
        .is_some_and(|extension| extension == "retired")));
    // Matching the current UI confirmation is insufficient when a retained
    // actor belongs to a different authenticated bootstrap identity.
    let server_id = original.identity.server_id.clone();
    for wrong_account in [true, false] {
        let c = alice.worker.connection.as_mut().unwrap();
        if wrong_account {
            c.session.user.id = bob_public.identity.user_id.clone();
        } else {
            c.server_id = uuid::Uuid::new_v4().to_string();
        }
        let selected_user = c.session.user.id.clone();
        assert!(alice
            .worker
            .handle(Command::CryptoResetDevice {
                confirmation: "RESET DEVICE".into(),
                url: server.url.clone(),
                user_id: selected_user,
                expected_device_id: device_id.clone()
            })
            .is_err());
        assert_eq!(alice.own().identity, original.identity);
        assert_eq!(fs::read(&old_path).unwrap(), old_blob);
        assert!(!alice
            .drain()
            .iter()
            .any(|update| matches!(update, Update::VoiceLeft)));
        let c = alice.worker.connection.as_mut().unwrap();
        c.session.user.id = user_id.clone();
        c.server_id = server_id.clone();
    }

    // A second synthetic signing device has only public registration on this
    // server; no second local store competes for this fixture's OS marker.
    let mut spare = relay_crypto::Device::new(relay_crypto::Identity {
        server_id: original.identity.server_id.clone(),
        user_id: user_id.clone(),
        device_id: uuid::Uuid::new_v4().to_string(),
    })
    .unwrap();
    let public = spare.key_package().unwrap();
    let _: CryptoDevice = parse(
        alice
            .worker
            .request(reqwest::Method::PUT, "/api/e2ee/device")
            .unwrap()
            .json(&RegisterCryptoDevice {
                device_id: public.identity.device_id.clone(),
                identity: public.fingerprint.clone(),
                key_package: STANDARD.encode(public.key_package),
            })
            .send()
            .unwrap(),
    )
    .unwrap();
    alice.handle(Command::CryptoRefresh);
    assert!(alice
        .worker
        .connection
        .as_ref()
        .unwrap()
        .crypto
        .as_ref()
        .unwrap()
        .status
        .own_devices
        .iter()
        .any(|d| d.device_id == public.identity.device_id));
    for wrong_account in [true, false] {
        let c = alice.worker.connection.as_mut().unwrap();
        if wrong_account {
            c.session.user.id = bob_public.identity.user_id.clone();
        } else {
            c.server_id = uuid::Uuid::new_v4().to_string();
        }
        let selected_user = c.session.user.id.clone();
        assert!(alice
            .worker
            .handle(Command::CryptoRevokeOwnDevice {
                device_id: public.identity.device_id.clone(),
                url: server.url.clone(),
                user_id: selected_user
            })
            .is_err());
        alice.drain();
        let c = alice.worker.connection.as_mut().unwrap();
        c.session.user.id = user_id.clone();
        c.server_id = server_id.clone();
        assert!(
            own_device_directory(&alice)
                .iter()
                .any(|d| d.device_id == public.identity.device_id),
            "a retained actor mismatch must prevent remote revocation too"
        );
    }
    alice.handle(Command::CryptoRevokeOwnDevice {
        device_id: public.identity.device_id.clone(),
        url: server.url.clone(),
        user_id: user_id.clone(),
    });
    assert!(!own_device_directory(&alice)
        .iter()
        .any(|d| d.device_id == public.identity.device_id));
    assert!(own_device_directory(&alice)
        .iter()
        .any(|d| d.device_id == device_id));
    let old_blob = fs::read(&old_path).unwrap();

    // Revoke the HTTP session externally, as can happen while its UI still has
    // the correctly bound account selected. The reset's first DELETE must fail
    // with actual authentication rejection, not silently publish the new device.
    let token = alice
        .worker
        .connection
        .as_ref()
        .unwrap()
        .session
        .token
        .clone();
    let response = Client::new()
        .post(format!("{}/api/logout", server.url))
        .bearer_auth(&token)
        .json(&json!({}))
        .send()
        .unwrap();
    assert!(response.status().is_success());
    assert!(alice
        .worker
        .handle(Command::CryptoResetDevice {
            confirmation: "RESET DEVICE".into(),
            url: server.url.clone(),
            user_id: user_id.clone(),
            expected_device_id: device_id.clone()
        })
        .is_err());
    alice.drain();
    let replacement = alice.own();
    assert_ne!(replacement.identity.device_id, device_id);
    assert_ne!(replacement.fingerprint, original.fingerprint);
    let actor = alice
        .worker
        .connection
        .as_ref()
        .unwrap()
        .crypto
        .as_ref()
        .unwrap();
    assert_eq!(actor.storage_root().unwrap(), alice.keys);
    assert_eq!(actor.pending_revocations(), vec![device_id.clone()]);
    assert!(!alice.worker.connection.as_ref().unwrap().crypto_registered);
    let retired: Vec<_> = fs::read_dir(&alice.keys)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "retired")
        })
        .collect();
    assert_eq!(retired.len(), 1);
    assert_eq!(fs::read(&retired[0]).unwrap(), old_blob);
    let directory: Vec<CryptoDevice> = bob.worker.get("/api/e2ee/directory").unwrap();
    assert!(directory.iter().any(|d| d.device_id == device_id));
    assert!(!directory
        .iter()
        .any(|d| d.device_id == replacement.identity.device_id));
    let pending_blob = fs::read(&old_path).unwrap();
    assert!(
        alice
            .worker
            .handle(Command::CryptoResetDevice {
                confirmation: "RESET DEVICE".into(),
                url: server.url.clone(),
                user_id: user_id.clone(),
                expected_device_id: replacement.identity.device_id.clone()
            })
            .is_err(),
        "a second reset must not discard the previous device's pending revocation"
    );
    assert_eq!(alice.own().identity, replacement.identity);
    assert_eq!(fs::read(&old_path).unwrap(), pending_blob);
    assert_eq!(
        alice
            .worker
            .connection
            .as_ref()
            .unwrap()
            .crypto
            .as_ref()
            .unwrap()
            .pending_revocations(),
        vec![device_id.clone()]
    );
    assert!(!alice
        .drain()
        .iter()
        .any(|update| matches!(update, Update::VoiceLeft)));
    assert_eq!(
        fs::read_dir(&alice.keys)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path
                .extension()
                .is_some_and(|extension| extension == "retired"))
            .count(),
        1
    );

    // Crash/reopen the actor before credentials are restored. The revocation
    // intent must still precede registration when production ensure_crypto runs.
    let c = alice.worker.connection.as_mut().unwrap();
    drop(c.crypto.take());
    let reopened =
        CryptoSession::open_for_test(alice.keys.clone(), &c.server_id, &c.session.user.id).unwrap();
    assert_eq!(reopened.pending_revocations(), vec![device_id.clone()]);
    assert_eq!(reopened.advertised().identity, replacement.identity);
    c.crypto = Some(reopened);
    c.session = parse(
        Client::new()
            .post(format!("{}/api/login", server.url))
            .json(&AuthRequest {
                second_factor: None,
                initial_protection: None,
                username: "crypto_alice".into(),
                password: "Owned_Crypto_Test_1234!".into(),
                invite: String::new(),
            })
            .send()
            .unwrap(),
    )
    .unwrap();
    alice.handle(Command::CryptoRefresh);
    assert!(alice.worker.connection.as_ref().unwrap().crypto_registered);
    assert!(alice
        .worker
        .connection
        .as_ref()
        .unwrap()
        .crypto
        .as_ref()
        .unwrap()
        .pending_revocations()
        .is_empty());
    let directory = own_device_directory(&alice);
    assert!(!directory.iter().any(|d| d.device_id == device_id));
    assert!(directory
        .iter()
        .any(|d| d.device_id == replacement.identity.device_id));
    let scope = alice.worker.crypto_scope(&target).unwrap();
    assert!(
        alice
            .worker
            .connection
            .as_mut()
            .unwrap()
            .crypto
            .as_mut()
            .unwrap()
            .decrypt_content(&scope, &historical_wire)
            .is_err(),
        "explicit reset does not manufacture access to the retired key's history"
    );
    bob.handle(Command::CryptoRefresh);
    let peer = bob
        .worker
        .connection
        .as_ref()
        .unwrap()
        .crypto
        .as_ref()
        .unwrap()
        .status
        .peers
        .iter()
        .find(|d| d.device_id == replacement.identity.device_id)
        .unwrap();
    assert!(
        !peer.verified,
        "new fingerprints require an explicit peer verification"
    );
    alice.reopen_keys();
    assert_eq!(alice.own().identity, replacement.identity);
    assert!(alice
        .worker
        .connection
        .as_ref()
        .unwrap()
        .crypto
        .as_ref()
        .unwrap()
        .pending_revocations()
        .is_empty());
    fs::write(server.root.join("mls-device-reset-evidence.json"),serde_json::to_vec_pretty(&json!({
        "misbound_reset_preserves_keys":true,"misbound_revocation_and_foreign_account_rejected":true,
        "current_device_direct_revocation_blocked":true,"own_spare_revocation_succeeds":true,
        "retired_blob_preserved":true,"new_identity_and_fingerprint":true,
        "http_auth_failure_prevents_new_registration":true,"pending_revocation_survives_actor_restart":true,
        "second_reset_cannot_discard_pending_revocation":true,
        "reauthentication_revokes_old_before_registering_new":true,"new_peer_verification_required":true,
        "reset_does_not_decrypt_retired_history":true,"all_actor_roots_owned_work":true,"physical_devices_opened":false
    })).unwrap()).unwrap();
    println!("MLS explicit recovery evidence: {}", server.root.display());
}

fn assert_protected_storage(root: &Path, private: &[&[u8]]) {
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            assert_protected_storage(&path, private);
        } else if path.extension().is_none_or(|extension| extension != "lock") {
            let bytes = fs::read(&path).unwrap();
            for secret in private {
                assert_absent(&bytes, secret);
            }
        }
    }
}

#[test]
#[ignore = "requires RELAY_NETWORK_TEST_SERVER; localhost synthetic MLS/files only"]
fn actual_server_mls_worker_chat_files_and_restart() {
    let mut server = CryptoServer::start();
    let (mut alice, mut bob, mut eve, target) = workers(&server);
    let alice_public = alice.own();
    let bob_public = bob.own();
    // Trust on first use: a direct message to someone nobody verified is sent
    // encrypted right away (see the first-contact test). Verification below
    // only adds the Verified mark.
    let dm_a = Target::Direct(bob_public.identity.user_id.clone());
    assert!(alice.raw_messages(&dm_a).is_empty());
    alice.pin(&bob_public);
    bob.pin(&alice_public);
    let secret = b"synthetic private attachment 7ab33ce5";
    let filename = "untrusted-host-must-not-see-this-name.txt";
    let source = server.root.join(filename);
    fs::write(&source, secret).unwrap();
    let attachment = alice.upload(&target, &source);
    let first_text = "protected channel text 5e7db9bd";
    alice.select(&target);
    let mention_send = alice.handle(Command::SendRich {
        binding: None,
        origin: CommandOrigin::from(alice.worker.connection.as_ref().unwrap()),
        target: target.clone(),
        content: first_text.into(),
        attachment_ids: vec![attachment.id.clone()],
        client_id: "protected-first".into(),
        reply_to_id: None,
        mentions: vec![bob_public.identity.user_id.clone()],
    });
    assert!(mention_send
        .iter()
        .any(|update| matches!(update, Update::SentRich { .. })));
    let raw = alice.raw_messages(&target);
    assert_eq!(raw.len(), 1);
    assert!(raw[0].content.starts_with(MESSAGE_PREFIX));
    assert_absent(raw[0].content.as_bytes(), first_text.as_bytes());
    assert!(
        raw[0].mentions.is_empty(),
        "mention recipients must stay inside the MLS envelope"
    );
    assert_ne!(raw[0].attachments[0].name, filename);
    assert_eq!(raw[0].attachments[0].mime, "application/octet-stream");
    alice.reopen_keys();
    let retried_send = alice.handle(Command::SendRich {
        binding: None,
        origin: CommandOrigin::from(alice.worker.connection.as_ref().unwrap()),
        target: target.clone(),
        content: first_text.into(),
        attachment_ids: vec![attachment.id.clone()],
        client_id: "protected-first".into(),
        reply_to_id: None,
        mentions: vec![bob_public.identity.user_id.clone()],
    });
    assert!(retried_send
        .iter()
        .any(|update| matches!(update, Update::SentRich { .. })));
    let retried = alice.raw_messages(&target);
    assert_eq!(
        retried.len(),
        1,
        "a completed protected send retry must not append another message"
    );
    assert_eq!(
        retried[0].content, raw[0].content,
        "retry after client reopen must reuse the exact MLS ciphertext"
    );
    let file_wire = alice
        .worker
        .request(reqwest::Method::GET, &attachment.url)
        .unwrap()
        .send()
        .unwrap()
        .bytes()
        .unwrap();
    assert_absent(&file_wire, secret);
    let messages = bob.history(&target);
    assert_eq!(messages[0].content, first_text);
    assert_eq!(
        messages[0].mentions,
        vec![bob_public.identity.user_id.clone()]
    );
    assert_eq!(messages[0].attachments[0].name, filename);
    assert!(bob
        .search_all(filename)
        .iter()
        .any(|(t, m)| *t == target && m.id == messages[0].id && m.attachments[0].name == filename));
    let saved = server.root.join("explicitly-downloaded.txt");
    bob.download(&messages[0].attachments[0], &saved);
    assert_eq!(fs::read(&saved).unwrap(), secret);
    // Eve is a member with access to this channel. Nobody verified her, but
    // trust on first use gave her device the keys like any new member's.
    let member = eve.history(&target);
    assert_eq!(member[0].content, first_text);
    // A verified receiver rejects host edits to ciphertext, outer author and scope.
    for attack in 0..3 {
        let mut altered = raw.clone();
        match attack {
            0 => altered[0].content.push('!'),
            1 => altered[0].author = eve.worker.connection.as_ref().unwrap().session.user.clone(),
            _ => altered[0].client_id = Some("substituted-retry-id".into()),
        }
        bob.worker.decrypt_messages(&target, &mut altered);
        assert_eq!(altered[0].content, "Encrypted message unavailable");
        assert!(altered[0].attachments.is_empty());
        assert!(bob
            .drain()
            .iter()
            .any(|u| matches!(u,Update::MessageProtections {protections,..}
            if protections.iter().any(|(_,p)| matches!(p,MessageProtection::Unavailable(_))))));
    }
    let mut wrong_scope = raw.clone();
    bob.worker.decrypt_messages(
        &Target::Direct(alice_public.identity.user_id.clone()),
        &mut wrong_scope,
    );
    assert_eq!(
        wrong_scope[0].content, "Encrypted message unavailable",
        "channel ciphertext must not decrypt in a DM"
    );
    bob.drain();
    let reply_text = "protected reply 01e276ff";
    bob.send(
        &target,
        reply_text,
        Vec::new(),
        "protected-reply",
        Some(raw[0].id.clone()),
    );
    let reply = alice.history(&target);
    assert_eq!(reply.last().unwrap().content, reply_text);
    assert_eq!(
        reply.last().unwrap().reply_to_id.as_deref(),
        Some(raw[0].id.as_str())
    );
    let dm_b = Target::Direct(alice_public.identity.user_id.clone());
    alice.send(
        &dm_a,
        "protected direct text 1ad0144a",
        Vec::new(),
        "protected-direct",
        None,
    );
    assert_eq!(
        bob.history(&dm_b)[0].content,
        "protected direct text 1ad0144a"
    );
    bob.select(&target);
    assert!(bob
        .worker
        .handle(Command::EditMessage {
            origin: CommandOrigin::from(bob.worker.connection.as_ref().unwrap()),
            message_id: raw[0].id.clone(),
            content: "recipient cannot rewrite encrypted author".into()
        })
        .is_err());
    assert_eq!(alice.raw_messages(&target)[0].content, raw[0].content);
    alice.select(&target);
    let edited_text = "protected edited attachment caption 61cda2aa";
    let edits = alice.handle(Command::EditMessage {
        origin: CommandOrigin::from(alice.worker.connection.as_ref().unwrap()),
        message_id: raw[0].id.clone(),
        content: edited_text.into(),
    });
    assert!(edits
        .iter()
        .any(|update| matches!(update,Update::Edited(id) if id==&raw[0].id)));
    let edited_raw = alice.raw_messages(&target);
    let edited_raw = edited_raw
        .iter()
        .find(|message| message.id == raw[0].id)
        .unwrap();
    assert!(edited_raw.content.starts_with(MESSAGE_PREFIX));
    assert_ne!(edited_raw.content, raw[0].content);
    assert_eq!(edited_raw.sequence, raw[0].sequence);
    assert_absent(edited_raw.content.as_bytes(), edited_text.as_bytes());
    assert_eq!(edited_raw.attachments[0].id, attachment.id);
    let edited = bob.history(&target);
    let edited = edited
        .iter()
        .find(|message| message.id == raw[0].id)
        .unwrap();
    assert_eq!(edited.content, edited_text);
    assert_eq!(edited.mentions, vec![bob_public.identity.user_id.clone()]);
    assert_eq!(edited.attachments[0].name, filename);
    let reply_id = reply.last().unwrap().id.clone();
    bob.select(&target);
    let edited_reply = "protected edited reply 3694f47a";
    bob.handle(Command::EditMessage {
        origin: CommandOrigin::from(bob.worker.connection.as_ref().unwrap()),
        message_id: reply_id.clone(),
        content: edited_reply.into(),
    });
    let replies = alice.history(&target);
    let reply = replies
        .iter()
        .find(|message| message.id == reply_id)
        .unwrap();
    assert_eq!(reply.content, edited_reply);
    assert_eq!(reply.reply_to_id.as_deref(), Some(raw[0].id.as_str()));
    bob.handle(Command::React {
        origin: CommandOrigin::from(bob.worker.connection.as_ref().unwrap()),
        target: target.clone(),
        message_id: raw[0].id.clone(),
        target_client_id: Some("protected-first".into()),
        emoji: "🧪".into(),
        active: true,
    });
    let reacted = alice.history(&target);
    let reacted = reacted
        .iter()
        .find(|message| message.id == raw[0].id)
        .unwrap();
    assert_eq!(reacted.reactions.len(), 1);
    assert_eq!(reacted.reactions[0].emoji, "🧪");
    assert_eq!(
        reacted.reactions[0].user_ids,
        vec![bob_public.identity.user_id.clone()]
    );
    assert_eq!(
        alice.history(&target).len(),
        2,
        "encrypted reaction actions must not become visible chat rows"
    );
    let bob_bootstrap: Bootstrap = bob.worker.get("/api/bootstrap").unwrap();
    let mut bob_read = bob_bootstrap.read_states.clone();
    encrypted::reconcile_read_states(bob.worker.connection.as_ref().unwrap(), &mut bob_read);
    let channel = match &target {
        Target::Channel(id) => id,
        _ => unreachable!(),
    };
    let bob_badge = bob_read
        .iter()
        .find(|state| state.channel_id.as_ref() == Some(channel))
        .unwrap();
    assert_eq!(bob_badge.mention_count,1,"authenticated mention must notify despite empty host mention metadata, including edited duplicates");
    let alice_bootstrap: Bootstrap = alice.worker.get("/api/bootstrap").unwrap();
    let mut alice_read = alice_bootstrap.read_states.clone();
    let raw_count = alice_read
        .iter()
        .find(|state| state.channel_id.as_ref() == Some(channel))
        .unwrap()
        .unread_count;
    encrypted::reconcile_read_states(alice.worker.connection.as_ref().unwrap(), &mut alice_read);
    let alice_badge = alice_read
        .iter()
        .find(|state| state.channel_id.as_ref() == Some(channel))
        .unwrap();
    assert_eq!(
        alice_badge.unread_count + 1,
        raw_count,
        "authenticated reaction action must not create a message notification"
    );
    let mut read_mention = vec![ReadState {
        channel_id: Some(channel.clone()),
        last_read_sequence: raw[0].sequence,
        ..Default::default()
    }];
    encrypted::reconcile_read_states(bob.worker.connection.as_ref().unwrap(), &mut read_mention);
    assert_eq!(
        read_mention[0].mention_count, 0,
        "read encrypted mentions must not reappear"
    );
    // A connection identity change may not expose the previous actor's cached
    // bodies, reaction intentions or badge reconciliation to another account.
    let old_user = alice
        .worker
        .connection
        .as_ref()
        .unwrap()
        .session
        .user
        .id
        .clone();
    let old_server = alice.worker.connection.as_ref().unwrap().server_id.clone();
    let other_user = eve.own().identity.user_id;
    let actor_path = protected_device_path(&alice.keys);
    let saved_actor = fs::read(&actor_path).unwrap();
    let raw_for_guard = alice.raw_messages(&target);
    alice.drain();
    for wrong_account in [true, false] {
        let c = alice.worker.connection.as_mut().unwrap();
        if wrong_account {
            c.session.user.id = other_user.clone();
        } else {
            c.server_id = uuid::Uuid::new_v4().to_string();
        }
        let mut states = vec![ReadState {
            channel_id: Some(channel.clone()),
            unread_count: 17,
            mention_count: 19,
            ..Default::default()
        }];
        encrypted::reconcile_read_states(alice.worker.connection.as_ref().unwrap(), &mut states);
        assert_eq!(
            (states[0].unread_count, states[0].mention_count),
            (17, 19),
            "another identity cannot project cached read counts"
        );
        let mut messages = raw_for_guard.clone();
        alice.worker.decrypt_messages(&target, &mut messages);
        assert!(messages
            .iter()
            .all(|message| message.content == "Encrypted message unavailable"));
        assert!(
            messages.iter().all(|message| message.reactions.is_empty()
                && message.mentions.is_empty()
                && message.attachments.is_empty()),
            "cached reactions and attachment metadata must be unavailable with mismatched keys"
        );
        assert!(
            !alice
                .drain()
                .iter()
                .any(|update| matches!(update, Update::ReactionSnapshot { .. })),
            "cached reaction events may not cross a connection identity change"
        );
        assert_eq!(
            fs::read(&actor_path).unwrap(),
            saved_actor,
            "mismatched cached reads must not ratchet or rewrite keys"
        );
        let c = alice.worker.connection.as_mut().unwrap();
        c.session.user.id = old_user.clone();
        c.server_id = old_server.clone();
    }
    let cursor = bob
        .worker
        .connection
        .as_ref()
        .unwrap()
        .crypto
        .as_ref()
        .unwrap()
        .cursor(&bob.worker.crypto_scope(&target).unwrap());
    assert!(cursor > 0);
    // Client restart preserves replay/cache state and server restart preserves ciphertext.
    bob.reopen_keys();
    alice.reopen_keys();
    server.restart();
    assert_eq!(bob.history(&target)[0].content, edited_text);
    assert_eq!(
        alice.history(&dm_a)[0].content,
        "protected direct text 1ad0144a"
    );
    let new_cursor = bob
        .worker
        .connection
        .as_ref()
        .unwrap()
        .crypto
        .as_ref()
        .unwrap()
        .cursor(&bob.worker.crypto_scope(&target).unwrap());
    assert_eq!(
        new_cursor, cursor,
        "replayed delivery must not advance the cursor"
    );
    server.stop();
    assert_protected_storage(
        &server.root.join("data"),
        &[
            first_text.as_bytes(),
            reply_text.as_bytes(),
            edited_text.as_bytes(),
            edited_reply.as_bytes(),
            "🧪".as_bytes(),
            b"protected direct text 1ad0144a",
            filename.as_bytes(),
            secret,
        ],
    );
    assert_protected_storage(
        &bob.keys,
        &[
            first_text.as_bytes(),
            reply_text.as_bytes(),
            edited_text.as_bytes(),
            edited_reply.as_bytes(),
            secret,
        ],
    );
    fs::write(server.root.join("mls-worker-evidence.json"),serde_json::to_vec_pretty(&json!({
        "chat_ciphertext_at_rest":true,"file_ciphertext_and_opaque_name_at_rest":true,
        "mutual_fingerprint_pins":true,"reply_decrypted":true,"dm_scope":true,
        "unverified_plaintext_fallback_blocked":true,"host_cipher_author_id_tamper_rejected":true,
        "client_and_server_restart":true,"cursor_replay_idempotent":true,"physical_devices_opened":false
        ,"protected_completed_send_retry":true,"edited_attachment_caption_and_reply_preserved":true,
        "recipient_rewrite_denied":true,"authenticated_encrypted_reaction_projected":true,
        "encrypted_mentions_notify_without_outer_metadata":true,"reaction_actions_excluded_from_notification_count":true
    })).unwrap()).unwrap();
    println!("MLS worker evidence: {}", server.root.display());
}

#[test]
#[ignore = "requires RELAY_NETWORK_TEST_SERVER; synthetic MLS membership/recovery only"]
fn actual_server_mls_worker_device_revocation_rejoin_and_commit_ack_recovery() {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    let server = CryptoServer::start();
    let (mut alice, mut bob, eve, target) = workers(&server);
    let alice_public = alice.own();
    let old_bob = bob.own();
    alice.pin(&old_bob);
    bob.pin(&alice_public);
    alice.send(
        &target,
        "before device revocation 459fae32",
        Vec::new(),
        "before-revoke",
        None,
    );
    assert_eq!(
        bob.history(&target)[0].content,
        "before device revocation 459fae32"
    );
    let scope = alice.worker.crypto_scope(&target).unwrap();
    let before = alice
        .worker
        .connection
        .as_ref()
        .unwrap()
        .crypto
        .as_ref()
        .unwrap()
        .device
        .group_state(&scope)
        .unwrap()
        .epoch;
    let response = bob
        .worker
        .request(
            reqwest::Method::DELETE,
            &format!("/api/e2ee/device/{}", old_bob.identity.device_id),
        )
        .unwrap()
        .send()
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::NO_CONTENT);
    alice.send(
        &target,
        "after device revocation 664c4870",
        Vec::new(),
        "after-revoke",
        None,
    );
    let rotated = alice
        .worker
        .connection
        .as_ref()
        .unwrap()
        .crypto
        .as_ref()
        .unwrap()
        .device
        .group_state(&scope)
        .unwrap()
        .epoch;
    assert!(
        rotated > before,
        "controller must rotate when a registered device is revoked"
    );
    let future = alice.raw_messages(&target).last().unwrap().content.clone();
    let bob_scope = bob.worker.crypto_scope(&target).unwrap();
    assert!(
        bob.worker
            .connection
            .as_mut()
            .unwrap()
            .crypto
            .as_mut()
            .unwrap()
            .decrypt_content(&bob_scope, &future)
            .is_err(),
        "removed device must not decrypt the future epoch even if the host leaks its ciphertext"
    );
    assert!(
        bob.worker.ensure_crypto_conversation(&target).is_err(),
        "revoked directory identity must stay denied"
    );

    // Exercise the explicit production recovery action in this owned actor root.
    bob.handle(Command::CryptoResetDevice {
        confirmation: "RESET DEVICE".into(),
        url: server.url.clone(),
        user_id: old_bob.identity.user_id.clone(),
        expected_device_id: old_bob.identity.device_id.clone(),
    });
    let new_bob = bob.own();
    assert_ne!(new_bob.identity.device_id, old_bob.identity.device_id);
    bob.pin(&alice_public);
    alice.pin(&new_bob);
    alice.send(
        &target,
        "new verified device rejoins 2bdb8849",
        Vec::new(),
        "rejoined-device",
        None,
    );
    assert_eq!(
        bob.history(&target).last().unwrap().content,
        "new verified device rejoins 2bdb8849"
    );

    // Persist an Add+Welcome outbox, lose the server acknowledgement, then reopen
    // the client and retry the exact operation through the production worker.
    // Trust on first use added the original Eve with the first message, so a
    // member who joins only now takes her place here.
    let invite: serde_json::Value = alice.worker.post("/api/invites", &json!({})).unwrap();
    let _first_eve = eve;
    let mut eve = CryptoWorker::new(
        &server,
        register(&server, "crypto_late_eve", invite["code"].as_str().unwrap()),
    );
    let eve_public = eve.own();
    alice.pin(&eve_public);
    eve.pin(&alice_public);
    eve.pin(&new_bob);
    let own = alice.own();
    let channel = match &target {
        Target::Channel(id) => id,
        _ => unreachable!(),
    };
    let context: CryptoContext = parse(
        alice
            .worker
            .request(reqwest::Method::GET, "/api/e2ee/context")
            .unwrap()
            .query(&[
                ("channel_id", channel.as_str()),
                ("device_id", own.identity.device_id.as_str()),
                ("after", "0"),
            ])
            .send()
            .unwrap(),
    )
    .unwrap();
    let reserved = context
        .devices
        .iter()
        .find(|device| device.device_id == eve_public.identity.device_id)
        .unwrap();
    let add = relay_crypto::PublicDevice {
        identity: eve_public.identity.clone(),
        fingerprint: eve_public.fingerprint.clone(),
        key_package: STANDARD.decode(&reserved.key_package).unwrap(),
    };
    let batch = alice
        .worker
        .connection
        .as_mut()
        .unwrap()
        .crypto
        .as_mut()
        .unwrap()
        .stage_membership(
            &scope,
            CryptoTarget {
                channel_id: Some(channel.clone()),
                peer_id: None,
            },
            context.group_id.clone().unwrap(),
            context.sequence,
            &[add],
            &[],
            false,
        )
        .unwrap();
    let committed: CryptoContext = alice.worker.post("/api/e2ee/events", &batch).unwrap();
    assert!(committed
        .events
        .iter()
        .any(|event| event.operation_id == batch.operation_id));
    alice.reopen_keys();
    assert!(
        alice
            .worker
            .connection
            .as_ref()
            .unwrap()
            .crypto
            .as_ref()
            .unwrap()
            .pending_control(&scope)
            .is_some(),
        "unacknowledged control outbox must survive client restart"
    );
    alice.worker.ensure_crypto_conversation(&target).unwrap();
    assert!(alice
        .worker
        .connection
        .as_ref()
        .unwrap()
        .crypto
        .as_ref()
        .unwrap()
        .pending_control(&scope)
        .is_none());
    let after: CryptoContext = parse(
        alice
            .worker
            .request(reqwest::Method::GET, "/api/e2ee/context")
            .unwrap()
            .query(&[
                ("channel_id", channel.as_str()),
                ("device_id", own.identity.device_id.as_str()),
                ("after", "0"),
            ])
            .send()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        after
            .events
            .iter()
            .filter(|event| event.operation_id == batch.operation_id)
            .count(),
        batch
            .events
            .iter()
            .filter(|event| event.kind != "welcome")
            .count(),
        "retrying must not duplicate the controller-visible membership commit"
    );
    assert_eq!(
        after.sequence, committed.sequence,
        "retry must not allocate any new group sequence"
    );
    let eve_after: CryptoContext = parse(
        eve.worker
            .request(reqwest::Method::GET, "/api/e2ee/context")
            .unwrap()
            .query(&[
                ("channel_id", channel.as_str()),
                ("device_id", eve_public.identity.device_id.as_str()),
                ("after", "0"),
            ])
            .send()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        eve_after
            .events
            .iter()
            .filter(|event| event.operation_id == batch.operation_id)
            .count(),
        batch.events.len(),
        "the intended recipient must receive exactly one commit and its one Welcome"
    );
    alice.send(
        &target,
        "welcome recovered across lost acknowledgement",
        Vec::new(),
        "after-ack-recovery",
        None,
    );
    assert_eq!(
        eve.history(&target).last().unwrap().content,
        "welcome recovered across lost acknowledgement"
    );
    fs::write(
        server.root.join("mls-membership-evidence.json"),
        serde_json::to_vec_pretty(&json!({
            "revoked_device_future_epoch_denied":true,"explicit_new_verified_device_rejoined":true,
            "pending_add_welcome_survives_restart":true,"server_ack_lost_idempotent_recovery":true,
            "epoch_before":before,"epoch_after_revocation":rotated,"physical_devices_opened":false
        }))
        .unwrap(),
    )
    .unwrap();
    println!("MLS membership evidence: {}", server.root.display());
}

#[test]
#[ignore = "requires RELAY_NETWORK_TEST_SERVER; offline synthetic MLS epoch catch-up"]
fn actual_server_mls_worker_offline_catch_up_orders_chat_before_multiple_membership_epochs() {
    let server = CryptoServer::start();
    let (mut alice, mut bob, _first_eve, target) = workers(&server);
    let alice_public = alice.own();
    let bob_public = bob.own();
    alice.pin(&bob_public);
    bob.pin(&alice_public);
    alice.send(
        &target,
        "offline initial synchronized message",
        Vec::new(),
        "offline-initial",
        None,
    );
    assert_eq!(
        bob.history(&target)[0].content,
        "offline initial synchronized message"
    );
    let mut expected = vec!["offline initial synchronized message".to_string()];
    // Bob's worker does not refresh or receive anything until all three epochs exist.
    for index in 0..6 {
        let text = format!("offline original epoch message {index}");
        alice.send(
            &target,
            &text,
            Vec::new(),
            &format!("offline-before-{index}"),
            None,
        );
        expected.push(text);
    }
    // A member who joins while Bob is offline, and whose device is revoked
    // before Bob returns: Bob never sees it listed, yet follows both changes
    // because nobody verified this person (trust on first use).
    let invite: serde_json::Value = alice.worker.post("/api/invites", &json!({})).unwrap();
    let mut eve = CryptoWorker::new(
        &server,
        register(&server, "crypto_offline_eve", invite["code"].as_str().unwrap()),
    );
    let eve_public = eve.own();
    alice.pin(&eve_public);
    eve.pin(&alice_public);
    eve.pin(&bob_public);
    alice.worker.ensure_crypto_conversation(&target).unwrap();
    for index in 0..3 {
        let text = format!("offline added member epoch message {index}");
        alice.send(
            &target,
            &text,
            Vec::new(),
            &format!("offline-added-{index}"),
            None,
        );
        expected.push(text);
    }
    let response = eve
        .worker
        .request(
            reqwest::Method::DELETE,
            &format!("/api/e2ee/device/{}", eve_public.identity.device_id),
        )
        .unwrap()
        .send()
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::NO_CONTENT);
    alice.worker.ensure_crypto_conversation(&target).unwrap();
    for index in 0..3 {
        let text = format!("offline revoked member epoch message {index}");
        alice.send(
            &target,
            &text,
            Vec::new(),
            &format!("offline-removed-{index}"),
            None,
        );
        expected.push(text);
    }
    bob.reopen_keys();
    let messages = bob.history(&target);
    fs::write(server.root.join("mls-offline-diagnostic.json"),serde_json::to_vec_pretty(&json!({
        "expected":expected,"decrypted":messages.iter().map(|message|(&message.id,&message.content)).collect::<Vec<_>>()
    })).unwrap()).unwrap();
    assert_eq!(messages.len(), expected.len());
    for text in &expected {
        assert!(
            messages.iter().any(|message| &message.content == text),
            "offline catch-up lost authenticated text: {text}"
        );
    }
    assert!(!messages
        .iter()
        .any(|message| message.content == "Encrypted message unavailable"));
    let scope = bob.worker.crypto_scope(&target).unwrap();
    let epoch = bob
        .worker
        .connection
        .as_ref()
        .unwrap()
        .crypto
        .as_ref()
        .unwrap()
        .device
        .group_state(&scope)
        .unwrap()
        .epoch;
    assert!(epoch >= 3, "receiver must process both membership changes");
    assert_eq!(
        bob.history(&target)
            .iter()
            .map(|message| message.content.clone())
            .collect::<Vec<_>>(),
        messages
            .iter()
            .map(|message| message.content.clone())
            .collect::<Vec<_>>(),
        "reloading the mailbox must use authenticated cache/replay state"
    );
    fs::write(server.root.join("mls-offline-evidence.json"),serde_json::to_vec_pretty(&json!({
        "messages_recovered":messages.len(),"membership_epoch":epoch,"membership_changes_while_offline":2,
        "client_reopened_before_catch_up":true,"repeat_mailbox_idempotent":true,"physical_devices_opened":false
    })).unwrap()).unwrap();
    println!("MLS offline evidence: {}", server.root.display());
}

#[test]
#[ignore = "requires RELAY_NETWORK_TEST_SERVER; protected logical voice joins, no devices"]
fn actual_server_mls_worker_voice_pending_membership_leave_and_rejoin() {
    let server = CryptoServer::start();
    let (mut alice, mut bob, _eve, _text) = workers(&server);
    let alice_public = alice.own();
    let bob_public = bob.own();
    let bootstrap: Bootstrap = alice.worker.get("/api/bootstrap").unwrap();
    let channel = bootstrap
        .channels
        .iter()
        .find(|channel| channel.kind == "voice")
        .unwrap()
        .id
        .clone();
    let initial = alice.handle(Command::JoinVoiceRequest {
        channel_id: channel.clone(),
        request_id: 901,
    });
    let initial_keys = initial
        .into_iter()
        .find_map(|update| match update {
            Update::VoiceJoined {
                keys,
                request_id: 901,
                ..
            } => Some(keys),
            _ => None,
        })
        .expect("one verified local device can establish its protected call");
    let pending = bob.handle(Command::JoinVoiceRequest {
        channel_id: channel.clone(),
        request_id: 902,
    });
    assert!(pending
        .iter()
        .any(|update| matches!(update, Update::VoiceEncryptionPaused { .. })));
    assert!(
        !pending
            .iter()
            .any(|update| matches!(update, Update::VoiceJoined { .. })),
        "unverified logical join must not expose keys/open audio"
    );
    let bootstrap: Bootstrap = alice.worker.get("/api/bootstrap").unwrap();
    for selected in [&alice_public, &bob_public] {
        let member = bootstrap
            .voice_members
            .iter()
            .find(|member| member.user.id == selected.identity.user_id)
            .unwrap();
        assert_eq!(
            member.device_id.as_deref(),
            Some(selected.identity.device_id.as_str())
        );
    }
    alice.worker.refresh_encryption();
    assert!(
        alice
            .drain()
            .iter()
            .any(|update| matches!(update, Update::VoiceEncryptionPaused { .. })),
        "existing media must pause while a new call device awaits verification"
    );
    alice.pin(&bob_public);
    bob.pin(&alice_public);
    let (changed, joined_bob_updates) = settle_voice_keys(&mut alice, &mut bob);
    let joined_alice = changed
        .into_iter()
        .rev()
        .find_map(|update| match update {
            Update::VoiceEncryptionChanged { keys, .. } => Some(keys),
            _ => None,
        })
        .expect("verified membership must reopen existing participant with fresh epoch");
    let joined_bob = joined_bob_updates
        .into_iter()
        .find_map(|update| match update {
            Update::VoiceJoined {
                keys,
                request_id: 902,
                ..
            } => Some(keys),
            _ => None,
        })
        .expect("pending original join request must resume after verified Welcome");
    assert!(joined_alice.epoch > initial_keys.epoch);
    assert_eq!(joined_alice.epoch, joined_bob.epoch);
    assert_eq!(joined_alice.group_id, joined_bob.group_id);
    for user in [&alice_public.identity.user_id, &bob_public.identity.user_id] {
        assert_eq!(
            &*joined_alice.sender_keys[user].opus,
            &*joined_bob.sender_keys[user].opus
        );
        assert_eq!(
            &*joined_alice.sender_keys[user].rtc,
            &*joined_bob.sender_keys[user].rtc
        );
    }
    let reopened = bob
        .handle(Command::JoinVoiceRequest {
            channel_id: channel.clone(),
            request_id: 905,
        })
        .into_iter()
        .find_map(|update| match update {
            Update::VoiceJoined {
                keys,
                request_id: 905,
                ..
            } => Some(keys),
            _ => None,
        })
        .expect("same selected device can replace its transport without new membership");
    assert!(
        Arc::ptr_eq(&joined_bob, &reopened),
        "a real same-epoch rejoin must retain the receiver replay window"
    );
    bob.handle(Command::LeaveVoice);
    alice.worker.refresh_encryption();
    let after_leave = alice
        .drain()
        .into_iter()
        .find_map(|update| match update {
            Update::VoiceEncryptionChanged { keys, .. } => Some(keys),
            _ => None,
        })
        .expect("leave must rotate the remaining call");
    assert!(after_leave.epoch > joined_alice.epoch);
    assert_eq!(after_leave.sender_keys.len(), 1);
    assert_ne!(
        &*after_leave.sender_keys[&alice_public.identity.user_id].opus,
        &*joined_alice.sender_keys[&alice_public.identity.user_id].opus
    );
    let rejoin = bob.handle(Command::JoinVoiceRequest {
        channel_id: channel.clone(),
        request_id: 903,
    });
    assert!(
        !rejoin
            .iter()
            .any(|update| matches!(update, Update::VoiceJoined { .. })),
        "removed membership cannot reuse an old call epoch on rejoin"
    );
    let (new_alice_updates, new_bob_updates) = settle_voice_keys(&mut alice, &mut bob);
    let new_alice = new_alice_updates
        .into_iter()
        .rev()
        .find_map(|update| match update {
            Update::VoiceEncryptionChanged { keys, .. } => Some(keys),
            _ => None,
        })
        .expect("rejoining selected device needs a new membership epoch");
    let new_bob = new_bob_updates
        .into_iter()
        .find_map(|update| match update {
            Update::VoiceJoined {
                keys,
                request_id: 903,
                ..
            } => Some(keys),
            _ => None,
        })
        .expect("removed device must consume a new single-use Welcome to rejoin");
    assert!(new_alice.epoch > after_leave.epoch);
    assert_eq!(new_alice.epoch, new_bob.epoch);
    assert_eq!(
        &*new_alice.sender_keys[&bob_public.identity.user_id].opus,
        &*new_bob.sender_keys[&bob_public.identity.user_id].opus
    );
    alice.handle(Command::LeaveVoice);
    bob.worker.refresh_encryption();
    let handover = bob
        .drain()
        .into_iter()
        .find_map(|update| match update {
            Update::VoiceEncryptionChanged { keys, .. } => Some(keys),
            _ => None,
        })
        .expect("remaining verified member must continue when the original controller leaves");
    assert!(handover.epoch > new_bob.epoch);
    assert_eq!(handover.sender_keys.len(), 1);
    assert!(handover
        .sender_keys
        .contains_key(&bob_public.identity.user_id));
    let scope = bob
        .worker
        .crypto_scope(&Target::Channel(channel.clone()))
        .unwrap();
    assert_eq!(
        bob.worker
            .connection
            .as_ref()
            .unwrap()
            .crypto
            .as_ref()
            .unwrap()
            .device
            .group_state(&scope)
            .unwrap()
            .controller
            .user_id,
        bob_public.identity.user_id,
        "controller handover must select the remaining verified MLS leaf"
    );
    let returning = alice.handle(Command::JoinVoiceRequest {
        channel_id: channel.clone(),
        request_id: 904,
    });
    assert!(
        !returning
            .iter()
            .any(|update| matches!(update, Update::VoiceJoined { .. })),
        "former controller must not reopen its removed epoch when returning"
    );
    let (return_alice_updates, return_bob_updates) = settle_voice_keys(&mut alice, &mut bob);
    let return_bob = return_bob_updates
        .into_iter()
        .rev()
        .find_map(|update| match update {
            Update::VoiceEncryptionChanged { keys, .. } => Some(keys),
            _ => None,
        })
        .expect("new controller must admit the returning verified starter with a new Welcome");
    eprintln!(
        "returning starter encryption status: {}",
        alice
            .worker
            .connection
            .as_ref()
            .unwrap()
            .crypto
            .as_ref()
            .unwrap()
            .status
            .status
    );
    let return_alice = return_alice_updates
        .into_iter()
        .find_map(|update| match update {
            Update::VoiceJoined {
                keys,
                request_id: 904,
                ..
            } => Some(keys),
            _ => None,
        })
        .expect("returning starter must process historical signed handover then fresh Welcome");
    assert!(return_alice.epoch > handover.epoch);
    assert_eq!(return_alice.epoch, return_bob.epoch);
    assert_eq!(return_alice.group_id, initial_keys.group_id);
    assert_eq!(
        &*return_alice.sender_keys[&alice_public.identity.user_id].rtc,
        &*return_bob.sender_keys[&alice_public.identity.user_id].rtc
    );
    assert_eq!(
        alice
            .worker
            .connection
            .as_ref()
            .unwrap()
            .crypto
            .as_ref()
            .unwrap()
            .device
            .group_state(&scope)
            .unwrap()
            .controller
            .user_id,
        bob_public.identity.user_id,
        "returning starter does not silently reclaim the current controller"
    );
    alice.handle(Command::LeaveVoice);
    bob.handle(Command::LeaveVoice);
    fs::write(
        server.root.join("mls-voice-membership-evidence.json"),
        serde_json::to_vec_pretty(&json!({
            "unverified_join_no_media_keys":true,"existing_member_pauses_before_commit":true,
            "selected_device_metadata":true,"pending_request_id_preserved":true,
            "participant_keys_agree":true,"leave_rotates_and_excludes_member":true,
            "same_device_transport_rejoin_retains_call_context":true,
            "removed_selected_device_rejoins_with_new_welcome":true,"original_controller_leaves_remaining_call_continues":true,
            "former_controller_rejoins_using_fresh_welcome":true,"final_epoch":return_alice.epoch,
            "physical_devices_opened":false
        }))
        .unwrap(),
    )
    .unwrap();
    println!("MLS voice membership evidence: {}", server.root.display());
}

#[test]
#[ignore = "requires RELAY_NETWORK_TEST_SERVER; owned controller actor/Opus replay only"]
fn actual_server_call_refresh_reopen_finishes_old_commit_and_generates_new_entropy() {
    let server = CryptoServer::start();
    let (mut alice, mut bob, _eve, _) = workers(&server);
    let a = alice.own();
    let b = bob.own();
    alice.pin(&b);
    bob.pin(&a);
    let bootstrap: Bootstrap = alice.worker.get("/api/bootstrap").unwrap();
    let channel = bootstrap
        .channels
        .iter()
        .find(|channel| channel.kind == "voice")
        .unwrap()
        .id
        .clone();
    alice.handle(Command::JoinVoiceRequest {
        channel_id: channel.clone(),
        request_id: 9701,
    });
    bob.handle(Command::JoinVoiceRequest {
        channel_id: channel.clone(),
        request_id: 9702,
    });
    let _ = settle_voice_keys(&mut alice, &mut bob);
    let old_alice = alice.worker.protected_call_keys(&channel).unwrap();
    let old_bob = bob.worker.protected_call_keys(&channel).unwrap();
    assert_eq!(old_alice.epoch, old_bob.epoch);
    let target = Target::Channel(channel.clone());
    let scope = alice.worker.crypto_scope(&target).unwrap();
    let actor = alice
        .worker
        .connection
        .as_mut()
        .unwrap()
        .crypto
        .as_mut()
        .unwrap();
    let cursor = actor.cursor(&scope);
    let pending = actor
        .stage_call_refresh(
            &scope,
            CryptoTarget {
                channel_id: Some(channel.clone()),
                peer_id: None,
            },
            cursor,
        )
        .unwrap();
    assert!(actor
        .call_keys(
            &scope,
            &[a.identity.user_id.clone(), b.identity.user_id.clone()]
        )
        .is_err());
    // The server durably accepts the exact update, but the old actor never sees
    // its acknowledgement. Reopening must retry this operation and then rotate again.
    let ack: CryptoContext = alice.worker.post("/api/e2ee/events", &pending).unwrap();
    assert!(ack
        .events
        .iter()
        .any(|event| event.body == pending.events[0].body));
    let path = protected_device_path(&alice.keys);
    let protected_pending = fs::read(&path).unwrap();
    alice.reopen_keys();
    assert_eq!(fs::read(&path).unwrap(), protected_pending);
    let fresh_alice = alice.worker.protected_call_keys(&channel).unwrap();
    let fresh_bob = bob.worker.protected_call_keys(&channel).unwrap();
    assert!(fresh_alice.epoch > pending.epoch);
    assert_eq!(fresh_alice.epoch, fresh_bob.epoch);
    assert_eq!(fresh_alice.local_device_id, a.identity.device_id);
    assert_eq!(fresh_bob.local_device_id, b.identity.device_id);
    assert_eq!(fresh_alice.group_id, old_alice.group_id);
    assert!(!Arc::ptr_eq(&old_alice, &fresh_alice));
    for user in [&a.identity.user_id, &b.identity.user_id] {
        assert_ne!(
            &*old_alice.sender_keys[user].opus,
            &*fresh_alice.sender_keys[user].opus
        );
        assert_eq!(
            &*fresh_alice.sender_keys[user].opus,
            &*fresh_bob.sender_keys[user].opus
        );
    }
    crate::media::actor_reopen_voice_probe(old_alice, fresh_alice.clone(), fresh_bob).unwrap();
    let unchanged = alice.worker.protected_call_keys(&channel).unwrap();
    assert!(
        Arc::ptr_eq(&fresh_alice, &unchanged),
        "a live actor does not rotate on every poll"
    );
    let actor = alice
        .worker
        .connection
        .as_ref()
        .unwrap()
        .crypto
        .as_ref()
        .unwrap();
    assert!(!actor.needs_controller_call_refresh(&scope));
    assert!(actor.pending_control(&scope).is_none());
    assert_eq!(
        actor.device.group_state(&scope).unwrap().controller,
        a.identity
    );
    assert_protected_storage(
        &alice.keys,
        &[&*fresh_alice.sender_keys[&a.identity.user_id].opus],
    );
    fs::write(server.root.join("controller-call-refresh-evidence.json"), serde_json::to_vec_pretty(&json!({
        "old_epoch":old_bob.epoch,"saved_pending_epoch":pending.epoch,"fresh_epoch":fresh_alice.epoch,
        "exact_pending_retry_then_new_rotation":true,"same_verified_members_and_controller":true,
        "old_and_forged_epoch_opus_rejected":true,"fresh_10ms_opus_decoded":true,
        "same_actor_keys_reused_without_rotation_loop":true,"physical_devices_opened":false,
        "non_controller_actor_reopen_not_accepted":true
    })).unwrap()).unwrap();
    println!(
        "Controller call refresh evidence: {}",
        server.root.display()
    );
}

fn settle_voice_keys(
    alice: &mut CryptoWorker,
    bob: &mut CryptoWorker,
) -> (Vec<Update>, Vec<Update>) {
    let mut a = Vec::new();
    let mut b = Vec::new();
    // Exercise actual pending joins, encrypted requests, authenticated replies
    // and exact ACKs. No fixture grants or direct freshness markers.
    for _ in 0..4 {
        alice.worker.refresh_encryption();
        a.extend(alice.drain());
        bob.worker.refresh_encryption();
        b.extend(bob.drain());
    }
    (a, b)
}

fn latest_voice_transport(
    updates: Vec<Update>,
) -> Option<(VoiceTicket, Arc<crate::e2ee::CallKeys>)> {
    updates.into_iter().rev().find_map(|update| match update {
        Update::VoiceJoined { ticket, keys, .. }
        | Update::VoiceEncryptionChanged { ticket, keys, .. } => Some((ticket, keys)),
        _ => None,
    })
}

#[test]
#[ignore = "requires owned RELAY_NETWORK_TEST_SERVER; actual protected UDP workers, synthetic audio, no physical devices"]
fn actual_server_living_voice_restart_fresh_keys_old_recordings_cancel_and_revoke() {
    let mut server = CryptoServer::start_with_udp();
    let (mut alice, mut bob, _eve, _text) = workers(&server);
    let a = alice.own();
    let b = bob.own();
    alice.pin(&b);
    bob.pin(&a);
    let boot: Bootstrap = alice.worker.get("/api/bootstrap").unwrap();
    let channel = boot
        .channels
        .iter()
        .find(|c| c.kind == "voice")
        .unwrap()
        .id
        .clone();
    let mut au = alice.handle(Command::JoinVoiceRequest {
        channel_id: channel.clone(),
        request_id: 9801,
    });
    let mut bu = bob.handle(Command::JoinVoiceRequest {
        channel_id: channel.clone(),
        request_id: 9802,
    });
    let (ax, bx) = settle_voice_keys(&mut alice, &mut bob);
    au.extend(ax);
    bu.extend(bx);
    let (old_at, old_ak) = latest_voice_transport(au).unwrap();
    let (old_bt, old_bk) = latest_voice_transport(bu).unwrap();
    assert_eq!(old_ak.epoch, old_bk.epoch);
    let mut old_audio_a = crate::media::LivingVoiceProbe::start(&old_at, old_ak.clone());
    let mut old_audio_b = crate::media::LivingVoiceProbe::start(&old_bt, old_bk);
    let mut recording = old_audio_a.recording();
    let until = Instant::now() + Duration::from_secs(4);
    while !old_audio_a.played() || !old_audio_b.played() {
        old_audio_a.feed();
        old_audio_b.feed();
        assert!(
            Instant::now() < until,
            "both original generated UDP calls must play"
        );
        thread::sleep(Duration::from_millis(10));
    }
    old_audio_a.assert_healthy();
    old_audio_b.assert_healthy();
    server.stop();
    alice.worker.refresh();
    bob.worker.refresh();
    old_audio_a.assert_healthy();
    old_audio_b.assert_healthy();
    for worker in [&alice, &bob] {
        let c = worker.worker.connection.as_ref().unwrap();
        assert!(c.voice_recovery.is_some());
        assert!(c.crypto_call_epoch.is_none());
        assert!(!c.sharing && !c.camera);
    }
    old_audio_a.finish();
    old_audio_b.finish();
    let started = Instant::now();
    server.spawn();
    let mut au = Vec::new();
    let mut bu = Vec::new();
    for _ in 0..8 {
        alice.worker.refresh();
        au.extend(alice.drain());
        bob.worker.refresh();
        bu.extend(bob.drain());
    }
    let (at, ak) =
        latest_voice_transport(au).expect("controller must resume without a manual join");
    let (bt, bk) =
        latest_voice_transport(bu).expect("participant must resume without a manual join");
    assert_eq!(ak.epoch, bk.epoch);
    assert!(ak.epoch > old_ak.epoch);
    assert_ne!(at.key, old_at.key);
    assert_ne!(bt.key, old_bt.key);
    for worker in [&alice, &bob] {
        assert!(worker
            .worker
            .connection
            .as_ref()
            .unwrap()
            .voice_recovery
            .is_none());
    }
    let mut audio_a = crate::media::LivingVoiceProbe::start(&at, ak.clone());
    let mut audio_b = crate::media::LivingVoiceProbe::start(&bt, bk.clone());
    let until = Instant::now() + Duration::from_secs(4);
    while !audio_a.played() || !audio_b.played() {
        audio_a.feed();
        audio_b.feed();
        assert!(
            Instant::now() < until,
            "both resumed generated UDP calls must play"
        );
        thread::sleep(Duration::from_millis(10));
    }
    let recovery_elapsed_ms = started.elapsed().as_millis();
    audio_a.assert_healthy();
    audio_b.assert_healthy();
    audio_a.finish();
    let before = audio_b.rejected();
    audio_a.inject(1 << 62, &recording);
    recording[26..34].copy_from_slice(&ak.epoch.to_le_bytes());
    audio_a.inject((1 << 62) + 1, &recording);
    let until = Instant::now() + Duration::from_secs(3);
    while audio_b.rejected() < before + 2 {
        assert!(
            Instant::now() < until,
            "old and forged-fresh recordings must be rejected by living receiver"
        );
        thread::sleep(Duration::from_millis(5));
    }
    audio_b.finish();
    crate::media::actor_reopen_voice_probe(old_ak, ak, bk).unwrap();
    server.stop();
    alice.worker.command(Command::LeaveVoice);
    alice.drain();
    bob.worker.refresh();
    bob.drain();
    assert!(alice
        .worker
        .connection
        .as_ref()
        .unwrap()
        .voice_channel
        .is_none());
    server.spawn();
    for _ in 0..4 {
        alice.worker.refresh();
        assert!(latest_voice_transport(alice.drain()).is_none());
        bob.worker.refresh();
        bob.drain();
    }
    let boot: Bootstrap = bob.worker.get("/api/bootstrap").unwrap();
    assert!(boot
        .voice_members
        .iter()
        .all(|v| v.user.id != a.identity.user_id));
    assert!(bob
        .worker
        .connection
        .as_ref()
        .unwrap()
        .voice_channel
        .is_some());
    server.stop();
    bob.worker.refresh();
    bob.drain();
    server.spawn();
    let response = bob
        .worker
        .request(
            reqwest::Method::DELETE,
            &format!("/api/e2ee/device/{}", b.identity.device_id),
        )
        .unwrap()
        .send()
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::NO_CONTENT);
    bob.worker.refresh();
    assert!(latest_voice_transport(bob.drain()).is_none());
    assert!(bob
        .worker
        .connection
        .as_ref()
        .unwrap()
        .voice_channel
        .is_none());
    fs::write(server.root.join("living-call-restart-evidence.json"),serde_json::to_vec_pretty(&json!({
        "both_actors_retained":true,"both_actual_udp_workers_play_before_and_after_restart":true,
        "generated_packet_ms":10,"manual_rejoin":false,"fresh_authenticated_epoch":true,
        "fresh_outer_keys":true,"old_and_forged_fresh_recordings_rejected":2,
        "cancel_during_outage_stays_cancelled":true,"offline_selected_device_revocation_stops_recovery":true,
            "recovery_elapsed_ms":recovery_elapsed_ms,"physical_devices_opened":false,
        "scope":"Owned actual server process restarts, two retained network actors and two actual generated-media UDP workers; no native UI/hardware/WAN or video-resume acceptance."
    })).unwrap()).unwrap();
    println!("Living call restart evidence: {}", server.root.display());
}

#[test]
#[ignore = "requires RELAY_NETWORK_TEST_SERVER; owned controller/participant/whisper actors and generated Opus only"]
fn actual_server_participant_call_freshness_reopen_request_retry_server_restart_and_whisper() {
    let mut server = CryptoServer::start();
    let (mut alice, mut bob, _eve, text) = workers(&server);
    let a = alice.own();
    let b = bob.own();
    alice.pin(&b);
    bob.pin(&a);
    let bootstrap: Bootstrap = alice.worker.get("/api/bootstrap").unwrap();
    assert!(alice
        .worker
        .connection
        .as_ref()
        .unwrap()
        .chat
        .supports("e2ee-call-freshness-v1"));
    let channel = bootstrap
        .channels
        .iter()
        .find(|c| c.kind == "voice")
        .unwrap()
        .id
        .clone();
    alice.handle(Command::JoinVoiceRequest {
        channel_id: channel.clone(),
        request_id: 9801,
    });
    bob.handle(Command::JoinVoiceRequest {
        channel_id: channel.clone(),
        request_id: 9802,
    });
    let _ = settle_voice_keys(&mut alice, &mut bob);
    let old_sender = alice.worker.protected_call_keys(&channel).unwrap();
    let old_receiver = bob.worker.protected_call_keys(&channel).unwrap();
    assert_eq!(old_sender.epoch, old_receiver.epoch);
    let target = Target::Channel(channel.clone());
    let scope = bob.worker.crypto_scope(&target).unwrap();
    let pending = bob
        .worker
        .connection
        .as_mut()
        .unwrap()
        .crypto
        .as_mut()
        .unwrap()
        .stage_call_challenge(
            &scope,
            CryptoTarget {
                channel_id: Some(channel.clone()),
                peer_id: None,
            },
        )
        .unwrap();
    // Persisted delivery with a lost response: do not grant/clear anything locally.
    let _: CallKeyDelivery = bob
        .worker
        .post("/api/e2ee/call-requests", &pending)
        .unwrap();
    let path = protected_device_path(&bob.keys);
    let saved = fs::read(&path).unwrap();
    bob.reopen_keys();
    assert_eq!(fs::read(&path).unwrap(), saved);
    assert_eq!(
        bob.worker
            .connection
            .as_ref()
            .unwrap()
            .crypto
            .as_ref()
            .unwrap()
            .pending_call_request(&scope),
        Some(pending.clone())
    );
    // Restart only this test's owned server; re-establish logical selected-device
    // presence before either actor processes the durable conversation roster.
    server.restart();
    for (worker, own) in [(&mut alice, &a), (&mut bob, &b)] {
        let _: VoiceTicket = worker
            .worker
            .post(
                "/api/voice/join",
                &VoiceJoin {
                    open_revision: None,
                    channel_id: channel.clone(),
                    device_id: Some(own.identity.device_id.clone()),
                },
            )
            .unwrap();
    }
    let current_bootstrap: Bootstrap = bob.worker.get("/api/bootstrap").unwrap();
    let mut legacy_capabilities = current_bootstrap.clone();
    legacy_capabilities
        .capabilities
        .retain(|c| c != "e2ee-call-freshness-v1");
    bob.worker
        .connection
        .as_mut()
        .unwrap()
        .chat
        .update_bootstrap(&legacy_capabilities);
    let old_server_error = bob.worker.protected_call_keys(&channel).unwrap_err();
    assert!(old_server_error.contains("Update the local server"));
    assert!(bob
        .worker
        .connection
        .as_ref()
        .unwrap()
        .crypto
        .as_ref()
        .unwrap()
        .needs_call_refresh(&scope));
    bob.worker
        .connection
        .as_mut()
        .unwrap()
        .chat
        .update_bootstrap(&current_bootstrap);
    assert!(
        bob.worker.protected_call_keys(&channel).is_err(),
        "an offline key manager cannot release restored participant keys"
    );
    assert!(bob
        .worker
        .connection
        .as_ref()
        .unwrap()
        .crypto
        .as_ref()
        .unwrap()
        .needs_call_refresh(&scope));
    let _ = settle_voice_keys(&mut alice, &mut bob);
    let fresh_sender = alice.worker.protected_call_keys(&channel).unwrap();
    let fresh_receiver = bob.worker.protected_call_keys(&channel).unwrap();
    assert!(fresh_receiver.epoch > old_receiver.epoch);
    assert_eq!(fresh_sender.epoch, fresh_receiver.epoch);
    assert_eq!(fresh_receiver.local_device_id, b.identity.device_id);
    crate::media::actor_reopen_voice_probe(
        old_sender,
        fresh_sender.clone(),
        fresh_receiver.clone(),
    )
    .unwrap();
    let current = bob.worker.protected_call_keys(&channel).unwrap();
    assert!(
        Arc::ptr_eq(&fresh_receiver, &current),
        "a live participant retains its replay window without an epoch loop"
    );
    // Independent process/actor lifetimes on both sides must converge again.
    alice.reopen_keys();
    bob.reopen_keys();
    assert!(bob.worker.protected_call_keys(&channel).is_err());
    let _ = settle_voice_keys(&mut alice, &mut bob);
    let both_fresh_sender = alice.worker.protected_call_keys(&channel).unwrap();
    let both_fresh_receiver = bob.worker.protected_call_keys(&channel).unwrap();
    assert!(both_fresh_sender.epoch > fresh_sender.epoch);
    assert_eq!(both_fresh_sender.epoch, both_fresh_receiver.epoch);
    crate::media::actor_reopen_voice_probe(fresh_sender, both_fresh_sender, both_fresh_receiver)
        .unwrap();
    // Private whisper uses the same challenge barrier in its separate MLS scope.
    for (worker, peer) in [
        (&mut alice, &b.identity.user_id),
        (&mut bob, &a.identity.user_id),
    ] {
        let _: serde_json::Value = worker
            .worker
            .post(&format!("/api/friends/{peer}"), &json!({}))
            .unwrap();
    }
    let private = Target::Direct(b.identity.user_id.clone());
    let private_initial = alice
        .worker
        .protected_whisper_keys(&b.identity.user_id)
        .unwrap();
    assert!(bob
        .worker
        .protected_whisper_keys(&a.identity.user_id)
        .is_err());
    let selected_before = alice.worker.connection.as_ref().unwrap().current.clone();
    alice.worker.refresh_encryption();
    assert_eq!(
        alice.worker.connection.as_ref().unwrap().current,
        selected_before,
        "private call inbox does not navigate the chat"
    );
    let private_receiver = bob
        .worker
        .protected_whisper_keys(&a.identity.user_id)
        .unwrap();
    let private_sender = alice
        .worker
        .protected_whisper_keys(&b.identity.user_id)
        .unwrap();
    assert!(private_sender.epoch > private_initial.epoch);
    assert_eq!(private_sender.epoch, private_receiver.epoch);
    bob.reopen_keys();
    assert!(bob
        .worker
        .protected_whisper_keys(&a.identity.user_id)
        .is_err());
    alice.worker.refresh_encryption();
    let private_fresh_receiver = bob
        .worker
        .protected_whisper_keys(&a.identity.user_id)
        .unwrap();
    let private_fresh_sender = alice
        .worker
        .protected_whisper_keys(&b.identity.user_id)
        .unwrap();
    crate::media::actor_reopen_voice_probe(
        private_sender,
        private_fresh_sender.clone(),
        private_fresh_receiver,
    )
    .unwrap();
    assert!(
        alice.raw_messages(&private).is_empty(),
        "call control must not become chat/history/unread messages"
    );
    assert_protected_storage(
        &bob.keys,
        &[&*fresh_receiver.sender_keys[&b.identity.user_id].opus],
    );
    assert_eq!(alice.own().fingerprint, a.fingerprint);
    assert_eq!(bob.own().fingerprint, b.fingerprint);
    // Normal encrypted text still works after both actor and server restarts.
    alice.send(
        &text,
        "owned post-call encrypted reply",
        vec![],
        "post-call-freshness",
        None,
    );
    assert_eq!(
        bob.history(&text).last().unwrap().content,
        "owned post-call encrypted reply"
    );
    fs::write(server.root.join("participant-call-freshness-evidence.json"),serde_json::to_vec_pretty(&json!({
        "old_epoch":old_receiver.epoch,"participant_fresh_epoch":fresh_receiver.epoch,
        "private_fresh_epoch":private_fresh_sender.epoch,"exact_old_request_retry_without_nonce_adoption":true,
        "owned_server_restart_and_reselected_logical_devices":true,"both_actors_reopened":true,
        "private_inbox_without_navigation":true,"call_control_absent_from_chat":true,
        "old_forged_and_repeated_opus_rejected":true,"fresh_10ms_opus_decoded":true,
        "same_verified_identity_and_fingerprints":true,"physical_devices_opened":false
    })).unwrap()).unwrap();
    println!(
        "Participant call freshness evidence: {}",
        server.root.display()
    );
}

#[test]
#[ignore = "requires RELAY_NETWORK_TEST_SERVER; actual delayed community reply and notification event uniqueness"]
fn actual_server_delayed_background_reply_after_community_switch_delivers_created_once() {
    let server_a = CryptoServer::start();
    let server_b = CryptoServer::start();
    let (mut alice_a, mut bob_a, _eve_a, target_a) = workers(&server_a);
    let (mut alice_b, mut bob_b, _eve_b, target_b) = workers(&server_b);
    alice_a.pin(&bob_a.own());
    bob_a.pin(&alice_a.own());
    alice_b.pin(&bob_b.own());
    bob_b.pin(&alice_b.own());
    alice_a.send(
        &target_a,
        "A before switch",
        Vec::new(),
        "notification-a-before",
        None,
    );
    assert_eq!(bob_a.history(&target_a)[0].content, "A before switch");
    alice_b.send(
        &target_b,
        "B stays separate",
        Vec::new(),
        "notification-b-before",
        None,
    );
    assert_eq!(bob_b.history(&target_b)[0].content, "B stays separate");
    let bootstrap: Bootstrap = bob_a.worker.get("/api/bootstrap").unwrap();
    let mut parked = bob_a.worker.connection.take().unwrap();
    parked.change_cursor = bootstrap.latest_change_sequence;
    let actor_a = parked.crypto.as_ref().unwrap().device.identity().clone();
    let actor_b = bob_b.own().identity;
    bob_b.worker.saved_connections.insert(
        server_a.url.clone(),
        SavedConnection {
            connection: parked,
            stop: Arc::new(AtomicBool::new(false)),
            dirty: Arc::new(AtomicBool::new(true)),
        },
    );
    bob_b.drain_checked_community_owners();
    alice_a.send(
        &target_a,
        "A arrives during pending switch",
        Vec::new(),
        "notification-a-arrival",
        None,
    );
    // This is the real background HTTP result. Delay its delivery, as the public
    // IO thread may finish before the command loop gets to drain its queue.
    bob_b.worker.background_at = Instant::now() - Duration::from_secs(1);
    bob_b.worker.refresh_background();
    let delayed = bob_b
        .worker
        .background_results
        .recv_timeout(Duration::from_secs(10))
        .unwrap();
    assert_eq!(delayed.url, server_a.url);
    assert!(delayed.bootstrap.is_ok());
    assert!(delayed
        .changes
        .as_ref()
        .unwrap()
        .as_ref()
        .unwrap()
        .changes
        .iter()
        .any(|c| c
            .message
            .as_ref()
            .is_some_and(|m| m.client_id.as_deref() == Some("notification-a-arrival")
                && m.content.starts_with(MESSAGE_PREFIX))));
    bob_b
        .worker
        .handle(Command::SwitchCommunity {
            url: server_a.url.clone(),
        })
        .unwrap();
    assert!(bob_b.worker.load_changes());
    let mut received: Vec<_> = bob_b
        .drain_checked_community_owners()
        .into_iter()
        .filter_map(|u| {
            if let Update::Changes { changes, .. } = u {
                Some(changes)
            } else {
                None
            }
        })
        .flatten()
        .collect();
    assert!(received.iter().any(|c| c
        .message
        .as_ref()
        .is_some_and(|m| m.content == "A arrives during pending switch")));
    bob_b
        .worker
        .handle(Command::SwitchCommunity {
            url: server_b.url.clone(),
        })
        .unwrap();
    bob_b.worker.background_completed.send(delayed).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        // Do not schedule another public poll while releasing this exact result.
        bob_b.worker.background_at = Instant::now();
        bob_b.worker.refresh_background();
        received.extend(
            bob_b
                .drain_checked_community_owners()
                .into_iter()
                .filter_map(|u| {
                    if let Update::Changes { changes, .. } = u {
                        Some(changes)
                    } else {
                        None
                    }
                })
                .flatten(),
        );
        if bob_b.worker.background_jobs.is_empty() && bob_b.worker.background_pending.is_empty() {
            break;
        }
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
    let copies = received
        .iter()
        .filter(|c| {
            c.kind == "created"
                && c.message
                    .as_ref()
                    .is_some_and(|m| m.client_id.as_deref() == Some("notification-a-arrival"))
        })
        .count();
    assert_eq!(bob_b.own().identity, actor_b);
    assert_eq!(
        bob_b.worker.saved_connections[&server_a.url]
            .connection
            .crypto
            .as_ref()
            .unwrap()
            .device
            .identity(),
        &actor_a
    );
    fs::write(server_a.root.join("notification-switch-diagnostic.json"),serde_json::to_vec_pretty(&json!({"created_deliveries_for_one_real_encrypted_message":copies,"real_delayed_background_HTTP_reply":true,"two_server_actor_identities_preserved":true,"physical_devices_opened":false})).unwrap()).unwrap();
    println!("Notification switch evidence: {}", server_a.root.display());
    assert_eq!(copies,1,"One actual encrypted arrival was delivered again by a delayed background reply after switching away and back");
}

#[test]
#[ignore = "requires RELAY_NETWORK_TEST_SERVER; actual delayed community reply and notification event uniqueness"]
fn actual_server_older_background_reply_after_active_progress_never_rewinds_or_replays() {
    let server_a = CryptoServer::start();
    let server_b = CryptoServer::start();
    let (mut alice_a, mut bob_a, _eve_a, target_a) = workers(&server_a);
    let (mut alice_b, mut bob_b, _eve_b, target_b) = workers(&server_b);
    alice_a.pin(&bob_a.own());
    bob_a.pin(&alice_a.own());
    alice_b.pin(&bob_b.own());
    bob_b.pin(&alice_b.own());
    alice_a.send(
        &target_a,
        "A before switch",
        Vec::new(),
        "notification-a-before",
        None,
    );
    assert_eq!(bob_a.history(&target_a)[0].content, "A before switch");
    alice_b.send(
        &target_b,
        "B stays separate",
        Vec::new(),
        "notification-b-before",
        None,
    );
    assert_eq!(bob_b.history(&target_b)[0].content, "B stays separate");
    let bootstrap: Bootstrap = bob_a.worker.get("/api/bootstrap").unwrap();
    let mut parked = bob_a.worker.connection.take().unwrap();
    parked.change_cursor = bootstrap.latest_change_sequence;
    let actor_a = parked.crypto.as_ref().unwrap().device.identity().clone();
    let actor_b = bob_b.own().identity;
    bob_b.worker.saved_connections.insert(
        server_a.url.clone(),
        SavedConnection {
            connection: parked,
            stop: Arc::new(AtomicBool::new(false)),
            dirty: Arc::new(AtomicBool::new(true)),
        },
    );
    bob_b.drain_checked_community_owners();
    alice_a.send(
        &target_a,
        "A arrives during pending switch",
        Vec::new(),
        "notification-a-arrival",
        None,
    );
    // This is the real background HTTP result. Delay its delivery, as the public
    // IO thread may finish before the command loop gets to drain its queue.
    bob_b.worker.background_at = Instant::now() - Duration::from_secs(1);
    bob_b.worker.refresh_background();
    let delayed = bob_b
        .worker
        .background_results
        .recv_timeout(Duration::from_secs(10))
        .unwrap();
    assert_eq!(delayed.url, server_a.url);
    assert!(delayed.bootstrap.is_ok());
    assert!(delayed
        .changes
        .as_ref()
        .unwrap()
        .as_ref()
        .unwrap()
        .changes
        .iter()
        .any(|c| c
            .message
            .as_ref()
            .is_some_and(|m| m.client_id.as_deref() == Some("notification-a-arrival")
                && m.content.starts_with(MESSAGE_PREFIX))));
    alice_a.send(
        &target_a,
        "A newer than queued bootstrap",
        Vec::new(),
        "notification-a-newer",
        None,
    );
    bob_b
        .worker
        .handle(Command::SwitchCommunity {
            url: server_a.url.clone(),
        })
        .unwrap();
    assert!(bob_b.worker.load_changes());
    let mut received: Vec<_> = bob_b
        .drain_checked_community_owners()
        .into_iter()
        .filter_map(|u| {
            if let Update::Changes { changes, .. } = u {
                Some(changes)
            } else {
                None
            }
        })
        .flatten()
        .collect();
    assert!(received.iter().any(|c| c
        .message
        .as_ref()
        .is_some_and(|m| m.content == "A arrives during pending switch")));
    let accepted_cursor = bob_b.worker.connection.as_ref().unwrap().change_cursor;
    bob_b
        .worker
        .handle(Command::SwitchCommunity {
            url: server_b.url.clone(),
        })
        .unwrap();
    bob_b.worker.background_completed.send(delayed).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        // Do not schedule another public poll while releasing this exact result.
        bob_b.worker.background_at = Instant::now();
        bob_b.worker.refresh_background();
        received.extend(
            bob_b
                .drain_checked_community_owners()
                .into_iter()
                .filter_map(|u| {
                    if let Update::Changes { changes, .. } = u {
                        Some(changes)
                    } else {
                        None
                    }
                })
                .flatten(),
        );
        if bob_b.worker.background_jobs.is_empty() && bob_b.worker.background_pending.is_empty() {
            break;
        }
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        bob_b.worker.saved_connections[&server_a.url]
            .connection
            .change_cursor,
        accepted_cursor,
        "Delayed metadata rewound an already accepted cursor"
    );
    bob_b
        .worker
        .handle(Command::SwitchCommunity {
            url: server_a.url.clone(),
        })
        .unwrap();
    assert!(bob_b.worker.load_changes());
    received.extend(
        bob_b
            .drain_checked_community_owners()
            .into_iter()
            .filter_map(|u| {
                if let Update::Changes { changes, .. } = u {
                    Some(changes)
                } else {
                    None
                }
            })
            .flatten(),
    );
    let newer_copies = received
        .iter()
        .filter(|c| {
            c.kind == "created"
                && c.message
                    .as_ref()
                    .is_some_and(|m| m.client_id.as_deref() == Some("notification-a-newer"))
        })
        .count();
    assert_eq!(
        newer_copies, 1,
        "A later fresh message was replayed after the old response"
    );
    bob_b
        .worker
        .handle(Command::SwitchCommunity {
            url: server_b.url.clone(),
        })
        .unwrap();
    let copies = received
        .iter()
        .filter(|c| {
            c.kind == "created"
                && c.message
                    .as_ref()
                    .is_some_and(|m| m.client_id.as_deref() == Some("notification-a-arrival"))
        })
        .count();
    assert_eq!(bob_b.own().identity, actor_b);
    assert_eq!(
        bob_b.worker.saved_connections[&server_a.url]
            .connection
            .crypto
            .as_ref()
            .unwrap()
            .device
            .identity(),
        &actor_a
    );
    fs::write(server_a.root.join("notification-switch-diagnostic.json"),serde_json::to_vec_pretty(&json!({"created_deliveries_for_one_real_encrypted_message":copies,"real_delayed_background_HTTP_reply":true,"two_server_actor_identities_preserved":true,"physical_devices_opened":false})).unwrap()).unwrap();
    println!("Notification switch evidence: {}", server_a.root.display());
    assert_eq!(copies,1,"One actual encrypted arrival was delivered again by a delayed background reply after switching away and back");
}

#[test]
#[ignore = "requires RELAY_NETWORK_TEST_SERVER; actual delayed community reply and notification event uniqueness"]
fn actual_server_delayed_reply_from_replaced_account_cannot_mutate_new_actor() {
    let server_a = CryptoServer::start();
    let server_b = CryptoServer::start();
    let (mut alice_a, mut bob_a, mut eve_a, target_a) = workers(&server_a);
    let (mut alice_b, mut bob_b, _eve_b, target_b) = workers(&server_b);
    alice_a.pin(&bob_a.own());
    bob_a.pin(&alice_a.own());
    alice_b.pin(&bob_b.own());
    bob_b.pin(&alice_b.own());
    alice_a.send(
        &target_a,
        "A before switch",
        Vec::new(),
        "notification-a-before",
        None,
    );
    assert_eq!(bob_a.history(&target_a)[0].content, "A before switch");
    alice_b.send(
        &target_b,
        "B stays separate",
        Vec::new(),
        "notification-b-before",
        None,
    );
    assert_eq!(bob_b.history(&target_b)[0].content, "B stays separate");
    let bootstrap: Bootstrap = bob_a.worker.get("/api/bootstrap").unwrap();
    let mut parked = bob_a.worker.connection.take().unwrap();
    parked.change_cursor = bootstrap.latest_change_sequence;
    let actor_a = parked.crypto.as_ref().unwrap().device.identity().clone();
    let actor_b = bob_b.own().identity;
    bob_b.worker.saved_connections.insert(
        server_a.url.clone(),
        SavedConnection {
            connection: parked,
            stop: Arc::new(AtomicBool::new(false)),
            dirty: Arc::new(AtomicBool::new(true)),
        },
    );
    bob_b.drain_checked_community_owners();
    alice_a.send(
        &target_a,
        "A arrives during pending switch",
        Vec::new(),
        "notification-a-arrival",
        None,
    );
    // This is the real background HTTP result. Delay its delivery, as the public
    // IO thread may finish before the command loop gets to drain its queue.
    bob_b.worker.background_at = Instant::now() - Duration::from_secs(1);
    bob_b.worker.refresh_background();
    let delayed = bob_b
        .worker
        .background_results
        .recv_timeout(Duration::from_secs(10))
        .unwrap();
    assert_eq!(delayed.url, server_a.url);
    assert!(delayed.bootstrap.is_ok());
    assert!(delayed
        .changes
        .as_ref()
        .unwrap()
        .as_ref()
        .unwrap()
        .changes
        .iter()
        .any(|c| c
            .message
            .as_ref()
            .is_some_and(|m| m.client_id.as_deref() == Some("notification-a-arrival")
                && m.content.starts_with(MESSAGE_PREFIX))));
    // A different real authenticated account replaces the saved actor on the
    // same server URL before its older IO result is consumed. No fake identity.
    let old = bob_b
        .worker
        .saved_connections
        .remove(&server_a.url)
        .unwrap();
    drop(old);
    let replacement = eve_a.worker.connection.take().unwrap();
    let replacement_identity = replacement
        .crypto
        .as_ref()
        .unwrap()
        .device
        .identity()
        .clone();
    let replacement_revision = replacement
        .crypto
        .as_ref()
        .unwrap()
        .checkpoint_revision_for_test();
    let replacement_cursor = replacement.change_cursor;
    bob_b.worker.saved_connections.insert(
        server_a.url.clone(),
        SavedConnection {
            connection: replacement,
            stop: Arc::new(AtomicBool::new(false)),
            dirty: Arc::new(AtomicBool::new(false)),
        },
    );
    let mut received = Vec::<MessageChange>::new();
    bob_b.worker.background_completed.send(delayed).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        // Do not schedule another public poll while releasing this exact result.
        bob_b.worker.background_at = Instant::now();
        bob_b.worker.refresh_background();
        received.extend(
            bob_b
                .drain_checked_community_owners()
                .into_iter()
                .filter_map(|u| {
                    if let Update::Changes { changes, .. } = u {
                        Some(changes)
                    } else {
                        None
                    }
                })
                .flatten(),
        );
        if bob_b.worker.background_jobs.is_empty() && bob_b.worker.background_pending.is_empty() {
            break;
        }
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
    let copies = received
        .iter()
        .filter(|c| {
            c.kind == "created"
                && c.message
                    .as_ref()
                    .is_some_and(|m| m.client_id.as_deref() == Some("notification-a-arrival"))
        })
        .count();
    assert_eq!(bob_b.own().identity, actor_b);
    let replacement = &bob_b.worker.saved_connections[&server_a.url].connection;
    assert_eq!(
        replacement.crypto.as_ref().unwrap().device.identity(),
        &replacement_identity
    );
    assert_ne!(replacement_identity.user_id, actor_a.user_id);
    assert_eq!(
        replacement
            .crypto
            .as_ref()
            .unwrap()
            .checkpoint_revision_for_test(),
        replacement_revision
    );
    assert_eq!(replacement.change_cursor, replacement_cursor);
    fs::write(server_a.root.join("notification-switch-diagnostic.json"),serde_json::to_vec_pretty(&json!({"created_deliveries_for_one_real_encrypted_message":copies,"real_delayed_background_HTTP_reply":true,"two_server_actor_identities_preserved":true,"physical_devices_opened":false})).unwrap()).unwrap();
    println!("Notification switch evidence: {}", server_a.root.display());
    assert_eq!(
        copies, 0,
        "An old account reply reached the replacement account"
    );
}

#[test]
#[ignore = "requires owned server; actual revoked session and independent encrypted active chat"]
fn actual_server_background_revocation_preserves_active_actor_and_removes_login() {
    let server_a = CryptoServer::start();
    let server_b = CryptoServer::start();
    let (mut alice_a, mut bob_a, _eve_a, target_a) = workers(&server_a);
    let (mut alice_b, mut bob_b, _eve_b, target_b) = workers(&server_b);
    alice_a.pin(&bob_a.own());
    bob_a.pin(&alice_a.own());
    alice_b.pin(&bob_b.own());
    bob_b.pin(&alice_b.own());
    alice_a.send(
        &target_a,
        "Private A before revoked login",
        Vec::new(),
        "revoked-a-before",
        None,
    );
    assert_eq!(
        bob_a.history(&target_a)[0].content,
        "Private A before revoked login"
    );
    alice_b.send(
        &target_b,
        "Private B stays active",
        Vec::new(),
        "revoked-b-before",
        None,
    );
    assert_eq!(
        bob_b.history(&target_b)[0].content,
        "Private B stays active"
    );
    let actor_a = bob_a.own().identity;
    let actor_b = bob_b.own().identity;
    let active_origin = bob_b.origin();
    let active_epoch = bob_b.worker.live.epoch.load(Ordering::SeqCst);
    let active_media_epoch = bob_b.worker.chat_media_epoch.load(Ordering::SeqCst);
    let active_stop = bob_b.worker.socket_stop.clone();
    let token_a = bob_a
        .worker
        .connection
        .as_ref()
        .unwrap()
        .session
        .token
        .clone();
    let stop = Arc::new(AtomicBool::new(false));
    let dirty = Arc::new(AtomicBool::new(true));
    bob_b.worker.saved_connections.insert(
        server_a.url.clone(),
        SavedConnection {
            connection: bob_a.worker.connection.take().unwrap(),
            stop: stop.clone(),
            dirty: dirty.clone(),
        },
    );
    bob_b.drain();
    let response = bob_b
        .worker
        .http
        .post(format!("{}/api/logout", server_a.url))
        .bearer_auth(&token_a)
        .json(&json!({}))
        .send()
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::NO_CONTENT);
    bob_b.worker.background_at = Instant::now() - Duration::from_secs(1);
    bob_b.worker.refresh_background();
    let result = bob_b
        .worker
        .background_results
        .recv_timeout(Duration::from_secs(10))
        .unwrap();
    assert_eq!(result.bootstrap.as_ref().unwrap_err(), "Sign in again");
    bob_b.worker.background_completed.send(result).unwrap();
    bob_b.worker.background_at = Instant::now();
    bob_b.worker.refresh_background();
    let updates = bob_b.drain();
    assert!(updates.iter().any(|u|matches!(u,Update::ConnectionState {url,status:ConnectionStatus::SignInRequired} if url==&server_a.url)));
    assert!(
        !bob_b.worker.saved_connections.contains_key(&server_a.url),
        "Revoked background account must no longer be advertised as signed in"
    );
    assert!(updates.iter().any(|u|matches!(u,Update::Communities(rows) if rows.len()==1 && rows[0].url==server_b.url && rows[0].current && rows[0].user.id==actor_b.user_id)));
    assert!(stop.load(Ordering::SeqCst));
    assert!(!dirty.load(Ordering::SeqCst));
    assert!(!bob_b.worker.background_jobs.contains_key(&server_a.url));
    assert!(!bob_b.worker.background_retry.contains_key(&server_a.url));
    assert_eq!(bob_b.origin(), active_origin);
    assert_eq!(bob_b.own().identity, actor_b);
    assert_eq!(bob_b.worker.live.epoch.load(Ordering::SeqCst), active_epoch);
    assert_eq!(
        bob_b.worker.chat_media_epoch.load(Ordering::SeqCst),
        active_media_epoch
    );
    assert!(Arc::ptr_eq(&active_stop, &bob_b.worker.socket_stop));
    assert!(!active_stop.load(Ordering::SeqCst));
    alice_b.send(
        &target_b,
        "Private B after A loses login",
        Vec::new(),
        "revoked-b-after",
        None,
    );
    assert!(bob_b
        .history(&target_b)
        .iter()
        .any(|m| m.content == "Private B after A loses login"));
    bob_b.send(
        &target_b,
        "B can still reply privately",
        Vec::new(),
        "revoked-b-reply",
        None,
    );
    assert!(alice_b
        .history(&target_b)
        .iter()
        .any(|m| m.content == "B can still reply privately"));
    let session: Session = parse(
        Client::new()
            .post(format!("{}/api/login", server_a.url))
            .json(&AuthRequest {
                second_factor: None,
                initial_protection: None,
                username: "crypto_bob".into(),
                password: "Owned_Crypto_Test_1234!".into(),
                invite: String::new(),
            })
            .send()
            .unwrap(),
    )
    .unwrap();
    let mut reopened = CryptoWorker::new(&server_a, session);
    assert_eq!(reopened.own().identity, actor_a);
    assert!(reopened
        .history(&target_a)
        .iter()
        .any(|m| m.content == "Private A before revoked login"));
    alice_a.send(
        &target_a,
        "Private A after explicit new login",
        Vec::new(),
        "revoked-a-reopened",
        None,
    );
    assert!(reopened
        .history(&target_a)
        .iter()
        .any(|m| m.content == "Private A after explicit new login"));
}

#[test]
#[ignore = "requires owned server; actual stopped/restarted background transport and late 401"]
fn actual_server_background_outage_retries_same_actor_and_recovers() {
    let mut server_a = CryptoServer::start();
    let server_b = CryptoServer::start();
    let session_a = register(&server_a, "outage_a", "");
    let session_b = register(&server_b, "outage_b", "");
    let mut parked = CryptoWorker::new(&server_a, session_a);
    let mut active = CryptoWorker::new(&server_b, session_b);
    let actor_a = parked.own().identity;
    let actor_b = active.own().identity;
    let active_origin = active.origin();
    let epoch = active.worker.live.epoch.load(Ordering::SeqCst);
    let active_media_epoch = active.worker.chat_media_epoch.load(Ordering::SeqCst);
    let stop = Arc::new(AtomicBool::new(false));
    let dirty = Arc::new(AtomicBool::new(true));
    let connection = parked.worker.connection.take().unwrap();
    let cursor = connection.change_cursor;
    active.worker.saved_connections.insert(
        server_a.url.clone(),
        SavedConnection {
            connection,
            stop: stop.clone(),
            dirty: dirty.clone(),
        },
    );
    active.drain();
    server_a.stop();
    active.worker.background_at = Instant::now() - Duration::from_secs(1);
    active.worker.refresh_background();
    let failed = active
        .worker
        .background_results
        .recv_timeout(Duration::from_secs(10))
        .unwrap();
    assert!(failed.bootstrap.is_err());
    active.worker.background_completed.send(failed).unwrap();
    active.worker.background_at = Instant::now();
    active.worker.refresh_background();
    assert!(active.drain().iter().any(|u|matches!(u,Update::ConnectionState {url,status:ConnectionStatus::Reconnecting} if url==&server_a.url)));
    assert!(!stop.load(Ordering::SeqCst));
    assert!(dirty.load(Ordering::SeqCst));
    let saved = &active.worker.saved_connections[&server_a.url];
    assert_eq!(
        saved.connection.crypto.as_ref().unwrap().device.identity(),
        &actor_a
    );
    assert_eq!(saved.connection.change_cursor, cursor);
    active.worker.background_at = Instant::now() - Duration::from_secs(1);
    active.worker.refresh_background();
    assert!(
        active.worker.background_jobs.is_empty(),
        "Backoff must not start another immediate request"
    );
    assert!(active.drain().is_empty());
    server_a.restart();
    active
        .worker
        .background_retry
        .get_mut(&server_a.url)
        .unwrap()
        .1 = Instant::now() - Duration::from_secs(1);
    active.worker.background_at = Instant::now() - Duration::from_secs(1);
    active.worker.refresh_background();
    let recovered = active
        .worker
        .background_results
        .recv_timeout(Duration::from_secs(10))
        .unwrap();
    assert!(recovered.bootstrap.is_ok());
    active.worker.background_completed.send(recovered).unwrap();
    active.worker.background_at = Instant::now();
    active.worker.refresh_background();
    let updates = active.drain_checked_community_owners();
    assert!(updates.iter().any(|u|matches!(u,Update::ConnectionState {url,status:ConnectionStatus::Connected} if url==&server_a.url)));
    assert!(!active.worker.background_retry.contains_key(&server_a.url));
    let saved = &active.worker.saved_connections[&server_a.url];
    assert_eq!(
        saved.connection.crypto.as_ref().unwrap().device.identity(),
        &actor_a
    );
    assert_eq!(active.own().identity, actor_b);
    assert_eq!(active.origin(), active_origin);
    assert_eq!(active.worker.live.epoch.load(Ordering::SeqCst), epoch);
    assert_eq!(
        active.worker.chat_media_epoch.load(Ordering::SeqCst),
        active_media_epoch
    );
    // Controlled real-HTTP interleaving: authentication expires after a valid
    // bootstrap, before the subsequent changes request. This is not a claimed
    // naturally observed production race or a synthetic 401 response.
    let token = saved.connection.session.token.clone();
    let bootstrap: Bootstrap = parse(
        active
            .worker
            .http
            .get(format!("{}/api/bootstrap", server_a.url))
            .bearer_auth(&token)
            .send()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        active
            .worker
            .http
            .post(format!("{}/api/logout", server_a.url))
            .bearer_auth(&token)
            .json(&json!({}))
            .send()
            .unwrap()
            .status(),
        reqwest::StatusCode::NO_CONTENT
    );
    let changes: Result<ChangePage, String> = parse(
        active
            .worker
            .http
            .get(format!("{}/api/changes", server_a.url))
            .bearer_auth(&token)
            .query(&[("after", cursor)])
            .send()
            .unwrap(),
    );
    assert_eq!(changes.as_ref().unwrap_err(), "Sign in again");
    active
        .worker
        .background_jobs
        .insert(server_a.url.clone(), token.clone());
    active
        .worker
        .background_completed
        .send(BackgroundResult {
            url: server_a.url.clone(),
            token,
            bootstrap: Ok(bootstrap),
            changes: Some(changes),
            keys: Ok(Vec::new()),
            protections: Vec::new(),
        })
        .unwrap();
    active.worker.background_at = Instant::now();
    active.worker.refresh_background();
    assert!(!active.worker.saved_connections.contains_key(&server_a.url));
    assert!(stop.load(Ordering::SeqCst));
    assert!(active.drain().iter().any(|u|matches!(u,Update::ConnectionState {url,status:ConnectionStatus::SignInRequired} if url==&server_a.url)));
    assert_eq!(active.origin(), active_origin);
    assert_eq!(active.own().identity, actor_b);
}

#[test]
#[ignore = "requires owned localhost server; actual event socket disconnect without manually dirtying the worker"]
fn actual_server_background_socket_loss_wakes_poll() {
    let mut server = CryptoServer::start();
    let session = register(&server, "socket_loss", "");
    let actor = CryptoWorker::new(&server, session.clone());
    let stop = Arc::new(AtomicBool::new(false));
    let dirty = Arc::new(AtomicBool::new(false));
    let events = Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new()));
    spawn_events(
        server.url.clone(),
        session.token,
        dirty.clone(),
        stop.clone(),
        events,
        actor.worker.wake.clone(),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while !dirty.load(Ordering::SeqCst) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    if !dirty.load(Ordering::SeqCst) {
        stop.store(true, Ordering::SeqCst);
        panic!("Owned event socket did not connect");
    }
    dirty.store(false, Ordering::SeqCst);
    server.stop();
    let deadline = Instant::now() + Duration::from_secs(3);
    while !dirty.load(Ordering::SeqCst) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    let woke = dirty.load(Ordering::SeqCst);
    stop.store(true, Ordering::SeqCst);
    assert!(
        woke,
        "Websocket disconnect must wake background authentication/availability poll"
    );
}

fn background_block_case(blocked_by_recipient: bool) {
    let server_a = CryptoServer::start();
    let server_b = CryptoServer::start();
    let (mut alice_a, mut bob_a, mut eve_a, channel_a) = workers(&server_a);
    let (mut alice_b, mut bob_b, _eve_b, channel_b) = workers(&server_b);
    alice_a.pin(&bob_a.own());
    alice_a.pin(&eve_a.own());
    bob_a.pin(&alice_a.own());
    bob_a.pin(&eve_a.own());
    eve_a.pin(&alice_a.own());
    eve_a.pin(&bob_a.own());
    alice_b.pin(&bob_b.own());
    bob_b.pin(&alice_b.own());
    alice_a.send(
        &channel_a,
        "A permitted history before block",
        vec![],
        "access-a-channel-before",
        None,
    );
    assert_eq!(
        bob_a.history(&channel_a)[0].content,
        "A permitted history before block"
    );
    eve_a.history(&channel_a);
    let direct_to_bob = Target::Direct(bob_a.origin().user_id);
    let direct_to_alice = Target::Direct(alice_a.origin().user_id);
    alice_a.send(
        &direct_to_bob,
        "A direct history before block",
        vec![],
        "access-a-dm-before",
        None,
    );
    assert_eq!(
        bob_a.history(&direct_to_alice)[0].content,
        "A direct history before block"
    );
    alice_b.send(
        &channel_b,
        "B active before A changes access",
        vec![],
        "access-b-before",
        None,
    );
    assert_eq!(
        bob_b.history(&channel_b)[0].content,
        "B active before A changes access"
    );
    bob_b.select(&channel_b);
    let bootstrap: Bootstrap = bob_a.worker.get("/api/bootstrap").unwrap();
    let parked_origin = bob_a.origin();
    let active_origin = bob_b.origin();
    let active_identity = bob_b.own().identity;
    let active_media_generation = bob_b.worker.live.epoch.load(Ordering::SeqCst);
    let active_chat_generation = bob_b.worker.chat_media_epoch.load(Ordering::SeqCst);
    let actor_a = bob_a.own().identity;
    let scope_direct = bob_a.worker.crypto_scope(&direct_to_alice).unwrap();
    let direct_control = bob_a.worker.crypto().unwrap().cursor(&scope_direct);
    let direct_application = bob_a
        .worker
        .crypto()
        .unwrap()
        .application_cursor(&scope_direct);
    let mut parked = bob_a.worker.connection.take().unwrap();
    parked.change_cursor = bootstrap.latest_change_sequence;
    let cursor_before = parked.change_cursor;
    let dirty = Arc::new(AtomicBool::new(true));
    bob_b.worker.saved_connections.insert(
        server_a.url.clone(),
        SavedConnection {
            connection: parked,
            stop: Arc::new(AtomicBool::new(false)),
            dirty: dirty.clone(),
        },
    );
    bob_b.drain();
    // This protected direct message reached the actual server before the block,
    // but the parked recipient has not polled or changed its private actor yet.
    alice_a.send(
        &direct_to_bob,
        "A direct queued before block",
        vec![],
        "access-a-dm-queued",
        None,
    );
    let (token, peer) = if blocked_by_recipient {
        (
            &bob_b.worker.saved_connections[&server_a.url]
                .connection
                .session
                .token,
            &parked_origin.user_id,
        )
    } else {
        (
            &alice_a.worker.connection.as_ref().unwrap().session.token,
            &parked_origin.user_id,
        )
    };
    let blocked_peer = if blocked_by_recipient {
        &alice_a.origin().user_id
    } else {
        peer
    };
    let response = Client::new()
        .post(format!("{}/api/blocks/{blocked_peer}", server_a.url))
        .bearer_auth(token)
        .json(&json!({}))
        .send()
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    eve_a.send(
        &channel_a,
        "A permitted peer after direct block",
        vec![],
        "access-a-channel-after",
        None,
    );
    let saved = &bob_b.worker.saved_connections[&server_a.url].connection;
    let actual_bootstrap: Bootstrap = parse(
        Client::new()
            .get(format!("{}/api/bootstrap", server_a.url))
            .bearer_auth(&saved.session.token)
            .send()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        actual_bootstrap
            .social
            .blocked
            .contains(&alice_a.origin().user_id),
        blocked_by_recipient
    );
    let raw: ChangePage = parse(
        Client::new()
            .get(format!("{}/api/changes", server_a.url))
            .bearer_auth(&saved.session.token)
            .query(&[("after", cursor_before)])
            .send()
            .unwrap(),
    )
    .unwrap();
    assert!(
        raw.changes.iter().any(|change| change.channel_id.is_none()),
        "Fixture must contain the older direct delivery"
    );
    assert!(raw.changes.iter().any(|change| change.channel_id
        == match &channel_a {
            Target::Channel(id) => Some(id.clone()),
            _ => None,
        }));
    let denied = Client::new()
        .get(format!("{}/api/e2ee/context", server_a.url))
        .bearer_auth(&saved.session.token)
        .query(&[
            ("device_id", actor_a.device_id.clone()),
            ("peer_id", alice_a.origin().user_id),
        ])
        .send()
        .unwrap();
    assert_eq!(denied.status(), reqwest::StatusCode::FORBIDDEN);
    bob_b.worker.background_at = Instant::now() - Duration::from_secs(1);
    bob_b.worker.refresh_background();
    let result = bob_b
        .worker
        .background_results
        .recv_timeout(Duration::from_secs(15))
        .unwrap();
    fs::write(server_a.root.join("background-block-first-result.json"), serde_json::to_vec_pretty(&json!({
        "blocked_by_recipient":blocked_by_recipient,"direct_context_status":403,"valid_bootstrap":result.bootstrap.is_ok(),
        "keys_error":result.keys.as_ref().err(),"raw_page_contains_direct_and_permitted_channel":true,
        "active_owner":{"url":active_origin.url,"server":active_origin.server_id,"user":active_origin.user_id},
        "parked_owner":{"url":parked_origin.url,"server":parked_origin.server_id,"user":parked_origin.user_id},"physical_devices_opened":false
    })).unwrap()).unwrap();
    println!("Blocked background evidence: {}", server_a.root.display());
    assert!(result.keys.is_ok(), "An inaccessible direct conversation must not stop all permitted background deliveries: {:?}", result.keys.as_ref().err());
    bob_b.worker.background_completed.send(result).unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut contents = Vec::new();
    let mut metadata_seen = false;
    loop {
        bob_b.worker.background_at = Instant::now();
        bob_b.worker.refresh_background();
        for update in bob_b.drain_checked_community_owners() {
            match update {
                Update::CommunityBootstrap { url, bootstrap } if url == server_a.url => {
                    assert_eq!(
                        bootstrap.social.blocked.contains(&alice_a.origin().user_id),
                        blocked_by_recipient
                    );
                    metadata_seen = true;
                }
                Update::Changes { origin, changes } if origin == parked_origin => {
                    assert!(
                        !changes.iter().any(|change| change.channel_id.is_none()),
                        "Inaccessible direct data must not be decrypted or delivered"
                    );
                    contents.extend(
                        changes
                            .into_iter()
                            .filter_map(|change| change.message)
                            .map(|message| message.content),
                    );
                }
                _ => {}
            }
        }
        if bob_b.worker.background_jobs.is_empty() {
            break;
        }
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
    assert!(metadata_seen);
    assert!(contents
        .iter()
        .any(|content| content == "A permitted peer after direct block"));
    assert_eq!(bob_b.origin(), active_origin);
    assert_eq!(bob_b.own().identity, active_identity);
    assert_eq!(
        bob_b.worker.live.epoch.load(Ordering::SeqCst),
        active_media_generation
    );
    assert_eq!(
        bob_b.worker.chat_media_epoch.load(Ordering::SeqCst),
        active_chat_generation
    );
    let parked = &bob_b.worker.saved_connections[&server_a.url].connection;
    assert_eq!(parked.crypto.as_ref().unwrap().device.identity(), &actor_a);
    assert_eq!(
        parked.crypto.as_ref().unwrap().cursor(&scope_direct),
        direct_control
    );
    assert_eq!(
        parked
            .crypto
            .as_ref()
            .unwrap()
            .application_cursor(&scope_direct),
        direct_application
    );
    assert!(parked.change_cursor > cursor_before);
    dirty.store(true, Ordering::Relaxed);
    bob_b.worker.background_at = Instant::now() - Duration::from_secs(1);
    bob_b.worker.refresh_background();
    let repeat = bob_b
        .worker
        .background_results
        .recv_timeout(Duration::from_secs(15))
        .unwrap();
    bob_b.worker.background_completed.send(repeat).unwrap();
    bob_b.worker.background_at = Instant::now();
    bob_b.worker.refresh_background();
    assert!(
        !bob_b
            .drain()
            .iter()
            .any(|update| matches!(update, Update::Changes { .. })),
        "Repeated refresh must not replay allowed Created deliveries"
    );
    alice_b.send(
        &channel_b,
        "B remains protected after blocked A catch-up",
        vec![],
        "access-b-after",
        None,
    );
    assert!(bob_b
        .history(&channel_b)
        .iter()
        .any(|message| message.content == "B remains protected after blocked A catch-up"));
    bob_a.worker.connection = Some(
        bob_b
            .worker
            .saved_connections
            .remove(&server_a.url)
            .unwrap()
            .connection,
    );
}

#[test]
#[ignore = "requires owned servers; actual queued protected DM, own block and independent channel delivery"]
fn actual_server_background_own_block_keeps_other_conversations_live() {
    background_block_case(true);
}

#[test]
#[ignore = "requires owned servers; actual queued protected DM, peer block and independent channel delivery"]
fn actual_server_background_peer_block_keeps_other_conversations_live() {
    background_block_case(false);
}

fn background_access_race_case(revoke_device: bool) {
    let server_a = CryptoServer::start();
    let server_b = CryptoServer::start();
    let (mut alice_a, mut bob_a, mut eve_a, channel_a) = workers(&server_a);
    let (mut alice_b, mut bob_b, _eve_b, channel_b) = workers(&server_b);
    alice_a.pin(&bob_a.own());
    alice_a.pin(&eve_a.own());
    bob_a.pin(&alice_a.own());
    bob_a.pin(&eve_a.own());
    eve_a.pin(&alice_a.own());
    eve_a.pin(&bob_a.own());
    alice_b.pin(&bob_b.own());
    bob_b.pin(&alice_b.own());
    alice_a.send(
        &channel_a,
        "A before access race",
        vec![],
        "race-a-before",
        None,
    );
    bob_a.history(&channel_a);
    eve_a.history(&channel_a);
    let dm_to_bob = Target::Direct(bob_a.origin().user_id);
    let dm_to_alice = Target::Direct(alice_a.origin().user_id);
    alice_a.send(
        &dm_to_bob,
        "A direct before access race",
        vec![],
        "race-dm-before",
        None,
    );
    bob_a.history(&dm_to_alice);
    alice_b.send(
        &channel_b,
        "B before access race",
        vec![],
        "race-b-before",
        None,
    );
    bob_b.history(&channel_b);
    bob_b.select(&channel_b);
    let bootstrap_before: Bootstrap = bob_a.worker.get("/api/bootstrap").unwrap();
    let parked_origin = bob_a.origin();
    let active_origin = bob_b.origin();
    let actor_a = bob_a.own();
    let actor_b = bob_b.own();
    let media_before = (
        bob_b.worker.live.epoch.load(Ordering::SeqCst),
        bob_b.worker.chat_media_epoch.load(Ordering::SeqCst),
    );
    let scope_a = bob_a.worker.crypto_scope(&channel_a).unwrap();
    let crypto = bob_a.worker.crypto().unwrap();
    let private_before = (
        crypto.cursor(&scope_a),
        crypto.application_cursor(&scope_a),
        crypto.checkpoint_revision_for_test(),
    );
    let mut parked = bob_a.worker.connection.take().unwrap();
    parked.change_cursor = bootstrap_before.latest_change_sequence;
    let cursor_before = parked.change_cursor;
    let token = parked.session.token.clone();
    let plans = background_crypto::plans(&parked);
    let dirty = Arc::new(AtomicBool::new(false));
    bob_b.worker.saved_connections.insert(
        server_a.url.clone(),
        SavedConnection {
            connection: parked,
            stop: Arc::new(AtomicBool::new(false)),
            dirty: dirty.clone(),
        },
    );
    bob_b.drain();
    alice_a.send(
        &channel_a,
        "A channel queued before access race",
        vec![],
        "race-channel-queued",
        None,
    );
    alice_a.send(
        &dm_to_bob,
        "A permitted direct delivery",
        vec![],
        "race-dm-queued",
        None,
    );
    let http = Client::new();
    // Capture real public replies while access is still allowed, then change
    // policy before key retrieval. This deterministically exercises the race
    // without copying or reopening an MLS checkpoint or mocking HTTP replies.
    let mut bootstrap: Bootstrap = parse(
        http.get(format!("{}/api/bootstrap", server_a.url))
            .bearer_auth(&token)
            .send()
            .unwrap(),
    )
    .unwrap();
    let mut page: ChangePage = parse(
        http.get(format!("{}/api/changes", server_a.url))
            .bearer_auth(&token)
            .query(&[("after", cursor_before)])
            .send()
            .unwrap(),
    )
    .unwrap();
    assert!(page
        .changes
        .iter()
        .any(|change| change.channel_id.is_some()));
    assert!(page
        .changes
        .iter()
        .any(|change| change.channel_id.is_none()));
    let Target::Channel(channel_id) = &channel_a else {
        panic!("channel fixture required")
    };
    let edited: Channel = parse(alice_a.worker.request(reqwest::Method::PATCH, &format!("/api/channels/{channel_id}")).unwrap()
        .json(&json!({"topic":"Actual access metadata refreshed","visibility":if revoke_device {"public"} else {"private"}}))
        .send().unwrap()).unwrap();
    assert_eq!(edited.topic, "Actual access metadata refreshed");
    if revoke_device {
        let response = http
            .delete(format!(
                "{}/api/e2ee/device/{}",
                server_a.url, actor_a.identity.device_id
            ))
            .bearer_auth(&token)
            .send()
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::NO_CONTENT);
    } else {
        let response = alice_a.worker.request(reqwest::Method::PUT, &format!("/api/channels/{channel_id}/permissions")).unwrap()
            .json(&json!({"entries":[{"subject_kind":"user","subject_id":eve_a.origin().user_id,"read":true,"send":true,"join":true,"stream":true}]}))
            .send().unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
    }
    let denied = http
        .get(format!("{}/api/e2ee/context", server_a.url))
        .bearer_auth(&token)
        .query(&[
            ("device_id", actor_a.identity.device_id.as_str()),
            ("channel_id", channel_id.as_str()),
        ])
        .send()
        .unwrap();
    assert_eq!(denied.status(), reqwest::StatusCode::FORBIDDEN);
    let keys = background_crypto::fetch(
        &http,
        &server_a.url,
        &token,
        plans,
        &mut bootstrap,
        &mut page.changes,
    );
    if revoke_device {
        assert!(
            keys.as_ref()
                .is_err_and(|error| error.contains("no longer registered")),
            "Revoked own device must fail closed: {:?}",
            keys.as_ref().err()
        );
        assert_eq!(
            bootstrap
                .channels
                .iter()
                .find(|channel| channel.id == *channel_id)
                .unwrap()
                .topic,
            "Actual access metadata refreshed"
        );
    } else {
        assert!(
            keys.is_ok(),
            "One denied channel must not stop a permitted direct conversation: {:?}",
            keys.as_ref().err()
        );
        assert!(!bootstrap
            .channels
            .iter()
            .any(|channel| channel.id == *channel_id));
        assert!(!page
            .changes
            .iter()
            .any(|change| change.channel_id.as_ref() == Some(channel_id)));
    }
    let keys_error = keys.as_ref().err().cloned();
    bob_b
        .worker
        .background_jobs
        .insert(server_a.url.clone(), token.clone());
    bob_b
        .worker
        .background_completed
        .send(BackgroundResult {
            url: server_a.url.clone(),
            token,
            bootstrap: Ok(bootstrap),
            changes: Some(Ok(page)),
            keys,
            protections: Vec::new(),
        })
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut metadata_seen = false;
    let mut deliveries = Vec::new();
    loop {
        bob_b.worker.background_at = Instant::now();
        bob_b.worker.refresh_background();
        for update in bob_b.drain_checked_community_owners() {
            match update {
                Update::CommunityBootstrap { url, bootstrap } if url == server_a.url => {
                    metadata_seen = true;
                    if revoke_device {
                        assert_eq!(
                            bootstrap
                                .channels
                                .iter()
                                .find(|channel| channel.id == *channel_id)
                                .unwrap()
                                .topic,
                            "Actual access metadata refreshed"
                        );
                    } else {
                        assert!(!bootstrap
                            .channels
                            .iter()
                            .any(|channel| channel.id == *channel_id));
                    }
                }
                Update::Changes { origin, changes } if origin == parked_origin => {
                    deliveries.extend(changes)
                }
                Update::MessageProtections { origin, .. }
                    if origin == parked_origin && revoke_device =>
                {
                    panic!("A revoked device published message protection")
                }
                _ => {}
            }
        }
        if bob_b.worker.background_jobs.is_empty() {
            break;
        }
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
    assert!(
        metadata_seen,
        "Verified public policy must update even while the own device needs recovery"
    );
    assert_eq!(bob_b.origin(), active_origin);
    assert_eq!(bob_b.own(), actor_b);
    assert_eq!(
        (
            bob_b.worker.live.epoch.load(Ordering::SeqCst),
            bob_b.worker.chat_media_epoch.load(Ordering::SeqCst)
        ),
        media_before
    );
    let parked = &bob_b.worker.saved_connections[&server_a.url].connection;
    let crypto = parked.crypto.as_ref().unwrap();
    assert_eq!(crypto.advertised(), actor_a);
    assert_eq!(crypto.cursor(&scope_a), private_before.0);
    assert_eq!(crypto.application_cursor(&scope_a), private_before.1);
    if revoke_device {
        assert!(deliveries.is_empty());
        assert_eq!(crypto.checkpoint_revision_for_test(), private_before.2);
        assert_eq!(parked.change_cursor, cursor_before);
        assert!(dirty.load(Ordering::Relaxed));
    } else {
        assert!(deliveries.iter().all(|change| change.channel_id.is_none()));
        assert!(deliveries.iter().any(|change| change
            .message
            .as_ref()
            .is_some_and(|message| message.content == "A permitted direct delivery")));
        assert!(parked.change_cursor > cursor_before);
    }
    alice_b.send(
        &channel_b,
        "B protected after A access change",
        vec![],
        "race-b-after",
        None,
    );
    assert!(bob_b
        .history(&channel_b)
        .iter()
        .any(|message| message.content == "B protected after A access change"));
    fs::write(server_a.root.join("background-access-race.json"),serde_json::to_vec_pretty(&json!({
        "actual_context_status":403,"revoke_own_device":revoke_device,"keys_error":keys_error,
        "public_metadata_updated":metadata_seen,"denied_channel_actor_cursors_preserved":true,
        "active_other_community_actor_and_media_preserved":true,"actual_b_protected_chat_after":true,
        "delivered_count":deliveries.len(),"checkpoint_reopened":false,"physical_devices_opened":false
    })).unwrap()).unwrap();
    println!(
        "Background access race evidence: {}",
        server_a.root.display()
    );
    bob_a.worker.connection = Some(
        bob_b
            .worker
            .saved_connections
            .remove(&server_a.url)
            .unwrap()
            .connection,
    );
}

#[test]
#[ignore = "requires owned servers; actual channel denial between public page and key fetch"]
fn actual_server_background_channel_access_race_keeps_other_conversations_live() {
    background_access_race_case(false);
}

#[test]
#[ignore = "requires owned servers; own device revoked while another protected community stays active"]
fn actual_server_background_device_revocation_fails_closed() {
    background_access_race_case(true);
}
