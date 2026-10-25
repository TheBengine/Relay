//! The renewal walk keeps its place in a private folder and puts every page of signed
//! records in the outbox before it moves on. Hosts are modelled with real keys and a
//! real signed handover chain, and every folder is an owned temporary one.
use super::*;
use crate::host_locator_outbox::Outbox;
use ed25519_dalek::{Signer, SigningKey};
use relay_protocol::handover::{self as model, HostIdentity};

/// A fixed "now" far enough ahead that the real clock never expires a test record.
const NOW: u64 = 1_900_000_000_000;
const SERVICE: &str = "https://friends.example/people";
const OTHER_SERVICE: &str = "https://elsewhere.example/people";

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
        public_key: URL_SAFE_NO_PAD.encode(key.verifying_key().as_bytes()),
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
                .map(|i| SigningKey::from_bytes(&[i as u8 + 111; 32]))
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
        use relay_protocol::handover::{Receipt, ReceiptBody};
        let mut out: Vec<ActivationGrant> = vec![];
        for i in 0..path.len() - 1 {
            let body = ReceiptBody {
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
                challenge: URL_SAFE_NO_PAD.encode([4; 32]),
            };
            let receipt = Receipt {
                signature: URL_SAFE_NO_PAD.encode(
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
            grant.signature = URL_SAFE_NO_PAD.encode(
                self.keys[path[i]]
                    .sign(&model::grant_message(&grant))
                    .to_bytes(),
            );
            out.push(grant);
        }
        out
    }
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
            nonce: URL_SAFE_NO_PAD.encode([3u8; 12]),
            ciphertext: URL_SAFE_NO_PAD.encode(vec![seed; 40]),
            signature: String::new(),
        };
        self.sign(&mut record, path[at]);
        record
    }
    fn sign(&self, record: &mut Record, key: usize) {
        record.signature = URL_SAFE_NO_PAD.encode(
            self.keys[key]
                .sign(&locator::record_message(record))
                .to_bytes(),
        );
    }
}

fn h(n: u128) -> String {
    format!("{n:064x}")
}

/// A host with two earlier hosts, an owned host-data folder and an owned profile
/// folder for the outbox, passed explicitly without changing process environment.
struct Lab {
    _profile: tempfile::TempDir,
    host: tempfile::TempDir,
    w: World,
    path: [usize; 3],
    history: Vec<ActivationGrant>,
}

