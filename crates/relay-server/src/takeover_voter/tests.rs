//! Synthetic fixed-seed keys and in-memory log stores only: no personal key, file, network
//! or database. Written for the later focused run; not executed when written.
use super::*;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use ed25519_dalek::{Signer as _, SigningKey};
use relay_protocol::{
    handover::RoutePin,
    takeover::{AcceptBody, SetBody},
};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
};

const SERVER: &str = "0a1b2c3d-0000-4000-8000-0000000000aa";

struct Key {
    signing: SigningKey,
    id: HostIdentity,
}
impl Key {
    fn new(seed: u8) -> Self {
        let signing = SigningKey::from_bytes(&[seed; 32]);
        let id = crate::handover_target::identity(&signing.verifying_key());
        Self { signing, id }
    }
    fn sign(&self, message: &[u8]) -> String {
        URL_SAFE_NO_PAD.encode(self.signing.sign(message).to_bytes())
    }
}
struct Test(Rc<Key>);
impl HostSigner for Test {
    fn identity(&self) -> &HostIdentity {
        &self.0.id
    }
    fn sign(&self, message: &[u8]) -> String {
        self.0.sign(message)
    }
}
/// `bytes` is the metadata value, `entries` the retained full records. The flags make it
/// fail like a real store: `fail` loses the whole commit, `fail_entry` loses the record
/// write, `tear` is a FAULTY store that keeps the record but loses the metadata, `limit` is
/// the total record capacity in bytes.
#[derive(Clone, Default)]
struct Mem {
    bytes: Rc<RefCell<Option<Vec<u8>>>>,
    entries: Rc<RefCell<BTreeMap<u64, Vec<u8>>>>,
    fail: Rc<Cell<bool>>,
    fail_entry: Rc<Cell<bool>>,
    tear: Rc<Cell<bool>>,
    limit: Rc<Cell<Option<usize>>>,
}
impl LogStore for Mem {
    fn load_state(&mut self) -> Result<Option<Vec<u8>>, StoreError> {
        Ok(self.bytes.borrow().clone())
    }
    fn load_entry(&mut self, index: u64) -> Result<Option<Vec<u8>>, StoreError> {
        Ok(self.entries.borrow().get(&index).cloned())
    }
    fn commit(&mut self, entry: Option<(u64, &[u8])>, state: &[u8]) -> Result<(), StoreError> {
        if self.fail.get() {
            return Err(StoreError::Failed);
        }
        if let Some((index, record)) = entry {
            if self.fail_entry.get() {
                return Err(StoreError::Failed);
            }
            let existing = self.entries.borrow().get(&index).cloned();
            match existing {
                Some(old) if old.as_slice() != record => return Err(StoreError::Conflict),
                Some(_) => {}
                None => {
                    let used: usize = self.entries.borrow().values().map(Vec::len).sum();
                    if self
                        .limit
                        .get()
                        .is_some_and(|limit| used + record.len() > limit)
                    {
                        return Err(StoreError::Full);
                    }
                    self.entries.borrow_mut().insert(index, record.to_vec());
                }
            }
            if self.tear.get() {
                return Err(StoreError::Failed);
            }
        }
        *self.bytes.borrow_mut() = Some(state.to_vec());
        Ok(())
    }
}
fn persisted(mem: &Mem) -> State {
    decode(&mem.bytes.borrow().clone().unwrap()).unwrap()
}

