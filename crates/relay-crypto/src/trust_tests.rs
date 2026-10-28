//! Trust on first use: who is trusted automatically, who waits for the user,
//! and how a group update may complete a listing without a KeyPackage.
use super::*;

fn device(user: &str, device: &str) -> Device {
    Device::new(Identity {
        server_id: "synthetic-community".into(),
        user_id: user.into(),
        device_id: device.into(),
    })
    .unwrap()
}
fn durable<T>(device: &mut Device, op: impl FnOnce(&mut Device) -> Result<T>) -> T {
    device.transact(op, |_| Ok(())).unwrap()
}
fn package(device: &mut Device) -> PublicDevice {
    durable(device, |d| d.key_package())
}
/// The same device ID presenting another signing key, as a server could list it.
fn substitute(public: &PublicDevice) -> PublicDevice {
    let mut other = Device::new(public.identity.clone()).unwrap();
    package(&mut other)
}

#[test]
fn a_first_contact_is_trusted_and_both_sides_exchange_messages_without_a_comparison() {
    let mut alice = device("alice", "alice-pc");
    let mut bob = device("bob", "bob-pc");
    let a = package(&mut alice);
    let b = package(&mut bob);
    assert_eq!(alice.first_use_decision(&b), FirstUse::Trusted);
    assert_eq!(
        durable(&mut alice, |d| d.trust_on_first_use(&b)),
        FirstUse::Trusted
    );
    assert_eq!(
        durable(&mut bob, |d| d.trust_on_first_use(&a)),
        FirstUse::Trusted
    );
    assert_eq!(alice.trust(&b), Some(Trust::FirstUse));
    assert!(!alice.is_verified(&b), "first use is trusted, not verified");
    assert_eq!(
        durable(&mut alice, |d| d.trust_on_first_use(&b)),
        FirstUse::Known
    );

    durable(&mut alice, |d| d.create_group("dm:bob", "fixture-dm"));
    let change = durable(&mut alice, |d| {
        d.add_members("dm:bob", std::slice::from_ref(&b))
    });
    durable(&mut bob, |d| {
        d.join(
            "dm:bob",
            "fixture-dm",
            a.identity.clone(),
            change.welcome.as_ref().unwrap(),
        )
    });
    let wire = durable(&mut alice, |d| {
        d.encrypt("dm:bob", b"hi, no codes compared")
    });
    let Received::Application { author, bytes } = durable(&mut bob, |d| d.receive("dm:bob", &wire))
    else {
        panic!("expected a chat message");
    };
    assert_eq!(author, a.identity);
    assert_eq!(bytes, b"hi, no codes compared");

    // Comparing codes later only adds the Verified mark; removing it keeps trust.
    durable(&mut alice, |d| d.approve(&b, &b.fingerprint));
    assert_eq!(alice.trust(&b), Some(Trust::Verified));
    durable(&mut alice, |d| d.unverify(&b.identity.device_id));
    assert_eq!(alice.trust(&b), Some(Trust::FirstUse));
    durable(&mut alice, |d| d.encrypt("dm:bob", b"still works"));
}

#[test]
fn a_new_device_is_trusted_unless_the_person_was_verified_by_hand() {
    let mut alice = device("alice", "alice-pc");
    let mut bob_pc = device("bob", "bob-pc");
    let mut bob_phone = device("bob", "bob-phone");
    let mut bob_tablet = device("bob", "bob-tablet");
    let pc = package(&mut bob_pc);
    let phone = package(&mut bob_phone);
    let tablet = package(&mut bob_tablet);
    durable(&mut alice, |d| d.trust_on_first_use(&pc));
    // Nobody verified Bob: his new phone is a first contact too.
    assert_eq!(
        durable(&mut alice, |d| d.trust_on_first_use(&phone)),
        FirstUse::Trusted
    );
    // After a comparison, a further new device of Bob waits for Alice.
    durable(&mut alice, |d| d.approve(&pc, &pc.fingerprint));
    assert!(alice.person_verified("bob"));
    assert_eq!(
        durable(&mut alice, |d| d.trust_on_first_use(&tablet)),
        FirstUse::Held
    );
    assert!(alice.trust(&tablet).is_none());
    durable(&mut alice, |d| d.create_group("dm:bob", "fixture-dm"));
    assert!(
        alice
            .transact(
                |d| d.add_members("dm:bob", std::slice::from_ref(&tablet)),
                |_| Ok(())
            )
            .is_err(),
        "a held device never receives keys"
    );
    // One click accepts it (trusted, not verified); comparing makes it verified.
    durable(&mut alice, |d| d.accept_new_key(&tablet, None));
    assert_eq!(alice.trust(&tablet), Some(Trust::FirstUse));
    durable(&mut alice, |d| {
        d.add_members("dm:bob", std::slice::from_ref(&tablet))
    });
}

