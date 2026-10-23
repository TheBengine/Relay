use super::*;
use async_trait::async_trait;
use aws_lc_rs::{
    rand::SystemRandom,
    signature::{Ed25519KeyPair, KeyPair},
};
use serde_json::{json, Value};
use tough::{TransportError, TransportErrorKind, TransportStream};

#[path = "storage_tests.rs"]
mod storage_recovery;

fn canonical(value: &Value) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut writer =
        serde_json::Serializer::with_formatter(&mut bytes, olpc_cjson::CanonicalFormatter::new());
    value.serialize(&mut writer).unwrap();
    bytes
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
struct SigningKey {
    pair: Ed25519KeyPair,
    id: String,
    key: Value,
}
impl SigningKey {
    fn new() -> Self {
        let rng = SystemRandom::new();
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        let key = json!({"keytype":"ed25519","scheme":"ed25519","keyval":{"public":hex(pair.public_key().as_ref())}});
        let id = digest(&canonical(&key));
        Self { pair, id, key }
    }
}
fn signed(value: Value, keys: &[&SigningKey]) -> Vec<u8> {
    let bytes = canonical(&value);
    let signatures: Vec<Value> = keys
        .iter()
        .map(|k| json!({"keyid":k.id,"sig":hex(k.pair.sign(&bytes).as_ref())}))
        .collect();
    serde_json::to_vec(&json!({"signed":value,"signatures":signatures})).unwrap()
}
struct Keys {
    roots: [SigningKey; 2],
    online: SigningKey,
}
impl Keys {
    fn new() -> Self {
        Self {
            roots: [SigningKey::new(), SigningKey::new()],
            online: SigningKey::new(),
        }
    }
    fn root(&self, version: u64, previous: Option<&Self>) -> Vec<u8> {
        let mut map = serde_json::Map::new();
        for key in [&self.roots[0], &self.roots[1], &self.online] {
            map.insert(key.id.clone(), key.key.clone());
        }
        let role = json!({"keyids":[self.online.id],"threshold":1});
        let body = json!({"_type":"root","spec_version":"1.0.0","version":version,"expires":expiration(365),"consistent_snapshot":false,"keys":map,
            "roles":{"root":{"keyids":[self.roots[0].id,self.roots[1].id],"threshold":2},"targets":role,"snapshot":role,"timestamp":role}});
        let mut signers = vec![&self.roots[0], &self.roots[1]];
        if let Some(old) = previous {
            signers.extend([&old.roots[0], &old.roots[1]]);
        }
        signed(body, &signers)
    }
}
fn expiration(days: i64) -> String {
    (chrono::Utc::now() + chrono::Duration::days(days))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}
fn description(bytes: &[u8], version: u64) -> Value {
    json!({"version":version,"length":bytes.len(),"hashes":{"sha256":digest(bytes)}})
}