struct Net {
    leader: Rc<Key>,
    a: Rc<Key>,
    b: Rc<Key>,
    set: AuthoritySet,
    set_sha: String,
    base: Tip,
}
fn net() -> Net {
    let leader = Key::new(1);
    let (mut a, mut b) = (Key::new(2), Key::new(3));
    if a.id.host_id > b.id.host_id {
        std::mem::swap(&mut a, &mut b);
    }
    let body = SetBody {
        schema: 1,
        pin: RoutePin {
            server_id: SERVER.into(),
            host: leader.id.clone(),
            term: 1,
        },
        epoch: 1,
        candidates: [a.id.clone(), b.id.clone()],
    };
    let signature = leader.sign(&body.message());
    let set = AuthoritySet { body, signature };
    let set_sha = set_digest(&set);
    Net {
        leader: Rc::new(leader),
        a: Rc::new(a),
        b: Rc::new(b),
        set,
        set_sha,
        base: Tip {
            term: 1,
            index: 0,
            digest: sha256_hex(b"seed snapshot"),
        },
    }
}
fn consent(n: &Net, who: &Key) -> CandidateAccept {
    let body = AcceptBody {
        schema: 1,
        server_id: SERVER.into(),
        set_sha256: n.set_sha.clone(),
        epoch: 1,
        candidate: who.id.clone(),
        recipient_key: "A".repeat(43),
        full_data: true,
        takeover: true,
    };
    let signature = who.sign(&body.message());
    CandidateAccept { body, signature }
}
type Node = Voter<Mem, Test>;
fn node(n: &Net, key: &Rc<Key>, with_consent: bool) -> (Node, Mem) {
    let mem = Mem::default();
    let accept = with_consent.then(|| consent(n, key));
    let voter = Voter::initialize(
        mem.clone(),
        Test(key.clone()),
        n.set.clone(),
        accept,
        n.base.clone(),
    )
    .unwrap();
    (voter, mem)
}
fn trio(n: &Net) -> (Node, Node, Node, Mem, Mem, Mem) {
    let (l, lm) = node(n, &n.leader, false);
    let (a, am) = node(n, &n.a, true);
    let (b, bm) = node(n, &n.b, true);
    (l, a, b, lm, am, bm)
}
fn p(text: &str) -> Payload {
    Payload::new(text).unwrap()
}
fn ack(answer: AppendAnswer) -> Ack {
    match answer {
        AppendAnswer::Ack(ack) => ack,
        other => panic!("expected an ack, got {other:?}"),
    }
}
fn refused(answer: AppendAnswer) -> Nack {
    match answer {
        AppendAnswer::Nack(nack) => nack,
        other => panic!("expected a nack, got {other:?}"),
    }
}
fn granted(answer: VoteAnswer) -> Vote {
    match answer {
        VoteAnswer::Granted(vote) => vote,
        other => panic!("expected a vote, got {other:?}"),
    }
}
fn declined(answer: VoteAnswer) -> Nack {
    match answer {
        VoteAnswer::Refused(nack) => nack,
        other => panic!("expected a refusal, got {other:?}"),
    }
}
/// An entry signed by the term's leader, built by hand (also used to make an equivocating leader).
fn entry(n: &Net, key: &Key, term: u64, index: u64, previous: &str, payload: &str) -> LogEntry {
    let body = EntryBody {
        schema: 1,
        server_id: SERVER.into(),
        set_sha256: n.set_sha.clone(),
        epoch: 1,
        term,
        index,
        previous: previous.into(),
        leader: key.id.clone(),
        payload: p(payload),
    };
    let mut entry = LogEntry {
        body,
        signature: String::new(),
    };
    entry.signature = key.sign(&entry.signing_message());
    entry
}

