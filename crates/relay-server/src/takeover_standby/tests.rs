//! Own temporary SQLite files and fixed-seed synthetic keys only (fresh folders under the system
//! temp directory): no Relay profile, key, device, network or running server. The interruption is
//! a test-only injected I/O error after N page writes; no power loss is claimed. Written for the
//! later focused run; not executed when written.
use super::*;
use crate::takeover_wal_vfs::{
    CommitSink, Config as WalConfig, LocalCommit, Refusal, TxAssembler, WalTransaction, WalVfs,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use ed25519_dalek::{Signer as _, SigningKey};
use relay_protocol::{handover::RoutePin, takeover::SetBody};
use std::sync::{Arc, Mutex};

const SERVER: &str = "0a1b2c3d-0000-4000-8000-0000000000aa";
const LIMITS: Limits = Limits::DEFAULT;

struct Key {
    signing: SigningKey,
    id: HostIdentity,
}
fn key(seed: u8) -> Key {
    let signing = SigningKey::from_bytes(&[seed; 32]);
    let id = handover_target::identity(&signing.verifying_key());
    Key { signing, id }
}
/// The leader's signed set for `epoch`, and the two candidates in ascending host-id order.
fn authority(epoch: u64) -> (AuthoritySet, HostIdentity, HostIdentity) {
    let leader = key(1);
    let (mut a, mut b) = (key(2), key(3));
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
        epoch,
        candidates: [a.id.clone(), b.id.clone()],
    };
    let signature = URL_SAFE_NO_PAD.encode(leader.signing.sign(&body.message()).to_bytes());
    (AuthoritySet { body, signature }, a.id, b.id)
}
fn tip(n: u64) -> Tip {
    Tip {
        term: 1,
        index: n,
        digest: hex(&Sha256::digest(n.to_be_bytes())),
    }
}

/// Keeps every approved transaction as the bounded chunks the log would retain.
#[derive(Default)]
struct Capture {
    chunks: Mutex<Vec<Vec<Vec<u8>>>>,
}
impl CommitSink for Capture {
    fn approve(&self, tx: &WalTransaction) -> Result<(), Refusal> {
        let chunks: Vec<Vec<u8>> = tx.chunks(64 << 10)?.collect();
        self.chunks.lock().unwrap().push(chunks);
        Ok(())
    }
    fn local_committed(&self, _receipt: &LocalCommit) -> Result<(), Refusal> {
        Ok(())
    }
}
fn assemble(chunks: &[Vec<u8>]) -> AssembledTx {
    let mut assembler = TxAssembler::new(64 << 20);
    let mut done = None;
    for chunk in chunks {
        done = assembler.add(chunk).unwrap();
    }
    done.expect("the last chunk completes the transaction")
}
impl Capture {
    fn count(&self) -> usize {
        self.chunks.lock().unwrap().len()
    }
    /// The n-th (0-based) captured commit, rebuilt from its retained chunks.
    fn tx(&self, n: usize) -> AssembledTx {
        assemble(&self.chunks.lock().unwrap()[n])
    }
}
/// A synthetic one-chunk transaction, for shapes SQLite would never emit.
fn craft(page_size: u32, db_pages: u32, pages: &[(u32, Vec<u8>)]) -> AssembledTx {
    let mut chunk = b"RWTX".to_vec();
    chunk.extend_from_slice(&[1, 0, 0, 0]);
    chunk.extend_from_slice(&1u64.to_be_bytes());
    let count = pages.len() as u32;
    for word in [page_size, 0, 1, 2, 1, db_pages, count, 0, 1, count, 0] {
        chunk.extend_from_slice(&word.to_be_bytes());
    }
    chunk.extend_from_slice(&[0; 4]);
    for (pgno, page) in pages {
        chunk.extend_from_slice(&pgno.to_be_bytes());
        chunk.extend_from_slice(page);
    }
    assemble(&[chunk])
}

