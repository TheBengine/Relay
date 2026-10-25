//! The outbox keeps signed, opaque host-locator records until they are published.
//! Hosts are modelled with real keys and a real signed handover chain, and every
//! store lives in an explicit temporary folder, never in a real profile.
use super::*;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
use ed25519_dalek::{Signer, SigningKey};
use relay_protocol::handover::{self as model, ActivationGrant, HostIdentity, RoutePin};
use sha2::{Digest, Sha256};

/// A fixed "now" far enough ahead that the real clock never expires a test record.
const NOW: u64 = 1_900_000_000_000;

fn identity(key: &SigningKey) -> HostIdentity {
    let mut hash = Sha256::new();
    hash.update(b"relay-host-identity-v1\0");
    hash.update(key.verifying_key().as_bytes());
    let digest = hash.finalize();
    let mut bytes: [u8; 16] = digest[..16].try_into().unwrap();
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    HostIdentity {
        host_id: uuid::Uuid::from_bytes(bytes).to_string(),
        public_key: B64.encode(key.verifying_key().as_bytes()),
    }
}

struct World {
    keys: Vec<SigningKey>,
    server: String,
}

impl World {
    fn new() -> Self {
        Self {
            keys: (0..10)
                .map(|i| SigningKey::from_bytes(&[i as u8 + 61; 32]))
                .collect(),
            server: uuid::Uuid::new_v4().to_string(),
        }
    }
    fn pin(&self, key: usize, term: u64) -> RoutePin {
        RoutePin {
            server_id: self.server.clone(),
            host: identity(&self.keys[key]),
            term,
        }
    }
    /// A complete signed chain along `path`; the key at position `i` holds term `i + 1`.
    fn history(&self, path: &[usize]) -> Vec<ActivationGrant> {
        let mut out: Vec<ActivationGrant> = vec![];
        for i in 0..path.len() - 1 {
            let body = model::ReceiptBody {
                server_id: self.server.clone(),
                source_host_id: uuid::Uuid::new_v4().to_string(),
                term: i as u64 + 1,
                preparation_id: uuid::Uuid::new_v4().to_string(),
                export_id: uuid::Uuid::new_v4().to_string(),
                target_identity: identity(&self.keys[path[i + 1]]),
                target_url: "https://target.example/".into(),
                database_sha256: "a".repeat(64),
                manifest_sha256: "b".repeat(64),
                database_bytes: 45,
                challenge: B64.encode([4; 32]),
            };
            let receipt = model::Receipt {
                signature: B64.encode(
                    self.keys[path[i + 1]]
                        .sign(&model::receipt_message(&body))
                        .to_bytes(),
                ),
                body,
            };
            let mut grant = ActivationGrant {
                receipt,
                source_identity: identity(&self.keys[path[i]]),
                next_term: i as u64 + 2,
                history_sha256: (i > 0)
                    .then(|| format!("{:x}", Sha256::digest(model::history_message(&out)))),
                signature: String::new(),
            };
            grant.signature = B64.encode(
                self.keys[path[i]]
                    .sign(&model::grant_message(&grant))
                    .to_bytes(),
            );
            out.push(grant);
        }
        out
    }
    /// A signed record whose current host is `path[history.len()]`, anchored at `path[anchor_at]`.
    fn record(
        &self,
        path: &[usize],
        history: &[ActivationGrant],
        anchor_at: usize,
        id: &str,
        issued_ms: u64,
        seed: u8,
    ) -> Record {
        let at = history.len();
        let mut record = Record {
            id: id.into(),
            anchor: self.pin(path[anchor_at], anchor_at as u64 + 1),
            current: self.pin(path[at], at as u64 + 1),
            history: history.to_vec(),
            issued_ms,
            expires_ms: issued_ms + 3_600_000,
            nonce: B64.encode([3u8; 12]),
            ciphertext: B64.encode(vec![seed; 40]),
            signature: String::new(),
        };
        self.sign(&mut record, path[at]);
        record
    }
    fn sign(&self, record: &mut Record, key: usize) {
        record.signature = B64.encode(
            self.keys[key]
                .sign(&locator::record_message(record))
                .to_bytes(),
        );
    }
}