#[test]
fn only_verified_set_consent_and_two_distinct_bound_members_grant_a_commit() {
    let n = net();
    // Set and consent validation. A tampered, duplicated or unsorted set never initializes.
    let init = |set: AuthoritySet, key: &Rc<Key>, accept: Option<CandidateAccept>| {
        Voter::initialize(
            Mem::default(),
            Test(key.clone()),
            set,
            accept,
            n.base.clone(),
        )
        .err()
    };
    let mut tampered = n.set.clone();
    tampered.body.epoch = 2;
    assert!(matches!(
        init(tampered, &n.leader, None),
        Some(Refused::BadSignature(_))
    ));
    let mut duplicate = n.set.body.clone();
    duplicate.candidates[0] = n.leader.id.clone();
    let signature = n.leader.sign(&duplicate.message());
    let duplicate = AuthoritySet {
        body: duplicate,
        signature,
    };
    assert!(matches!(
        init(duplicate, &n.leader, None),
        Some(Refused::Invalid(_))
    ));
    let mut unsorted = n.set.body.clone();
    unsorted.candidates.swap(0, 1);
    let signature = n.leader.sign(&unsorted.message());
    let unsorted = AuthoritySet {
        body: unsorted,
        signature,
    };
    assert!(matches!(
        init(unsorted, &n.leader, None),
        Some(Refused::Invalid(_))
    ));
    assert!(
        matches!(init(n.set.clone(), &n.a, None), Some(Refused::Mismatch(_))),
        "a candidate needs its own consent"
    );
    assert!(matches!(
        init(n.set.clone(), &n.leader, Some(consent(&n, &n.a))),
        Some(Refused::Mismatch(_))
    ));
    assert!(matches!(
        init(n.set.clone(), &n.b, Some(consent(&n, &n.a))),
        Some(Refused::Mismatch(_) | Refused::NotCandidate)
    ));
    let outsider = Rc::new(Key::new(9));
    assert_eq!(
        init(n.set.clone(), &outsider, None),
        Some(Refused::NotMember)
    );

    let (mut l, mut a, mut b, ..) = trio(&n);
    let (e1, own) = l.propose(p("AAAA")).unwrap();
    let ack_a = ack(a.append(&e1).unwrap());
    assert_eq!(l.commit_gate(&e1, &[ack_a.clone()], &[]), Decision::Commit);
    assert_eq!(
        l.commit_gate(&e1, &[], &[]),
        Decision::Rollback(Why::Insufficient)
    );
    // The leader's own ack, even repeated, is the local append, never a second member.
    assert_eq!(
        l.commit_gate(&e1, &[own.clone(), own.clone()], &[]),
        Decision::Rollback(Why::Insufficient)
    );
    // Forged: a's body signed by another member, or by an outsider presenting a valid signature.
    let mut forged = ack_a.clone();
    forged.signature = n.b.sign(&forged.body.message());
    assert_eq!(
        l.commit_gate(&e1, &[forged], &[]),
        Decision::Rollback(Why::Insufficient)
    );
    let mut body = ack_a.body.clone();
    body.voter = outsider.id.clone();
    let foreign = Ack {
        signature: outsider.sign(&body.message()),
        body,
    };
    assert_eq!(
        l.commit_gate(&e1, &[foreign], &[]),
        Decision::Rollback(Why::Insufficient)
    );
    // Correctly signed by a, but bound to something else.
    let resigned = |change: &dyn Fn(&mut AckBody)| {
        let mut body = ack_a.body.clone();
        change(&mut body);
        Ack {
            signature: n.a.sign(&body.message()),
            body,
        }
    };
    for mismatched in [
        resigned(&|b| b.digest = sha256_hex(b"another entry")),
        resigned(&|b| b.index = 2),
        resigned(&|b| b.term = 2),
        resigned(&|b| b.epoch = 2),
        resigned(&|b| b.set_sha256 = sha256_hex(b"another set")),
        resigned(&|b| b.server_id = "0a1b2c3d-0000-4000-8000-0000000000bb".into()),
    ] {
        assert_eq!(
            l.commit_gate(&e1, &[mismatched], &[]),
            Decision::Rollback(Why::Insufficient)
        );
    }
    // A stale ack (for e1) never commits the next entry.
    let (e2, _) = l.propose(p("BBBB")).unwrap();
    assert_eq!(
        l.commit_gate(&e2, &[ack_a.clone()], &[]),
        Decision::Rollback(Why::Insufficient)
    );
    // Without this host's own persisted append there is nothing to commit.
    assert_eq!(
        b.commit_gate(&e1, &[ack_a.clone()], &[]),
        Decision::Rollback(Why::NotPersisted)
    );
    // Too much evidence is refused outright.
    assert_eq!(
        l.commit_gate(&e1, &vec![ack_a.clone(); 17], &[]),
        Decision::Rollback(Why::Oversized)
    );
    // A nack whose signature does not verify changes nothing; a verified fence withholds the commit.
    b.start_election().unwrap();
    let fence = refused(b.append(&e1).unwrap());
    assert_eq!(
        (fence.body.reason, fence.body.promised_term),
        (NackReason::Fenced, 2)
    );
    let unverified = Nack {
        signature: n.a.sign(&fence.body.message()),
        body: fence.body.clone(),
    };
    assert_eq!(
        l.commit_gate(&e1, &[ack_a.clone()], &[unverified]),
        Decision::Commit
    );
    assert_eq!(
        l.commit_gate(&e1, &[ack_a], &[fence]),
        Decision::Rollback(Why::Fenced)
    );
}

