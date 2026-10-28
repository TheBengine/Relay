//! Trust on first use with real encryption sessions and real MLS groups.
//! Synthetic actors only; no server, account, network or personal data.
//! Each actor has its own random community binding, and its Windows
//! revision marker is removed when the actor is dropped.
use super::*;
use relay_crypto::FirstUse;

struct Actor {
    root: PathBuf,
    server: String,
    user: String,
    inner: Option<CryptoSession>,
}

impl Actor {
    fn new(server: &str, user: &str) -> Self {
        let root = std::env::var_os("RELAY_TEST_WORK_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("relay-tests"))
            .join(format!("trust-{user}-{}", uuid::Uuid::new_v4()));
        let session = CryptoSession::open_for_test(root.clone(), server, user).unwrap();
        Self {
            root,
            server: server.into(),
            user: user.into(),
            inner: Some(session),
        }
    }
    fn reopen(&mut self) {
        drop(self.inner.take());
        self.inner = Some(
            CryptoSession::open_for_test(self.root.clone(), &self.server, &self.user).unwrap(),
        );
    }
    /// What the community's device directory would show for this actor.
    fn listed(&self) -> PublicDevice {
        self.advertised()
    }
    fn sees(&mut self, public: &PublicDevice) {
        self.directory
            .insert(public.identity.device_id.clone(), public.clone());
        self.status
            .peers
            .retain(|p| p.device_id != public.identity.device_id);
        self.status.peers.push(CryptoPeerStatus {
            user_id: public.identity.user_id.clone(),
            name: public.identity.user_id.clone(),
            device_id: public.identity.device_id.clone(),
            fingerprint: public.fingerprint.clone(),
            verified: false,
            trusted: false,
            changed: false,
            needs_review: false,
        });
    }
    fn peer(&self, device_id: &str) -> CryptoPeerStatus {
        self.status
            .peers
            .iter()
            .find(|p| p.device_id == device_id)
            .unwrap()
            .clone()
    }
}

impl std::ops::Deref for Actor {
    type Target = CryptoSession;
    fn deref(&self) -> &CryptoSession {
        self.inner.as_ref().unwrap()
    }
}

impl std::ops::DerefMut for Actor {
    fn deref_mut(&mut self) -> &mut CryptoSession {
        self.inner.as_mut().unwrap()
    }
}

impl Drop for Actor {
    fn drop(&mut self) {
        // Remove the marker even if a failed test left no open actor.
        drop(self.inner.take());
        protected::remove_head(&format!("Relay/crypto/v1/{}/{}", self.server, self.user));
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn server() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn target() -> relay_protocol::CryptoTarget {
    relay_protocol::CryptoTarget {
        channel_id: Some("general".into()),
        peer_id: None,
    }
}

/// A device that is not a full actor here: another device of a person, or a
/// device the server made up. Its private key never leaves the test.
fn raw_device(server: &str, user: &str, device: &str) -> Device {
    Device::new(Identity {
        server_id: server.into(),
        user_id: user.into(),
        device_id: device.into(),
    })
    .unwrap()
}

fn package(device: &mut Device) -> PublicDevice {
    device.transact(|d| d.key_package(), |_| Ok(())).unwrap()
}

/// The same device ID with a different signing key, as a server could list it.
fn substitute(public: &PublicDevice) -> PublicDevice {
    package(&mut Device::new(public.identity.clone()).unwrap())
}

fn welcome_event(
    batch: &relay_protocol::AppendCryptoEvents,
    author: &PublicDevice,
    to: &str,
    sequence: u64,
) -> relay_protocol::CryptoEvent {
    let welcome = batch.events.iter().find(|e| e.kind == "welcome").unwrap();
    relay_protocol::CryptoEvent {
        sequence,
        epoch: batch.epoch,
        group_id: batch.group_id.clone(),
        kind: "welcome".into(),
        author_user_id: author.identity.user_id.clone(),
        author_device_id: author.identity.device_id.clone(),
        recipient_device_id: Some(to.into()),
        body: welcome.body.clone(),
        ..Default::default()
    }
}

fn raw_event(
    kind: &str,
    group_id: &str,
    author: &PublicDevice,
    to: Option<&str>,
    body: &[u8],
    epoch: u64,
    sequence: u64,
) -> relay_protocol::CryptoEvent {
    relay_protocol::CryptoEvent {
        sequence,
        epoch,
        group_id: group_id.into(),
        kind: kind.into(),
        author_user_id: author.identity.user_id.clone(),
        author_device_id: author.identity.device_id.clone(),
        recipient_device_id: to.map(str::to_string),
        body: STANDARD.encode(body),
        ..Default::default()
    }
}

fn envelope(scope: &str, id: &str, text: &str) -> Envelope {
    Envelope {
        version: 1,
        scope: scope.into(),
        client_id: id.into(),
        content: text.into(),
        reply_to_id: None,
        mentions: Vec::new(),
        attachments: Vec::new(),
        action: None,
    }
}

#[test]
fn first_contact_works_both_ways_without_comparing_codes_or_any_warning() {
    let server = server();
    let scope = format!("{server}/dm/alice-bob");
    let mut alice = Actor::new(&server, "alice");
    let mut bob = Actor::new(&server, "bob");
    let (a, b) = (alice.listed(), bob.listed());
    alice.sees(&b);
    bob.sees(&a);
    assert!(
        alice.trust_directory(true).unwrap().is_empty(),
        "a first contact is silent"
    );
    assert!(bob.trust_directory(true).unwrap().is_empty());
    let shown = alice.peer(&b.identity.device_id);
    assert!(shown.trusted && !shown.verified && !shown.changed && !shown.needs_review);
    assert!(alice.devices_needing_review("bob").is_empty());

    // Alice is the key manager: Bob's trusted device is added at once.
    assert!(alice.device.is_approved(&b));
    let batch = alice
        .stage_membership(
            &scope,
            target(),
            "fixture-dm".into(),
            0,
            std::slice::from_ref(&b),
            &[],
            true,
        )
        .unwrap();
    bob.apply_control(
        &scope,
        &welcome_event(&batch, &a, &b.identity.device_id, 3),
        &server,
        None,
    )
    .unwrap();
    alice.committed_control(&scope, 3).unwrap();

    let wire = alice
        .encrypted_content(envelope(&scope, "m1", "hej bror"))
        .unwrap();
    alice.committed("m1").unwrap();
    assert_eq!(
        bob.decrypt_content(&scope, &wire).unwrap().envelope.content,
        "hej bror"
    );
    let reply = bob
        .encrypted_content(envelope(&scope, "m2", "hej"))
        .unwrap();
    bob.committed("m2").unwrap();
    assert_eq!(
        alice
            .decrypt_content(&scope, &reply)
            .unwrap()
            .envelope
            .content,
        "hej"
    );

    // The trust survives a restart.
    alice.reopen();
    assert_eq!(alice.device.trust(&b), Some(relay_crypto::Trust::FirstUse));
    assert!(alice.trust_notices().is_empty());
}

#[test]
fn a_new_device_of_an_unverified_person_is_trusted_and_one_of_a_verified_person_waits() {
    let server = server();
    let mut alice = Actor::new(&server, "alice");
    let bob_pc = package(&mut raw_device(&server, "bob", "bob-pc"));
    let bob_phone = package(&mut raw_device(&server, "bob", "bob-phone"));
    let carol_pc = package(&mut raw_device(&server, "carol", "carol-pc"));
    let carol_phone = package(&mut raw_device(&server, "carol", "carol-phone"));
    alice.sees(&bob_pc);
    alice.sees(&carol_pc);
    alice.trust_directory(true).unwrap();
    alice.approve("carol-pc", &carol_pc.fingerprint).unwrap();

    alice.sees(&bob_phone);
    alice.sees(&carol_phone);
    assert!(
        alice.trust_directory(true).unwrap().is_empty(),
        "new devices are not key changes"
    );
    assert!(
        alice.peer("bob-phone").trusted,
        "unverified Bob's new phone just works"
    );
    let held = alice.peer("carol-phone");
    assert!(held.needs_review && !held.trusted && !held.changed);
    assert_eq!(
        alice.devices_needing_review("carol"),
        vec!["carol-phone".to_string()]
    );
    assert!(
        !alice.device.is_approved(&carol_phone),
        "the key manager never adds it"
    );

    // One click: trusted, not verified. The decision survives a restart.
    alice
        .accept_new_key("carol-phone", &carol_phone.fingerprint)
        .unwrap();
    assert!(alice.peer("carol-phone").trusted && !alice.peer("carol-phone").verified);
    alice.reopen();
    assert_eq!(
        alice.device.trust(&carol_phone),
        Some(relay_crypto::Trust::FirstUse)
    );
    assert!(
        alice.device.person_verified("carol"),
        "Carol keeps her Verified pc"
    );
}

#[test]
fn a_changed_key_is_announced_for_unverified_people_and_held_for_verified_people() {
    let server = server();
    let mut alice = Actor::new(&server, "alice");
    let bob = package(&mut raw_device(&server, "bob", "bob-pc"));
    let carol = package(&mut raw_device(&server, "carol", "carol-pc"));
    alice.sees(&bob);
    alice.sees(&carol);
    alice.trust_directory(true).unwrap();
    alice.approve("carol-pc", &carol.fingerprint).unwrap();

    let new_bob = substitute(&bob);
    let new_carol = substitute(&carol);
    alice.sees(&new_bob);
    alice.sees(&new_carol);
    let notices = alice.trust_directory(true).unwrap();
    assert_eq!(
        notices.len(),
        1,
        "only the change Relay accepted is announced"
    );
    assert_eq!(notices[0].kind, TrustNoticeKind::CodeChanged);
    assert_eq!(notices[0].device_id, "bob-pc");
    assert!(
        alice.peer("bob-pc").trusted,
        "messaging with Bob keeps working"
    );
    assert_eq!(alice.status.notices, notices);

    let held = alice.peer("carol-pc");
    assert!(held.changed && held.needs_review && !held.trusted);
    assert_eq!(
        alice.device.trust(&carol),
        Some(relay_crypto::Trust::Verified),
        "the old key stays"
    );
    // A stale code cannot be accepted; the current one can, with one click.
    assert!(alice
        .accept_new_key("carol-pc", &carol.fingerprint)
        .is_err());
    alice
        .accept_new_key("carol-pc", &new_carol.fingerprint)
        .unwrap();
    assert!(alice.peer("carol-pc").trusted && !alice.peer("carol-pc").needs_review);

    // The notice stays until it is read, also across a restart.
    alice.reopen();
    assert_eq!(alice.trust_notices().len(), 1);
    alice.dismiss_notice("bob-pc").unwrap();
    alice.reopen();
    assert!(alice.trust_notices().is_empty());
}

#[test]
fn comparing_a_changed_code_verifies_it_and_removing_the_mark_keeps_trust() {
    let server = server();
    let mut alice = Actor::new(&server, "alice");
    let carol = package(&mut raw_device(&server, "carol", "carol-pc"));
    alice.sees(&carol);
    alice.trust_directory(true).unwrap();
    alice.approve("carol-pc", &carol.fingerprint).unwrap();
    let new_carol = substitute(&carol);
    alice.sees(&new_carol);
    alice.trust_directory(true).unwrap();
    assert!(
        alice.approve("carol-pc", &carol.fingerprint).is_err(),
        "the old code no longer matches"
    );
    alice.approve("carol-pc", &new_carol.fingerprint).unwrap();
    assert!(alice.peer("carol-pc").verified);
    alice.unapprove("carol-pc").unwrap();
    let shown = alice.peer("carol-pc");
    assert!(shown.trusted && !shown.verified && !shown.needs_review);
}

#[test]
fn a_device_the_server_injects_for_a_verified_person_never_receives_or_joins_silently() {
    let server = server();
    let scope = format!("{server}/channel/general");
    let mut alice = Actor::new(&server, "alice");
    let a = alice.listed();
    // Carol manages the channel's keys and trusts everyone on first use.
    let mut carol = raw_device(&server, "carol", "carol-pc");
    let c = package(&mut carol);
    let bob = package(&mut raw_device(&server, "bob", "bob-pc"));
    let injected = package(&mut raw_device(&server, "bob", "bob-injected"));
    for public in [&a, &bob, &injected] {
        carol
            .transact(|d| d.trust_on_first_use(public).map(|_| ()), |_| Ok(()))
            .unwrap();
    }
    carol
        .transact(|d| d.create_group(&scope, "fixture-group"), |_| Ok(()))
        .unwrap();
    let add_alice = carol
        .transact(
            |d| d.add_members(&scope, std::slice::from_ref(&a)),
            |_| Ok(()),
        )
        .unwrap();
    alice.sees(&c);
    alice.sees(&bob);
    alice.trust_directory(true).unwrap();
    alice.approve("bob-pc", &bob.fingerprint).unwrap();
    alice
        .apply_control(
            &scope,
            &raw_event(
                "welcome",
                "fixture-group",
                &c,
                Some(&a.identity.device_id),
                add_alice.welcome.as_ref().unwrap(),
                add_alice.epoch,
                3,
            ),
            &server,
            None,
        )
        .unwrap();

    // The server lists a new "Bob" device. Alice verified Bob, so it is held.
    alice.sees(&injected);
    assert!(alice.trust_directory(true).unwrap().is_empty());
    assert!(alice.peer("bob-injected").needs_review);
    assert_eq!(alice.device.first_use_decision(&injected), FirstUse::Held);
    assert!(!alice.device.is_approved(&injected));

    // Carol (who never verified Bob) adds it. Alice refuses the update and
    // keeps her state; nothing about the conversation's keys moved.
    let add_injected = carol
        .transact(
            |d| d.add_members(&scope, std::slice::from_ref(&injected)),
            |_| Ok(()),
        )
        .unwrap();
    let commit = raw_event(
        "commit",
        "fixture-group",
        &c,
        None,
        &add_injected.commit,
        add_injected.epoch,
        4,
    );
    let before = alice.cursor(&scope);
    assert!(alice.apply_control(&scope, &commit, &server, None).is_err());
    assert_eq!(alice.cursor(&scope), before);
    assert!(alice.device.expected_fingerprint("bob-injected").is_none());
    let epoch = alice.device.group_state(&scope).unwrap().epoch;
    assert_eq!(
        epoch, add_alice.epoch,
        "Alice stays in the epoch before the injected device"
    );

    // Only Alice's own decision lets the update in.
    alice
        .accept_new_key("bob-injected", &injected.fingerprint)
        .unwrap();
    alice.apply_control(&scope, &commit, &server, None).unwrap();
    assert_eq!(
        alice.device.group_state(&scope).unwrap().epoch,
        add_injected.epoch
    );
}

#[test]
fn a_listing_without_a_package_joins_only_through_its_exact_key_in_a_group_update() {
    let server = server();
    let scope = format!("{server}/channel/general");
    let mut alice = Actor::new(&server, "alice");
    let a = alice.listed();
    let mut carol = raw_device(&server, "carol", "carol-pc");
    let c = package(&mut carol);
    let bob = package(&mut raw_device(&server, "bob", "bob-pc"));
    for public in [&a, &bob] {
        carol
            .transact(|d| d.trust_on_first_use(public).map(|_| ()), |_| Ok(()))
            .unwrap();
    }
    carol
        .transact(|d| d.create_group(&scope, "fixture-group"), |_| Ok(()))
        .unwrap();
    carol
        .transact(
            |d| d.add_members(&scope, std::slice::from_ref(&bob)),
            |_| Ok(()),
        )
        .unwrap();
    let add_alice = carol
        .transact(
            |d| d.add_members(&scope, std::slice::from_ref(&a)),
            |_| Ok(()),
        )
        .unwrap();
    // Bob's only KeyPackage was used by his Welcome; the directory lists him without one.
    let mut listed_bob = bob.clone();
    listed_bob.key_package.clear();
    alice.sees(&c);
    alice.sees(&listed_bob);
    alice.trust_directory(true).unwrap();
    assert!(alice.device.expected_fingerprint("bob-pc").is_none());
    alice
        .apply_control(
            &scope,
            &raw_event(
                "welcome",
                "fixture-group",
                &c,
                Some(&a.identity.device_id),
                add_alice.welcome.as_ref().unwrap(),
                add_alice.epoch,
                2,
            ),
            &server,
            None,
        )
        .unwrap();
    assert_eq!(
        alice.device.trust(&bob),
        Some(relay_crypto::Trust::FirstUse)
    );
    let members = alice.device.members(&scope).unwrap();
    assert!(members.contains(&bob.identity) && members.contains(&c.identity));
}

#[test]
fn a_new_device_on_the_own_account_is_trusted_and_announced_except_on_first_start() {
    let server = server();
    let mut alice = Actor::new(&server, "alice");
    let laptop = package(&mut raw_device(&server, "alice", "alice-laptop"));
    // First start: every listed device is a first contact, including own ones.
    alice.sees(&laptop);
    assert!(alice.trust_directory(true).unwrap().is_empty());
    assert!(alice.device.is_approved(&laptop));
    // Later: a further own device is trusted at once (never stuck) and announced.
    let phone = package(&mut raw_device(&server, "alice", "alice-phone"));
    alice.sees(&phone);
    let notices = alice.trust_directory(false).unwrap();
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].kind, TrustNoticeKind::NewOwnDevice);
    assert_eq!(notices[0].device_id, "alice-phone");
    assert!(alice.device.is_approved(&phone));
    // Comparing codes with the own laptop never makes later own devices wait.
    alice.approve("alice-laptop", &laptop.fingerprint).unwrap();
    let tablet = package(&mut raw_device(&server, "alice", "alice-tablet"));
    alice.sees(&tablet);
    alice.trust_directory(false).unwrap();
    assert!(alice.device.is_approved(&tablet));
    // A new key on an own device reads as a new device on the account.
    let rekeyed = substitute(&laptop);
    alice.sees(&rekeyed);
    let notices = alice.trust_directory(false).unwrap();
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].kind, TrustNoticeKind::NewOwnDevice);
    assert!(alice.device.is_approved(&rekeyed));
}