fn id_of(n: u32) -> String {
    format!("{:064x}", n as u128 + 1)
}

fn simple(w: &World, id: &str, issued: u64, seed: u8) -> Record {
    w.record(&[0, 1], &[], 0, id, issued, seed)
}

fn outbox() -> (tempfile::TempDir, Outbox) {
    let dir = tempfile::tempdir().unwrap();
    let outbox = Outbox::open(dir.path().to_path_buf()).unwrap();
    (dir, outbox)
}

fn reopened(dir: &tempfile::TempDir) -> Outbox {
    Outbox::open(dir.path().to_path_buf()).unwrap()
}

fn on_disk(dir: &tempfile::TempDir) -> Option<Vec<u8>> {
    fs::read(dir.path().join(FILE)).ok()
}

fn same(a: &Record, b: &Record) -> bool {
    text(a) == text(b)
}

#[test]
fn a_queued_record_waits_for_its_backoff_and_everything_survives_reopening() {
    let (dir, mut outbox) = outbox();
    let w = World::new();
    let record = simple(&w, &id_of(1), NOW - 1_000, 1);
    assert!(outbox.due(NOW).is_none());
    // Durable before any attempt: a fresh process already sees it, due at once.
    outbox.enqueue(record.clone(), NOW).unwrap();
    assert!(same(&reopened(&dir).due(NOW).unwrap(), &record));
    // The first failure waits 5 s, then 10 s, doubling up to 30 minutes.
    outbox.failed(&record, NOW).unwrap();
    assert!(outbox.due(NOW + 4_999).is_none());
    assert!(outbox.due(NOW + 5_000).is_some());
    outbox.failed(&record, NOW + 5_000).unwrap();
    assert!(outbox.due(NOW + 14_999).is_none());
    assert!(outbox.due(NOW + 15_000).is_some());
    assert_eq!(backoff_ms(1), 5_000);
    assert_eq!(backoff_ms(2), 10_000);
    assert_eq!(backoff_ms(9), 1_280_000);
    assert_eq!(backoff_ms(10), LAST_RETRY_MS);
    assert_eq!(backoff_ms(u32::MAX), LAST_RETRY_MS);
    // The schedule is on disk too.
    let again = reopened(&dir);
    assert!(again.due(NOW + 14_999).is_none());
    assert!(same(&again.due(NOW + 15_000).unwrap(), &record));
    // Publishing it forgets it, durably.
    outbox.sent(&record).unwrap();
    assert!(outbox.due(NOW + 60_000_000).is_none());
    assert!(reopened(&dir).due(NOW + 60_000_000).is_none());
}

#[test]
fn only_the_exact_revision_is_completed_or_delayed_and_the_oldest_due_goes_first() {
    let (dir, mut outbox) = outbox();
    let w = World::new();
    let id = id_of(1);
    let first = simple(&w, &id, NOW - 9_000, 1);
    let second = simple(&w, &id, NOW - 5_000, 2);
    outbox.enqueue(first.clone(), NOW).unwrap();
    outbox.enqueue(second.clone(), NOW + 1).unwrap();
    // The older revision was replaced; reports about it change nothing.
    outbox.failed(&first, NOW + 2).unwrap();
    assert!(same(&outbox.due(NOW + 2).unwrap(), &second));
    outbox.sent(&first).unwrap();
    assert!(same(&reopened(&dir).due(NOW + 2).unwrap(), &second));
    // The same ID under a record that was never queued is untouched as well.
    let stranger = simple(&w, &id, NOW - 4_000, 3);
    outbox.sent(&stranger).unwrap();
    outbox.failed(&stranger, NOW + 3).unwrap();
    assert!(same(&outbox.due(NOW + 3).unwrap(), &second));
    // Two people's records: the one that has waited longest is offered first.
    let other = simple(&w, &id_of(2), NOW - 3_000, 4);
    outbox.enqueue(other.clone(), NOW + 100).unwrap();
    assert!(same(&outbox.due(NOW + 100).unwrap(), &second));
    outbox.failed(&second, NOW + 100).unwrap();
    assert!(same(&outbox.due(NOW + 100).unwrap(), &other));
    outbox.sent(&second).unwrap();
    assert!(same(&reopened(&dir).due(NOW + 100).unwrap(), &other));
    outbox.sent(&other).unwrap();
    assert!(reopened(&dir).due(NOW + 100).is_none());
}