#[test]
fn state_is_durable_before_any_reply_and_a_failing_store_returns_no_authority() {
    let n = net();
    let (mut l, mut a, mut b, lm, am, bm) = trio(&n);
    let (e1, _) = l.propose(p("AAAA")).unwrap();
    assert_eq!(
        persisted(&lm).tip.index,
        1,
        "the proposal was stored before it was returned"
    );
    let ack1 = ack(a.append(&e1).unwrap());
    let stored = persisted(&am).tip;
    assert_eq!(
        (stored.index, stored.digest.clone()),
        (ack1.body.index, ack1.body.digest.clone())
    );

    let (e2, _) = l.propose(p("BBBB")).unwrap();
    am.fail.set(true);
    assert_eq!(a.append(&e2).err(), Some(Refused::Persist));
    assert!(a.is_poisoned());
    assert_eq!(a.tip().index, 1, "nothing was adopted in memory");
    am.fail.set(false);
    assert_eq!(
        a.append(&e2).err(),
        Some(Refused::Poisoned),
        "stopped until reloaded"
    );
    assert_eq!(
        a.commit_gate(&e1, &[], &[]),
        Decision::Rollback(Why::Poisoned)
    );
    assert_eq!(persisted(&am).tip.index, 1);
    let mut a = Voter::open(am.clone(), Test(n.a.clone())).unwrap();
    assert_eq!(a.tip().index, 1);
    ack(a.append(&e2).unwrap());

    // A vote is stored before it is returned; a failed store leaves the promise unmoved.
    bm.fail.set(true);
    assert_eq!(b.start_election().err(), Some(Refused::Persist));
    assert_eq!(b.promised_term(), 1);
    bm.fail.set(false);
    let b = Voter::open(bm.clone(), Test(n.b.clone())).unwrap();
    assert_eq!(b.promised_term(), 1);

    // Losing the FULL-ENTRY write is the same: no ack, nothing adopted, nothing retained.
    let (e3, _) = l.propose(p("CCCC")).unwrap();
    am.fail_entry.set(true);
    assert_eq!(a.append(&e3).err(), Some(Refused::Persist));
    assert!(a.is_poisoned());
    assert!(!am.entries.borrow().contains_key(&3));
    assert_eq!(persisted(&am).tip.index, 2);
    am.fail_entry.set(false);
    let mut a = Voter::open(am.clone(), Test(n.a.clone())).unwrap();
    assert_eq!(a.tip().index, 2);
    ack(a.append(&e3).unwrap());
    assert!(
        am.entries.borrow().contains_key(&3),
        "the ack came with the retained record"
    );
}