#[derive(Clone, Debug)]
struct Memory {
    objects: Arc<Mutex<BTreeMap<String, Vec<u8>>>>,
    broken: Option<String>,
    requested: Arc<Mutex<Vec<String>>>,
}
#[async_trait]
impl Transport for Memory {
    async fn fetch(&self, url: url::Url) -> std::result::Result<TransportStream, TransportError> {
        self.requested.lock().unwrap().push(url.path().into());
        let data = self
            .objects
            .lock()
            .unwrap()
            .get(url.path())
            .cloned()
            .ok_or_else(|| TransportError::new(TransportErrorKind::FileNotFound, &url))?;
        if self.broken.as_deref() == Some(url.path()) {
            return Ok(Box::pin(futures_util::stream::iter(vec![
                Ok(data[..data.len() / 2].to_vec().into()),
                Err(TransportError::new(TransportErrorKind::Other, &url)),
            ])));
        }
        Ok(Box::pin(futures_util::stream::iter(
            data.chunks(37)
                .map(|b| Ok(bytes::Bytes::copy_from_slice(b)))
                .collect::<Vec<_>>(),
        )))
    }
}
struct Fixture {
    keys: Keys,
    root: Vec<u8>,
    release: Release,
    installer: Vec<u8>,
    memory: Memory,
}
impl Fixture {
    fn new() -> Self {
        let keys = Keys::new();
        let root = keys.root(1, None);
        let installer = b"MZ--owned signed-target fixture, never executable--".to_vec();
        let release = Release {
            schema: 1,
            app_id: APP_ID.into(),
            platform: "windows-x64".into(),
            channel: "stable".into(),
            version: "0.10.1".into(),
            sequence: "20261010000000".into(),
            installer_target: "windows-x64/stable/Relay-0.10.1-20261010000000-Setup.exe".into(),
            installer_sha256: digest(&installer),
            installer_bytes: installer.len() as u64,
            notes: "A fixture release".into(),
            handoff: Some("wait-parent-v1".into()),
        };
        let memory = Memory {
            objects: Arc::new(Mutex::new(BTreeMap::new())),
            broken: None,
            requested: Arc::new(Mutex::new(Vec::new())),
        };
        let fixture = Self {
            keys,
            root,
            release,
            installer,
            memory,
        };
        fixture.publish(1);
        fixture
    }
    fn publish(&self, version: u64) {
        self.publish_with(version, &self.keys, None);
    }
    fn publish_with(&self, version: u64, keys: &Keys, expired: Option<&str>) {
        let bytes = serde_json::to_vec(&self.release).unwrap();
        let mut targets = serde_json::Map::new();
        targets.insert(
            "windows-x64/stable/release.json".into(),
            json!({"length":bytes.len(),"hashes":{"sha256":digest(&bytes)}}),
        );
        targets.insert(
            self.release.installer_target.clone(),
            json!({"length":self.installer.len(),"hashes":{"sha256":digest(&self.installer)}}),
        );
        let targets = signed(
            json!({"_type":"targets","spec_version":"1.0.0","version":version,"expires":if expired==Some("targets"){"2000-01-01T00:00:00Z".into()}else{expiration(7)},"targets":targets}),
            &[&keys.online],
        );
        let snapshot = signed(
            json!({"_type":"snapshot","spec_version":"1.0.0","version":version,"expires":expiration(1),"meta":{"targets.json":description(&targets,version)}}),
            &[&keys.online],
        );
        let timestamp = signed(
            json!({"_type":"timestamp","spec_version":"1.0.0","version":version,"expires":if expired==Some("timestamp"){"2000-01-01T00:00:00Z".into()}else{expiration(1)},"meta":{"snapshot.json":description(&snapshot,version)}}),
            &[&keys.online],
        );
        let mut map = self.memory.objects.lock().unwrap();
        map.insert("/metadata/targets.json".into(), targets);
        map.insert("/metadata/snapshot.json".into(), snapshot);
        map.insert("/metadata/timestamp.json".into(), timestamp);
        map.insert("/targets/windows-x64/stable/release.json".into(), bytes);
        map.insert(
            format!("/targets/{}", self.release.installer_target),
            self.installer.clone(),
        );
    }
    fn publisher(&self) -> Publisher {
        Publisher::from_bundled_root(
            self.root.clone(),
            "https://publisher.fixture.invalid/metadata/",
            "https://publisher.fixture.invalid/targets/",
            "stable",
        )
        .unwrap()
    }
    fn engine(&self, directory: &Path) -> Engine {
        Engine::new(
            self.publisher(),
            InstalledRelease {
                version: "0.10.0".into(),
                sequence: "20261009034055".into(),
            },
            directory.into(),
        )
        .unwrap()
    }
}
fn directory() -> tempfile::TempDir {
    let root = std::env::var_os("RELAY_TEST_WORK_ROOT")
        .map(PathBuf::from)
        .expect("Updater tests require an explicit owned work root.");
    storage::guard(&root).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    tempfile::Builder::new()
        .prefix("updater67-")
        .tempdir_in(root)
        .unwrap()
}

#[cfg(windows)]
pub(crate) fn handoff_fixture() -> (tempfile::TempDir, PreparedInstaller) {
    handoff_fixture_bytes(None)
}

#[cfg(windows)]
pub(crate) fn handoff_fixture_bytes(
    installer: Option<Vec<u8>>,
) -> (tempfile::TempDir, PreparedInstaller) {
    let root = directory();
    let mut fixture = Fixture::new();
    if let Some(bytes) = installer {
        fixture.installer = bytes;
        fixture.release.installer_bytes = fixture.installer.len() as u64;
        fixture.release.installer_sha256 = digest(&fixture.installer);
        fixture.publish(1);
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let ready = runtime
        .block_on(
            fixture
                .engine(root.path())
                .download_with(&fixture.release, fixture.memory.clone()),
        )
        .unwrap();
    (root, ready)
}

#[tokio::test]
async fn legacy_signed_release_stages_but_does_not_gain_execution_permission() {
    let root = directory();
    let mut fixture = Fixture::new();
    fixture.release.handoff = None;
    fixture.publish(1);
    let ready = fixture
        .engine(root.path())
        .download_with(&fixture.release, fixture.memory.clone())
        .await
        .unwrap();
    assert!(ready.release().handoff.is_none());
    ready.revalidate().unwrap();
}

#[tokio::test]
async fn unknown_signed_handoff_protocol_is_not_accepted() {
    let root = directory();
    let mut fixture = Fixture::new();
    fixture.release.handoff = Some("run-without-parent-consent".into());
    fixture.publish(1);
    assert!(fixture
        .engine(root.path())
        .check_with(fixture.memory.clone())
        .await
        .is_err());
    assert!(generations(root.path()) == 0);
}
fn generations(directory: &Path) -> usize {
    std::fs::read_dir(directory)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("metadata-"))
        .count()
}