#[test]
fn damaged_foreign_or_redirected_storage_fails_closed_and_stays_as_it_was() {
    let w = World::new();
    let good = simple(&w, &id_of(1), NOW - 1_000, 1);
    let mut tampered = simple(&w, &id_of(2), NOW - 1_000, 1);
    tampered.ciphertext = B64.encode(vec![9u8; 40]);
    let contents: Vec<(&str, Vec<u8>)> = vec![
        ("not json", b"definitely not json".to_vec()),
        (
            "another program's file",
            b"{\"settings\":{\"theme\":\"dark\"}}".to_vec(),
        ),
        ("wrong version", b"{\"version\":7,\"items\":[]}".to_vec()),
        (
            "a record whose signature no longer matches",
            serde_json::to_vec(&serde_json::json!({
                "version": 1,
                "items": [{"record": tampered, "attempts": 0, "next_ms": 0}]
            }))
            .unwrap(),
        ),
        (
            "the same ID twice",
            serde_json::to_vec(&serde_json::json!({
                "version": 1,
                "items": [
                    {"record": good, "attempts": 0, "next_ms": 0},
                    {"record": good, "attempts": 0, "next_ms": 0}
                ]
            }))
            .unwrap(),
        ),
        ("far too large", vec![b' '; MAX_FILE_BYTES as usize + 1]),
    ];
    for (name, bytes) in contents {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(FILE), &bytes).unwrap();
        assert!(
            Outbox::open(dir.path().to_path_buf()).is_err(),
            "{name} must not load"
        );
        // A caller that wrapped an empty outbox around it still cannot overwrite it.
        let mut blank = Outbox {
            dir: dir.path().to_path_buf(),
            items: vec![],
            unconfirmed: false,
            sync: sync_directory,
        };
        assert_eq!(
            blank.enqueue(good.clone(), NOW).unwrap_err(),
            UNREADABLE,
            "{name}"
        );
        assert_eq!(on_disk(&dir).unwrap(), bytes, "{name} stays unchanged");
    }
    // A folder where the file should be is not a store either.
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join(FILE)).unwrap();
    assert!(Outbox::open(dir.path().to_path_buf()).is_err());
    // Errors never carry record content or capabilities.
    for message in [INVALID, CONFLICT, FULL, UNREADABLE, UNSAVED, BUSY] {
        assert!(!message.contains(&id_of(1)) && !message.contains("ciphertext"));
    }
}

/// Makes the next replacement of the file fail inside the owned folder, while
/// reading it still works. Dropping the returned guard lifts the fault.
#[cfg(windows)]
fn block_replacement(dir: &tempfile::TempDir) -> Box<dyn std::any::Any> {
    use std::os::windows::fs::OpenOptionsExt;
    // A handle that does not share delete access: the file cannot be replaced.
    Box::new(
        fs::OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(dir.path().join(FILE))
            .unwrap(),
    )
}
#[cfg(unix)]
fn block_replacement(dir: &tempfile::TempDir) -> Box<dyn std::any::Any> {
    use std::os::unix::fs::PermissionsExt;
    struct Restore(PathBuf);
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o700));
        }
    }
    // Read-only folder: no new file can be created in it.
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o500)).unwrap();
    Box::new(Restore(dir.path().to_path_buf()))
}