#[test]
fn append_needs_the_exact_position_and_replay_gap_divergence_and_a_higher_promise_refuse() {
    let n = net();
    let (mut l, mut a, mut b, _, am, _) = trio(&n);
    let (e1, _) = l.propose(p("AAAA")).unwrap();
    let (e2, _) = l.propose(p("BBBB")).unwrap();
    let (e3, _) = l.propose(p("CCCC")).unwrap();
    let first = ack(a.append(&e1).unwrap());
    assert_eq!(
        ack(a.append(&e1).unwrap()),
        first,
        "an identical replay returns the identical ack"
    );
    // Gap: nothing changes.
    let before = am.bytes.borrow().clone();
    assert_eq!(refused(a.append(&e3).unwrap()).body.reason, NackReason::Gap);
    assert_eq!(*am.bytes.borrow(), before);
    ack(a.append(&e2).unwrap());
    // An equivocating leader: another valid entry for an accepted position is a divergence.
    let rival = entry(&n, &n.leader, 1, 2, &e1.body.previous, "DDDD");
    let rival_at_two = entry(&n, &n.leader, 1, 2, &first.body.digest, "DDDD");
    assert_eq!(
        refused(a.append(&rival).unwrap()).body.reason,
        NackReason::Divergent
    );
    assert_eq!(
        refused(a.append(&rival_at_two).unwrap()).body.reason,
        NackReason::Divergent
    );
    let wrong_previous = entry(&n, &n.leader, 1, 3, &sha256_hex(b"not the tip"), "EEEE");
    assert_eq!(
        refused(a.append(&wrong_previous).unwrap()).body.reason,
        NackReason::Divergent
    );
    assert_eq!(a.tip().index, 2);
    // Unsigned or foreign-signed entries are errors, not nacks.
    let mut forged = e3.clone();
    forged.signature = n.b.sign(&forged.signing_message());
    assert!(matches!(a.append(&forged), Err(Refused::BadSignature(_))));
    // Candidate b, level with a, opens term 2; a grants it. Every older append and replay is
    // then fenced.
    ack(b.append(&e1).unwrap());
    ack(b.append(&e2).unwrap());
    let (request, _) = b.start_election().unwrap();
    let vote = granted(a.grant_vote(&request).unwrap());
    assert_eq!((vote.body.term, &vote.body.voter), (2, &n.a.id));
    assert_eq!(a.promised_term(), 2);
    let fenced = refused(a.append(&e3).unwrap());
    assert_eq!(
        (fenced.body.reason, fenced.body.promised_term),
        (NackReason::Fenced, 2)
    );
    assert_eq!(
        refused(a.append(&e1).unwrap()).body.reason,
        NackReason::Fenced,
        "no replayed ack after a higher promise"
    );
    assert_eq!(a.tip().index, 2);
    // A later-term entry from a leader this host has not promised is not accepted either.
    let (late, _) = b.propose(p("FFFF")).unwrap();
    assert_eq!(
        refused(l.append(&late).unwrap()).body.reason,
        NackReason::UnknownTerm
    );
}