const SCHEMA: &str = "
    PRAGMA auto_vacuum = FULL;
    PRAGMA journal_mode = WAL;
    CREATE TABLE messages(id TEXT PRIMARY KEY, content TEXT NOT NULL);
    CREATE TABLE bulk(id INTEGER PRIMARY KEY, data BLOB NOT NULL);
    INSERT INTO messages VALUES ('seed', 'seed message');
    PRAGMA wal_checkpoint(TRUNCATE);";
const GROW: &str = "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 60) \
                    INSERT INTO bulk SELECT i, randomblob(3000) FROM n;";

/// A real captured source, and a backup-derived own standby seed in its private root.
struct World {
    _dir: tempfile::TempDir,
    source: PathBuf,
    root: PathBuf,
    vfs: WalVfs,
    conn: Connection,
    capture: Arc<Capture>,
    set: AuthoritySet,
    member: HostIdentity,
}
fn world() -> World {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source.sqlite3");
    {
        let conn = Connection::open(&source).unwrap();
        conn.execute_batch(SCHEMA).unwrap();
    }
    let capture = Arc::new(Capture::default());
    let vfs = WalVfs::register(&source, WalConfig::DEFAULT, capture.clone()).unwrap();
    let conn = vfs
        .open_connection(OpenFlags::SQLITE_OPEN_READ_WRITE)
        .unwrap();
    let root = dir.path().join("standby");
    fs::create_dir_all(&root).unwrap();
    assert_eq!(vfs.snapshot(&conn, &root.join(IMAGE_NAME)).unwrap(), 0);
    let (set, member, _) = authority(1);
    World {
        _dir: dir,
        source,
        root,
        vfs,
        conn,
        capture,
        set,
        member,
    }
}
impl World {
    fn create(&self) -> Standby {
        Standby::create_new(&self.root, &self.set, &self.member, &tip(0), LIMITS).unwrap()
    }
    fn reopen(&self) -> Result<Standby, StandbyError> {
        Standby::open_existing(&self.root, &self.set, &self.member, LIMITS)
    }
    fn image(&self) -> PathBuf {
        self.root.join(IMAGE_NAME)
    }
    /// Runs one statement batch that is exactly one real commit and returns its index.
    fn commit(&self, sql: &str) -> usize {
        let before = self.capture.count();
        self.conn.execute_batch(sql).unwrap();
        assert_eq!(self.capture.count(), before + 1, "exactly one commit");
        before
    }
}

fn dump(path: &Path) -> Vec<String> {
    let conn = Connection::open(path).unwrap();
    let check: String = conn
        .query_row("PRAGMA quick_check", [], |r| r.get(0))
        .unwrap();
    assert_eq!(check, "ok");
    let mut out = Vec::new();
    for sql in [
        "SELECT id || '|' || content FROM messages ORDER BY id",
        "SELECT id || '|' || length(data) || '|' || hex(substr(data, 1, 8)) FROM bulk ORDER BY id",
    ] {
        let mut statement = conn.prepare(sql).unwrap();
        let rows = statement.query_map([], |r| r.get::<_, String>(0)).unwrap();
        for row in rows {
            out.push(row.unwrap());
        }
    }
    let pages: i64 = conn
        .query_row("PRAGMA page_count", [], |r| r.get(0))
        .unwrap();
    out.push(format!("pages {pages}"));
    out
}
/// The standby's logical content, read from a COPY (opening the image itself would write a WAL
/// beside it), against the source's content after its connection and wrapper are gone.
fn assert_same_content(w: World) {
    let World {
        _dir,
        source,
        root,
        vfs,
        conn,
        ..
    } = w;
    drop(conn);
    drop(vfs);
    let copy = _dir.path().join("compare.sqlite3");
    fs::copy(root.join(IMAGE_NAME), &copy).unwrap();
    assert_eq!(dump(&copy), dump(&source));
}

