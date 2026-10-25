//! A queued route update is finished only when a newer record that stands on its own
//! signatures and ancestry already replaces it. Hosts are real keys and a real signed
//! handover chain.
use super::*;
use ed25519_dalek::{Signer, SigningKey};
use relay_protocol::handover::{
    self as model, ActivationGrant, HostIdentity, Receipt, ReceiptBody,
};
use sha2::{Digest, Sha256};

/// A fixed "now" far enough ahead that the real clock plays no part.
const NOW: u64 = 1_900_000_000_000;
const ID: &str = "00000000000000000000000000000000000000000000000000000000000000ab";

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
                .map(|i| SigningKey::from_bytes(&[i as u8 + 151; 32]))
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
    /// A record signed by `path[history.len()]`, anchored at `path[anchor_at]`.
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

#[test]
fn the_same_record_a_later_revision_and_a_verified_successor_supersede_the_queued_one() {
    let w = World::new();
    let main = [0, 1, 2];
    let full = w.history(&main);
    // Queued: the first host's own update (term 1), anchored at itself.
    let queued = w.record(&main[..1], &full[..0], 0, ID, NOW - 9_000, 1);
    assert!(supersedes(&queued, &queued.clone(), NOW));
    // A strictly later revision by the same host over the same history.
    let later = w.record(&main[..1], &full[..0], 0, ID, NOW - 5_000, 2);
    assert!(supersedes(&queued, &later, NOW));
    // The second host's record, with the history that proves the first is its ancestor.
    let second = w.record(&main[..2], &full[..1], 0, ID, NOW - 4_000, 3);
    assert!(supersedes(&queued, &second, NOW));
    // And the third, whose complete history still begins with the second's.
    let third = w.record(&main, &full, 0, ID, NOW - 3_000, 4);
    assert!(supersedes(&second, &third, NOW));
    assert!(supersedes(&queued, &third, NOW));
    // An anchor in the middle of a longer chain works the same way.
    let queued_mid = w.record(&main[..2], &full[..1], 1, ID, NOW - 9_000, 5);
    let stored_mid = w.record(&main, &full, 1, ID, NOW - 3_000, 6);
    assert!(supersedes(&queued_mid, &stored_mid, NOW));
}

#[test]
fn rollback_conflicts_rewritten_ancestry_and_other_identities_never_supersede() {
    let w = World::new();
    let main = [0, 1, 2];
    let full = w.history(&main);
    let queued = w.record(&main[..2], &full[..1], 0, ID, NOW - 5_000, 1);
    // Older revision, and another body at exactly the same revision.
    let older = w.record(&main[..2], &full[..1], 0, ID, NOW - 8_000, 2);
    let conflict = w.record(&main[..2], &full[..1], 0, ID, NOW - 5_000, 3);
    assert!(!supersedes(&queued, &older, NOW));
    assert!(!supersedes(&queued, &conflict, NOW));
    // A lower term: the first host's record never replaces the second's.
    let rollback = w.record(&main[..1], &full[..0], 0, ID, NOW - 1_000, 4);
    assert!(!supersedes(&queued, &rollback, NOW));
    // The same term from another host, and a higher term on a different path.
    let fork = [0, 8];
    let forked = w.history(&fork);
    let sibling = w.record(&fork, &forked[..1], 0, ID, NOW - 1_000, 5);
    assert!(!supersedes(&queued, &sibling, NOW));
    let rewritten_path = [0, 9, 2];
    let rewritten = w.history(&rewritten_path);
    let rival = w.record(&rewritten_path, &rewritten, 0, ID, NOW - 1_000, 6);
    assert!(!supersedes(&queued, &rival, NOW));
    // Same hosts and term, but the history differs (the grants were re-issued).
    let reissued = w.history(&main);
    let again = w.record(&main[..2], &reissued[..1], 0, ID, NOW - 1_000, 7);
    assert!(!supersedes(&queued, &again, NOW));
    // A higher term whose history was shortened so that it no longer contains the queued one.
    let mut shortened = w.record(&main, &full, 0, ID, NOW - 1_000, 8);
    shortened.history.remove(0);
    w.sign(&mut shortened, 2);
    assert!(!supersedes(&queued, &shortened, NOW));
    // Wrong ID, other anchor, another community.
    let wrong_id = w.record(&main, &full, 0, &ID.replace('a', "c"), NOW - 1_000, 9);
    assert!(!supersedes(&queued, &wrong_id, NOW));
    let other_anchor = w.record(&main, &full, 1, ID, NOW - 1_000, 9);
    assert!(!supersedes(&queued, &other_anchor, NOW));
    let elsewhere = World::new();
    let foreign_chain = elsewhere.history(&main);
    let foreign = elsewhere.record(&main, &foreign_chain, 0, ID, NOW - 1_000, 9);
    assert!(!supersedes(&queued, &foreign, NOW));
    // The right successor is still accepted next to all of these.
    let honest = w.record(&main, &full, 0, ID, NOW - 1_000, 10);
    assert!(supersedes(&queued, &honest, NOW));
}