#[test]
fn votes_are_one_per_term_idempotent_and_refuse_stale_or_conflicting_candidate_tips() {
    let n = net();
    let (mut l, mut a, mut b, lm, ..) = trio(&n);
    let (e1, _) = l.propose(p("AAAA")).unwrap();
    let (e2, _) = l.propose(p("BBBB")).unwrap();
    ack(a.append(&e1).unwrap());
    ack(b.append(&e1).unwrap());
    ack(b.append(&e2).unwrap());
    // a is one entry behind b and behind the leader: stale, and nothing is promised.
    let (request_a, self_vote) = a.start_election().unwrap();
    assert_eq!(
        (self_vote.body.term, &self_vote.body.candidate),
        (2, &n.a.id)
    );
    assert_eq!(
        declined(b.grant_vote(&request_a).unwrap()).body.reason,
        NackReason::StaleTip
    );
    assert_eq!(
        declined(l.grant_vote(&request_a).unwrap()).body.reason,
        NackReason::StaleTip
    );
    assert_eq!((b.promised_term(), l.promised_term()), (1, 1));
    // b is ahead of a, but a already voted for itself in term 2: one vote per term.
    let (request_b, _) = b.start_election().unwrap();
    assert_eq!(request_b.body.term, 2);
    assert_eq!(
        declined(a.grant_vote(&request_b).unwrap()).body.reason,
        NackReason::AlreadyVoted
    );
    // The leader grants b; a retry returns the identical vote; a rival in the same term loses.
    let first = granted(l.grant_vote(&request_b).unwrap());
    assert_eq!(granted(l.grant_vote(&request_b).unwrap()), first);
    assert_eq!(persisted(&lm).promised_term, 2);
    // The vote states the binding it persisted. A retry that changes the candidate tip or the
    // consent is never reported as that request; the original still gets its identical vote.
    assert_eq!(first.body.candidate_tip, request_b.body.tip);
    assert_eq!(
        first.body.accept_sha256,
        accept_digest(&request_b.body.accept)
    );
    let mut other_tip = request_b.clone();
    other_tip.body.tip = Tip {
        term: 1,
        index: 2,
        digest: sha256_hex(b"another chain"),
    };
    other_tip.signature = n.b.sign(&other_tip.body.message());
    assert_eq!(
        declined(l.grant_vote(&other_tip).unwrap()).body.reason,
        NackReason::ChangedRequest
    );
    let mut other_consent = request_b.clone();
    let mut accept = other_consent.body.accept.body.clone();
    accept.recipient_key = format!("{}A", "B".repeat(42));
    other_consent.body.accept = CandidateAccept {
        signature: n.b.sign(&accept.message()),
        body: accept,
    };
    other_consent.signature = n.b.sign(&other_consent.body.message());
    assert_eq!(
        declined(l.grant_vote(&other_consent).unwrap()).body.reason,
        NackReason::ChangedRequest
    );
    assert_eq!(granted(l.grant_vote(&request_b).unwrap()), first);
    assert_eq!(
        declined(l.grant_vote(&request_a).unwrap()).body.reason,
        NackReason::AlreadyVoted
    );
    // Once a later term is promised, the earlier request is fenced.
    let (later, _) = b.start_election().unwrap();
    assert_eq!(later.body.term, 3);
    granted(l.grant_vote(&later).unwrap());
    assert_eq!(
        declined(l.grant_vote(&request_b).unwrap()).body.reason,
        NackReason::Fenced
    );
    // A forged request, or one whose consent belongs to another candidate, is an error.
    let mut forged = later.clone();
    forged.signature = n.a.sign(&forged.body.message());
    assert!(matches!(
        l.grant_vote(&forged),
        Err(Refused::BadSignature(_))
    ));
    let mut stolen = request_a.clone();
    stolen.body.accept = consent(&n, &n.b);
    stolen.signature = n.a.sign(&stolen.body.message());
    assert!(l.grant_vote(&stolen).is_err());

    // Same position, different digest: a leader that equivocated at index 1.
    let m = net();
    let (mut l2, mut a2, mut b2, ..) = trio(&m);
    let (x, _) = l2.propose(p("AAAA")).unwrap();
    let y = entry(&m, &m.leader, 1, 1, &m.base.digest, "ZZZZ");
    ack(a2.append(&x).unwrap());
    ack(b2.append(&y).unwrap());
    let (request, _) = a2.start_election().unwrap();
    assert_eq!(
        declined(b2.grant_vote(&request).unwrap()).body.reason,
        NackReason::Divergent
    );
    assert_eq!(b2.promised_term(), 1);
}