fn files(dir: &tempfile::TempDir) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn a_failed_replacement_reports_precisely_and_changes_neither_the_file_nor_the_queue() {
    let (dir, mut outbox) = outbox();
    let w = World::new();
    let kept = simple(&w, &id_of(1), NOW - 2_000, 1);
    outbox.enqueue(kept.clone(), NOW).unwrap();
    let before = on_disk(&dir).unwrap();
    let names = files(&dir);
    let fault = block_replacement(&dir);
    let more = simple(&w, &id_of(2), NOW - 1_000, 2);
    assert_eq!(outbox.enqueue(more.clone(), NOW + 1).unwrap_err(), UNSAVED);
    assert_eq!(outbox.failed(&kept, NOW + 1).unwrap_err(), UNSAVED);
    assert_eq!(outbox.sent(&kept).unwrap_err(), UNSAVED);
    // The file is as it was, no scratch file of the failed writes is left behind,
    // and memory did not move ahead of the disk.
    assert_eq!(on_disk(&dir).unwrap(), before);
    assert_eq!(files(&dir), names);
    assert!(same(&outbox.due(NOW + 1).unwrap(), &kept));
    assert_eq!(outbox.items.len(), 1);
    // Once storage works again, everything proceeds from the same state.
    drop(fault);
    outbox.enqueue(more.clone(), NOW + 1).unwrap();
    assert_eq!(reopened(&dir).items.len(), 2);
}

#[test]
fn a_file_that_is_not_ours_beside_the_queue_is_never_touched() {
    let (dir, mut outbox) = outbox();
    let w = World::new();
    // The names an earlier version used for its scratch file, and a lookalike.
    let foreign = [
        (
            "host-locator-outbox.json.new",
            b"someone else's data".to_vec(),
        ),
        ("host-locator-outbox.json.0.new", b"also not ours".to_vec()),
        ("notes.txt", b"keep".to_vec()),
    ];
    for (name, bytes) in &foreign {
        fs::write(dir.path().join(name), bytes).unwrap();
    }
    let record = simple(&w, &id_of(1), NOW - 1_000, 1);
    outbox.enqueue(record.clone(), NOW).unwrap();
    outbox.failed(&record, NOW).unwrap();
    outbox.sent(&record).unwrap();
    for (name, bytes) in &foreign {
        assert_eq!(&fs::read(dir.path().join(name)).unwrap(), bytes, "{name}");
    }
    // Every write cleaned up its own scratch file: the queue file and its lease
    // are all that this module added.
    let mut expected: Vec<String> = foreign.iter().map(|(n, _)| (*n).to_string()).collect();
    expected.extend([FILE.to_string(), LOCK.to_string()]);
    expected.sort();
    assert_eq!(files(&dir), expected);
}

static FLUSHES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
fn failing_flush(_: &Path) -> std::io::Result<()> {
    Err(std::io::Error::other("flush failed"))
}
fn counting_flush(_: &Path) -> std::io::Result<()> {
    FLUSHES.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    Ok(())
}

#[test]
fn a_folder_flush_that_fails_after_the_replacement_is_reported_as_unconfirmed_not_unchanged() {
    let (dir, mut outbox) = outbox();
    let w = World::new();
    let record = simple(&w, &id_of(1), NOW - 1_000, 1);
    outbox.sync = failing_flush;
    let error = outbox.enqueue(record.clone(), NOW).unwrap_err();
    assert_eq!(error, UNCONFIRMED);
    assert_ne!(error, UNSAVED);
    // Nothing is undone: the replacement is installed and the queue holds it.
    assert!(same(&reopened(&dir).due(NOW).unwrap(), &record));
    assert!(same(&outbox.due(NOW).unwrap(), &record));
    // Further changes still work on top of it, and keep asking for confirmation.
    let other = simple(&w, &id_of(2), NOW - 900, 2);
    assert_eq!(outbox.enqueue(other.clone(), NOW).unwrap_err(), UNCONFIRMED);
    assert_eq!(reopened(&dir).items.len(), 2);
    // Settling writes the same content again and clears the doubt once confirmed.
    outbox.sync = counting_flush;
    let before = FLUSHES.load(std::sync::atomic::Ordering::SeqCst);
    outbox.settle().unwrap();
    assert_eq!(
        FLUSHES.load(std::sync::atomic::Ordering::SeqCst),
        before + 1
    );
    assert_eq!(reopened(&dir).items.len(), 2);
    // Confirmed: a no-op settle writes nothing more.
    outbox.settle().unwrap();
    assert_eq!(
        FLUSHES.load(std::sync::atomic::Ordering::SeqCst),
        before + 1
    );
}

