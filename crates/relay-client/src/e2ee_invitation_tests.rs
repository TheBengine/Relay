//! R3.2: invitation-based device verification with real encryption sessions.
//! Synthetic actors only; no server, account, network or personal data.
use super::*;
use crate::invite_proof;
use relay_protocol::{CryptoDevice, InviteAttestation};

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
            .join(format!("invitation-{user}-{}", uuid::Uuid::new_v4()));
        let session = CryptoSession::open_for_test(root.clone(), server, user).unwrap();
        Self {
            root,
            server: server.into(),
            user: user.into(),
            inner: Some(session),
        }
    }
    /// Closes the actor and opens its protected state again, as after a restart.
    fn reopen(&mut self) {
        drop(self.inner.take());
        self.inner = Some(
            CryptoSession::open_for_test(self.root.clone(), &self.server, &self.user).unwrap(),
        );
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
        if let Some(session) = &self.inner {
            session.remove_test_checkpoint();
        }
    }
}

fn server() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn now() -> u64 {
    unix_now()
}

/// A friend's public device as the host's directory would list it.
fn friend_device(server: &str, user: &str, device: &str) -> PublicDevice {
    let mut friend = Device::new(Identity {
        server_id: server.into(),
        user_id: user.into(),
        device_id: device.into(),
    })
    .unwrap();
    friend.key_package().unwrap()
}

fn listing(public: &PublicDevice, proof: Option<(&str, String)>) -> CryptoDevice {
    CryptoDevice {
        user_id: public.identity.user_id.clone(),
        device_id: public.identity.device_id.clone(),
        identity: public.fingerprint.clone(),
        key_package: STANDARD.encode(&public.key_package),
        invite_attestation: proof.map(|(invite_id, mac)| InviteAttestation {
            invite_id: invite_id.into(),
            mac,
        }),
    }
}

fn proof_for(host: &CryptoSession, public: &PublicDevice, invite: &InviteProof) -> String {
    invite_proof::attest(
        invite.secret(),
        &host.device.identity().server_id,
        &invite.invite_id,
        &public.identity.user_id,
        &public.identity.device_id,
        &public.fingerprint,
    )
}

fn invite_id(seed: u8) -> String {
    format!("{seed:02x}").repeat(32)
}

#[test]
fn a_valid_proof_approves_the_friends_device_once_and_survives_a_restart() {
    let mut host = Actor::new(&server(), "host");
    let friend = friend_device(&host.server, "friend", "friend-device");
    host.directory
        .insert("friend-device".into(), friend.clone());
    let invite = host.issue_invitation(&invite_id(1), now() + 3600).unwrap();
    assert!(host.has_active_invitations());
    assert_eq!(invite.host_device_id, host.device.identity().device_id);
    assert_eq!(invite.host_fingerprint, host.status.own_fingerprint);
    // Before the friend's proof exists nothing is approved.
    let none = listing(&friend, None);
    assert!(host
        .apply_invitation_attestations(&[none])
        .unwrap()
        .is_empty());
    assert!(!host.device.is_approved(&friend));
    let mac = proof_for(&host, &friend, &invite);
    let approved = host
        .apply_invitation_attestations(&[listing(&friend, Some((&invite.invite_id, mac.clone())))])
        .unwrap();
    assert_eq!(approved, ["friend-device"]);
    assert!(host.device.is_approved(&friend));
    assert_eq!(host.status.via_invitation, ["friend-device"]);
    // The invitation is spent: it is no longer active and cannot approve another device.
    assert!(!host.has_active_invitations());
    let second = friend_device(&host.server, "friend", "second-device");
    host.directory
        .insert("second-device".into(), second.clone());
    let second_mac = proof_for(&host, &second, &invite);
    assert!(host
        .apply_invitation_attestations(&[listing(&second, Some((&invite.invite_id, second_mac)))])
        .unwrap()
        .is_empty());
    assert!(!host.device.is_approved(&second));
    // The approval and its "via invitation" mark are in the protected state.
    host.reopen();
    assert!(host.device.is_approved(&friend));
    assert_eq!(host.status.via_invitation, ["friend-device"]);
    assert!(!host.has_active_invitations());
    // Removing the verification also removes the mark; the key stays trusted.
    host.unapprove("friend-device").unwrap();
    assert!(!host.device.is_verified(&friend));
    assert!(host.device.is_approved(&friend), "still trusted, as on first use");
    assert!(host.status.via_invitation.is_empty());
}