#[test]
fn loading_refuses_missing_corrupt_tampered_and_mismatched_state_and_never_reinitializes() {
    let n = net();
    let (mut l, _, mut b, lm, _, bm) = trio(&n);
    l.propose(p("AAAA")).unwrap();
    assert_eq!(
        Voter::open(Mem::default(), Test(n.leader.clone())).err(),
        Some(Refused::Missing)
    );
    // A journal that holds anything is never initialized over, valid or not.
    let again = Voter::initialize(
        lm.clone(),
        Test(n.leader.clone()),
        n.set.clone(),
        None,
        n.base.clone(),
    );
    assert_eq!(again.err(), Some(Refused::AlreadyInitialized));
    let junk = Mem::default();
    *junk.bytes.borrow_mut() = Some(b"not a journal".to_vec());
    assert_eq!(
        Voter::initialize(
            junk.clone(),
            Test(n.leader.clone()),
            n.set.clone(),
            None,
            n.base.clone()
        )
        .err(),
        Some(Refused::AlreadyInitialized)
    );
    assert!(matches!(
        Voter::open(junk.clone(), Test(n.leader.clone())).err(),
        Some(Refused::Corrupt(_))
    ));
    assert_eq!(
        &junk.bytes.borrow().clone().unwrap()[..],
        b"not a journal",
        "nothing was repaired"
    );
    // A flipped byte, a truncation and an oversized file are refused.
    let good = lm.bytes.borrow().clone().unwrap();
    let flipped = Mem::default();
    let mut bytes = good.clone();
    let at = bytes
        .windows(15)
        .position(|w| w == b"\"promised_term\"")
        .unwrap()
        + 16;
    bytes[at] = if bytes[at] == b'1' { b'2' } else { b'1' };
    *flipped.bytes.borrow_mut() = Some(bytes);
    assert!(matches!(
        Voter::open(flipped, Test(n.leader.clone())).err(),
        Some(Refused::Corrupt(_))
    ));
    let cut = Mem::default();
    *cut.bytes.borrow_mut() = Some(good[..good.len() / 2].to_vec());
    assert!(matches!(
        Voter::open(cut, Test(n.leader.clone())).err(),
        Some(Refused::Corrupt(_))
    ));
    let huge = Mem::default();
    *huge.bytes.borrow_mut() = Some(vec![b' '; MAX_STATE_BYTES + 1]);
    assert!(matches!(
        Voter::open(huge, Test(n.leader.clone())).err(),
        Some(Refused::Corrupt(_))
    ));
    // A self-consistent file whose set was altered still fails: the leader's signature breaks.
    let mut state = persisted(&lm);
    state.set.body.epoch = 2;
    let altered = Mem::default();
    *altered.bytes.borrow_mut() = Some(encode(&state).unwrap());
    assert!(matches!(
        Voter::open(altered, Test(n.leader.clone())).err(),
        Some(Refused::BadSignature(_))
    ));
    // Another host's key never reads this host's state.
    assert_eq!(
        Voter::open(lm.clone(), Test(n.a.clone())).err(),
        Some(Refused::Mismatch("host identity"))
    );
    // A good file round-trips, and a promise made before the restart still fences afterwards.
    let (request, _) = b.start_election().unwrap();
    assert_eq!(request.body.term, 2);
    let mut reopened = Voter::open(bm.clone(), Test(n.b.clone())).unwrap();
    assert_eq!(reopened.promised_term(), 2);
    let (older, _) = l.propose(p("BBBB")).unwrap();
    assert_eq!(
        refused(reopened.append(&older).unwrap()).body.reason,
        NackReason::Fenced
    );
    let (next, _) = reopened.start_election().unwrap();
    assert_eq!(next.body.term, 3);
}

#[test]
fn the_exact_full_entry_is_retained_recoverable_and_verified_before_every_ack_and_commit() {
    let n = net();
    let (mut l, mut a, _, lm, am, _) = trio(&n);
    let big = "QUJD".repeat(30_000);
    let (e1, _) = l.propose(p(&big)).unwrap();
    // The complete signed entry, payload included, sits in the store; the metadata stays small.
    let record = serde_json::to_vec(&e1).unwrap();
    assert_eq!(lm.entries.borrow().get(&1).unwrap(), &record);
    assert!(record.len() > 100_000 && lm.bytes.borrow().as_ref().unwrap().len() < 32 * 1024);
    let first = ack(a.append(&e1).unwrap());
    assert_eq!(am.entries.borrow().get(&1).unwrap(), &record);
    // After a restart the whole opaque payload comes back byte for byte, and replay still acks.
    let mut a = Voter::open(am.clone(), Test(n.a.clone())).unwrap();
    let back = a.entry(1).unwrap();
    assert_eq!(back, e1);
    assert_eq!(back.body.payload.as_str(), big);
    assert_eq!(ack(a.append(&e1).unwrap()), first);
    assert_eq!(
        a.entry(0).err(),
        Some(Refused::Missing),
        "the base is not an entry"
    );
    assert_eq!(a.entry(2).err(), Some(Refused::Missing));
    // Missing: no replay ack, the voter stops, and a reload refuses.
    let saved = am.entries.borrow().get(&1).cloned().unwrap();
    am.entries.borrow_mut().remove(&1);
    assert_eq!(
        a.append(&e1).err(),
        Some(Refused::Corrupt("retained entry missing"))
    );
    assert!(a.is_poisoned());
    let reload = || Voter::open(am.clone(), Test(n.a.clone()));
    assert_eq!(
        reload().err(),
        Some(Refused::Corrupt("retained entry missing"))
    );
    // Corrupt: a changed payload character breaks the signature.
    let mut bad = saved.clone();
    let at = bad.len() / 2;
    bad[at] = if bad[at] == b'Q' { b'R' } else { b'Q' };
    am.entries.borrow_mut().insert(1, bad);
    assert!(matches!(reload().err(), Some(Refused::Corrupt(_))));
    // Not the canonical record (trailing bytes) is corrupt too.
    let mut spaced = saved.clone();
    spaced.push(b' ');
    am.entries.borrow_mut().insert(1, spaced);
    assert!(matches!(reload().err(), Some(Refused::Corrupt(_))));
    // Conflicting: a different, validly signed entry at the same index.
    let rival = entry(&n, &n.leader, 1, 1, &n.base.digest, "DDDD");
    am.entries
        .borrow_mut()
        .insert(1, serde_json::to_vec(&rival).unwrap());
    assert!(matches!(reload().err(), Some(Refused::Corrupt(_))));
    am.entries.borrow_mut().insert(1, saved);
    let mut a = reload().unwrap();
    // The commit gate rests on the retained record as well, never on a remembered position.
    let ack_a = ack(a.append(&e1).unwrap());
    assert_eq!(l.commit_gate(&e1, &[ack_a.clone()], &[]), Decision::Commit);
    lm.entries.borrow_mut().remove(&1);
    assert_eq!(
        l.commit_gate(&e1, &[ack_a], &[]),
        Decision::Rollback(Why::NotPersisted)
    );
}