#[test]
fn incremental_apply_of_real_captured_commits_reproduces_the_source_and_verifies_once() {
    let w = world();
    let mut standby = w.create();
    assert_eq!(standby.applied(), &tip(0));
    assert!(standby.recovery_needed().is_none());
    let seed_pages = standby.pages();
    let script = [
        "BEGIN; INSERT INTO messages VALUES ('m1', 'one'); \
         INSERT INTO messages VALUES ('m2', 'two'); COMMIT;",
        "UPDATE messages SET content = 'one-changed' WHERE id = 'm1';",
        GROW,
        "DELETE FROM bulk WHERE id % 2 = 0;",
        "DELETE FROM bulk;",
        "INSERT INTO messages VALUES ('m3', 'three');",
    ];
    let mut grown = 0;
    for (i, sql) in script.iter().enumerate() {
        let n = w.commit(sql);
        assert_eq!(n, i);
        let step = i as u64;
        standby
            .apply(&tip(step), &tip(step + 1), &w.capture.tx(n))
            .unwrap();
        assert_eq!(standby.applied(), &tip(step + 1));
        assert!(standby.recovery_needed().is_none());
        if i == 2 {
            grown = standby.pages();
            assert!(grown > seed_pages, "the bulk insert grows the image");
        }
        if i == 4 {
            assert!(standby.pages() < grown, "freed pages shrink the final size");
        }
    }
    // The explicit final verification: the exact tip only, once, leaving no scratch or side file.
    assert_eq!(standby.verify(&tip(5)), Err(StandbyError::Parent));
    let verified = standby.verify(&tip(6)).unwrap();
    assert_eq!(
        (verified.tip.index, verified.pages, verified.page_size),
        (6, standby.pages(), standby.page_size())
    );
    assert!(!w.root.join(VERIFY_COPY).exists());
    for suffix in ["-wal", "-shm", "-journal"] {
        assert!(!sidecar(&w.image(), suffix).exists());
    }
    let debug = format!("{standby:?}");
    assert!(!debug.contains(&tip(6).digest) && !debug.contains("standby-image"));
    drop(standby);
    // A re-open sees exactly the applied tip and nothing pending.
    let again = w.reopen().unwrap();
    assert_eq!((again.applied(), again.pages()), (&tip(6), verified.pages));
    assert!(again.recovery_needed().is_none());
    drop(again);
    assert_same_content(w);
}

#[test]
fn an_interrupted_apply_refuses_readiness_and_resumes_only_with_the_exact_payload() {
    let w = world();
    let mut standby = w.create();
    let first = w.commit("INSERT INTO messages VALUES ('m1', 'one');");
    standby
        .apply(&tip(0), &tip(1), &w.capture.tx(first))
        .unwrap();
    let big = w.commit(GROW);
    let tx = w.capture.tx(big);
    assert!(tx.frames > 4);

    // An I/O error after two page writes: the pending record is already durable.
    standby.interrupt_after = Some(2);
    assert_eq!(
        standby.apply(&tip(1), &tip(2), &tx),
        Err(StandbyError::Failed)
    );
    let pending = standby.recovery_needed().expect("pending stays");
    assert_eq!((&pending.parent, &pending.end), (&tip(1), &tip(2)));
    assert_eq!(standby.applied(), &tip(1), "no fallback, no advance");
    assert_eq!(
        standby.apply(&tip(1), &tip(2), &tx),
        Err(StandbyError::RecoveryNeeded)
    );
    for expected in [tip(1), tip(2)] {
        assert_eq!(standby.verify(&expected), Err(StandbyError::RecoveryNeeded));
    }
    drop(standby);

    // Simulate interruption after the new page-1 header but in the middle of growth.
    // The old image is no longer a valid SQLite file; redo must not demand that it is.
    {
        use std::io::{Seek, SeekFrom, Write};
        let mut image = fs::OpenOptions::new().write(true).open(w.image()).unwrap();
        let page_one = tx.pages().find(|(pgno, _)| *pgno == 1).unwrap().1;
        image.seek(SeekFrom::Start(0)).unwrap();
        image.write_all(page_one).unwrap();
        let length = image.metadata().unwrap().len();
        image.set_len(length + 7).unwrap();
        image.sync_all().unwrap();
    }
    // After a restart the recovery need is reported again and the image is not opened.
    let mut standby = w.reopen().unwrap();
    assert_eq!(standby.recovery_needed().as_ref(), Some(&pending));
    assert_eq!(standby.verify(&tip(1)), Err(StandbyError::RecoveryNeeded));
    // Wrong tips and an altered payload never clear it.
    assert_eq!(
        standby.recover(&tip(1), &tip(3), &tx),
        Err(StandbyError::WrongRecovery)
    );
    assert_eq!(
        standby.recover(&tip(0), &tip(2), &tx),
        Err(StandbyError::WrongRecovery)
    );
    let mut altered = w.capture.chunks.lock().unwrap()[big].clone();
    *altered.last_mut().unwrap().last_mut().unwrap() ^= 1;
    assert_eq!(
        standby.recover(&tip(1), &tip(2), &assemble(&altered)),
        Err(StandbyError::WrongRecovery)
    );
    assert_eq!(standby.recovery_needed().as_ref(), Some(&pending));
    // The exact same transaction finishes it, idempotently over the half-written pages.
    standby
        .recover(&tip(1), &tip(2), &w.capture.tx(big))
        .unwrap();
    assert_eq!(standby.applied(), &tip(2));
    assert!(standby.recovery_needed().is_none());
    assert_eq!(
        standby.recover(&tip(1), &tip(2), &tx),
        Err(StandbyError::NothingPending)
    );
    // Normal incremental work continues, and the end state is the source's.
    let last = w.commit("DELETE FROM bulk WHERE id > 10;");
    standby
        .apply(&tip(2), &tip(3), &w.capture.tx(last))
        .unwrap();
    standby.verify(&tip(3)).unwrap();
    drop(standby);
    assert_same_content(w);
}