#[tokio::test]
async fn valid_signatures_stage_complete_bytes_and_revalidate_before_handoff() {
    let fixture = Fixture::new();
    let dir = directory();
    let engine = fixture.engine(dir.path());
    assert_eq!(
        engine.check_with(fixture.memory.clone()).await.unwrap(),
        Check::Available(Box::new(fixture.release.clone()))
    );
    let ready = engine
        .download_with(&fixture.release, fixture.memory.clone())
        .await
        .unwrap();
    assert_eq!(std::fs::read(ready.path()).unwrap(), fixture.installer);
    ready.revalidate().unwrap();
    assert_eq!(generations(dir.path()), 1);
    std::fs::write(ready.path(), b"tampered executable").unwrap();
    assert!(ready.revalidate().unwrap_err().contains("changed"));
}
#[tokio::test]
async fn corrupt_signature_and_wrong_signer_do_not_commit_a_release() {
    for mode in ["modified", "wrong-key"] {
        let fixture = Fixture::new();
        let dir = directory();
        let engine = fixture.engine(dir.path());
        let mut value: Value = serde_json::from_slice(
            fixture
                .memory
                .objects
                .lock()
                .unwrap()
                .get("/metadata/timestamp.json")
                .unwrap(),
        )
        .unwrap();
        value["signed"]["version"] = json!(2);
        let bytes = if mode == "wrong-key" {
            signed(value["signed"].clone(), &[&SigningKey::new()])
        } else {
            serde_json::to_vec(&value).unwrap()
        };
        fixture
            .memory
            .objects
            .lock()
            .unwrap()
            .insert("/metadata/timestamp.json".into(), bytes);
        assert!(engine.check_with(fixture.memory.clone()).await.is_err());
        assert!(!dir.path().join("head.json").exists());
        assert_eq!(generations(dir.path()), 0);
    }
}
#[tokio::test]
async fn expired_timestamp_and_targets_are_refused() {
    for role in ["timestamp", "targets"] {
        let fixture = Fixture::new();
        fixture.publish_with(1, &fixture.keys, Some(role));
        let dir = directory();
        assert!(fixture
            .engine(dir.path())
            .check_with(fixture.memory.clone())
            .await
            .is_err());
        assert!(!dir.path().join("head.json").exists());
    }
}
#[tokio::test]
async fn persisted_metadata_rejects_rollback_and_preserves_the_previous_checkpoint() {
    let fixture = Fixture::new();
    let dir = directory();
    let engine = fixture.engine(dir.path());
    fixture.publish(2);
    engine.check_with(fixture.memory.clone()).await.unwrap();
    let head = std::fs::read(dir.path().join("head.json")).unwrap();
    fixture.publish(1);
    assert!(engine.check_with(fixture.memory.clone()).await.is_err());
    assert_eq!(std::fs::read(dir.path().join("head.json")).unwrap(), head);
    assert_eq!(generations(dir.path()), 1);
}
#[tokio::test]
async fn mix_and_match_signed_snapshot_hashes_is_refused() {
    let fixture = Fixture::new();
    let old = fixture
        .memory
        .objects
        .lock()
        .unwrap()
        .get("/metadata/targets.json")
        .unwrap()
        .clone();
    fixture.publish(2);
    fixture
        .memory
        .objects
        .lock()
        .unwrap()
        .insert("/metadata/targets.json".into(), old);
    let dir = directory();
    assert!(fixture
        .engine(dir.path())
        .check_with(fixture.memory.clone())
        .await
        .is_err());
    assert!(!dir.path().join("head.json").exists());
}
#[tokio::test]
async fn root_rotation_is_cross_signed_cached_and_revalidated_back_to_bundle() {
    let fixture = Fixture::new();
    let dir = directory();
    let engine = fixture.engine(dir.path());
    engine.check_with(fixture.memory.clone()).await.unwrap();
    let rotated = Keys::new();
    let root = rotated.root(2, Some(&fixture.keys));
    fixture
        .memory
        .objects
        .lock()
        .unwrap()
        .insert("/metadata/2.root.json".into(), root);
    fixture.publish_with(2, &rotated, None);
    engine.check_with(fixture.memory.clone()).await.unwrap();
    fixture
        .memory
        .objects
        .lock()
        .unwrap()
        .remove("/metadata/2.root.json");
    fixture.memory.requested.lock().unwrap().clear();
    engine.check_with(fixture.memory.clone()).await.unwrap();
    assert!(!fixture
        .memory
        .requested
        .lock()
        .unwrap()
        .iter()
        .any(|p| p == "/metadata/2.root.json"));
    let head: Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("head.json")).unwrap()).unwrap();
    let generation = dir.path().join(head["generation"].as_str().unwrap());
    std::fs::write(generation.join("chain-2.json"), b"{}").unwrap();
    assert!(engine.check_with(fixture.memory.clone()).await.is_err());
}
#[tokio::test]
async fn replacement_root_without_old_threshold_is_refused() {
    let fixture = Fixture::new();
    let rotated = Keys::new();
    fixture
        .memory
        .objects
        .lock()
        .unwrap()
        .insert("/metadata/2.root.json".into(), rotated.root(2, None));
    fixture.publish_with(2, &rotated, None);
    let dir = directory();
    assert!(fixture
        .engine(dir.path())
        .check_with(fixture.memory.clone())
        .await
        .is_err());
}
#[tokio::test]
async fn downloaded_payload_tamper_and_stream_interruption_never_return_an_installer() {
    for mode in ["tamper", "interrupt"] {
        let fixture = Fixture::new();
        let dir = directory();
        let engine = fixture.engine(dir.path());
        engine.check_with(fixture.memory.clone()).await.unwrap();
        let head = std::fs::read(dir.path().join("head.json")).unwrap();
        let initial_head: Value = serde_json::from_slice(&head).unwrap();
        let metadata_before: BTreeMap<&str, Value> = [
            "root.json",
            "timestamp.json",
            "snapshot.json",
            "targets.json",
        ]
        .into_iter()
        .map(|name| {
            let bytes = std::fs::read(
                dir.path()
                    .join(initial_head["generation"].as_str().unwrap())
                    .join(name),
            )
            .unwrap();
            (name, serde_json::from_slice(&bytes).unwrap())
        })
        .collect();
        let mut memory = fixture.memory.clone();
        let target = format!("/targets/{}", fixture.release.installer_target);
        if mode == "tamper" {
            memory
                .objects
                .lock()
                .unwrap()
                .insert(target.clone(), vec![b'X'; fixture.installer.len()]);
        } else {
            memory.broken = Some(target);
        }
        assert!(engine
            .download_with(&fixture.release, memory)
            .await
            .is_err());
        let before: Value = serde_json::from_slice(&head).unwrap();
        let after: Value =
            serde_json::from_slice(&std::fs::read(dir.path().join("head.json")).unwrap()).unwrap();
        assert_eq!(after["highest"], before["highest"]);
        for (name, expected) in &metadata_before {
            let bytes = std::fs::read(
                dir.path()
                    .join(after["generation"].as_str().unwrap())
                    .join(name),
            )
            .unwrap();
            assert_eq!(&serde_json::from_slice::<Value>(&bytes).unwrap(), expected);
        }
        drop(Store::open(dir.path(), &fixture.publisher().identity).unwrap());
        assert_eq!(
            std::fs::read_dir(dir.path().join("downloads"))
                .unwrap()
                .count(),
            0
        );
        assert_eq!(generations(dir.path()), 1);
        if mode == "interrupt" {
            let ready = engine
                .download_with(&fixture.release, fixture.memory.clone())
                .await
                .unwrap();
            ready.revalidate().unwrap();
        }
    }
}
#[tokio::test]
async fn signed_platform_channel_app_and_installer_binding_cannot_be_substituted() {
    for mode in [
        "platform", "channel", "app", "hash", "size", "path", "build-id",
    ] {
        let mut fixture = Fixture::new();
        match mode {
            "platform" => fixture.release.platform = "linux-x64".into(),
            "channel" => fixture.release.channel = "beta".into(),
            "app" => fixture.release.app_id = "another-app".into(),
            "hash" => fixture.release.installer_sha256 = "0".repeat(64),
            "size" => fixture.release.installer_bytes += 1,
            "path" => fixture.release.installer_target = "../../escape.exe".into(),
            "build-id" => fixture.release.sequence = "20260231000000".into(),
            _ => unreachable!(),
        }
        fixture.publish(1);
        let dir = directory();
        assert!(fixture
            .engine(dir.path())
            .check_with(fixture.memory.clone())
            .await
            .is_err());
        assert!(!dir.path().join("head.json").exists());
    }
}
#[tokio::test]
async fn signed_older_release_and_same_identifier_collision_are_refused() {
    let mut fixture = Fixture::new();
    let dir = directory();
    let engine = fixture.engine(dir.path());
    engine.check_with(fixture.memory.clone()).await.unwrap();
    let head = std::fs::read(dir.path().join("head.json")).unwrap();
    fixture.release.notes = "Different contents under the same build identifier".into();
    fixture.publish(2);
    assert!(engine.check_with(fixture.memory.clone()).await.is_err());
    for (metadata_version, version, sequence, reason) in [
        (3, "0.10.1", "20261009120000", "feed moved back"),
        (4, "0.10.1", "20261009000000", "older release"),
        (5, "0.9.9", "20261011000000", "older release"),
    ] {
        fixture.release.version = version.into();
        fixture.release.sequence = sequence.into();
        fixture.release.installer_target =
            format!("windows-x64/stable/Relay-{version}-{sequence}-Setup.exe");
        fixture.publish(metadata_version);
        // Keep every other binding valid so this exercises version/sequence policy itself.
        let error = engine.check_with(fixture.memory.clone()).await.unwrap_err();
        assert!(error.contains(reason), "Unexpected refusal: {error}");
    }
    assert_eq!(std::fs::read(dir.path().join("head.json")).unwrap(), head);
}
#[tokio::test]
async fn stale_approval_cannot_download_a_newly_replaced_offer() {
    let mut fixture = Fixture::new();
    let dir = directory();
    let engine = fixture.engine(dir.path());
    let approved = fixture.release.clone();
    engine.check_with(fixture.memory.clone()).await.unwrap();
    fixture.release.sequence = "20261011000000".into();
    fixture.release.installer_target =
        "windows-x64/stable/Relay-0.10.1-20261011000000-Setup.exe".into();
    fixture.publish(2);
    assert!(engine
        .download_with(&approved, fixture.memory.clone())
        .await
        .err()
        .unwrap()
        .contains("changed"));
}
#[tokio::test]
async fn interrupted_metadata_generation_is_removed_without_downgrading_active_state() {
    let fixture = Fixture::new();
    let dir = directory();
    let engine = fixture.engine(dir.path());
    engine.check_with(fixture.memory.clone()).await.unwrap();
    let orphan = dir.path().join("metadata-crash67");
    std::fs::create_dir(&orphan).unwrap();
    std::fs::write(
        orphan.join("generation-owner.json"),
        serde_json::to_vec(&json!({"schema":1,"publisher":fixture.publisher().identity})).unwrap(),
    )
    .unwrap();
    std::fs::write(orphan.join("timestamp.json"), b"{").unwrap();
    engine.check_with(fixture.memory.clone()).await.unwrap();
    assert!(!orphan.exists());
    assert_eq!(generations(dir.path()), 1);
}
#[tokio::test]
async fn changed_active_metadata_fails_closed_instead_of_discarding_rollback_state() {
    let fixture = Fixture::new();
    let dir = directory();
    let engine = fixture.engine(dir.path());
    engine.check_with(fixture.memory.clone()).await.unwrap();
    let head = std::fs::read(dir.path().join("head.json")).unwrap();
    let value: Value = serde_json::from_slice(&head).unwrap();
    std::fs::write(
        dir.path()
            .join(value["generation"].as_str().unwrap())
            .join("timestamp.json"),
        b"{",
    )
    .unwrap();
    assert!(engine.check_with(fixture.memory.clone()).await.is_err());
    assert_eq!(std::fs::read(dir.path().join("head.json")).unwrap(), head);
}
#[tokio::test]
async fn concurrent_checks_are_refused_and_unknown_application_files_are_preserved() {
    let fixture = Fixture::new();
    let dir = directory();
    let root = dir.path().join("updates");
    let engine = fixture.engine(&root);
    let profile = dir.path().join("encryption.device");
    std::fs::write(&profile, b"owned key preservation sentinel").unwrap();
    let store = Store::open(&root, &fixture.publisher().identity).unwrap();
    assert!(engine.check_with(fixture.memory.clone()).await.is_err());
    drop(store);
    engine.check_with(fixture.memory.clone()).await.unwrap();
    assert_eq!(
        std::fs::read(profile).unwrap(),
        b"owned key preservation sentinel"
    );
}
#[test]
fn bundled_publisher_policy_is_valid_and_unsafe_transport_bases_are_rejected() {
    assert!(bundled_publisher().is_ok());
    for url in [
        "http://publisher.example/meta/",
        "file:///C:/repo/",
        "https://user:secret@publisher.example/meta/",
        "https://publisher.example/meta/?token=secret",
        "https://publisher.example/meta/#fragment",
        "https://publisher.example/meta",
    ] {
        assert!(transport::base_url(url).is_err());
    }
}