#[test]
fn a_queue_belongs_to_one_service_and_another_provider_never_sees_it() {
    let profile = tempfile::tempdir().unwrap();
    let w = World::new();
    let friends = "https://friends.example/people";
    let at = |base: &str| scoped(profile.path(), base);
    let record = simple(&w, &id_of(1), NOW - 1_000, 1);
    let mut first = Outbox::open(at(friends).unwrap()).unwrap();
    first.enqueue(record.clone(), NOW).unwrap();
    // The same provider, written differently, reopens the same queue.
    for same_service in [
        "https://friends.example/people/",
        "https://FRIENDS.example/people",
        "  https://friends.example:443/people  ",
    ] {
        let again = Outbox::open(at(same_service).unwrap()).unwrap();
        assert!(same(&again.due(NOW).unwrap(), &record), "{same_service}");
    }
    // Another provider, another path on the same host, or another port: empty.
    for other in [
        "https://other.example/people",
        "https://friends.example/other",
        "https://friends.example:8443/people",
    ] {
        assert_ne!(at(other).unwrap(), at(friends).unwrap());
        assert!(Outbox::open(at(other).unwrap()).unwrap().due(NOW).is_none());
    }
    // Only an HTTPS service address of the provider's plain form is accepted.
    for refused in [
        "http://friends.example/people",
        "https://user:secret@friends.example/people",
        "https://friends.example/people?x=1",
        "https://friends.example/people#frag",
        "ftp://friends.example/people",
        "friends.example/people",
        "",
    ] {
        assert!(scoped(profile.path(), refused).is_err(), "{refused:?}");
    }
    assert_eq!(scoped(profile.path(), "").unwrap_err(), NO_SERVICE);
    // Only the one service folder exists below the profile, holding one queue file;
    // the other services were only looked at, never created.
    let folders: Vec<_> = fs::read_dir(profile.path())
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(folders, vec![at(friends).unwrap()]);
    assert!(at(friends).unwrap().join(FILE).is_file());
}

#[test]
fn caps_expiry_and_invalid_records_never_lose_an_unexpired_item() {
    let (dir, mut outbox) = outbox();
    let w = World::new();
    // Not valid to queue at all: tampered, from the future, already expired.
    let mut tampered = simple(&w, &id_of(900), NOW - 1_000, 1);
    tampered.ciphertext = B64.encode(vec![5u8; 40]);
    assert_eq!(outbox.enqueue(tampered, NOW).unwrap_err(), INVALID);
    let future = simple(&w, &id_of(901), NOW + 120_000, 1);
    assert_eq!(outbox.enqueue(future, NOW).unwrap_err(), INVALID);
    let old = simple(&w, &id_of(902), NOW - 7_200_000, 1);
    assert_eq!(outbox.enqueue(old, NOW).unwrap_err(), INVALID);
    let mut unsigned = simple(&w, &id_of(903), NOW - 1_000, 1);
    unsigned.signature.clear();
    assert_eq!(outbox.enqueue(unsigned, NOW).unwrap_err(), INVALID);
    let long: Vec<usize> = (0..10).collect();
    let too_long = w.record(&long, &w.history(&long), 0, &id_of(904), NOW - 1_000, 1);
    assert_eq!(outbox.enqueue(too_long, NOW).unwrap_err(), INVALID);
    assert!(
        on_disk(&dir).is_none(),
        "nothing invalid ever touches the disk"
    );

    // One short-lived record, then the rest of the 32 places.
    let mut short = simple(&w, &id_of(0), NOW - 1_000, 1);
    short.expires_ms = NOW + 10_000;
    w.sign(&mut short, 0);
    outbox.enqueue(short.clone(), NOW).unwrap();
    for n in 1..32 {
        outbox
            .enqueue(simple(&w, &id_of(n), NOW - 1_000, 1), NOW)
            .unwrap();
    }
    // Full: a new ID is refused, and nothing waiting is dropped to make room.
    let extra = simple(&w, &id_of(50), NOW - 1_000, 1);
    assert_eq!(outbox.enqueue(extra.clone(), NOW + 1).unwrap_err(), FULL);
    assert_eq!(reopened(&dir).items.len(), 32);
    // A refresh of an ID that is already queued still fits.
    outbox
        .enqueue(simple(&w, &id_of(1), NOW - 500, 2), NOW + 1)
        .unwrap();
    assert_eq!(reopened(&dir).items.len(), 32);
    // Expired records are never offered, and leave on the next successful write.
    assert!(outbox.due(NOW + 20_000).is_some_and(|r| !same(&r, &short)));
    outbox.enqueue(extra.clone(), NOW + 20_000).unwrap();
    let after = reopened(&dir);
    assert_eq!(after.items.len(), 32);
    assert!(after.items.iter().all(|item| item.record.id != short.id));
    assert!(after.items.iter().any(|item| item.record.id == extra.id));
}