#[test]
fn an_unusable_listing_is_not_trusted_and_causes_no_checkpoint_writes() {
    let server = server();
    let mut alice = Actor::new(&server, "alice");
    alice.trust_directory(true).unwrap();
    let mut broken = package(&mut raw_device(&server, "mallory", "mallory-pc"));
    broken.fingerprint = "0".repeat(64);
    alice.sees(&broken);
    let revision = alice.checkpoint_revision_for_test();
    alice.trust_directory(true).unwrap();
    alice.trust_directory(false).unwrap();
    assert_eq!(alice.checkpoint_revision_for_test(), revision);
    assert!(!alice.device.is_approved(&broken));
    assert!(!alice.peer("mallory-pc").trusted);
}

#[test]
fn an_invitation_proof_upgrades_a_first_use_trust_to_verified() {
    let server = server();
    let mut friend = Actor::new(&server, "friend");
    let host = package(&mut raw_device(&server, "host", "host-pc"));
    friend.sees(&host);
    friend.trust_directory(true).unwrap();
    assert_eq!(
        friend.device.trust(&host),
        Some(relay_crypto::Trust::FirstUse)
    );
    let proof =
        InviteProof::new(&"ab".repeat(32), "host-pc", &host.fingerprint, [7u8; 32]).unwrap();
    assert!(friend.approve_invited_host(&proof).unwrap());
    assert_eq!(
        friend.device.trust(&host),
        Some(relay_crypto::Trust::Verified)
    );
    // A link naming another key never replaces the trusted one.
    let other = InviteProof::new(
        &"cd".repeat(32),
        "host-pc",
        &substitute(&host).fingerprint,
        [8u8; 32],
    )
    .unwrap();
    assert!(!friend.approve_invited_host(&other).unwrap());
}