#[test]
fn compiled_publisher_policy_requires_reviewed_root_hash_and_verified_signatures() {
    let fixture = Fixture::new();
    let policy = json!({
        "schema": 1, "mode": "signed",
        "metadata": "https://publisher.fixture.invalid/metadata/",
        "targets": "https://publisher.fixture.invalid/targets/",
        "channel": "stable", "root_sha256": digest(&fixture.root),
    });
    let parse_policy = |value: &Value, root: &[u8]| {
        publisher_policy::parse(&serde_json::to_vec(value).unwrap(), root)
    };
    let publisher = parse_policy(&policy, &fixture.root).unwrap().unwrap();
    assert_eq!(
        publisher.policy_identity(),
        fixture.publisher().policy_identity()
    );
    assert!(publisher_policy::parse(
        br#"{"schema":1,"mode":"disabled"}"#,
        b"unparsed unused root"
    )
    .unwrap()
    .is_none());
    let mut altered = policy.clone();
    altered["root_sha256"] = json!("0".repeat(64));
    assert!(parse_policy(&altered, &fixture.root).is_err());
    for field in ["schema", "channel", "metadata", "mode"] {
        let mut invalid = policy.clone();
        invalid[field] = json!("unsupported");
        assert!(parse_policy(&invalid, &fixture.root).is_err());
    }
    let mut unknown = policy.clone();
    unknown["root_path"] = json!("untrusted-runtime-override");
    assert!(parse_policy(&unknown, &fixture.root).is_err());
    assert!(publisher_policy::parse(
        br#"{"schema":1,"mode":"disabled","metadata":"https://untrusted.invalid/"}"#,
        &fixture.root
    )
    .is_err());
    let mut unsigned: Value = serde_json::from_slice(&fixture.root).unwrap();
    unsigned["signatures"] = json!([]);
    let unsigned = serde_json::to_vec(&unsigned).unwrap();
    let mut unsigned_policy = policy.clone();
    unsigned_policy["root_sha256"] = json!(digest(&unsigned));
    assert!(
        parse_policy(&unsigned_policy, &unsigned).is_err(),
        "a matching hash does not authorize an unsigned root"
    );
}