#[test]
fn a_torn_or_full_store_returns_no_authority_and_the_metadata_never_grows_with_the_log() {
    let n = net();
    let (mut l, mut a, _, _, am, _) = trio(&n);
    // A faulty store that keeps the record but loses the metadata: no ack was ever issued.
    let (e1, _) = l.propose(p("AAAA")).unwrap();
    am.tear.set(true);
    assert_eq!(a.append(&e1).err(), Some(Refused::Persist));
    assert!(a.is_poisoned());
    assert!(am.entries.borrow().contains_key(&1));
    assert_eq!(persisted(&am).tip.index, 0);
    am.tear.set(false);
    // A different entry at the orphaned index conflicts and stops the voter; the identical one
    // is idempotent.
    let mut a = Voter::open(am.clone(), Test(n.a.clone())).unwrap();
    assert_eq!(a.tip().index, 0);
    let rival = entry(&n, &n.leader, 1, 1, &n.base.digest, "DDDD");
    assert_eq!(
        a.append(&rival).err(),
        Some(Refused::Corrupt("retained entry conflict"))
    );
    assert!(a.is_poisoned());
    let mut a = Voter::open(am.clone(), Test(n.a.clone())).unwrap();
    ack(a.append(&e1).unwrap());
    // Exhausted storage: nothing written, nothing adopted, no eviction, still usable.
    let (e2, _) = l.propose(p("BBBB")).unwrap();
    let used: usize = am.entries.borrow().values().map(Vec::len).sum();
    am.limit.set(Some(used));
    let before = am.bytes.borrow().clone();
    assert_eq!(a.append(&e2).err(), Some(Refused::Full));
    assert!(!a.is_poisoned() && a.tip().index == 1);
    assert_eq!(*am.bytes.borrow(), before);
    assert!(am.entries.borrow().contains_key(&1) && !am.entries.borrow().contains_key(&2));
    am.limit.set(None);
    ack(a.append(&e2).unwrap());
    // Metadata is bounded: the window stays at 64 positions while every entry stays retained.
    let (mut at_70, mut at_100) = (0, 0);
    for index in 3..=100u32 {
        let (e, _) = l.propose(p("CCCC")).unwrap();
        ack(a.append(&e).unwrap());
        let size = am.bytes.borrow().as_ref().unwrap().len();
        if index == 70 {
            at_70 = size;
        }
        at_100 = size;
    }
    assert!(at_100 <= at_70 + 64 && at_100 < MAX_STATE_BYTES);
    assert_eq!(persisted(&am).recent.len(), RECENT);
    assert_eq!(am.entries.borrow().len(), 100);
    assert_eq!(
        a.entry(1).unwrap(),
        e1,
        "an old acknowledged entry is still retrievable"
    );
    assert_eq!(a.entry(100).unwrap().body.index, 100);
    assert!(Voter::open(am.clone(), Test(n.a.clone())).is_ok());
}