#[test]
fn a_manual_comparison_upgrades_invitation_approval_only_for_the_correct_code() {
    let mut host = Actor::new(&server(), "host");
    let friend = friend_device(&host.server, "friend", "friend-device");
    host.directory
        .insert("friend-device".into(), friend.clone());
    let invite = host.issue_invitation(&invite_id(7), now() + 3600).unwrap();
    let mac = proof_for(&host, &friend, &invite);
    host.apply_invitation_attestations(&[listing(&friend, Some((&invite.invite_id, mac)))])
        .unwrap();
    // Real conversation contexts omit a fresh KeyPackage for an existing member.
    // Reconfirmation must keep the original signing-key pin, not need a package.
    host.directory
        .get_mut("friend-device")
        .unwrap()
        .key_package
        .clear();
    assert!(host.approve("friend-device", &"00".repeat(32)).is_err());
    assert_eq!(host.status.via_invitation, ["friend-device"]);
    host.approve("friend-device", &friend.fingerprint).unwrap();
    assert!(host.device.is_approved(&friend));
    assert!(host.status.via_invitation.is_empty());
    host.reopen();
    assert!(host.device.is_approved(&friend));
    assert!(host.status.via_invitation.is_empty());
    assert!(!host.has_active_invitations());
}

#[test]
fn a_wrong_unknown_or_expired_proof_never_approves() {
    let mut host = Actor::new(&server(), "host");
    let friend = friend_device(&host.server, "friend", "friend-device");
    host.directory
        .insert("friend-device".into(), friend.clone());
    let invite = host.issue_invitation(&invite_id(2), now() + 3600).unwrap();
    let good = proof_for(&host, &friend, &invite);
    let mut flipped = good.clone();
    flipped.replace_range(..1, if good.starts_with('0') { "1" } else { "0" });
    // A proof that does not verify leaves the device unapproved and flags it.
    let outcome = host
        .apply_invitation_attestations(&[listing(&friend, Some((&invite.invite_id, flipped)))])
        .unwrap();
    assert!(outcome.is_empty() && !host.device.is_approved(&friend));
    assert_eq!(host.status.invitation_unconfirmed, ["friend-device"]);
    assert!(
        host.has_active_invitations(),
        "a bad proof does not spend the invitation"
    );
    // A proof for an invitation this device never made is ignored, not flagged.
    let foreign = invite_proof::attest(
        invite.secret(),
        &host.server,
        &invite_id(9),
        "friend",
        "friend-device",
        &friend.fingerprint,
    );
    assert!(host
        .apply_invitation_attestations(&[listing(&friend, Some((&invite_id(9), foreign)))])
        .unwrap()
        .is_empty());
    assert!(host.status.invitation_unconfirmed.is_empty());
    assert!(!host.device.is_approved(&friend));
    // The right proof for the wrong account or device does not carry over.
    let other = friend_device(&host.server, "someone-else", "friend-device");
    host.directory.insert("friend-device".into(), other.clone());
    assert!(host
        .apply_invitation_attestations(&[listing(&other, Some((&invite.invite_id, good.clone())))])
        .unwrap()
        .is_empty());
    assert!(!host.device.is_approved(&other));
    // An expired invitation approves nothing, even with a correct proof.
    host.directory
        .insert("friend-device".into(), friend.clone());
    let expired = host.issue_invitation(&invite_id(3), 1).unwrap();
    let expired_mac = proof_for(&host, &friend, &expired);
    assert_eq!(
        host.apply_invitation_attestations(&[listing(
            &friend,
            Some((&expired.invite_id, expired_mac))
        )])
        .unwrap(),
        Vec::<String>::new()
    );
    assert!(!host.device.is_approved(&friend));
}