#[test]
fn aliasing_one_root_public_key_with_extra_fields_cannot_satisfy_independence() {
    let keys = Keys::new();
    let root = keys.root(1, None);
    let mut signed_root: Value = serde_json::from_slice(&root).unwrap();
    let mut alias = keys.roots[0].key.clone();
    alias["label"] = json!("different key ID, same signing material");
    let alias_id = digest(&canonical(&alias));
    signed_root["signed"]["keys"][&alias_id] = alias;
    signed_root["signed"]["roles"]["root"]["keyids"] = json!([keys.roots[0].id, alias_id]);
    let body = signed_root["signed"].clone();
    let signature = hex(keys.roots[0].pair.sign(&canonical(&body)).as_ref());
    let aliased_root = serde_json::to_vec(&json!({
        "signed": body,
        "signatures": [
            {"keyid": keys.roots[0].id, "sig": signature},
            {"keyid": alias_id, "sig": signature},
        ],
    }))
    .unwrap();
    // Show that the underlying metadata verifier accepts these two key IDs;
    // Relay's stronger initial publisher policy must then reject the alias.
    let root: tough::schema::Signed<tough::schema::Root> =
        serde_json::from_slice(&aliased_root).unwrap();
    root.signed.verify_role(&root).unwrap();
    assert!(Publisher::from_bundled_root(
        aliased_root,
        "https://publisher.fixture.invalid/metadata/",
        "https://publisher.fixture.invalid/targets/",
        "stable"
    )
    .unwrap_err()
    .contains("aliases"));
}
#[test]
fn unrelated_nonempty_folder_and_hardlinked_update_files_are_refused() {
    let fixture = Fixture::new();
    let dir = directory();
    let foreign = dir.path().join("existing profile");
    std::fs::create_dir(&foreign).unwrap();
    let keep = foreign.join("preferences.json");
    std::fs::write(&keep, b"keep existing profile").unwrap();
    assert!(Store::open(&foreign, &fixture.publisher().identity).is_err());
    assert_eq!(std::fs::read(keep).unwrap(), b"keep existing profile");
    let store = Store::open(&dir.path().join("updates"), &fixture.publisher().identity).unwrap();
    drop(store);
    let sentinel = dir.path().join("outside owned sentinel");
    std::fs::write(&sentinel, b"keep hardlink target").unwrap();
    std::fs::hard_link(&sentinel, dir.path().join("updates/linked.json")).unwrap();
    assert!(Store::open(&dir.path().join("updates"), &fixture.publisher().identity).is_err());
    assert_eq!(std::fs::read(sentinel).unwrap(), b"keep hardlink target");
}