#[test]
fn a_changed_key_replaces_silently_trusted_pins_but_waits_for_verified_people() {
    let mut alice = device("alice", "alice-pc");
    let mut bob = device("bob", "bob-pc");
    let mut carol = device("carol", "carol-pc");
    let b = package(&mut bob);
    let c = package(&mut carol);
    durable(&mut alice, |d| d.trust_on_first_use(&b));
    durable(&mut alice, |d| d.trust_on_first_use(&c));
    durable(&mut alice, |d| d.approve(&c, &c.fingerprint));

    // Unverified Bob: the new key is trusted and reported as a key change.
    let new_b = substitute(&b);
    assert_eq!(
        durable(&mut alice, |d| d.trust_on_first_use(&new_b)),
        FirstUse::KeyReplaced
    );
    assert_eq!(alice.trust(&new_b), Some(Trust::FirstUse));
    assert!(
        alice.trust(&b).is_none(),
        "the old key is no longer trusted"
    );

    // Verified Carol: the old key stays pinned until Alice decides.
    let new_c = substitute(&c);
    assert_eq!(
        durable(&mut alice, |d| d.trust_on_first_use(&new_c)),
        FirstUse::Held
    );
    assert_eq!(alice.trust(&c), Some(Trust::Verified));
    assert!(alice.trust(&new_c).is_none());
    assert!(
        alice
            .transact(|d| d.approve(&new_c, &new_c.fingerprint), |_| Ok(()))
            .is_err(),
        "a plain approval cannot overwrite a verified key"
    );
    assert!(
        alice
            .transact(
                |d| d.accept_new_key(&new_c, Some(&"0".repeat(64))),
                |_| Ok(())
            )
            .is_err(),
        "a wrong comparison changes nothing"
    );
    durable(&mut alice, |d| {
        d.accept_new_key(&new_c, Some(&new_c.fingerprint))
    });
    assert_eq!(alice.trust(&new_c), Some(Trust::Verified));
}

#[test]
fn the_key_manager_removes_a_member_whose_trusted_key_was_replaced() {
    let mut alice = device("alice", "alice-pc");
    let mut bob = device("bob", "bob-pc");
    let a = package(&mut alice);
    let b = package(&mut bob);
    durable(&mut alice, |d| d.trust_on_first_use(&b));
    durable(&mut bob, |d| d.trust_on_first_use(&a));
    durable(&mut alice, |d| {
        d.create_group("channel:one", "fixture-group")
    });
    durable(&mut alice, |d| {
        d.add_members("channel:one", std::slice::from_ref(&b))
    });
    assert!(alice.untrusted_members("channel:one").unwrap().is_empty());
    let new_b = substitute(&b);
    durable(&mut alice, |d| d.trust_on_first_use(&new_b));
    assert_eq!(
        alice.untrusted_members("channel:one").unwrap(),
        vec!["bob-pc".to_string()]
    );
    assert!(
        alice
            .transact(|d| d.encrypt("channel:one", b"x"), |_| Ok(()))
            .is_err(),
        "nothing is sent to the old key"
    );
    durable(&mut alice, |d| {
        d.remove_members("channel:one", &["bob-pc".into()])
    });
    durable(&mut alice, |d| {
        d.add_members("channel:one", std::slice::from_ref(&new_b))
    });
    durable(&mut alice, |d| d.encrypt("channel:one", b"to the new key"));
}

#[test]
fn a_listing_without_a_package_is_adopted_only_with_its_exact_fingerprint() {
    // Carol manages the group and adds Bob; Alice joins later and sees Bob
    // listed without a spare KeyPackage.
    let mut carol = device("carol", "carol-pc");
    let mut bob = device("bob", "bob-pc");
    let mut alice = device("alice", "alice-pc");
    let c = package(&mut carol);
    let b = package(&mut bob);
    let a = package(&mut alice);
    durable(&mut carol, |d| d.trust_on_first_use(&b));
    durable(&mut carol, |d| d.trust_on_first_use(&a));
    durable(&mut carol, |d| {
        d.create_group("channel:one", "fixture-group")
    });
    durable(&mut carol, |d| {
        d.add_members("channel:one", std::slice::from_ref(&b))
    });
    let welcome = durable(&mut carol, |d| {
        d.add_members("channel:one", std::slice::from_ref(&a))
    })
    .welcome
    .unwrap();
    durable(&mut alice, |d| d.trust_on_first_use(&c));

    // A server listing Bob with someone else's fingerprint cannot get its key
    // trusted through the group update.
    let mut forged = b.clone();
    forged.key_package.clear();
    forged.fingerprint = substitute(&b).fingerprint;
    let mut fooled = Device::restore(&alice.checkpoint().unwrap(), &a.identity).unwrap();
    assert_eq!(
        durable(&mut fooled, |d| d.trust_on_first_use(&forged)),
        FirstUse::Deferred
    );
    assert!(fooled
        .transact(
            |d| d.join("channel:one", "fixture-group", c.identity.clone(), &welcome),
            |_| Ok(())
        )
        .is_err());
    assert!(fooled.expected_fingerprint("bob-pc").is_none());

    let mut listed = b.clone();
    listed.key_package.clear();
    assert_eq!(
        durable(&mut alice, |d| d.trust_on_first_use(&listed)),
        FirstUse::Deferred
    );
    assert!(
        alice.expected_fingerprint("bob-pc").is_none(),
        "nothing pinned from the listing alone"
    );
    durable(&mut alice, |d| {
        d.join("channel:one", "fixture-group", c.identity.clone(), &welcome)
    });
    assert_eq!(alice.trust(&b), Some(Trust::FirstUse));
    // Candidates are never saved: a restart forgets an unused one.
    let restored = Device::restore(&alice.checkpoint().unwrap(), &a.identity).unwrap();
    assert!(restored.first_use_candidates.is_empty());
}