#[test]
fn stale_future_malformed_or_forged_records_on_either_side_never_count() {
    let w = World::new();
    let main = [0, 1, 2];
    let full = w.history(&main);
    let queued = w.record(&main[..1], &full[..0], 0, ID, NOW - 5_000, 1);
    let stored = w.record(&main[..2], &full[..1], 0, ID, NOW - 4_000, 2);
    assert!(supersedes(&queued, &stored, NOW));
    // Expired on either side, or not yet issued (more than 30 s ahead), or too long-lived.
    assert!(!supersedes(&queued, &stored, NOW + 4_000_000));
    let mut ahead = stored.clone();
    ahead.issued_ms = NOW + 60_000;
    ahead.expires_ms = ahead.issued_ms + 1_000;
    w.sign(&mut ahead, 1);
    assert!(!supersedes(&queued, &ahead, NOW));
    let mut long = stored.clone();
    long.expires_ms = long.issued_ms + locator::MAX_LIFETIME_MS + 1;
    w.sign(&mut long, 1);
    assert!(!supersedes(&queued, &long, NOW));
    let mut huge = stored.clone();
    huge.expires_ms = u64::MAX;
    w.sign(&mut huge, 1);
    assert!(!supersedes(&queued, &huge, NOW));
    // The queued record must be sound too: a forged "queued" is not something to retire.
    let mut forged_queue = queued.clone();
    forged_queue.ciphertext = URL_SAFE_NO_PAD.encode(vec![9u8; 40]);
    assert!(!supersedes(&forged_queue, &stored, NOW));
    // Forgery in the stored record: changed body, other signer, tampered or missing ancestry.
    let mut body = stored.clone();
    body.ciphertext = URL_SAFE_NO_PAD.encode(vec![9u8; 40]);
    assert!(!supersedes(&queued, &body, NOW));
    let mut signer = stored.clone();
    w.sign(&mut signer, 5);
    assert!(!supersedes(&queued, &signer, NOW));
    let mut tampered = stored.clone();
    tampered.history[0].receipt.body.target_url = "https://elsewhere.example/".into();
    w.sign(&mut tampered, 1);
    assert!(!supersedes(&queued, &tampered, NOW));
    let mut unsigned = stored.clone();
    unsigned.signature.clear();
    assert!(!supersedes(&queued, &unsigned, NOW));
    // Malformed shapes: wrong-size or non-canonical nonce, tiny or oversized ciphertext.
    for fault in 0..4 {
        let mut broken = stored.clone();
        match fault {
            0 => broken.nonce = URL_SAFE_NO_PAD.encode([3u8; 11]),
            1 => broken.nonce = format!("{}=", broken.nonce),
            2 => broken.ciphertext = URL_SAFE_NO_PAD.encode(vec![1u8; 15]),
            _ => {
                broken.ciphertext =
                    URL_SAFE_NO_PAD.encode(vec![1u8; locator::MAX_CIPHERTEXT_BYTES + 1])
            }
        }
        w.sign(&mut broken, 1);
        assert!(!supersedes(&queued, &broken, NOW), "fault {fault}");
    }
    // A term the directory could not store.
    let mut wide = stored.clone();
    wide.current.term = u64::MAX;
    w.sign(&mut wide, 1);
    assert!(!supersedes(&queued, &wide, NOW));
}
