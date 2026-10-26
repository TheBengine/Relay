//! Explicit opt-in encrypted GUI fixture seeding. Never opens media devices.
use super::*;
use crate::e2ee::{CryptoSession, Envelope, MESSAGE_PREFIX};
use serde::Deserialize;
use serde_json::json;
use std::{collections::VecDeque, fs, path::Path, thread};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    base: String,
    server_id: String,
    work_root: PathBuf,
    crypto_dir: PathBuf,
    users: Vec<Session>,
    channels: HashMap<String, String>,
    gif_path: Option<PathBuf>,
    legacy_message_id: String,
    mode: String,
}

struct GuiWorker {
    worker: Worker,
    updates: mpsc::Receiver<Update>,
    successful_sends: VecDeque<Instant>,
}

impl GuiWorker {
    fn new(manifest: &Manifest, session: Session) -> Self {
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
                .get(format!("{}/api/bootstrap", manifest.base))
                .bearer_auth(&session.token)
                .send()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(bootstrap.server_id, manifest.server_id);
        assert_eq!(bootstrap.user.id, session.user.id);
        assert!(bootstrap
            .capabilities
            .iter()
            .any(|capability| capability == "e2ee_mls_v1"));
        assert!(bootstrap.voice_members.is_empty());
        // All accounts intentionally use the SAME directory. The production
        // binding hashes server/user IDs into independent protected files.
        let crypto = CryptoSession::open_for_test(
            manifest.crypto_dir.clone(),
            &bootstrap.server_id,
            &session.user.id,
        )
        .unwrap();
        worker.connection = Some(Connection {
            url: manifest.base.clone(),
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
            successful_sends: VecDeque::new(),
        }
    }
    fn handle(&mut self, command: Command) -> Vec<Update> {
        self.worker.handle(command).unwrap();
        self.updates.try_iter().collect()
    }
    fn pin(&mut self, peer: &relay_crypto::PublicDevice) {
        self.handle(Command::CryptoRefresh);
        self.handle(Command::CryptoVerify {
            origin: CommandOrigin::from(self.worker.connection.as_ref().unwrap()),
            user_id: peer.identity.user_id.clone(),
            device_id: peer.identity.device_id.clone(),
            fingerprint: peer.fingerprint.clone(),
        });
    }
    fn prepare(&mut self, target: &Target) {
        self.handle(Command::SelectConversation(target.clone()));
        self.worker
            .ensure_crypto_conversation(target)
            .unwrap_or_else(|error| {
                panic!(
                    "Owned fixture prepare {target:?} for {}: {error}",
                    self.worker
                        .connection
                        .as_ref()
                        .unwrap()
                        .session
                        .user
                        .username
                )
            });
        self.updates.try_iter().for_each(drop);
    }
    fn raw(&self, target: &Target) -> Vec<Message> {
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
    fn pace(&mut self) {
        // Seed only known intentional new messages. Never conceal a 429 behind
        // a generic retry. Four-per-window leaves room for a reaction control.
        while self.successful_sends.len() >= 4 {
            if let Some(wait) = Duration::from_millis(5100)
                .checked_sub(self.successful_sends.front().unwrap().elapsed())
            {
                thread::sleep(wait);
            }
            self.successful_sends.pop_front();
        }
    }
    fn send(
        &mut self,
        target: &Target,
        text: &str,
        files: Vec<String>,
        reply: Option<String>,
        mentions: Vec<String>,
        pace: bool,
    ) -> Message {
        if pace {
            self.pace();
        }
        self.prepare(target);
        let client_id = uuid::Uuid::new_v4().to_string();
        let updates = self.handle(Command::SendRich {
            binding: None,
            origin: CommandOrigin::from(self.worker.connection.as_ref().unwrap()),
            target: target.clone(),
            content: text.into(),
            attachment_ids: files,
            client_id: client_id.clone(),
            reply_to_id: reply,
            mentions,
        });
        assert!(
            updates.iter().any(
                |update| matches!(update,Update::SentRich {client_id:id,..} if id == &client_id)
            ),
            "encrypted fixture send must commit"
        );
        self.successful_sends.push_back(Instant::now());
        self.raw(target)
            .into_iter()
            .find(|message| message.client_id.as_deref() == Some(&client_id))
            .unwrap()
    }
    fn upload(&mut self, target: &Target, path: &Path) -> Attachment {
        self.handle(Command::StartUpload {
            origin: CommandOrigin::from(self.worker.connection.as_ref().unwrap()),
            target: target.clone(),
            path: path.to_path_buf(),
            operation_id: uuid::Uuid::new_v4().to_string(),
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            self.worker.poll_transfers();
            for update in self.updates.try_iter() {
                match update {
                    Update::UploadFinished { attachment, .. } => return attachment,
                    Update::UploadFailed { error, .. } => {
                        panic!("encrypted GIF seed failed: {error}")
                    }
                    _ => {}
                }
            }
            assert!(Instant::now() < deadline, "encrypted GIF seed timed out");
            thread::sleep(Duration::from_millis(5));
        }
    }
}
fn prepare_all(workers: &mut [GuiWorker], target: &Target) {
    // Selection can initialize a target on an actor while peer verification is
    // still propagating. Give its verified manager another explicit turn to
    // admit peers; only the documented pending-admission state is retryable.
    for _ in 0..4 {
        let mut ready = true;
        for actor in workers.iter_mut() {
            actor.handle(Command::SelectConversation(target.clone()));
            match actor.worker.ensure_crypto_conversation(target) {
                Ok(_) => {}
                Err(error) if error == "Waiting for this conversation's key manager to verify and approve your device." => ready = false,
                Err(error) => panic!("Owned fixture prepare {target:?}: {error}"),
            }
            actor.updates.try_iter().for_each(drop);
        }
        if ready {
            return;
        }
    }
    panic!("Owned fixture key admission did not complete: {target:?}");
}
impl Drop for GuiWorker {
    fn drop(&mut self) {
        self.worker.socket_stop.store(true, Ordering::Release);
        // Keep the production Credential Manager anti-rollback checkpoint:
        // subsequent release GUI processes must reopen this exact state.
    }
}

fn manifest() -> Manifest {
    let path = PathBuf::from(
        std::env::var_os("RELAY_GUI_CRYPTO_SEED_MANIFEST")
            .expect("explicit owned GUI manifest required"),
    );
    assert!(path.is_absolute());
    assert!(fs::metadata(&path).unwrap().len() <= 64 * 1024);
    let data: Manifest = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let owned_root = data.work_root.canonicalize().unwrap();
    let allowed = PathBuf::from(
        std::env::var_os("RELAY_TEST_WORK_ROOT").expect("explicit work boundary required"),
    )
    .canonicalize()
    .unwrap();
    assert!(owned_root.starts_with(&allowed) && owned_root != allowed);
    assert!(owned_root
        .file_name()
        .unwrap()
        .to_string_lossy()
        .starts_with("native-qa-settings-"));
    assert_eq!(path.canonicalize().unwrap().parent().unwrap(), owned_root);
    assert_eq!(
        data.crypto_dir.canonicalize().unwrap().parent().unwrap(),
        owned_root
    );
    assert!(!fs::symlink_metadata(&data.crypto_dir)
        .unwrap()
        .file_type()
        .is_symlink());
    if let Some(gif) = &data.gif_path {
        assert_eq!(gif.canonicalize().unwrap().parent().unwrap(), owned_root);
    }
    let url = url::Url::parse(&data.base).unwrap();
    assert_eq!(url.scheme(), "http");
    assert_eq!(url.host_str(), Some("127.0.0.1"));
    assert!(
        url.port().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.path() == "/"
            && url.query().is_none()
            && url.fragment().is_none()
    );
    let health: serde_json::Value = parse(
        Client::new()
            .get(format!("{}/health", data.base))
            .send()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(health["server_id"], data.server_id);
    assert_eq!(data.users.len(), 3);
    assert_eq!(
        data.users
            .iter()
            .map(|session| &session.user.id)
            .collect::<std::collections::HashSet<_>>()
            .len(),
        3
    );
    for name in ["general", "planning", "slow", "fast"] {
        assert!(data.channels.contains_key(name));
    }
    assert!(matches!(
        data.mode.as_str(),
        "initial" | "slow" | "spam" | "spam_prepare"
    ));
    data
}

#[test]
#[ignore = "requires RELAY_GUI_CRYPTO_SEED_MANIFEST; owned synthetic GUI data only"]
fn actual_server_gui_crypto_seed() {
    let manifest = manifest();
    let mut workers: Vec<_> = manifest
        .users
        .iter()
        .cloned()
        .map(|session| GuiWorker::new(&manifest, session))
        .collect();
    let publics: Vec<_> = workers
        .iter()
        .map(|worker| {
            worker
                .worker
                .connection
                .as_ref()
                .unwrap()
                .crypto
                .as_ref()
                .unwrap()
                .advertised()
        })
        .collect();
    for (index, worker) in workers.iter_mut().enumerate() {
        for (peer, public) in publics.iter().enumerate() {
            if index != peer {
                worker.pin(public);
            }
        }
    }
    let channel = |name: &str| Target::Channel(manifest.channels[name].clone());
    let general = channel("general");
    let mut ids = HashMap::new();
    if manifest.mode == "initial" {
        for name in ["general", "planning", "slow", "fast"] {
            let target = channel(name);
            prepare_all(&mut workers, &target);
        }
        // Optional new-feature targets come only from this bounded owned manifest.
        for name in ["voice_text", "group", "thread", "announcement", "poll"] {
            let Some(id) = manifest.channels.get(name) else {
                continue;
            };
            let target = Target::Channel(id.clone());
            prepare_all(&mut workers, &target);
            let content = if name == "poll" {
                "relay-poll:v1:{\"question\":\"What should we play on Friday?\",\"options\":[\"Co-op adventure\",\"Community tournament\",\"Something relaxed\"],\"multiple\":false}"
            } else {
                "Our game-night plans stay in this end-to-end encrypted conversation."
            };
            let message = workers[0].send(
                &target,
                content,
                vec![],
                None,
                vec![manifest.users[0].user.id.clone()],
                true,
            );
            if name == "poll" {
                workers[1].prepare(&target);
                let origin = CommandOrigin::from(workers[1].worker.connection.as_ref().unwrap());
                workers[1].handle(Command::React {
                    origin,
                    target: target.clone(),
                    message_id: message.id.clone(),
                    target_client_id: message.client_id.clone(),
                    emoji: "relay-poll-vote:0".into(),
                    active: true,
                });
                let received = workers[0].handle(Command::History {
                    target: target.clone(),
                    before: None,
                    request_id: 71,
                });
                assert!(received.iter().any(|u| matches!(u, Update::History { page, .. } if page.messages.iter().any(|m| m.id == message.id && m.content == content && m.reactions.iter().any(|r| r.emoji == "relay-poll-vote:0")))));
            }
            assert!(workers[0]
                .raw(&target)
                .iter()
                .all(|m| m.content.starts_with(MESSAGE_PREFIX)
                    && m.mentions.is_empty()
                    && m.reactions.is_empty()));
            ids.insert(name, message.id);
        }
        let texts = [
            (
                0,
                "Welcome to The Commons. This is our little place to hang out.",
            ),
            (
                1,
                "Game night at eight? I have a couple of maps we could try.",
            ),
            (2, "I am in. I will jump into the Lounge when I get home."),
            (0, "Sounds good. Text channels and voice rooms are ready."),
            (1, "I like having the server on our own PC."),
            (
                0,
                "We can keep plans in #game-night and use the Lounge for voice.",
            ),
            (0, "I will send an invite to the rest of the group."),
            (
                1,
                "@Alex I am bringing the usual squad.\nCan we start with a warm-up round?",
            ),
            (
                1,
                "This encrypted conversation is seeded test data for the Relay preview.",
            ),
        ];
        let mut reply = None;
        for (index, (author, text)) in texts.iter().enumerate() {
            if index == 4 {
                if let Some(path) = &manifest.gif_path {
                    let file = workers[1].upload(&general, path);
                    let gif = workers[1].send(
                        &general,
                        "A little game-night energy.",
                        vec![file.id],
                        None,
                        Vec::new(),
                        true,
                    );
                    ids.insert("gif", gif.id);
                }
            }
            let message = workers[*author].send(
                &general,
                text,
                Vec::new(),
                if index == 2 { reply.clone() } else { None },
                if index == 7 {
                    vec![manifest.users[0].user.id.clone()]
                } else {
                    Vec::new()
                },
                true,
            );
            if index == 1 {
                reply = Some(message.id.clone());
            }
            if index == 0 {
                ids.insert("first", message.id.clone());
            }
            if index == 2 {
                workers[0].pace();
                let origin = CommandOrigin::from(workers[0].worker.connection.as_ref().unwrap());
                workers[0].handle(Command::React {
                    origin,
                    target: general.clone(),
                    message_id: message.id.clone(),
                    target_client_id: message.client_id.clone(),
                    emoji: "👍".into(),
                    active: true,
                });
                workers[0].successful_sends.push_back(Instant::now());
            }
        }
        let long=workers[0].send(&general,"Game night details: bring your usual squad, join the Lounge at eight, and keep plans here so everyone can find them. We can choose a warm-up map together. This longer protected pin checks wrapping at the minimum size and largest text settings.",Vec::new(),None,Vec::new(),true);
        ids.insert("long_pin", long.id.clone());
        for id in [ids["first"].clone(), long.id] {
            workers[0].handle(Command::SetPin {
                channel_id: manifest.channels["general"].clone(),
                message_id: id,
                pinned: true,
                request_id: 1,
            });
        }
        workers[0].send(
            &channel("planning"),
            "Our private plans stay in this protected conversation.",
            Vec::new(),
            None,
            Vec::new(),
            true,
        );
        let dm = Target::Direct(manifest.users[1].user.id.clone());
        workers[0].send(
            &dm,
            "Hey Mira, want to join us for game night?",
            Vec::new(),
            None,
            Vec::new(),
            true,
        );
        workers[1].send(
            &Target::Direct(manifest.users[0].user.id.clone()),
            "Definitely. I will be there at eight.",
            Vec::new(),
            None,
            Vec::new(),
            true,
        );
        for worker in &mut workers {
            worker.prepare(&general);
        }
        for name in ["general", "planning", "slow", "fast"] {
            for message in workers[0].raw(&channel(name)) {
                assert!(
                    message.content.starts_with(MESSAGE_PREFIX)
                        || message.id == manifest.legacy_message_id,
                    "only the explicit legacy example may be plaintext"
                );
            }
        }
    } else if manifest.mode == "slow" {
        workers[1].send(
            &channel("slow"),
            "One confirmed protected message starts channel slowmode.",
            Vec::new(),
            None,
            Vec::new(),
            true,
        );
    } else if manifest.mode == "spam_prepare" {
        // All ratchet and protected-checkpoint work happens before the server's
        // five-second quota window starts. The screenshot runner sends these
        // exact durably staged bodies after the actors release their leases.
        let target = channel("fast");
        workers[1].prepare(&target);
        let scope = workers[1].worker.crypto_scope(&target).unwrap();
        let mut bodies = Vec::new();
        for index in 0..5 {
            let client_id = uuid::Uuid::new_v4().to_string();
            let content = workers[1]
                .worker
                .connection
                .as_mut()
                .unwrap()
                .crypto
                .as_mut()
                .unwrap()
                .encrypted_content(Envelope {
                    version: 1,
                    scope: scope.clone(),
                    client_id: client_id.clone(),
                    content: format!("Confirmed protected burst message {}.", index + 1),
                    reply_to_id: None,
                    mentions: Vec::new(),
                    attachments: Vec::new(),
                    action: None,
                })
                .unwrap();
            bodies.push(SendMessage {
                channel_id: Some(manifest.channels["fast"].clone()),
                content,
                client_id: Some(client_id),
                ..Default::default()
            });
        }
        let prepared = serde_json::to_vec(&json!({"server_id":manifest.server_id,
            "user_id":manifest.users[1].user.id,"channel_id":manifest.channels["fast"],"bodies":bodies})).unwrap();
        assert!(prepared.len() <= 128 * 1024);
        fs::write(
            manifest.work_root.join("private-spam-requests.json"),
            prepared,
        )
        .unwrap();
    } else {
        for index in 0..5 {
            workers[1].send(
                &channel("fast"),
                &format!("Confirmed protected burst message {}.", index + 1),
                Vec::new(),
                None,
                Vec::new(),
                false,
            );
        }
    }
    let own_devices: Vec<_> = publics.iter().map(|device|json!({"user_id":device.identity.user_id,"device_id":device.identity.device_id,"fingerprint":device.fingerprint})).collect();
    let record = json!({"mode":manifest.mode,"server_id":manifest.server_id,"message_ids":ids,"devices":own_devices,
        "shared_protected_crypto_directory":true,"mutually_verified":true,"new_messages_opaque":true,
        "legacy_examples":1,"physical_devices_opened":false});
    fs::write(
        manifest
            .work_root
            .join(format!("crypto-seed-{}.json", manifest.mode)),
        serde_json::to_vec_pretty(&record).unwrap(),
    )
    .unwrap();
}