impl Lab {
    fn new() -> Self {
        let profile = tempfile::tempdir().unwrap();
        let w = World::new();
        let path = [0, 1, 2];
        let history = w.history(&path);
        Self {
            _profile: profile,
            host: tempfile::tempdir().unwrap(),
            w,
            path,
            history,
        }
    }
    fn current(&self) -> RoutePin {
        self.w.pin(2, 3)
    }
    fn destination(url: &str) -> Destination {
        Destination {
            url: url.into(),
            iroh: None,
        }
    }
    fn renewal_for(&self, service: &str, url: &str) -> Result<Renewal, String> {
        Renewal::new(
            self.host.path().to_path_buf(),
            self.current(),
            self.history.clone(),
            service,
            &Self::destination(url),
        )
    }
    fn renewal(&self) -> Renewal {
        self.renewal_for(SERVICE, "https://source.example").unwrap()
    }
    fn queue(&self, service: &str) -> Outbox {
        Outbox::at_service(self._profile.path().to_path_buf(),service).unwrap()
    }
    fn session_renewal(&self) -> Renewal {
        Renewal::new_session(
            self.host.path().to_path_buf(),
            self.current(),
            self.history.clone(),
            SERVICE,
            &Self::destination("https://source.example"),
        )
        .unwrap()
    }
    /// The signed record the host would make for invite `n` under the anchor at `anchor_at`.
    fn made(&self, n: u128, anchor_at: usize, seed: u8) -> Record {
        self.made_for(Purpose::Invitation, n, anchor_at, seed)
    }
    /// The same for a session hash, whose lookup ID has its own domain.
    fn made_for(&self, purpose: Purpose, n: u128, anchor_at: usize, seed: u8) -> Record {
        let anchor = self.w.pin(self.path[anchor_at], anchor_at as u64 + 1);
        let hash = hash_bytes(&h(n)).unwrap();
        let material = match purpose {
            Purpose::Invitation => locator::capability_material(&anchor, &hash, false),
            Purpose::Session => locator::session_material(&anchor, &hash, false),
        };
        let id = hex(Sha256::digest(material));
        self.w
            .record(&self.path, &self.history, anchor_at, &id, NOW - 1_000, seed)
    }
    fn page(&self, pairs: &[(u128, usize)], next: Option<(u128, usize)>) -> Page {
        self.page_for(Purpose::Invitation, pairs, next)
    }
    fn page_for(
        &self,
        purpose: Purpose,
        pairs: &[(u128, usize)],
        next: Option<(u128, usize)>,
    ) -> Page {
        Page {
            current: self.current(),
            records: pairs
                .iter()
                .map(|(n, a)| self.made_for(purpose, *n, *a, 1))
                .collect(),
            next: next.map(|(n, a)| Cursor {
                invite_id: h(n),
                anchor_term: a as u64 + 1,
            }),
        }
    }
    /// Two invites over three anchors: six pairs, a full first page and a short last one.
    fn first(&self) -> Page {
        self.page(&[(1, 0), (1, 1), (1, 2), (2, 0)], Some((2, 0)))
    }
    fn last(&self) -> Page {
        self.page(&[(2, 1), (2, 2)], None)
    }
    fn saved(&self) -> Option<Vec<u8>> {
        fs::read(self.host.path().join(FILE)).ok()
    }
    fn saved_session(&self) -> Option<Vec<u8>> {
        fs::read(self.host.path().join(SESSION_FILE)).ok()
    }
    /// Rewrite the invitation checkpoint through its JSON, to make states a clock or an
    /// older version could leave behind.
    fn edit_saved(&self, edit: impl FnOnce(&mut serde_json::Value)) -> Vec<u8> {
        let mut value: serde_json::Value = serde_json::from_slice(&self.saved().unwrap()).unwrap();
        edit(&mut value);
        let bytes = serde_json::to_vec(&value).unwrap();
        fs::write(self.host.path().join(FILE), &bytes).unwrap();
        bytes
    }
}


fn never(_: Option<Cursor>) -> Result<Page, String> {
    panic!("nothing may be fetched");
}

fn position(after: Option<Cursor>) -> Option<(String, u64)> {
    after.map(|c| (c.invite_id, c.anchor_term))
}

#[test]
fn a_new_renewal_resumes_after_each_durable_page_and_finishes_without_fetching() {
    let lab = Lab::new();
    let mut queue = lab.queue(SERVICE);
    let renewal = lab.renewal();
    let first = lab.first();
    let r = renewal.advance(&mut queue, NOW, |after| {
        assert_eq!(position(after), None);
        Ok(first)
    });
    assert!(matches!(r, Ok(Advance::Queued { more: true })));
    assert_eq!(queue.room(NOW), 28);
    // A fresh process: same place, from the private file; the queue was durable too.
    drop(renewal);
    drop(queue);
    let mut queue = lab.queue(SERVICE);
    assert_eq!(queue.room(NOW), 28);
    let renewal = lab.renewal();
    let last = lab.last();
    let r = renewal.advance(&mut queue, NOW, |after| {
        assert_eq!(position(after), Some((h(2), 1)));
        Ok(last)
    });
    assert!(matches!(r, Ok(Advance::Queued { more: false })));
    assert_eq!(queue.room(NOW), 26);
    // Complete, and it stays complete in a new process, without asking the host.
    let r = lab.renewal().advance(&mut queue, NOW, never);
    assert!(matches!(r, Ok(Advance::Complete)));
    // Nothing private rides along: the file holds hashes and the cursor, not the destination.
    let saved = String::from_utf8(lab.saved().unwrap()).unwrap();
    assert!(!saved.contains("source.example") && !saved.contains("friends.example"));
    assert!(saved.len() as u64 <= MAX_FILE_BYTES);
    for item in [lab.made(1, 0, 1), lab.made(2, 2, 1)] {
        assert!(!saved.contains(&item.id));
    }
}

#[test]
fn an_empty_final_page_completes_the_scan_and_stays_complete_without_fetching() {
    let lab = Lab::new();
    let mut queue = lab.queue(SERVICE);
    let empty = lab.page(&[], None);
    let r = lab.renewal().advance(&mut queue, NOW, |_| Ok(empty));
    assert!(matches!(r, Ok(Advance::Complete)));
    assert!(matches!(
        lab.renewal().advance(&mut queue, NOW, never),
        Ok(Advance::Complete)
    ));
}