#[tokio::test]
async fn restart_revalidates_a_complete_staged_installer_without_redownloading_it() {
    let fixture = Fixture::new();
    let dir = directory();
    let first = fixture
        .engine(dir.path())
        .download_with(&fixture.release, fixture.memory.clone())
        .await
        .unwrap();
    let original_path = first.path().to_path_buf();
    drop(first);
    fixture.memory.requested.lock().unwrap().clear();
    let after_restart = fixture
        .engine(dir.path())
        .download_with(&fixture.release, fixture.memory.clone())
        .await
        .unwrap();
    assert_eq!(after_restart.path(), original_path);
    after_restart.revalidate().unwrap();
    let requested = fixture.memory.requested.lock().unwrap();
    assert!(requested.iter().any(|p| p == "/metadata/timestamp.json"));
    assert!(!requested
        .iter()
        .any(|p| p == &format!("/targets/{}", fixture.release.installer_target)));
}

#[tokio::test]
async fn changed_existing_installer_is_preserved_and_refused_before_another_download() {
    let fixture = Fixture::new();
    let dir = directory();
    let engine = fixture.engine(dir.path());
    let ready = engine
        .download_with(&fixture.release, fixture.memory.clone())
        .await
        .unwrap();
    std::fs::write(ready.path(), b"changed local file").unwrap();
    fixture.memory.requested.lock().unwrap().clear();
    assert!(engine
        .download_with(&fixture.release, fixture.memory.clone())
        .await
        .is_err());
    assert_eq!(std::fs::read(ready.path()).unwrap(), b"changed local file");
    assert!(!fixture
        .memory
        .requested
        .lock()
        .unwrap()
        .iter()
        .any(|p| p == &format!("/targets/{}", fixture.release.installer_target)));
}

#[tokio::test]
async fn incomplete_generation_owner_is_preserved_and_does_not_block_a_good_checkpoint() {
    let fixture = Fixture::new();
    let dir = directory();
    let engine = fixture.engine(dir.path());
    engine.check_with(fixture.memory.clone()).await.unwrap();
    let orphan = dir.path().join("metadata-owner-crash67");
    std::fs::create_dir(&orphan).unwrap();
    std::fs::write(orphan.join("generation-owner.json"), b"{").unwrap();
    let foreign = dir.path().join("metadata-unknown67");
    std::fs::create_dir(&foreign).unwrap();
    std::fs::write(foreign.join("unknown.keep"), b"preserve unknown data").unwrap();
    engine.check_with(fixture.memory.clone()).await.unwrap();
    assert_eq!(
        std::fs::read(orphan.join("generation-owner.json")).unwrap(),
        b"{"
    );
    assert_eq!(
        std::fs::read(foreign.join("unknown.keep")).unwrap(),
        b"preserve unknown data"
    );
}

#[tokio::test]
async fn current_release_is_not_downloaded_and_expired_handoff_is_refused() {
    let mut fixture = Fixture::new();
    let dir = directory();
    let engine = fixture.engine(dir.path());
    let mut ready = engine
        .download_with(&fixture.release, fixture.memory.clone())
        .await
        .unwrap();
    ready.valid_until = chrono::Utc::now().timestamp();
    assert!(ready.revalidate().unwrap_err().contains("expired"));
    let current_dir = directory();
    fixture.release.version = "0.10.0".into();
    fixture.release.sequence = "20261009034055".into();
    fixture.release.installer_target =
        "windows-x64/stable/Relay-0.10.0-20261009034055-Setup.exe".into();
    fixture.publish(1);
    let engine = fixture.engine(current_dir.path());
    assert_eq!(
        engine.check_with(fixture.memory.clone()).await.unwrap(),
        Check::Current
    );
    assert!(engine
        .download_with(&fixture.release, fixture.memory.clone())
        .await
        .is_err());
    assert!(!current_dir.path().join("downloads").exists());
}