#[test]
fn a_same_id_is_replaced_only_by_the_same_anchor_and_a_later_or_verified_successor() {
    let (dir, mut outbox) = outbox();
    let w = World::new();
    let main = [0, 1, 2, 3];
    let full = w.history(&main);
    let id = id_of(1);
    let stored = w.record(&main[..1], &full[..0], 0, &id, NOW - 9_000, 1);
    outbox.enqueue(stored.clone(), NOW).unwrap();
    // The identical record again is accepted and does not reset the retry schedule.
    outbox.failed(&stored, NOW).unwrap();
    outbox.enqueue(stored.clone(), NOW + 1).unwrap();
    assert!(outbox.due(NOW + 4_999).is_none());
    // Older, or a different record at the same revision, is refused.
    let older = w.record(&main[..1], &full[..0], 0, &id, NOW - 9_500, 2);
    let conflict = w.record(&main[..1], &full[..0], 0, &id, NOW - 9_000, 3);
    assert_eq!(outbox.enqueue(older, NOW + 1).unwrap_err(), CONFLICT);
    assert_eq!(outbox.enqueue(conflict, NOW + 1).unwrap_err(), CONFLICT);
    // Another anchor cannot take the ID.
    let other_anchor = w.record(&main[..2], &full[..1], 1, &id, NOW - 100, 4);
    assert_eq!(outbox.enqueue(other_anchor, NOW + 1).unwrap_err(), CONFLICT);
    assert!(same(&reopened(&dir).items[0].record, &stored));
    // A later revision by the same authority replaces it and is due at once.
    let later = w.record(&main[..1], &full[..0], 0, &id, NOW - 6_000, 5);
    outbox.enqueue(later.clone(), NOW + 2).unwrap();
    assert!(same(&outbox.due(NOW + 2).unwrap(), &later));
    // The verified successor, over the complete history, replaces that.
    let successor = w.record(&main[..2], &full[..1], 0, &id, NOW - 4_000, 6);
    outbox.enqueue(successor.clone(), NOW + 3).unwrap();
    assert!(same(&reopened(&dir).items[0].record, &successor));
    // Then: going back, a rival in the same term, rewritten ancestry and a
    // shortened one are all refused, and the stored record stays.
    let rollback = w.record(&main[..1], &full[..0], 0, &id, NOW - 100, 7);
    let fork = [0, 8];
    let forked = w.history(&fork);
    let sibling = w.record(&fork, &forked[..1], 0, &id, NOW - 100, 8);
    let rewritten_path = [0, 9, 2, 3];
    let rewritten = w.history(&rewritten_path);
    let rewritten = w.record(&rewritten_path, &rewritten[..3], 0, &id, NOW - 100, 9);
    let mut shortened = w.record(&main, &full[..3], 0, &id, NOW - 100, 10);
    shortened.history.remove(0);
    w.sign(&mut shortened, 3);
    for refused in [rollback, sibling, rewritten, shortened] {
        assert!(outbox.enqueue(refused, NOW + 4).is_err());
    }
    assert!(same(&reopened(&dir).items[0].record, &successor));
    // The honest continuation of the same ancestry is taken.
    let next = w.record(&main, &full[..3], 0, &id, NOW - 100, 11);
    outbox.enqueue(next.clone(), NOW + 5).unwrap();
    let after = reopened(&dir);
    assert_eq!(after.items.len(), 1);
    assert!(same(&after.items[0].record, &next));
}