#[test]
fn a_nearly_full_outbox_stops_before_any_fetch_and_before_any_progress() {
    // Room for 3 is not room for a page of 4.
    let lab = Lab::new();
    let mut queue = lab.queue(SERVICE);
    for n in 0..29 {
        queue.enqueue(lab.made(100 + n, 0, 1), NOW).unwrap();
    }
    assert_eq!(queue.room(NOW), 3);
    let r = lab.renewal().advance(&mut queue, NOW, never);
    assert!(matches!(r, Ok(Advance::Full)));
    assert!(lab.saved().is_none(), "no progress without a fetched page");
}

#[test]
fn progress_never_gets_ahead_of_the_records_that_could_not_be_queued() {
    let lab = Lab::new();
    let mut queue = lab.queue(SERVICE);
    let page = lab.first();
    // A newer record for the third ID is already waiting, so queueing the page stops there.
    let mut newer = page.records[2].clone();
    newer.issued_ms = NOW - 500;
    lab.w.sign(&mut newer, 2);
    queue.enqueue(newer, NOW).unwrap();
    let r = lab.renewal().advance(&mut queue, NOW, |_| Ok(page));
    assert!(r.is_err());
    assert!(lab.saved().is_none(), "the checkpoint did not move");
    // The rows before the failure may stay, and repeating the page is safe.
    assert_eq!(queue.room(NOW), 32 - 3);
    let r = lab.renewal().advance(&mut queue, NOW, |after| {
        assert_eq!(position(after), None, "still at the start");
        Ok(lab.first())
    });
    assert!(r.is_err());
    assert!(lab.saved().is_none());
}

#[test]
fn damaged_foreign_and_rolled_back_progress_is_kept_and_refused() {
    let lab = Lab::new();
    let valid = |current: &RoutePin| {
        serde_json::to_vec(&serde_json::json!({
            "schema": 1, "current": current, "service": "a".repeat(64),
            "destination": "b".repeat(64), "cursor": null, "complete": false
        }))
        .unwrap()
    };
    let other_server = RoutePin {
        server_id: uuid::Uuid::new_v4().to_string(),
        ..lab.current()
    };
    let unrelated_key = lab.w.pin(7, 3);
    let newer = lab.w.pin(3, 4);
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("not json", b"nope".to_vec()),
        ("unknown field", br#"{"schema":1,"extra":true}"#.to_vec()),
        (
            "wrong schema",
            serde_json::to_vec(&serde_json::json!({
                "schema": 2, "current": lab.current(), "service": "a".repeat(64),
                "destination": "b".repeat(64), "cursor": null, "complete": false
            }))
            .unwrap(),
        ),
        ("oversized", vec![b' '; MAX_FILE_BYTES as usize + 1]),
        ("another community", valid(&other_server)),
        ("an unrelated key", valid(&unrelated_key)),
        ("a newer authority (rollback)", valid(&newer)),
        (
            "a cursor that is not a hash",
            serde_json::to_vec(&serde_json::json!({
                "schema": 1, "current": lab.current(), "service": "a".repeat(64),
                "destination": "b".repeat(64),
                "cursor": {"invite_id": "ABC", "anchor_term": 1}, "complete": false
            }))
            .unwrap(),
        ),
        (
            "a cursor on another lineage",
            serde_json::to_vec(&serde_json::json!({
                "schema": 1, "current": lab.current(), "service": "a".repeat(64),
                "destination": "b".repeat(64),
                "cursor": {"invite_id": h(5), "anchor_term": 9}, "complete": false
            }))
            .unwrap(),
        ),
    ];
    for (name, bytes) in cases {
        fs::write(lab.host.path().join(FILE), &bytes).unwrap();
        assert!(
            lab.renewal_for(SERVICE, "https://source.example").is_err(),
            "{name}"
        );
        assert_eq!(lab.saved().unwrap(), bytes, "{name} stays as it was");
    }
    // A folder where the file belongs is not a store either.
    fs::remove_file(lab.host.path().join(FILE)).unwrap();
    fs::create_dir(lab.host.path().join(FILE)).unwrap();
    assert!(lab.renewal_for(SERVICE, "https://source.example").is_err());
}