#[test]
fn refusals_keep_the_image_and_record_and_the_final_check_refuses_an_altered_image() {
    let w = world();
    let mut standby = w.create();
    let image = w.image();
    let seed = fs::read(&image).unwrap();
    let ps = standby.page_size() as usize;
    let page = vec![7u8; ps];
    let real = w.commit("INSERT INTO messages VALUES ('m1', 'one');");
    let real = w.capture.tx(real);

    // Transaction and tip refusals happen before any record or image change.
    let lower = Tip { term: 2, ..tip(0) };
    let ps32 = ps as u32;
    let cases = [
        (tip(5), tip(6), real_clone(&w), StandbyError::Parent),
        (
            tip(0),
            tip(0),
            real_clone(&w),
            StandbyError::Invalid("tip order"),
        ),
        (
            lower,
            tip(1),
            real_clone(&w),
            StandbyError::Invalid("tip order"),
        ),
        (
            tip(0),
            tip(1),
            craft(1024, 4, &[(1, vec![0; 1024])]),
            StandbyError::Invalid("page size"),
        ),
        (
            tip(0),
            tip(1),
            craft(ps32, 1 << 30, &[(1, page.clone())]),
            StandbyError::Quota,
        ),
        (
            tip(0),
            tip(1),
            craft(ps32, 4, &[(0, page.clone())]),
            StandbyError::Invalid("transaction page"),
        ),
        (
            tip(0),
            tip(1),
            craft(ps32, 4, &[(0x8000_0000, page.clone())]),
            StandbyError::Invalid("transaction page"),
        ),
    ];
    for (parent, end, tx, expected) in &cases {
        assert_eq!(standby.apply(parent, end, tx), Err(*expected));
    }
    let tight = Limits {
        max_frames: 8,
        ..LIMITS
    };
    let many: Vec<(u32, Vec<u8>)> = (1..=9).map(|n| (n, page.clone())).collect();
    drop(standby);
    // The frame bound is a configuration of the open standby.
    let mut small = Standby::open_existing(&w.root, &w.set, &w.member, tight).unwrap();
    assert_eq!(
        small.apply(&tip(0), &tip(1), &craft(ps32, 4, &many)),
        Err(StandbyError::Quota)
    );
    drop(small);
    let mut standby = w.reopen().unwrap();
    assert_eq!(fs::read(&image).unwrap(), seed);
    assert!(standby.recovery_needed().is_none());
    assert_eq!(standby.applied(), &tip(0));

    // An obsolete spilled page beyond the final size neither grows nor changes the image.
    let pages = standby.pages();
    let obsolete = craft(
        ps32,
        pages,
        &[(1, seed[..ps].to_vec()), (pages + 3, page.clone())],
    );
    standby.apply(&tip(0), &tip(1), &obsolete).unwrap();
    assert_eq!(fs::read(&image).unwrap(), seed);
    // The real captured commit (taken against the same state) still applies after all that.
    standby.apply(&tip(1), &tip(2), &real).unwrap();
    standby.verify(&tip(2)).unwrap();
    drop(standby);
    let good = fs::read(&image).unwrap();

    // Context mismatches: another epoch, another member, a stranger, a forged signature.
    let (other_set, _, other_member) = authority(2);
    let stranger = key(9).id;
    let mut forged = w.set.clone();
    forged.signature = other_set.signature.clone();
    for (set, member, expected) in [
        (&other_set, &w.member, StandbyError::Mismatch),
        (&w.set, &other_member, StandbyError::Mismatch),
        (&w.set, &stranger, StandbyError::Invalid("member")),
        (
            &forged,
            &w.member,
            StandbyError::Invalid("authority set signature"),
        ),
    ] {
        assert_eq!(
            Standby::open_existing(&w.root, set, member, LIMITS).err(),
            Some(expected)
        );
    }
    // Storage: a configured bound the image exceeds, an existing standby, missing pieces,
    // side files and a wrong-sized image are refused and nothing is repaired or replaced.
    let tiny = Limits {
        max_image_bytes: 512,
        ..LIMITS
    };
    assert_eq!(
        Standby::open_existing(&w.root, &w.set, &w.member, tiny).err(),
        Some(StandbyError::Quota)
    );
    assert_eq!(
        Standby::create_new(&w.root, &w.set, &w.member, &tip(0), LIMITS).err(),
        Some(StandbyError::AlreadyExists)
    );
    let moved = w.root.join("moved");
    fs::rename(&image, &moved).unwrap();
    assert_eq!(w.reopen().err(), Some(StandbyError::Missing));
    fs::rename(&moved, &image).unwrap();
    let control = w.root.join(CONTROL_DIR);
    fs::rename(&control, &moved).unwrap();
    assert_eq!(w.reopen().err(), Some(StandbyError::Missing));
    fs::rename(&moved, &control).unwrap();
    for suffix in ["-wal", "-shm", "-journal"] {
        let side = sidecar(&image, suffix);
        fs::write(&side, b"x").unwrap();
        assert_eq!(w.reopen().err(), Some(StandbyError::Unsafe));
        fs::remove_file(&side).unwrap();
    }
    fs::OpenOptions::new()
        .write(true)
        .open(&image)
        .unwrap()
        .set_len(good.len() as u64 - ps as u64)
        .unwrap();
    assert_eq!(w.reopen().err(), Some(StandbyError::Corrupt));
    fs::write(&image, &good).unwrap();
    let mut standby = w.reopen().unwrap();
    assert_eq!(standby.applied(), &tip(2));
    assert_eq!(fs::read(&image).unwrap(), good);

    // The final check refuses an altered image: the table's root page gets an invalid type.
    let root_page: i64 = w
        .conn
        .query_row(
            "SELECT rootpage FROM sqlite_master WHERE name = 'messages'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    {
        use std::io::{Seek, SeekFrom, Write};
        let mut file = fs::OpenOptions::new().write(true).open(&image).unwrap();
        file.seek(SeekFrom::Start((root_page as u64 - 1) * ps as u64))
            .unwrap();
        file.write_all(&[0xFF; 16]).unwrap();
        file.sync_all().unwrap();
    }
    let damaged = fs::read(&image).unwrap();
    assert_eq!(standby.verify(&tip(2)), Err(StandbyError::Corrupt));
    assert!(
        !w.root.join(VERIFY_COPY).exists(),
        "the scratch copy is removed"
    );
    assert_eq!(
        fs::read(&image).unwrap(),
        damaged,
        "verification never repairs"
    );
}

/// A fresh copy of the first captured transaction (`AssembledTx` is not `Clone`).
fn real_clone(w: &World) -> AssembledTx {
    w.capture.tx(0)
}