#[test]
fn a_changed_or_already_pinned_device_is_never_auto_approved() {
    let mut host = Actor::new(&server(), "host");
    let original = friend_device(&host.server, "friend", "friend-device");
    host.directory
        .insert("friend-device".into(), original.clone());
    host.approve("friend-device", &original.fingerprint)
        .unwrap();
    // The same device ID now shows a different identity (a replacement or an attack).
    let replacement = friend_device(&host.server, "friend", "friend-device");
    assert_ne!(original.fingerprint, replacement.fingerprint);
    host.directory
        .insert("friend-device".into(), replacement.clone());
    let invite = host.issue_invitation(&invite_id(4), now() + 3600).unwrap();
    let mac = proof_for(&host, &replacement, &invite);
    assert!(host
        .apply_invitation_attestations(&[listing(&replacement, Some((&invite.invite_id, mac)))])
        .unwrap()
        .is_empty());
    assert!(!host.device.is_approved(&replacement));
    assert!(host.device.expected_fingerprint("friend-device").is_some());
    assert!(host.status.via_invitation.is_empty());
    // The host's own account is never approved this way.
    let own = friend_device(&host.server, "host", "host-second-device");
    host.directory
        .insert("host-second-device".into(), own.clone());
    let own_mac = proof_for(&host, &own, &invite);
    assert!(host
        .apply_invitation_attestations(&[listing(&own, Some((&invite.invite_id, own_mac)))])
        .unwrap()
        .is_empty());
    assert!(!host.device.is_approved(&own));
}

#[test]
fn the_friend_pins_only_the_exact_host_device_the_link_named() {
    let server = server();
    let mut host = Actor::new(&server, "host");
    let mut friend = Actor::new(&server, "friend");
    let host_public = host.advertised();
    let invite = host.issue_invitation(&invite_id(5), now() + 3600).unwrap();
    // Not in the directory yet: nothing happens.
    assert!(!friend.approve_invited_host(&invite).unwrap());
    friend
        .directory
        .insert(host_public.identity.device_id.clone(), host_public.clone());
    // A link naming a different fingerprint, or another device, pins nothing.
    let wrong_fingerprint = InviteProof::new(
        &invite.invite_id,
        &invite.host_device_id,
        &"ab".repeat(32),
        *invite.secret(),
    )
    .unwrap();
    assert!(!friend.approve_invited_host(&wrong_fingerprint).unwrap());
    let wrong_device = InviteProof::new(
        &invite.invite_id,
        "another-device",
        &invite.host_fingerprint,
        *invite.secret(),
    )
    .unwrap();
    assert!(!friend.approve_invited_host(&wrong_device).unwrap());
    assert!(!friend.device.is_approved(&host_public));
    // The exact device and fingerprint from the link are approved, once.
    assert!(friend.approve_invited_host(&invite).unwrap());
    assert!(friend.device.is_approved(&host_public));
    assert_eq!(
        friend.status.via_invitation,
        std::slice::from_ref(&host_public.identity.device_id)
    );
    assert!(!friend.approve_invited_host(&invite).unwrap());
}

#[test]
fn too_many_open_invitations_fall_back_to_ordinary_ones_and_spent_ones_are_dropped() {
    let mut host = Actor::new(&server(), "host");
    for seed in 0..MAX_ACTIVE_INVITATIONS as u8 {
        host.issue_invitation(&invite_id(seed), now() + 3600)
            .unwrap();
    }
    assert!(host
        .issue_invitation(&invite_id(200), now() + 3600)
        .is_err());
    // Expired ones make room again.
    let mut roomy = Actor::new(&server(), "roomy");
    for seed in 0..MAX_ACTIVE_INVITATIONS as u8 {
        roomy.issue_invitation(&invite_id(seed), 1).unwrap();
    }
    roomy
        .issue_invitation(&invite_id(201), now() + 3600)
        .unwrap();
    assert!(roomy.has_active_invitations());
    assert_eq!(roomy.state.invitations.len(), 1);
}