#[test]
fn a_changed_service_destination_or_older_authority_restarts_while_the_queue_is_kept() {
    let lab = Lab::new();
    let mut queue = lab.queue(SERVICE);
    let first = lab.first();
    lab.renewal()
        .advance(&mut queue, NOW, |_| Ok(first))
        .unwrap();
    let kept = queue.room(NOW);
    let start = |renewal: Renewal, queue: &mut Outbox, expect: Option<(String, u64)>| {
        let again = lab.first();
        renewal
            .advance(queue, NOW, |after| {
                assert_eq!(position(after), expect);
                Ok(again)
            })
            .unwrap();
    };
    // The same scope continues; each change below starts the walk over.
    let last = lab.last();
    lab.renewal()
        .advance(&mut queue, NOW, |after| {
            assert_eq!(position(after), Some((h(2), 1)));
            Ok(last)
        })
        .unwrap();
    // After completing, a different destination access is a new scan that fetches again.
    start(
        lab.renewal_for(SERVICE, "https://moved.example").unwrap(),
        &mut queue,
        None,
    );
    // And so is another service.
    start(
        lab.renewal_for(OTHER_SERVICE, "https://moved.example")
            .unwrap(),
        &mut queue,
        None,
    );
    // The records already queued for the first scope are still there.
    assert!(queue.room(NOW) <= kept);
    // An older authority's checkpoint (the previous host, term 2) restarts under the newer one.
    fs::remove_file(lab.host.path().join(FILE)).unwrap();
    let older = Renewal::new(
        lab.host.path().to_path_buf(),
        lab.w.pin(1, 2),
        lab.history[..1].to_vec(),
        SERVICE,
        &Lab::destination("https://source.example"),
    )
    .unwrap();
    older
        .save(&Progress {
            cursor: Some((h(9), 1)),
            complete: false,
            completed_ms: None,
        })
        .unwrap();
    start(lab.renewal(), &mut queue, None);
}

#[test]
fn a_page_that_does_not_fit_the_place_the_host_or_itself_is_refused_before_any_change() {
    let lab = Lab::new();
    let mut queue = lab.queue(SERVICE);
    let first = lab.first();
    lab.renewal()
        .advance(&mut queue, NOW, |_| Ok(first))
        .unwrap();
    let saved = lab.saved().unwrap();
    let room = queue.room(NOW);
    let good = || lab.last();
    let mut cases: Vec<(&str, Page)> = vec![];
    let mut add = |name, page: Page| cases.push((name, page));
    // The cursor must move strictly forward from (invite 2, term 1), and be the last record's own.
    add(
        "cursor stays put",
        lab.page(&[(2, 1), (2, 2), (3, 0), (3, 1)], Some((2, 0))),
    );
    add(
        "cursor goes back",
        lab.page(&[(2, 1), (2, 2), (3, 0), (3, 1)], Some((1, 2))),
    );
    add(
        "cursor is not the last record's",
        lab.page(&[(2, 1), (2, 2), (3, 0), (3, 1)], Some((3, 0))),
    );
    add(
        "cursor for another invite",
        lab.page(&[(2, 1), (2, 2), (3, 0), (3, 1)], Some((4, 1))),
    );
    let mut p = lab.page(&[(2, 1), (2, 2), (3, 0), (3, 1)], Some((3, 1)));
    p.next.as_mut().unwrap().anchor_term = 9;
    add("cursor term off the lineage", p);
    let mut p = lab.page(&[(2, 1), (2, 2), (3, 0), (3, 1)], Some((3, 1)));
    p.next.as_mut().unwrap().invite_id = h(3).to_uppercase().replace('0', "A");
    add("cursor not canonical", p);
    // Shape: counts and the cursor must agree.
    add("empty page with a cursor", lab.page(&[], Some((3, 1))));
    add(
        "short page with a cursor",
        lab.page(&[(2, 1), (2, 2)], Some((2, 2))),
    );
    add(
        "full page without a cursor",
        lab.page(&[(2, 1), (2, 2), (3, 0), (3, 1)], None),
    );
    add(
        "five records",
        lab.page(&[(2, 1), (2, 2), (3, 0), (3, 1), (3, 2)], Some((3, 2))),
    );
    // Authority and records.
    let mut p = good();
    p.current = lab.w.pin(7, 3);
    add("another current host", p);
    let mut p = good();
    p.records[0].anchor = lab.w.pin(8, 1);
    lab.w.sign(&mut p.records[0], 2);
    add("an anchor that is not on the lineage", p);
    let mut p = good();
    p.records[1].ciphertext = URL_SAFE_NO_PAD.encode(vec![9u8; 40]);
    add("a record changed after signing", p);
    let mut p = good();
    lab.w.sign(&mut p.records[0], 5);
    add("a record signed by another key", p);
    let mut p = good();
    p.records[1] = p.records[0].clone();
    add("the same record twice", p);
    let mut p = good();
    p.records[0].history.remove(0);
    lab.w.sign(&mut p.records[0], 2);
    add("a record with other history", p);
    for (name, page) in cases {
        let r = lab.renewal().advance(&mut queue, NOW, |after| {
            assert_eq!(position(after), Some((h(2), 1)));
            Ok(page)
        });
        assert!(r.is_err(), "{name}");
        assert_eq!(lab.saved().unwrap(), saved, "{name}: checkpoint unchanged");
        assert_eq!(queue.room(NOW), room, "{name}: nothing queued");
    }
    // A host that cannot answer changes nothing either.
    let r = lab
        .renewal()
        .advance(&mut queue, NOW, |_| Err("offline".to_string()));
    assert!(r.is_err());
    assert_eq!(lab.saved().unwrap(), saved);
}

