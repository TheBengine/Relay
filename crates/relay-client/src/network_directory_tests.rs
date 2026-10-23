//! Bounded untrusted directory-cache regressions; synthetic actors only.
use super::*;

struct DirectoryFixture {
    crypto: CryptoSession,
}
impl DirectoryFixture {
    fn new() -> Self {
        let root = std::env::var_os("RELAY_TEST_WORK_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("relay-tests"))
            .join(format!("bounded-directory-{}", uuid::Uuid::new_v4()));
        Self {
            crypto: CryptoSession::open_for_test(
                root,
                &uuid::Uuid::new_v4().to_string(),
                "directory-owner",
            )
            .unwrap(),
        }
    }
    fn peer(&self, user: &str, device_id: &str) -> PublicDevice {
        let mut device = relay_crypto::Device::new(Identity {
            server_id: self.crypto.device.identity().server_id.clone(),
            user_id: user.into(),
            device_id: device_id.into(),
        })
        .unwrap();
        device.key_package().unwrap()
    }
}
impl Drop for DirectoryFixture {
    fn drop(&mut self) {
        self.crypto.remove_test_checkpoint();
    }
}
fn record(public: &PublicDevice) -> CryptoDevice {
    CryptoDevice {
        user_id: public.identity.user_id.clone(),
        device_id: public.identity.device_id.clone(),
        identity: public.fingerprint.clone(),
        key_package: STANDARD.encode(&public.key_package),
        invite_attestation: None,
    }
}
fn untrusted_record(user: &str, id: &str) -> CryptoDevice {
    CryptoDevice {
        user_id: user.into(),
        device_id: id.into(),
        identity: "0".repeat(64),
        key_package: STANDARD.encode([1u8; 16]),
        invite_attestation: None,
    }
}
type DirectorySnapshot = (Vec<u8>, Vec<(String, String, bool, bool)>);
fn snapshot(crypto: &CryptoSession) -> DirectorySnapshot {
    let mut own: Vec<_> = crypto
        .status
        .own_devices
        .iter()
        .map(|d| {
            (
                d.device_id.clone(),
                d.fingerprint.clone(),
                d.verified,
                d.changed,
            )
        })
        .collect();
    own.sort();
    (serde_json::to_vec(&crypto.directory).unwrap(), own)
}

#[test]
fn repeated_full_directory_churn_is_bounded_retains_pins_and_tracks_changed_keys() {
    let mut fixture = DirectoryFixture::new();
    let own = record(&fixture.crypto.advertised());
    let first = fixture.peer("verified-first", "verified-first-device");
    let omitted = fixture.peer("verified-omitted", "verified-omitted-device");
    update_directory_cache(
        &mut fixture.crypto,
        &[own.clone(), record(&first), record(&omitted)],
        &[],
        false,
    )
    .unwrap();
    fixture
        .crypto
        .approve(&first.identity.device_id, &first.fingerprint)
        .unwrap();
    fixture
        .crypto
        .approve(&omitted.identity.device_id, &omitted.fingerprint)
        .unwrap();
    for round in 0..8 {
        let mut page = vec![own.clone(), record(&first)];
        for index in 0..100 {
            page.push(untrusted_record(
                &own.user_id,
                &format!("own-churn-{round}-{index}"),
            ));
        }
        for index in 0..154 {
            page.push(untrusted_record(
                "unverified-peer",
                &format!("peer-churn-{round}-{index}"),
            ));
        }
        assert_eq!(page.len(), MAX_DIRECTORY_DEVICES);
        update_directory_cache(&mut fixture.crypto, &page, &[], false).unwrap();
        assert_eq!(fixture.crypto.directory.len(), MAX_DIRECTORY_DEVICES);
        assert!(fixture.crypto.status.own_devices.len() <= MAX_DIRECTORY_DEVICES);
        assert!(fixture.crypto.status.peers.len() <= MAX_DIRECTORY_DEVICES);
        for public in [&first, &omitted] {
            let cached = &fixture.crypto.directory[&public.identity.device_id];
            assert_eq!(cached.fingerprint, public.fingerprint);
            assert!(fixture.crypto.device.is_approved(cached));
        }
        let active = fixture
            .crypto
            .status
            .own_devices
            .iter()
            .find(|d| d.device_id == own.device_id)
            .unwrap();
        assert!(active.verified && !active.changed);
        assert!(fixture
            .crypto
            .status
            .own_devices
            .iter()
            .any(|d| d.device_id == format!("own-churn-{round}-99")));
    }
    let changed = fixture.peer(&first.identity.user_id, &first.identity.device_id);
    assert_ne!(changed.fingerprint, first.fingerprint);
    update_directory_cache(
        &mut fixture.crypto,
        &[own.clone(), record(&changed)],
        &[],
        false,
    )
    .unwrap();
    let cached = &fixture.crypto.directory[&first.identity.device_id];
    assert_eq!(cached.fingerprint, changed.fingerprint);
    assert!(!fixture.crypto.device.is_approved(cached));
    assert_eq!(
        fixture
            .crypto
            .device
            .expected_fingerprint(&first.identity.device_id),
        Some(first.fingerprint.as_str())
    );
    let status = fixture
        .crypto
        .status
        .peers
        .iter()
        .find(|d| d.device_id == first.identity.device_id)
        .unwrap();
    assert!(status.changed && !status.verified);
    assert!(
        fixture
            .crypto
            .approve(&first.identity.device_id, &first.fingerprint)
            .is_err(),
        "old compared text cannot approve a newly substituted signature identity"
    );

    update_directory_cache(&mut fixture.crypto, std::slice::from_ref(&own), &[], true).unwrap();
    assert_eq!(
        fixture.crypto.status.own_devices.len(),
        1,
        "an authoritative own-device refresh must drop omitted stale spare rows"
    );
    assert_eq!(
        fixture.crypto.status.own_devices[0].device_id,
        own.device_id
    );
    assert_eq!(
        fixture.crypto.directory[&first.identity.device_id].fingerprint,
        changed.fingerprint
    );
}

#[test]
fn invalid_directory_pages_preserve_both_caches_and_never_replace_the_active_identity() {
    let mut fixture = DirectoryFixture::new();
    let own = record(&fixture.crypto.advertised());
    let peer = fixture.peer("verified-peer", "verified-peer-device");
    update_directory_cache(
        &mut fixture.crypto,
        &[own.clone(), record(&peer)],
        &[],
        false,
    )
    .unwrap();
    fixture
        .crypto
        .approve(&peer.identity.device_id, &peer.fingerprint)
        .unwrap();
    let before = snapshot(&fixture.crypto);
    let mut bad_entries = Vec::new();
    let mut bad = untrusted_record("", "empty-user");
    bad_entries.push(bad.clone());
    bad.user_id = "x".repeat(129);
    bad_entries.push(bad.clone());
    bad.user_id = "peer\ncontrol".into();
    bad_entries.push(bad.clone());
    bad.user_id = "peer".into();
    bad.device_id = "".into();
    bad_entries.push(bad.clone());
    bad.device_id = "x".repeat(129);
    bad_entries.push(bad.clone());
    bad.device_id = "peer-device".into();
    bad.identity = "z".repeat(64);
    bad_entries.push(bad.clone());
    bad.identity = "0".repeat(64);
    bad.key_package = "%%%".into();
    bad_entries.push(bad.clone());
    bad.key_package = STANDARD.encode(vec![0u8; 16 * 1024 + 1]);
    bad_entries.push(bad);
    for bad in bad_entries {
        assert!(
            update_directory_cache(&mut fixture.crypto, &[own.clone(), bad], &[], true).is_err()
        );
        assert_eq!(snapshot(&fixture.crypto), before);
    }
    assert!(
        update_directory_cache(&mut fixture.crypto, &[own.clone(), own.clone()], &[], true)
            .is_err()
    );
    let mut foreign = own.clone();
    foreign.user_id = "another-account".into();
    assert!(update_directory_cache(&mut fixture.crypto, &[foreign], &[], true).is_err());
    let oversized: Vec<_> = (0..257)
        .map(|i| untrusted_record("peer", &format!("large-{i}")))
        .collect();
    assert!(update_directory_cache(&mut fixture.crypto, &oversized, &[], true).is_err());
    assert_eq!(snapshot(&fixture.crypto), before);
    assert_eq!(fixture.crypto.advertised().fingerprint, own.identity);
}

#[test]
fn a_full_pinned_cache_rejects_new_entries_instead_of_discarding_trust() {
    let current: HashMap<_, _> = (0..256)
        .map(|index| {
            let id = format!("pinned-{index}");
            (
                id.clone(),
                PublicDevice {
                    identity: Identity {
                        server_id: "owned-algorithm-fixture".into(),
                        user_id: "peer".into(),
                        device_id: id,
                    },
                    fingerprint: "1".repeat(64),
                    key_package: Vec::new(),
                },
            )
        })
        .collect();
    let mut incoming = HashMap::new();
    let mut new_device = current["pinned-0"].clone();
    new_device.identity.device_id = "new-device".into();
    incoming.insert("new-device".into(), new_device);
    assert!(merge_directory_cache(&current, &incoming, |_| true).is_err());
    assert_eq!(current.len(), 256);
    assert!(current.contains_key("pinned-0"));
}