#[tokio::test]
async fn oversized_signed_release_is_refused_before_fetching_its_body() {
    let mut fixture = Fixture::new();
    fixture.release.notes = "X".repeat(MAX_RELEASE as usize);
    fixture.publish(1);
    let dir = directory();
    assert!(fixture
        .engine(dir.path())
        .check_with(fixture.memory.clone())
        .await
        .is_err());
    assert!(!fixture
        .memory
        .requested
        .lock()
        .unwrap()
        .iter()
        .any(|p| p == "/targets/windows-x64/stable/release.json"));
    assert!(!dir.path().join("head.json").exists());
}

#[test]
fn cached_roots_require_every_old_and_new_threshold_and_every_transition() {
    let keys = Keys::new();
    let second = Keys::new();
    let third = Keys::new();
    let original = keys.root(1, None);
    let dir = directory();
    std::fs::write(dir.path().join("chain-2.json"), second.root(2, Some(&keys))).unwrap();
    std::fs::write(
        dir.path().join("chain-3.json"),
        third.root(3, Some(&second)),
    )
    .unwrap();
    let verified: Value =
        serde_json::from_slice(&storage::cached_root(&original, dir.path()).unwrap()).unwrap();
    assert_eq!(verified["signed"]["version"], json!(3));
    std::fs::remove_file(dir.path().join("chain-2.json")).unwrap();
    assert!(storage::cached_root(&original, dir.path()).is_err());
    let mut broken: Value = serde_json::from_slice(&second.root(2, Some(&keys))).unwrap();
    let signature = second.roots[1].id.clone();
    broken["signatures"]
        .as_array_mut()
        .unwrap()
        .retain(|s| s["keyid"] != signature);
    std::fs::write(
        dir.path().join("chain-2.json"),
        serde_json::to_vec(&broken).unwrap(),
    )
    .unwrap();
    assert!(storage::cached_root(&original, dir.path()).is_err());
    std::fs::write(dir.path().join("chain-2.json"), second.root(2, Some(&keys))).unwrap();
    std::fs::write(
        dir.path().join("chain-02.json"),
        second.root(2, Some(&keys)),
    )
    .unwrap();
    assert!(storage::cached_root(&original, dir.path()).is_err());
}

#[tokio::test]
async fn unapproved_transport_addresses_are_rejected_before_network_io() {
    let fixture = Fixture::new();
    let transport = HttpTransport::new(&fixture.publisher()).unwrap();
    for address in [
        "http://publisher.fixture.invalid/metadata/timestamp.json",
        "https://another.fixture.invalid/metadata/timestamp.json",
        "https://publisher.fixture.invalid:444/metadata/timestamp.json",
        "https://publisher.fixture.invalid/metadata-other/timestamp.json",
        "https://user:secret@publisher.fixture.invalid/metadata/timestamp.json",
        "https://publisher.fixture.invalid/metadata/timestamp.json?secret=value",
        "https://publisher.fixture.invalid/metadata/timestamp.json#fragment",
        "file:///C:/fixture/timestamp.json",
    ] {
        let result = transport.fetch(url::Url::parse(address).unwrap()).await;
        let error = result
            .err()
            .expect("Unapproved addresses must be rejected.");
        assert_eq!(error.kind(), TransportErrorKind::UnsupportedUrlScheme);
    }
}

#[cfg(windows)]
#[test]
fn local_junction_is_refused_without_touching_its_owned_target() {
    use std::os::windows::process::CommandExt;
    let dir = directory();
    let target = dir.path().join("owned junction target");
    let junction = dir.path().join("junction");
    storage::guard(&target).unwrap();
    storage::guard(&junction).unwrap();
    std::fs::create_dir(&target).unwrap();
    let sentinel = target.join("preserve.keep");
    std::fs::write(&sentinel, b"preserve redirected target").unwrap();
    let literal = |p: &Path| format!("'{}'", p.to_string_lossy().replace('\'', "''"));
    let script = format!(
        "$ErrorActionPreference='Stop'; New-Item -ItemType Junction -Path {} -Target {} | Out-Null",
        literal(&junction),
        literal(&target)
    );
    let output = std::process::Command::new(
        "C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe",
    )
    .args(["-NoProfile", "-NonInteractive", "-Command", &script])
    .creation_flags(0x08000000)
    .output()
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(storage::guard(&junction).is_err());
    assert!(storage::guard(&junction.join("updates")).is_err());
    assert_eq!(
        std::fs::read(&sentinel).unwrap(),
        b"preserve redirected target"
    );
    // Remove just the observed junction; its checked local target is retained.
    std::fs::remove_dir(&junction).unwrap();
    assert_eq!(
        std::fs::read(&sentinel).unwrap(),
        b"preserve redirected target"
    );
}