fn failing_flush(_: &Path) -> std::io::Result<()> {
    Err(std::io::Error::other("flush failed"))
}

#[test]
fn a_flush_failure_after_the_replacement_is_reported_honestly_and_foreign_files_are_untouched() {
    let lab = Lab::new();
    let mut queue = lab.queue(SERVICE);
    let foreign = lab.host.path().join("locator-renewal.json.new");
    fs::write(&foreign, b"someone else's data").unwrap();
    let mut renewal = lab.renewal();
    renewal.sync = failing_flush;
    let first = lab.first();
    let error = match renewal.advance(&mut queue, NOW, |_| Ok(first)) {
        Err(e) => e,
        Ok(_) => panic!("must not claim a confirmed save"),
    };
    assert_eq!(error, UNCONFIRMED);
    // The new place is installed, not undone: the next process continues from it.
    let last = lab.last();
    let r = lab.renewal().advance(&mut queue, NOW, |after| {
        assert_eq!(position(after), Some((h(2), 1)));
        Ok(last)
    });
    assert!(matches!(r, Ok(Advance::Queued { more: false })));
    assert_eq!(fs::read(&foreign).unwrap(), b"someone else's data");
    // Only the progress file was added beside it.
    let mut names: Vec<_> = fs::read_dir(lab.host.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec![FILE.to_string(), "locator-renewal.json.new".to_string()]
    );
}

#[test]
fn invitation_and_session_walks_keep_separate_files_cursors_and_records() {
    let lab = Lab::new();
    let mut queue = lab.queue(SERVICE);
    let invitations = lab.renewal();
    let sessions = lab.session_renewal();
    let first = lab.first();
    invitations.advance(&mut queue, NOW, |_| Ok(first)).unwrap();
    let their_first = lab.page_for(
        Purpose::Session,
        &[(5, 0), (5, 1), (5, 2), (6, 0)],
        Some((6, 0)),
    );
    let r = sessions.advance(&mut queue, NOW, |after| {
        assert_eq!(
            position(after),
            None,
            "its own walk starts at its own start"
        );
        Ok(their_first)
    });
    assert!(matches!(r, Ok(Advance::Queued { more: true })));
    // Two files side by side, nothing else, and each is read only by its own purpose.
    let mut names: Vec<_> = fs::read_dir(lab.host.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names, vec![FILE.to_string(), SESSION_FILE.to_string()]);
    let (invitation_file, session_file) = (lab.saved().unwrap(), lab.saved_session().unwrap());
    assert_ne!(invitation_file, session_file);
    // Each resumes at its own cursor, in a new process.
    let last = lab.last();
    lab.renewal()
        .advance(&mut queue, NOW, |after| {
            assert_eq!(position(after), Some((h(2), 1)));
            Ok(last)
        })
        .unwrap();
    let their_last = lab.page_for(Purpose::Session, &[(6, 1), (6, 2)], None);
    lab.session_renewal()
        .advance(&mut queue, NOW, |after| {
            assert_eq!(position(after), Some((h(6), 1)));
            Ok(their_last)
        })
        .unwrap();
    // A page of the other purpose's records, with a cursor that matches them, is refused by
    // both walks, before anything is queued or saved.
    drop(lab);
    let lab = Lab::new();
    let mut queue = lab.queue(SERVICE);
    let full = |purpose| lab.page_for(purpose, &[(2, 1), (2, 2), (3, 0), (3, 1)], Some((3, 1)));
    let room = queue.room(NOW);
    let wrong_for_invitations = full(Purpose::Session);
    let r = lab
        .renewal()
        .advance(&mut queue, NOW, |_| Ok(wrong_for_invitations));
    assert_eq!(r.err().as_deref(), Some(BAD_PAGE));
    let wrong_for_sessions = full(Purpose::Invitation);
    let r = lab
        .session_renewal()
        .advance(&mut queue, NOW, |_| Ok(wrong_for_sessions));
    assert_eq!(r.err().as_deref(), Some(BAD_PAGE));
    assert!(lab.saved().is_none() && lab.saved_session().is_none());
    assert_eq!(queue.room(NOW), room);
    // The right kind passes for each.
    let mine = full(Purpose::Invitation);
    assert!(lab.renewal().advance(&mut queue, NOW, |_| Ok(mine)).is_ok());
    let theirs = full(Purpose::Session);
    assert!(lab
        .session_renewal()
        .advance(&mut queue, NOW, |_| Ok(theirs))
        .is_ok());
}