#[test]
fn a_new_own_device_is_always_trusted_and_this_device_cannot_be_replaced() {
    let mut pc = device("alice", "alice-pc");
    let mut laptop = device("alice", "alice-laptop");
    let mut phone = device("alice", "alice-phone");
    let l = package(&mut laptop);
    let p = package(&mut phone);
    durable(&mut pc, |d| d.trust_on_first_use(&l));
    durable(&mut pc, |d| d.approve(&l, &l.fingerprint));
    // Even after verifying one own device, another own device never gets stuck.
    assert!(!pc.person_verified("alice"));
    assert_eq!(
        durable(&mut pc, |d| d.trust_on_first_use(&p)),
        FirstUse::Trusted
    );
    let own = package(&mut pc);
    let forged_self = substitute(&own);
    assert_eq!(pc.first_use_decision(&forged_self), FirstUse::Known);
    assert!(pc
        .transact(|d| d.accept_new_key(&forged_self, None), |_| Ok(()))
        .is_err());
    assert!(pc.transact(|d| d.unverify("alice-pc"), |_| Ok(())).is_err());
}

#[test]
fn older_snapshots_keep_their_pins_verified_and_unused_first_use_pins_can_be_forgotten() {
    let mut alice = device("alice", "alice-pc");
    let mut bob = device("bob", "bob-pc");
    let mut carol = device("carol", "carol-pc");
    let b = package(&mut bob);
    let c = package(&mut carol);
    durable(&mut alice, |d| d.approve(&b, &b.fingerprint));
    durable(&mut alice, |d| d.trust_on_first_use(&c));
    let snapshot = alice.checkpoint().unwrap();
    let text = std::str::from_utf8(&snapshot).unwrap();
    assert_eq!(
        text.matches("\"first_use\":true").count(),
        1,
        "verified pins keep the old format"
    );
    let restored = Device::restore(&snapshot, alice.identity()).unwrap();
    assert_eq!(restored.trust(&b), Some(Trust::Verified));
    assert_eq!(restored.trust(&c), Some(Trust::FirstUse));
    let forgotten = durable(&mut alice, |d| d.forget_unused_first_use(&HashSet::new()));
    assert_eq!(forgotten, 1);
    assert!(alice.trust(&c).is_none());
    assert_eq!(
        alice.trust(&b),
        Some(Trust::Verified),
        "verified pins are never forgotten"
    );
}

#[test]
fn a_key_managers_addition_of_an_unlisted_device_is_followed_only_for_unverified_people() {
    // Carol manages the group. Devices she adds may already be revoked and
    // unlisted by the time Alice catches up.
    let mut carol = device("carol", "carol-pc");
    let mut alice = device("alice", "alice-pc");
    let c = package(&mut carol);
    let a = package(&mut alice);
    let bob = package(&mut device("bob", "bob-pc"));
    let dave_pc = package(&mut device("dave", "dave-pc"));
    let dave_phone = package(&mut device("dave", "dave-phone"));
    for public in [&a, &bob, &dave_pc, &dave_phone] {
        durable(&mut carol, |d| d.trust_on_first_use(public));
    }
    durable(&mut carol, |d| d.create_group("channel:one", "fixture-group"));
    let welcome = durable(&mut carol, |d| {
        d.add_members("channel:one", std::slice::from_ref(&a))
    })
    .welcome
    .unwrap();
    durable(&mut alice, |d| d.trust_on_first_use(&c));
    durable(&mut alice, |d| d.trust_on_first_use(&dave_pc));
    durable(&mut alice, |d| d.approve(&dave_pc, &dave_pc.fingerprint));
    durable(&mut alice, |d| {
        d.join("channel:one", "fixture-group", c.identity.clone(), &welcome)
    });

    // Bob was never listed for Alice and nobody verified him: followed.
    let add_bob = durable(&mut carol, |d| {
        d.add_members("channel:one", std::slice::from_ref(&bob))
    });
    durable(&mut alice, |d| d.receive("channel:one", &add_bob.commit));
    assert_eq!(alice.trust(&bob), Some(Trust::FirstUse));

    // Alice verified Dave: his unlisted new phone is refused.
    let add_phone = durable(&mut carol, |d| {
        d.add_members("channel:one", std::slice::from_ref(&dave_phone))
    });
    assert!(alice
        .transact(|d| d.receive("channel:one", &add_phone.commit), |_| Ok(()))
        .is_err());
    assert!(alice.expected_fingerprint("dave-phone").is_none());
    // Only her decision lets the update in.
    durable(&mut alice, |d| d.accept_new_key(&dave_phone, None));
    durable(&mut alice, |d| d.receive("channel:one", &add_phone.commit));
}