#[test]
fn pending_host_pin_finishes_once_when_the_exact_directory_entry_arrives() {
    let server = server();
    let mut host = Actor::new(&server, "host");
    let mut friend = Actor::new(&server, "friend");
    let public = host.advertised();
    let proof = host.issue_invitation(&invite_id(11), now() + 3600).unwrap();
    let start = std::time::Instant::now();
    friend.remember_invited_host(&proof, start);
    assert!(friend.has_pending_invited_host(start));
    assert!(!friend
        .retry_invited_host(start + std::time::Duration::from_secs(5))
        .unwrap());
    assert!(!friend.device.is_approved(&public));
    friend
        .directory
        .insert(public.identity.device_id.clone(), public.clone());
    assert!(friend
        .retry_invited_host(start + std::time::Duration::from_secs(6))
        .unwrap());
    assert!(friend.device.is_approved(&public));
    assert_eq!(
        friend.status.via_invitation,
        std::slice::from_ref(&public.identity.device_id)
    );
    assert!(!friend.has_pending_invited_host(start));
    assert!(!friend.retry_invited_host(start).unwrap());
}

#[test]
fn pending_host_pin_has_one_deadline_and_is_not_restored_with_the_actor() {
    let server = server();
    let mut host = Actor::new(&server, "host");
    let mut friend = Actor::new(&server, "friend");
    let public = host.advertised();
    let proof = host.issue_invitation(&invite_id(12), now() + 3600).unwrap();
    let start = std::time::Instant::now();
    friend.remember_invited_host(&proof, start);
    assert!(friend.has_pending_invited_host(start + std::time::Duration::from_secs(29)));
    friend
        .directory
        .insert(public.identity.device_id.clone(), public.clone());
    assert!(!friend
        .retry_invited_host(start + std::time::Duration::from_secs(30))
        .unwrap());
    assert!(!friend
        .retry_invited_host(start + std::time::Duration::from_secs(31))
        .unwrap());
    assert!(!friend.device.is_approved(&public));
    friend.remember_invited_host(&proof, start);
    friend.reopen();
    friend
        .directory
        .insert(public.identity.device_id.clone(), public.clone());
    assert!(!friend.has_pending_invited_host(start));
    assert!(!friend.retry_invited_host(start).unwrap());
    assert!(!friend.device.is_approved(&public));
}

#[test]
fn pending_host_pin_never_waits_past_a_mismatch_or_overrides_manual_verification() {
    let server = server();
    let mut host = Actor::new(&server, "host");
    let mut friend = Actor::new(&server, "friend");
    let public = host.advertised();
    let proof = host.issue_invitation(&invite_id(13), now() + 3600).unwrap();
    let start = std::time::Instant::now();
    friend.remember_invited_host(&proof, start);
    let replacement = friend_device(&server, "host", &public.identity.device_id);
    assert_ne!(replacement.fingerprint, public.fingerprint);
    friend
        .directory
        .insert(public.identity.device_id.clone(), replacement.clone());
    assert!(!friend.retry_invited_host(start).unwrap());
    assert!(!friend.has_pending_invited_host(start));
    friend
        .directory
        .insert(public.identity.device_id.clone(), public.clone());
    assert!(!friend.retry_invited_host(start).unwrap());
    assert!(!friend.device.is_approved(&public));
    friend.remember_invited_host(&proof, start);
    friend
        .approve(&public.identity.device_id, &public.fingerprint)
        .unwrap();
    assert!(!friend.has_pending_invited_host(start));
    assert!(
        friend.status.via_invitation.is_empty(),
        "manual approval remains manual"
    );
    friend.unapprove(&public.identity.device_id).unwrap();
    assert!(!friend.retry_invited_host(start).unwrap());
    assert!(!friend.device.is_verified(&public));
    friend
        .approve(&public.identity.device_id, &public.fingerprint)
        .unwrap();
    friend.remember_invited_host(&proof, start);
    assert!(
        !friend.has_pending_invited_host(start),
        "an existing pin is never replaced"
    );
}