#[test]
fn a_finished_walk_idles_until_due_then_starts_again_even_after_a_restart_or_from_an_older_file() {
    let lab = Lab::new();
    let mut queue = lab.queue(SERVICE);
    let empty = lab.page(&[], None);
    let r = lab.renewal().advance(&mut queue, NOW, |_| Ok(empty));
    assert!(matches!(r, Ok(Advance::Complete)));
    let finished: serde_json::Value = serde_json::from_slice(&lab.saved().unwrap()).unwrap();
    assert_eq!(finished["completed_ms"], NOW);
    assert_eq!(finished["complete"], true);
    // Not due: no fetch, no write, in this process or a new one.
    let saved = lab.saved().unwrap();
    for at in [NOW, NOW + 1, NOW + REFRESH_MS - 1] {
        assert!(matches!(
            lab.renewal().advance(&mut queue, at, never),
            Ok(Advance::Complete)
        ));
        assert_eq!(lab.saved().unwrap(), saved);
    }
    // Due: a new process starts from the beginning; the first page moves the checkpoint on.
    let due = NOW + REFRESH_MS;
    let first = lab.first();
    let r = lab.renewal().advance(&mut queue, due, |after| {
        assert_eq!(position(after), None);
        Ok(first)
    });
    assert!(matches!(r, Ok(Advance::Queued { more: true })));
    let midway: serde_json::Value = serde_json::from_slice(&lab.saved().unwrap()).unwrap();
    assert_eq!(midway["complete"], false);
    assert!(
        midway.get("completed_ms").is_none(),
        "no time while the walk is going"
    );
    let last = lab.last();
    let r = lab.renewal().advance(&mut queue, due + 5, |after| {
        assert_eq!(position(after), Some((h(2), 1)));
        Ok(last)
    });
    assert!(matches!(r, Ok(Advance::Queued { more: false })));
    let again: serde_json::Value = serde_json::from_slice(&lab.saved().unwrap()).unwrap();
    assert_eq!(again["completed_ms"], due + 5);
    assert!(matches!(
        lab.renewal().advance(&mut queue, due + REFRESH_MS, never),
        Ok(Advance::Complete)
    ));
    // An older finished file has no time: it is due on the next advance, then gets one.
    lab.edit_saved(|v| {
        v.as_object_mut().unwrap().remove("completed_ms");
    });
    let legacy = lab.saved().unwrap();
    let again = lab.page(&[], None);
    let r = lab.renewal().advance(&mut queue, NOW, |after| {
        assert_eq!(position(after), None);
        Ok(again)
    });
    assert!(matches!(r, Ok(Advance::Complete)));
    assert_ne!(lab.saved().unwrap(), legacy);
    // A due walk with a full outbox neither fetches nor loses its place.
    drop(lab);
    let lab = Lab::new();
    let mut queue = lab.queue(SERVICE);
    let empty = lab.page(&[], None);
    lab.renewal()
        .advance(&mut queue, NOW, |_| Ok(empty))
        .unwrap();
    for n in 0..29 {
        queue.enqueue(lab.made(100 + n, 0, 1), NOW).unwrap();
    }
    let saved = lab.saved().unwrap();
    let r = lab.renewal().advance(&mut queue, NOW + REFRESH_MS, never);
    assert!(matches!(r, Ok(Advance::Full)));
    assert_eq!(lab.saved().unwrap(), saved);
    // Sessions have the same rhythm in their own file.
    drop(lab);
    let lab = Lab::new();
    let mut queue = lab.queue(SERVICE);
    let empty = lab.page_for(Purpose::Session, &[], None);
    lab.session_renewal()
        .advance(&mut queue, NOW, |_| Ok(empty))
        .unwrap();
    assert!(matches!(
        lab.session_renewal().advance(&mut queue, NOW + 1, never),
        Ok(Advance::Complete)
    ));
    let first = lab.page_for(Purpose::Session, &[(1, 0)], None);
    let r = lab
        .session_renewal()
        .advance(&mut queue, NOW + REFRESH_MS, |after| {
            assert_eq!(position(after), None);
            Ok(first)
        });
    assert!(matches!(r, Ok(Advance::Queued { more: false })));
    assert!(
        lab.saved().is_none(),
        "the invitation file was never touched"
    );
}