fn finish_worker(worker: &mut Worker) -> Result<Outcome> {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match worker.poll() {
            Poll::Finished(result) => return result,
            Poll::Idle => panic!("Worker lost its active operation"),
            Poll::Running | Poll::Completing => {}
        }
        assert!(
            std::time::Instant::now() < deadline,
            "Worker did not terminate"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn worker_verifies_real_repository_exits_and_accepts_the_next_operation() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let fixture = Fixture::new();
    let dir = directory();
    let engine = Arc::new(fixture.engine(dir.path()));
    let mut worker = Worker::new(fixture.engine(dir.path()));
    let wakes = Arc::new(AtomicUsize::new(0));
    let wake = wakes.clone();
    let checking = engine.clone();
    let memory = fixture.memory.clone();
    worker
        .start(
            move || async move { checking.check_with(memory).await.map(Outcome::Checked) },
            move || {
                wake.fetch_add(1, Ordering::SeqCst);
            },
        )
        .unwrap();
    assert!(worker.running());
    assert!(worker
        .start(|| async { Ok(Outcome::Cancelled) }, || {})
        .is_err());
    let Outcome::Checked(check) = finish_worker(&mut worker).unwrap() else {
        panic!("Wrong worker result");
    };
    assert_eq!(check, Check::Available(Box::new(fixture.release.clone())));
    assert!(!worker.running());
    assert_eq!(wakes.load(Ordering::SeqCst), 1);
    let downloading = engine.clone();
    let memory = fixture.memory.clone();
    let approved = fixture.release.clone();
    worker
        .start(
            move || async move {
                downloading
                    .download_with(&approved, memory)
                    .await
                    .map(|p| Outcome::Downloaded(Box::new(p)))
            },
            || {},
        )
        .unwrap();
    let Outcome::Downloaded(ready) = finish_worker(&mut worker).unwrap() else {
        panic!("No prepared installer");
    };
    ready.revalidate().unwrap();
    assert_eq!(std::fs::read(ready.path()).unwrap(), fixture.installer);
    assert!(!worker.running());
}

#[test]
fn worker_cancellation_drops_pending_work_before_notifying_and_joins_the_thread() {
    use std::sync::atomic::{AtomicBool, Ordering};
    struct PendingGuard(Arc<AtomicBool>);
    impl Drop for PendingGuard {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let fixture = Fixture::new();
    let dir = directory();
    let mut worker = Worker::new(fixture.engine(dir.path()));
    let (entered, observed) = std::sync::mpsc::channel();
    let dropped = Arc::new(AtomicBool::new(false));
    let guard = dropped.clone();
    let wake_guard = dropped.clone();
    worker
        .start(
            move || async move {
                let _guard = PendingGuard(guard);
                entered.send(()).unwrap();
                std::future::pending::<Result<Outcome>>().await
            },
            move || {
                assert!(wake_guard.load(Ordering::SeqCst));
            },
        )
        .unwrap();
    observed.recv_timeout(Duration::from_secs(3)).unwrap();
    worker.cancel();
    assert!(matches!(
        finish_worker(&mut worker).unwrap(),
        Outcome::Cancelled
    ));
    assert!(dropped.load(Ordering::SeqCst));
    assert!(!worker.running());
    assert!(!dir.path().join("head.json").exists());
}

#[test]
fn dropping_worker_cancels_in_flight_operation_without_blocking_the_ui() {
    use std::sync::atomic::{AtomicBool, Ordering};
    struct PendingGuard(Arc<AtomicBool>);
    impl Drop for PendingGuard {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let fixture = Fixture::new();
    let dir = directory();
    let mut worker = Worker::new(fixture.engine(dir.path()));
    let dropped = Arc::new(AtomicBool::new(false));
    let capture = dropped.clone();
    let (entered, observed) = std::sync::mpsc::channel();
    let (completed, completion) = std::sync::mpsc::channel();
    worker
        .start(
            move || async move {
                let _guard = PendingGuard(capture);
                entered.send(()).unwrap();
                std::future::pending::<Result<Outcome>>().await
            },
            move || {
                completed.send(()).unwrap();
            },
        )
        .unwrap();
    observed.recv_timeout(Duration::from_secs(3)).unwrap();
    let started = std::time::Instant::now();
    drop(worker);
    assert!(started.elapsed() < Duration::from_millis(100));
    // The receiver is dropped together with the UI, so no completion callback is expected.
    assert!(completion.recv_timeout(Duration::from_secs(3)).is_err());
    assert!(dropped.load(Ordering::SeqCst));
}

#[test]
fn panicked_operation_returns_an_error_wakes_once_and_releases_the_worker() {
    let fixture = Fixture::new();
    let dir = directory();
    let mut worker = Worker::new(fixture.engine(dir.path()));
    let (completed, received) = std::sync::mpsc::channel();
    worker
        .start(
            || async {
                panic!("owned worker panic fixture");
            },
            move || {
                completed.send(()).unwrap();
            },
        )
        .unwrap();
    received.recv_timeout(Duration::from_secs(3)).unwrap();
    assert!(finish_worker(&mut worker)
        .unwrap_err()
        .contains("stopped unexpectedly"));
    assert!(!worker.running());
}

#[path = "https_tests.rs"]
mod https;