#[test]
fn impossible_times_damaged_files_and_failed_saves_leave_the_old_checkpoint_in_place() {
    let lab = Lab::new();
    let mut queue = lab.queue(SERVICE);
    let empty = lab.page(&[], None);
    lab.renewal()
        .advance(&mut queue, NOW, |_| Ok(empty))
        .unwrap();
    // A completion time from far in the future would stall the walk for good: refuse it,
    // keep the file. A little ahead (a clock that was set back a bit) just waits.
    let future = lab.edit_saved(|v| v["completed_ms"] = serde_json::json!(NOW + 2 * REFRESH_MS));
    let r = lab.renewal().advance(&mut queue, NOW, never);
    assert_eq!(r.err().as_deref(), Some(UNREADABLE));
    assert_eq!(lab.saved().unwrap(), future);
    lab.edit_saved(|v| v["completed_ms"] = serde_json::json!(NOW + 60_000));
    assert!(matches!(
        lab.renewal().advance(&mut queue, NOW, never),
        Ok(Advance::Complete)
    ));
    // A time on a walk that is not finished is damage too.
    let damaged = lab.edit_saved(|v| v["complete"] = serde_json::json!(false));
    assert!(lab.renewal_for(SERVICE, "https://source.example").is_err());
    assert_eq!(lab.saved().unwrap(), damaged);
    lab.edit_saved(|v| {
        v["complete"] = serde_json::json!(true);
        v["completed_ms"] = serde_json::json!(NOW);
    });
    // A due walk whose first page cannot be queued keeps the finished checkpoint, and is
    // still due afterwards.
    let finished = lab.saved().unwrap();
    let due = NOW + REFRESH_MS;
    let page = lab.first();
    let mut newer = page.records[2].clone();
    newer.issued_ms = NOW - 500;
    lab.w.sign(&mut newer, 2);
    queue.enqueue(newer, due).unwrap();
    let r = lab.renewal().advance(&mut queue, due, |_| Ok(page));
    assert!(r.is_err());
    assert_eq!(lab.saved().unwrap(), finished);
    let r = lab.renewal().advance(&mut queue, due, |after| {
        assert_eq!(position(after), None, "still due, from the start");
        Err("offline".to_string())
    });
    assert!(r.is_err());
    assert_eq!(lab.saved().unwrap(), finished);
    // A flush failure when a walk finishes is reported, and the completion is installed:
    // the next process sees it and waits instead of walking again at once.
    drop(lab);
    let lab = Lab::new();
    let mut queue = lab.queue(SERVICE);
    let mut renewal = lab.renewal();
    renewal.sync = failing_flush;
    let empty = lab.page(&[], None);
    let error = renewal.advance(&mut queue, NOW, |_| Ok(empty)).err();
    assert_eq!(error.as_deref(), Some(UNCONFIRMED));
    assert!(matches!(
        lab.renewal().advance(&mut queue, NOW + 1, never),
        Ok(Advance::Complete)
    ));
}
