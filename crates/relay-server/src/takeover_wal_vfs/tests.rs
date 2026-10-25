//! Own temporary SQLite databases only (fresh folders under the system temp directory): no Relay
//! profile, key, device, network or running server. Written for the later focused run; not
//! executed when written.
use super::*;
use std::sync::atomic::AtomicBool;

struct Recorded {
    sequence: u64,
    first_frame: u32,
    frames: u32,
    db_pages: u32,
    generation: (u32, [u32; 2]),
    chunks: Vec<Vec<u8>>,
}
/// The injected sink: records every approved transaction as bounded chunks, or vetoes.
struct Recorder {
    veto: AtomicBool,
    /// Make the receipt fail (an error) or panic, AFTER the commit really happened.
    fail_receipt: AtomicBool,
    panic_receipt: AtomicBool,
    limit: usize,
    transactions: Mutex<Vec<Recorded>>,
    receipts: Mutex<Vec<LocalCommit>>,
    /// "approve N" and "receipt N" in the exact order they happened.
    events: Mutex<Vec<String>>,
}
impl Recorder {
    fn new(limit: usize) -> Arc<Recorder> {
        Arc::new(Recorder {
            veto: AtomicBool::new(false),
            fail_receipt: AtomicBool::new(false),
            panic_receipt: AtomicBool::new(false),
            limit,
            transactions: Mutex::new(Vec::new()),
            receipts: Mutex::new(Vec::new()),
            events: Mutex::new(Vec::new()),
        })
    }
    fn count(&self) -> usize {
        self.transactions.lock().unwrap().len()
    }
}
impl CommitSink for Recorder {
    fn approve(&self, tx: &WalTransaction) -> Result<(), Refusal> {
        if self.veto.load(Ordering::SeqCst) {
            return Err(Refusal("vetoed by the test sink"));
        }
        let chunks: Vec<Vec<u8>> = tx.chunks(self.limit)?.collect();
        self.transactions.lock().unwrap().push(Recorded {
            sequence: tx.sequence(),
            first_frame: tx.first_frame(),
            frames: tx.frame_count(),
            db_pages: tx.db_pages(),
            generation: (tx.generation().checkpoint_seq, tx.generation().salt),
            chunks,
        });
        self.events
            .lock()
            .unwrap()
            .push(format!("approve {}", tx.sequence()));
        Ok(())
    }
    fn local_committed(&self, receipt: &LocalCommit) -> Result<(), Refusal> {
        if self.panic_receipt.load(Ordering::SeqCst) {
            panic!("injected receipt panic");
        }
        if self.fail_receipt.load(Ordering::SeqCst) {
            return Err(Refusal("injected receipt failure"));
        }
        self.events
            .lock()
            .unwrap()
            .push(format!("receipt {}", receipt.sequence));
        self.receipts.lock().unwrap().push(receipt.clone());
        Ok(())
    }
}
/// Feeds every recorded transaction, in order, through the assembler into the offline target.
fn apply_all(target: &Path, sink: &Recorder) {
    for recorded in sink.transactions.lock().unwrap().iter() {
        let mut assembler = TxAssembler::new(64 << 20);
        let mut finished = None;
        for chunk in &recorded.chunks {
            finished = assembler.add(chunk).unwrap();
        }
        let tx = finished.expect("the last chunk completes the transaction");
        assert_eq!(tx.sequence, recorded.sequence);
        apply_transaction(target, &tx).unwrap();
    }
}

const SCHEMA: &str = "
    PRAGMA journal_mode = WAL;
    CREATE TABLE messages(id TEXT PRIMARY KEY, channel TEXT NOT NULL, content TEXT NOT NULL);
    CREATE TABLE message_order(sequence INTEGER PRIMARY KEY AUTOINCREMENT, message_id TEXT NOT NULL UNIQUE);
    CREATE TABLE audit(sequence INTEGER PRIMARY KEY AUTOINCREMENT, note TEXT NOT NULL);
    CREATE TABLE uniq(v INTEGER PRIMARY KEY, tag TEXT);
    CREATE TABLE settings(value BLOB);
    CREATE VIRTUAL TABLE message_search USING fts5(message_id UNINDEXED, content);
    CREATE TRIGGER messages_ai AFTER INSERT ON messages BEGIN
        INSERT INTO message_order(message_id) VALUES (new.id);
        INSERT INTO message_search(message_id, content) VALUES (new.id, new.content);
        INSERT INTO audit(note) VALUES ('insert ' || new.id);
    END;
    INSERT INTO uniq(v, tag) VALUES (5, 'taken');
    INSERT INTO messages VALUES ('seed', 'general', 'seed message needle');
    PRAGMA wal_checkpoint(TRUNCATE);";

/// A WAL-mode database with FTS5, triggers and AUTOINCREMENT, fully checkpointed and closed.
fn fixture() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("source.sqlite3");
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(SCHEMA).unwrap();
    drop(conn);
    assert!(
        !sidecar(&db, "-wal").exists(),
        "the fixture starts with no WAL"
    );
    (dir, db)
}
fn sidecar(db: &Path, suffix: &str) -> PathBuf {
    let mut name = db.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}
const READ_WRITE: OpenFlags = OpenFlags::SQLITE_OPEN_READ_WRITE;

/// A logical dump (quick_check first) covering rows, trigger effects, sequences, FTS results and
/// the FTS shadow tables.
fn dump(path: &Path) -> Vec<String> {
    let conn = Connection::open(path).unwrap();
    let check: String = conn
        .query_row("PRAGMA quick_check", [], |r| r.get(0))
        .unwrap();
    assert_eq!(check, "ok");
    let mut out = Vec::new();
    // Compare the WHOLE content, including bytes beyond the display prefix. Keep
    // diagnostics small even for the multi-megabyte spill fixture.
    let mut messages = conn
        .prepare("SELECT id, channel, content FROM messages ORDER BY id")
        .unwrap();
    let rows = messages
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })
        .unwrap();
    for row in rows {
        let (id, channel, content) = row.unwrap();
        out.push(format!(
            "{id}|{channel}|{}|{:x}",
            content.len(),
            Sha256::digest(content.as_bytes())
        ));
    }
    out.push("--".into());
    for sql in [
        "SELECT type||'|'||name||'|'||sql FROM sqlite_schema WHERE sql IS NOT NULL ORDER BY type, name",
        "SELECT hex(value) FROM settings ORDER BY rowid",
        "SELECT sequence||'|'||message_id FROM message_order ORDER BY sequence",
        "SELECT sequence||'|'||note FROM audit ORDER BY sequence",
        "SELECT v||'|'||tag FROM uniq ORDER BY v",
        "SELECT message_id FROM message_search WHERE message_search MATCH 'needle' ORDER BY message_id",
        "SELECT name||'|'||seq FROM sqlite_sequence ORDER BY name",
        "SELECT (SELECT count(*) FROM message_search_data)||'|'||(SELECT count(*) FROM message_search_idx)||'|'||(SELECT count(*) FROM message_search_content)",
    ] {
        let mut statement = conn.prepare(sql).unwrap();
        let rows = statement.query_map([], |r| r.get::<_, String>(0)).unwrap();
        for row in rows {
            out.push(row.unwrap());
        }
        out.push("--".into());
    }
    out
}

#[test]
fn a_sink_veto_leaves_the_committed_database_unchanged_and_no_commit_marker_escapes() {
    let (_dir, db) = fixture();
    let before = dump(&db);
    let sink = Recorder::new(64 << 10);
    sink.veto.store(true, Ordering::SeqCst);
    let vfs = WalVfs::register(&db, Config::DEFAULT, sink.clone()).unwrap();
    let conn = vfs.open_connection(READ_WRITE).unwrap();
    assert!(conn
        .execute(
            "INSERT INTO messages VALUES ('m1','general','vetoed needle')",
            []
        )
        .is_err());
    // Nothing was published: the same connection does not see the row, no commit was counted and
    // not a single byte of the held tail reached the WAL file.
    let rows: i64 = conn
        .query_row("SELECT count(*) FROM messages", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 1);
    assert_eq!((vfs.commit_sequence(), sink.count()), (0, 0));
    let wal = sidecar(&db, "-wal");
    assert!(!wal.exists() || std::fs::metadata(&wal).unwrap().len() == 0);
    assert!(
        !vfs.is_poisoned(),
        "a veto is an ordinary refusal, not a protocol fault"
    );
    drop(conn);
    drop(vfs);
    assert_eq!(
        dump(&db),
        before,
        "a previously committed database is unchanged after reopen"
    );

    // With the sink approving, the same wrapper path commits and counts exactly one transaction.
    sink.veto.store(false, Ordering::SeqCst);
    let vfs = WalVfs::register(&db, Config::DEFAULT, sink.clone()).unwrap();
    let conn = vfs.open_connection(READ_WRITE).unwrap();
    conn.execute(
        "INSERT INTO messages VALUES ('m1','general','approved needle')",
        [],
    )
    .unwrap();
    assert_eq!((vfs.commit_sequence(), sink.count()), (1, 1));
    drop(conn);
    drop(vfs);
    assert!(dump(&db).iter().any(|line| line.starts_with("m1|")));
}

#[test]
fn a_spilled_multi_chunk_transaction_with_savepoints_and_a_caught_failure_applies_to_the_net_state()
{
    let (dir, db) = fixture();
    let sink = Recorder::new(1 << 20);
    let vfs = WalVfs::register(&db, Config::DEFAULT, sink.clone()).unwrap();
    let conn = vfs.open_connection(READ_WRITE).unwrap();
    let standby = dir.path().join("standby.sqlite3");
    assert_eq!(vfs.snapshot(&conn, &standby).unwrap(), 0);

    // A tiny page cache forces uncommitted frames to spill into the WAL before the commit.
    conn.execute_batch("PRAGMA cache_size = 10; BEGIN;")
        .unwrap();
    for i in 0..600 {
        conn.execute(
            "INSERT INTO messages VALUES (?1, 'general', ?2)",
            rusqlite::params![
                format!("m{i:04}"),
                format!("bulk {i} {}", "x".repeat(5_000))
            ],
        )
        .unwrap();
    }
    // A rolled back savepoint, a kept one, and a failed multi-row statement whose first rows are
    // undone by SQLite's statement journal (it is caught and the transaction goes on).
    conn.execute_batch(
        "SAVEPOINT s1; INSERT INTO messages VALUES ('rolled','general','rolled needle');
         ROLLBACK TO s1; RELEASE s1;
         SAVEPOINT s2; INSERT INTO messages VALUES ('kept','general','kept needle'); RELEASE s2;",
    )
    .unwrap();
    let failed = conn.execute(
        "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x<50)
         INSERT INTO uniq(v, tag) SELECT CASE WHEN x=40 THEN 5 ELSE x+1000 END, 'bulk' FROM c",
        [],
    );
    assert!(failed.is_err());
    conn.execute_batch(
        "UPDATE messages SET content = content || ' updated' WHERE id = 'kept';
         DELETE FROM messages WHERE id LIKE 'm000%';
         ALTER TABLE settings ADD COLUMN note TEXT;
         INSERT INTO settings(value, note) VALUES (zeroblob(9000), 'committed DDL and BLOB');
         CREATE INDEX messages_by_channel ON messages(channel);
         COMMIT;",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO messages VALUES ('after','general','after needle')",
        [],
    )
    .unwrap();
    assert!(!vfs.is_poisoned());
    assert_eq!(vfs.commit_sequence(), 2);
    drop(conn);
    assert!(
        !vfs.is_poisoned(),
        "the closing checkpoint was ordinary committed work"
    );
    drop(vfs);

    // The big transaction is one approved unit, larger than one 1 MiB wire entry, in bounded chunks.
    {
        let recorded = sink.transactions.lock().unwrap();
        assert_eq!(recorded.len(), 2);
        let big = &recorded[0];
        assert!(big.chunks.len() >= 3 && big.chunks.iter().all(|c| c.len() <= 1 << 20));
        assert!(u64::from(big.frames) * 4096 > 1 << 21);
        assert_eq!((big.sequence, recorded[1].sequence), (1, 2));
        // A transaction that is not complete is never applied.
        let mut partial = TxAssembler::new(64 << 20);
        for chunk in &big.chunks[..big.chunks.len() - 1] {
            assert!(partial.add(chunk).unwrap().is_none());
        }
        drop(partial);
    }
    apply_all(&standby, &sink);
    let (source, copy) = (dump(&db), dump(&standby));
    assert_eq!(
        source, copy,
        "the standby holds exactly the source's net state"
    );
    let text = source.join("\n");
    assert!(text.contains("kept|") && text.contains("after|"));
    assert!(
        !text.contains("rolled") && !text.contains("1001|bulk"),
        "undone work must not appear"
    );
    assert!(text.contains("insert kept") && !text.contains("insert rolled"));
}

#[test]
fn checkpoint_truncate_and_restart_start_new_generations_that_do_not_mix_with_earlier_frames() {
    let (dir, db) = fixture();
    let sink = Recorder::new(64 << 10);
    let vfs = WalVfs::register(&db, Config::DEFAULT, sink.clone()).unwrap();
    let conn = vfs.open_connection(READ_WRITE).unwrap();
    let standby = dir.path().join("standby.sqlite3");
    assert_eq!(vfs.snapshot(&conn, &standby).unwrap(), 0);
    let commit = |id: &str| {
        conn.execute(
            "INSERT INTO messages VALUES (?1, 'general', ?2)",
            rusqlite::params![id, format!("{id} needle")],
        )
        .unwrap();
    };
    commit("g1");
    let end_g1 = vfs.committed_wal_end();
    commit("g1b"); // same generation: the second commit continues after the first
    let mode = |name: &str| {
        let (_, _): (i64, i64) = conn
            .query_row(&format!("PRAGMA wal_checkpoint({name})"), [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
    };
    mode("TRUNCATE"); // the WAL file is emptied: a later commit starts a fresh generation
    commit("g2");
    mode("RESTART"); // the file keeps its length but restarts at offset 0 with new salts
    commit("g3");
    mode("PASSIVE");
    assert!(!vfs.is_poisoned());
    drop(conn);
    drop(vfs);

    {
        let recorded = sink.transactions.lock().unwrap();
        assert_eq!(recorded.len(), 4);
        let [g1, g1b, g2, g3] = [&recorded[0], &recorded[1], &recorded[2], &recorded[3]];
        assert_eq!(g1.generation, g1b.generation);
        assert_eq!(g1.first_frame, 1);
        // The next commit starts after EVERYTHING the first one wrote, padding repeats included.
        assert_eq!(
            u64::from(g1b.first_frame),
            (end_g1 - WAL_HEADER as u64) / (FRAME_HEADER as u64 + 4096) + 1
        );
        for (older, newer) in [(g1, g2), (g2, g3)] {
            assert_ne!(
                older.generation, newer.generation,
                "fresh checkpoint sequence and salts"
            );
            assert_eq!(newer.first_frame, 1, "frames restart in the new generation");
        }
    }
    apply_all(&standby, &sink);
    assert_eq!(dump(&db), dump(&standby));
}

#[test]
fn the_backup_snapshot_is_logically_equal_but_not_a_byte_copy() {
    let (dir, db) = fixture();
    let sink = Recorder::new(64 << 10);
    let vfs = WalVfs::register(&db, Config::DEFAULT, sink.clone()).unwrap();
    let conn = vfs.open_connection(READ_WRITE).unwrap();
    let standby = dir.path().join("standby.sqlite3");
    assert_eq!(vfs.snapshot(&conn, &standby).unwrap(), 0);
    drop(conn);
    drop(vfs);
    // Byte by byte: only the file change counter (24), the schema cookie (40) and
    // version-valid-for / SQLite version number (92..99) of page 1 may differ; the WAL format
    // bytes (18/19) and every other page are copied.
    let (source, copy) = (
        std::fs::read(&db).unwrap(),
        std::fs::read(&standby).unwrap(),
    );
    assert_eq!(source.len(), copy.len());
    assert_eq!((copy[18], copy[19]), (2, 2));
    let differing: Vec<usize> = (0..source.len())
        .filter(|i| source[*i] != copy[*i])
        .collect();
    assert!(
        differing
            .iter()
            .all(|i| matches!(*i, 24..=27 | 40..=43 | 92..=99)),
        "unexpected differing bytes: {differing:?}"
    );
    assert_eq!(dump(&db), dump(&standby));
}

#[test]
fn unsupported_layouts_quota_and_orphan_chunks_fail_closed_without_publishing_anything() {
    let (dir, db) = fixture();
    let sink = Recorder::new(8 << 10);

    // Quota: a transaction larger than the held-tail limit fails, the wrapper stays usable.
    let small = Config {
        max_staged_bytes: 64 << 10,
        max_tracked_frames: 10_000,
        ..Config::DEFAULT
    };
    let vfs = WalVfs::register(&db, small, sink.clone()).unwrap();
    let conn = vfs.open_connection(READ_WRITE).unwrap();
    conn.execute_batch("PRAGMA cache_size = 10; BEGIN;")
        .unwrap();
    let mut failed = false;
    for i in 0..100 {
        let step = conn.execute(
            "INSERT INTO messages VALUES (?1, 'general', ?2)",
            rusqlite::params![format!("q{i:03}"), "y".repeat(6_000)],
        );
        // SQLite may roll back the entire transaction on IOERR. Stop the
        // failed operation; continuing this loop would start new autocommits.
        if step.is_err() {
            failed = true;
            break;
        }
    }
    let _ = conn.execute_batch("ROLLBACK;");
    assert!(
        failed,
        "the oversized transaction must be refused, not truncated"
    );
    assert!(!vfs.is_poisoned());
    assert_eq!((vfs.commit_sequence(), sink.count()), (0, 0));
    let surviving: i64 = conn
        .query_row("SELECT count(*) FROM messages", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        surviving, 1,
        "no part of the oversized transaction survived"
    );
    conn.execute(
        "INSERT INTO messages VALUES ('ok','general','ok needle')",
        [],
    )
    .unwrap();
    assert_eq!(vfs.commit_sequence(), 1);

    // The authorizer: ATTACH, DETACH and changing synchronous/journal_mode are refused.
    let other = dir.path().join("other.sqlite3");
    assert!(conn
        .execute_batch(&format!("ATTACH DATABASE '{}' AS other;", other.display()))
        .is_err());
    assert!(conn.execute_batch("PRAGMA synchronous = OFF;").is_err());
    assert!(conn.execute_batch("PRAGMA journal_mode = DELETE;").is_err());
    let sync: i64 = conn
        .query_row("PRAGMA synchronous", [], |r| r.get(0))
        .unwrap();
    assert_eq!(sync, 2);
    // Another durable main database through the same wrapper is refused at open.
    let foreign = dir.path().join("foreign.sqlite3");
    Connection::open(&foreign)
        .unwrap()
        .execute_batch("PRAGMA journal_mode = WAL; CREATE TABLE t(x);")
        .unwrap();
    assert!(Connection::open_with_flags_and_vfs(&foreign, READ_WRITE, vfs.vfs_name()).is_err());
    assert!(!vfs.is_poisoned());
    drop(conn);
    drop(vfs);

    // A database that is not in WAL mode, and one with a leftover non-empty WAL, are refused.
    let rollback = dir.path().join("rollback.sqlite3");
    Connection::open(&rollback)
        .unwrap()
        .execute_batch("CREATE TABLE t(x); INSERT INTO t VALUES (1);")
        .unwrap();
    let vfs = WalVfs::register(&rollback, Config::DEFAULT, sink.clone()).unwrap();
    assert!(vfs.open_connection(READ_WRITE).is_err());
    drop(vfs);
    let crashed = dir.path().join("crashed.sqlite3");
    {
        let live = Connection::open(&db).unwrap();
        live.execute_batch("PRAGMA wal_autocheckpoint = 0;")
            .unwrap();
        live.execute(
            "INSERT INTO messages VALUES ('w1','general','w1 needle')",
            [],
        )
        .unwrap();
        std::fs::copy(&db, &crashed).unwrap();
        std::fs::copy(sidecar(&db, "-wal"), sidecar(&crashed, "-wal")).unwrap();
    }
    assert!(std::fs::metadata(sidecar(&crashed, "-wal")).unwrap().len() > 0);
    let vfs = WalVfs::register(&crashed, Config::DEFAULT, sink.clone()).unwrap();
    assert!(vfs.open_connection(READ_WRITE).is_err());
    drop(vfs);

    // Orphan, duplicate, out-of-order and foreign chunks are refused; an incomplete set never
    // reaches the target, which stays byte-identical.
    let (dir2, db2) = fixture();
    let multi = Recorder::new(8 << 10);
    let vfs = WalVfs::register(&db2, Config::DEFAULT, multi.clone()).unwrap();
    let conn = vfs.open_connection(READ_WRITE).unwrap();
    let standby = dir2.path().join("standby.sqlite3");
    vfs.snapshot(&conn, &standby).unwrap();
    for i in 0..40 {
        conn.execute(
            "INSERT INTO messages VALUES (?1, 'general', ?2)",
            rusqlite::params![format!("c{i:02}"), "z".repeat(1_000)],
        )
        .unwrap();
    }
    drop(conn);
    drop(vfs);
    let recorded = multi.transactions.lock().unwrap();
    let chunks = &recorded
        .iter()
        .find(|r| r.chunks.len() >= 2)
        .expect("a multi-chunk commit")
        .chunks;
    let target_before = std::fs::read(&standby).unwrap();
    let mut assembler = TxAssembler::new(64 << 20);
    assert!(
        assembler.add(&chunks[1]).is_err(),
        "must start at the first chunk"
    );
    assert!(assembler.add(&chunks[0]).unwrap().is_none());
    assert!(
        assembler.add(&chunks[0]).is_err(),
        "a duplicate chunk discards the partial transaction"
    );
    let mut assembler = TxAssembler::new(64 << 20);
    for chunk in &chunks[..chunks.len() - 1] {
        assert!(assembler.add(chunk).unwrap().is_none());
    }
    drop(assembler);
    assert_eq!(std::fs::read(&standby).unwrap(), target_before);
    let mut oversize = TxAssembler::new(1024);
    assert!(
        oversize.add(&chunks[0]).is_err(),
        "bounded assembly never truncates"
    );
}

// ---- review correction: sync patterns and snapshot ownership ----------------------------------

const SYNTHETIC_PAGE: u32 = 512;
fn wal_header(salt: [u32; 2]) -> Vec<u8> {
    let mut header = vec![0u8; WAL_HEADER];
    header[0..4].copy_from_slice(&WAL_MAGIC.to_be_bytes());
    header[4..8].copy_from_slice(&WAL_VERSION.to_be_bytes());
    header[8..12].copy_from_slice(&SYNTHETIC_PAGE.to_be_bytes());
    header[16..20].copy_from_slice(&salt[0].to_be_bytes());
    header[20..24].copy_from_slice(&salt[1].to_be_bytes());
    let sum = checksum(false, &header[..24], (0, 0));
    header[24..28].copy_from_slice(&sum.0.to_be_bytes());
    header[28..32].copy_from_slice(&sum.1.to_be_bytes());
    header
}
/// One frame with a valid checksum chained from `seed`; returns the frame and its checksum.
fn wal_frame(
    pgno: u32,
    commit: u32,
    salt: [u32; 2],
    seed: (u32, u32),
    fill: u8,
) -> (Vec<u8>, (u32, u32)) {
    let mut frame = vec![fill; FRAME_HEADER + SYNTHETIC_PAGE as usize];
    frame[0..4].copy_from_slice(&pgno.to_be_bytes());
    frame[4..8].copy_from_slice(&commit.to_be_bytes());
    frame[8..12].copy_from_slice(&salt[0].to_be_bytes());
    frame[12..16].copy_from_slice(&salt[1].to_be_bytes());
    let mut sum = checksum(false, &frame[..8], seed);
    sum = checksum(false, &frame[FRAME_HEADER..], sum);
    frame[16..20].copy_from_slice(&sum.0.to_be_bytes());
    frame[20..24].copy_from_slice(&sum.1.to_be_bytes());
    (frame, sum)
}

#[test]
fn only_the_exact_wal_header_sync_or_a_complete_commit_is_ever_acknowledged() {
    let salt = [7, 9];
    let header = wal_header(salt);
    let seed = checksum(false, &header[..24], (0, 0));
    let (spill, after_spill) = wal_frame(2, 0, salt, seed, 1);
    let (commit, _) = wal_frame(3, 5, salt, after_spill, 2);
    let state_with = |tail: Vec<u8>| DbState {
        page_size: SYNTHETIC_PAGE,
        tail,
        owner: 1,
        main_characteristics: Some(ffi::SQLITE_IOCAP_POWERSAFE_OVERWRITE),
        ..DbState::default()
    };
    let plan = |tail: Vec<u8>| plan_sync(&state_with(tail), true, 512);
    let join = |parts: &[&[u8]]| parts.concat();

    // Nothing held, or held by another file object: an ordinary sync of the real file.
    assert!(matches!(plan(Vec::new()), SyncPlan::Forward));
    assert!(matches!(
        plan_sync(&state_with(header.clone()), false, 512),
        SyncPlan::Forward
    ));
    // SQLite's header sync: exactly the valid 32 bytes, nothing else, may succeed alone.
    assert!(matches!(plan(header.clone()), SyncPlan::HeaderOnly));
    // Everything else that is not a complete verified commit is refused, never acknowledged.
    let mut bad_sum = header.clone();
    bad_sum[28] ^= 1;
    for refused in [
        header[..31].to_vec(),                                 // partial header
        bad_sum,                                               // malformed header
        vec![0xAB; WAL_HEADER],                                // not a WAL header
        join(&[&header, &spill]),                              // a spill: frames, no commit
        join(&[&header, &spill[..100]]),                       // a partial frame
        join(&[&header, &spill, &commit[..commit.len() - 1]]), // the commit page is incomplete
        join(&[&header, &commit[..FRAME_HEADER]]),             // only the commit header
    ] {
        assert!(matches!(plan(refused), SyncPlan::Refuse));
    }
    // A complete commit is recognised; a flipped page byte breaks the checksum chain.
    match plan(join(&[&header, &spill, &commit])) {
        SyncPlan::Commit(info) => {
            assert_eq!((info.frames, info.db_pages, info.first_frame), (2, 5, 1))
        }
        _ => panic!("a complete commit must be recognised"),
    }
    let mut flipped = join(&[&header, &spill, &commit]);
    let last = flipped.len() - 1;
    flipped[last] ^= 1;
    assert!(matches!(plan(flipped), SyncPlan::Refuse));
    // After committed frames, a header-sized tail is not the header sync, and a continuation
    // commit is verified against the stored generation and checksum chain.
    let (generation, _) = parse_wal_header(&header).unwrap();
    let (first, first_sum) = wal_frame(2, 4, salt, seed, 3);
    let (next, _) = wal_frame(3, 4, salt, first_sum, 4);
    let continued = |tail: Vec<u8>| DbState {
        page_size: SYNTHETIC_PAGE,
        generation: Some(generation.clone()),
        committed_end: (WAL_HEADER + first.len()) as u64,
        last_cksum: first_sum,
        tail,
        owner: 1,
        main_characteristics: Some(ffi::SQLITE_IOCAP_POWERSAFE_OVERWRITE),
        ..DbState::default()
    };
    assert!(matches!(
        plan_sync(&continued(vec![0u8; WAL_HEADER]), true, 512),
        SyncPlan::Refuse
    ));
    match plan_sync(&continued(next.clone()), true, 512) {
        SyncPlan::Commit(info) => assert_eq!((info.first_frame, info.frames), (2, 1)),
        _ => panic!("a continuation commit must be recognised"),
    }
    assert!(matches!(
        plan_sync(&continued(next[..next.len() - 1].to_vec()), true, 512),
        SyncPlan::Refuse
    ));
}

#[test]
fn snapshot_needs_the_exact_idle_admitted_connection_and_never_touches_an_existing_file() {
    let (dir, db) = fixture();
    let sink = Recorder::new(64 << 10);
    let vfs = WalVfs::register(&db, Config::DEFAULT, sink.clone()).unwrap();
    let conn = vfs.open_connection(READ_WRITE).unwrap();
    let target = dir.path().join("standby.sqlite3");

    // Exactly one MAIN connection: neither a second call nor a direct open by name gets in.
    assert!(vfs.open_connection(READ_WRITE).is_err());
    assert!(Connection::open_with_flags_and_vfs(&db, READ_WRITE, vfs.vfs_name()).is_err());
    // A foreign source is refused, even one that is a perfectly good database.
    let other = dir.path().join("other.sqlite3");
    let foreign = Connection::open(&other).unwrap();
    foreign
        .execute_batch("CREATE TABLE t(x); INSERT INTO t VALUES (1);")
        .unwrap();
    assert!(vfs.snapshot(&foreign, &target).is_err());
    assert!(!target.exists());
    drop(foreign);
    // A stale read transaction, and a half-read statement, are refused; ending them allows it.
    conn.execute_batch("BEGIN").unwrap();
    let _count: i64 = conn
        .query_row("SELECT count(*) FROM messages", [], |r| r.get(0))
        .unwrap();
    assert!(vfs.snapshot(&conn, &target).is_err());
    conn.execute_batch("COMMIT").unwrap();
    {
        let mut statement = conn.prepare("SELECT id FROM messages").unwrap();
        let mut rows = statement.query([]).unwrap();
        assert!(rows.next().unwrap().is_some());
        assert!(vfs.snapshot(&conn, &target).is_err());
    }
    assert!(!target.exists());

    // Existing destinations and side files survive byte for byte; the source is never a target.
    let sentinel = dir.path().join("sentinel.sqlite3");
    std::fs::write(&sentinel, b"keep me").unwrap();
    assert!(vfs.snapshot(&conn, &sentinel).is_err());
    assert_eq!(std::fs::read(&sentinel).unwrap(), b"keep me");
    let guarded_side = dir.path().join("guarded.sqlite3-wal");
    std::fs::write(&guarded_side, b"keep side").unwrap();
    assert!(vfs
        .snapshot(&conn, &dir.path().join("guarded.sqlite3"))
        .is_err());
    assert_eq!(std::fs::read(&guarded_side).unwrap(), b"keep side");
    assert!(!dir.path().join("guarded.sqlite3").exists());
    let source_before = std::fs::read(&db).unwrap();
    assert!(vfs.snapshot(&conn, &db).is_err());
    assert_eq!(std::fs::read(&db).unwrap(), source_before);
    assert!(vfs.snapshot(&conn, Path::new("relative.sqlite3")).is_err());
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains(".snapshot-"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "refused snapshots leave no temporary file"
    );

    // The legitimate snapshot: idle owned connection, fresh destination, after a real commit.
    conn.execute(
        "INSERT INTO messages VALUES ('s1','general','s1 needle')",
        [],
    )
    .unwrap();
    assert_eq!(vfs.snapshot(&conn, &target).unwrap(), 1);
    assert!(!sidecar(&target, "-journal").exists());
    assert!(
        vfs.snapshot(&conn, &target).is_err(),
        "a finished snapshot is never replaced"
    );
    assert!(!vfs.is_poisoned());
    // Once the admitted connection is gone nothing can open by name; open_connection admits anew.
    drop(conn);
    assert!(Connection::open_with_flags_and_vfs(&db, READ_WRITE, vfs.vfs_name()).is_err());
    let again = vfs.open_connection(READ_WRITE).unwrap();
    drop(again);
    drop(vfs);
    assert_eq!(
        dump(&db),
        dump(&target),
        "logical equality with the state after commit 1"
    );
}

// ---- sector padding (a device without POWERSAFE_OVERWRITE, like the bundled Windows VFS) ----------

/// What the bundled Windows VFS reports for its files: UNDELETABLE_WHEN_OPEN | SUBPAGE_READ.
const WINDOWS_FLAGS: c_int = 0x8800;

#[test]
fn sector_padding_is_accepted_only_as_exact_repeats_ending_at_sqlites_own_sync_point() {
    let salt = [3, 5];
    let header = wal_header(salt);
    let seed = checksum(false, &header[..24], (0, 0));
    let (spill, after_spill) = wal_frame(7, 0, salt, seed, 1);
    let (commit, after_commit) = wal_frame(2, 4, salt, after_spill, 9);
    // The i-th repeat of the commit frame, chained on from the previous one.
    let repeats = |count: usize| -> (Vec<Vec<u8>>, (u32, u32)) {
        let (mut frames, mut sum) = (Vec::new(), after_commit);
        for _ in 0..count {
            let (frame, next) = wal_frame(2, 4, salt, sum, 9);
            frames.push(frame);
            sum = next;
        }
        (frames, sum)
    };
    let held = |parts: &[&[u8]]| parts.concat();
    let state = |tail: Vec<u8>, flags: Option<c_int>| DbState {
        page_size: SYNTHETIC_PAGE,
        tail,
        owner: 1,
        main_characteristics: flags,
        ..DbState::default()
    };
    let plan =
        |tail: Vec<u8>, sector: u32| plan_sync(&state(tail, Some(WINDOWS_FLAGS)), true, sector);
    // header + spill + commit frame ends at 32 + 2 * 536 = 1104.
    let base = held(&[&header, &spill, &commit]);

    // Sector 512: the next boundary is 1536, 432 bytes into the first repeat (inside its page).
    let (one, _) = repeats(1);
    match plan(held(&[&base, &one[0][..432]]), 512) {
        SyncPlan::Commit(info) => {
            assert_eq!((info.frames, info.repeats, info.partial), (2, 0, 432));
            assert_eq!(
                info.rest,
                one[0][432..].to_vec(),
                "the exact remainder SQLite must still write"
            );
            assert_eq!(info.db_pages, 4);
        }
        _ => panic!("a padded commit must be recognised"),
    }
    // Sector 4096 over small pages: five complete repeats and a partial sixth.
    let (five, _) = repeats(6);
    let long: Vec<u8> = five[..5].concat();
    let boundary = 4096 - 1104 - long.len();
    match plan(held(&[&base, &long, &five[5][..boundary]]), 4096) {
        SyncPlan::Commit(info) => assert_eq!((info.repeats as usize, info.partial), (5, boundary)),
        _ => panic!("complete repeats followed by a partial one are one commit"),
    }
    // A sync point that falls inside the repeat's HEADER (10 bytes in).
    match plan(held(&[&base, &one[0][..10]]), 1114) {
        SyncPlan::Commit(info) => {
            assert_eq!((info.repeats, info.partial, info.rest.len()), (0, 10, 526))
        }
        _ => panic!("a split inside the header is still the padding sync"),
    }
    // Already on a boundary: SQLite does not pad, the plain commit is recognised.
    assert!(matches!(plan(base.clone(), 1104), SyncPlan::Commit(_)));
    assert!(matches!(
        plan(held(&[&base, &one[0][..10]]), 1104),
        SyncPlan::Refuse
    ));

    // Everything that is not exactly SQLite's padding is refused.
    let good = held(&[&base, &one[0][..432]]);
    let corrupt = |at: usize| {
        let mut tail = good.clone();
        tail[at] ^= 0x40;
        tail
    };
    let in_repeat = base.len();
    for refused in [
        good[..good.len() - 1].to_vec(), // one byte short of the sync point
        held(&[&good, &[0]]),            // one byte past it
        corrupt(in_repeat),              // page number of the repeat
        corrupt(in_repeat + 5),          // commit size of the repeat
        corrupt(in_repeat + 10),         // salt of the repeat
        corrupt(in_repeat + 20),         // checksum of the repeat
        corrupt(in_repeat + FRAME_HEADER + 100), // page bytes of the repeat
        corrupt(base.len() - 3),         // the real commit page (chain breaks)
    ] {
        assert!(matches!(plan(refused, 512), SyncPlan::Refuse));
    }
    let mut changed = five.clone();
    changed[2][FRAME_HEADER + 7] ^= 1; // a complete middle repeat with other page bytes
    assert!(matches!(
        plan(
            held(&[&base, &changed[..5].concat(), &changed[5][..boundary]]),
            4096
        ),
        SyncPlan::Refuse
    ));
    let (unchained, _) = wal_frame(2, 4, salt, seed, 9); // right content, checksum not chained on
    assert!(matches!(
        plan(held(&[&base, &unchained[..432]]), 512),
        SyncPlan::Refuse
    ));
    // Unknown device flags, and a device that does NOT pad, never accept padding.
    assert!(matches!(
        plan_sync(&state(good.clone(), None), true, 512),
        SyncPlan::Refuse
    ));
    assert!(matches!(
        plan_sync(
            &state(
                good,
                Some(ffi::SQLITE_IOCAP_POWERSAFE_OVERWRITE | WINDOWS_FLAGS)
            ),
            true,
            512
        ),
        SyncPlan::Refuse
    ));
}

#[test]
fn the_unwritten_rest_of_a_padding_frame_and_later_repeats_must_match_byte_for_byte() {
    let mut pad = PendingPad {
        owner: 1,
        next_at: 1024,
        rest: vec![1, 2, 3, 4, 5],
        cksum_after: (7, 8),
    };
    assert!(!pad.accept(2, 1024, &[1]), "another file object");
    assert!(!pad.accept(1, 1023, &[1]), "another offset");
    assert!(!pad.accept(1, 1024, &[9]), "another byte");
    assert!(
        !pad.accept(1, 1024, &[1, 2, 3, 4, 5, 6]),
        "more than the remainder"
    );
    assert!(!pad.accept(1, 1024, &[]), "nothing");
    assert!(pad.accept(1, 1024, &[1, 2]) && !pad.finished());
    assert!(!pad.accept(1, 1024, &[3]), "the offset moved on");
    assert!(pad.accept(1, 1026, &[3, 4, 5]) && pad.finished());

    // Appends after the first commit frame must repeat its page number, size, salts and page.
    let salt = [3, 5];
    let header = wal_header(salt);
    let seed = checksum(false, &header[..24], (0, 0));
    let (commit, after) = wal_frame(2, 4, salt, seed, 9);
    let tail = [header.clone(), commit.clone()].concat();
    let mark = CommitMark {
        end: tail.len(),
        head: commit[..16].try_into().unwrap(),
        page_len: SYNTHETIC_PAGE as usize,
    };
    let size = FRAME_HEADER + SYNTHETIC_PAGE as usize;
    let (repeat, _) = wal_frame(2, 4, salt, after, 9);
    assert!(repeats_commit_frame(
        &tail,
        &mark,
        size,
        tail.len(),
        &repeat
    ));
    assert!(repeats_commit_frame(
        &tail,
        &mark,
        size,
        tail.len(),
        &repeat[..24]
    ));
    assert!(repeats_commit_frame(
        &tail,
        &mark,
        size,
        tail.len() + size,
        &repeat
    ));
    let mut other_page = repeat.clone();
    other_page[FRAME_HEADER + 1] ^= 1;
    let mut other_head = repeat.clone();
    other_head[1] ^= 1;
    assert!(!repeats_commit_frame(
        &tail,
        &mark,
        size,
        tail.len(),
        &other_page
    ));
    assert!(!repeats_commit_frame(
        &tail,
        &mark,
        size,
        tail.len(),
        &other_head
    ));
    assert!(!repeats_commit_frame(
        &tail,
        &mark,
        size,
        tail.len() + 3,
        &repeat
    ));
}

/// The size of a file through its own handle (a path lookup can lag on Windows).
fn wal_len(path: &Path) -> u64 {
    std::fs::File::open(path).unwrap().metadata().unwrap().len()
}

#[test]
fn real_commits_follow_the_platforms_actual_padding_mode_and_keep_position_and_chain() {
    let (_dir, db) = fixture();
    let sink = Recorder::new(64 << 10);
    let vfs = WalVfs::register(&db, Config::DEFAULT, sink.clone()).unwrap();
    let conn = vfs.open_connection(READ_WRITE).unwrap();
    let wal = sidecar(&db, "-wal");
    let frame = FRAME_HEADER as u64 + 4096;
    let insert = |id: &str| {
        conn.execute(
            "INSERT INTO messages VALUES (?1, 'general', ?2)",
            rusqlite::params![id, format!("{id} needle")],
        )
        .unwrap();
    };
    // Whatever this platform's VFS reports (padding on Windows, none with POWERSAFE_OVERWRITE),
    // the bytes SQLite wrote are exactly the bytes accounted for, frame aligned, repeats included.
    insert("p1");
    let end1 = vfs.committed_wal_end();
    assert!(vfs.padding_observed().is_some());
    assert_eq!((end1 - WAL_HEADER as u64) % frame, 0);
    assert_eq!(wal_len(&wal), end1);
    insert("p2");
    insert("p3");
    let end3 = vfs.committed_wal_end();
    assert_eq!((end3 - WAL_HEADER as u64) % frame, 0);
    assert_eq!(wal_len(&wal), end3);
    // A later commit is verified against the chain that CONTINUES through any repeats; had the
    // position or checksum after the padding been wrong, the wrapper would be poisoned here.
    assert!(!vfs.is_poisoned());
    conn.query_row("PRAGMA wal_checkpoint(RESTART)", [], |r| r.get::<_, i64>(0))
        .unwrap();
    insert("p4");
    conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
        r.get::<_, i64>(0)
    })
    .unwrap();
    insert("p5");
    assert!(!vfs.is_poisoned());
    {
        let recorded = sink.transactions.lock().unwrap();
        assert_eq!(recorded.len(), 5);
        assert_eq!(recorded[0].first_frame, 1);
        assert_eq!(
            u64::from(recorded[1].first_frame),
            (end1 - WAL_HEADER as u64) / frame + 1
        );
        assert_eq!((recorded[3].first_frame, recorded[4].first_frame), (1, 1));
        assert_ne!(recorded[2].generation, recorded[3].generation);
        assert_ne!(recorded[3].generation, recorded[4].generation);
    }
    drop(conn);
    assert!(
        !vfs.is_poisoned(),
        "the closing checkpoint accepted only committed whole pages"
    );
    drop(vfs);
    assert!(dump(&db).iter().any(|line| line.starts_with("p5|")));
}

// ---- S2b-2: the local commit receipt and the preserved automatic checkpoint ------------------------

#[test]
fn every_real_commit_gets_exactly_one_receipt_in_order_and_a_veto_or_rollback_gets_none() {
    let (_dir, db) = fixture();
    let sink = Recorder::new(64 << 10);
    let vfs = WalVfs::register(&db, Config::DEFAULT, sink.clone()).unwrap();
    let conn = vfs.open_connection(READ_WRITE).unwrap();
    // One batch: two autocommit statements, an explicit two-statement transaction, one more
    // autocommit statement: four REAL commits.
    conn.execute_batch(
        "INSERT INTO messages VALUES ('b1','general','b1 needle');
         INSERT INTO messages VALUES ('b2','general','b2 needle');
         BEGIN;
         INSERT INTO messages VALUES ('b3','general','b3 needle');
         INSERT INTO messages VALUES ('b4','general','b4 needle');
         COMMIT;
         INSERT INTO messages VALUES ('b5','general','b5 needle');",
    )
    .unwrap();
    // Reads and a rolled back transaction create no commit and no receipt.
    let _rows: i64 = conn
        .query_row("SELECT count(*) FROM messages", [], |r| r.get(0))
        .unwrap();
    conn.execute_batch("BEGIN; INSERT INTO messages VALUES ('gone','general','x'); ROLLBACK;")
        .unwrap();
    assert_eq!(
        (
            sink.count(),
            vfs.commit_sequence(),
            vfs.local_commit_sequence()
        ),
        (4, 4, 4)
    );
    assert_eq!(vfs.unreceipted_commit(), None);
    // Strictly alternating: each commit is approved, really committed, THEN receipted, once.
    assert_eq!(
        *sink.events.lock().unwrap(),
        [
            "approve 1",
            "receipt 1",
            "approve 2",
            "receipt 2",
            "approve 3",
            "receipt 3",
            "approve 4",
            "receipt 4"
        ]
    );
    let frame = FRAME_HEADER as u64 + 4096;
    {
        let receipts = sink.receipts.lock().unwrap();
        let recorded = sink.transactions.lock().unwrap();
        for (n, (receipt, capture)) in receipts.iter().zip(recorded.iter()).enumerate() {
            assert_eq!(receipt.sequence, n as u64 + 1);
            assert_eq!(receipt.sequence, capture.sequence);
            assert_eq!(
                (receipt.first_frame, receipt.db_pages),
                (capture.first_frame, capture.db_pages)
            );
            assert_eq!(
                u64::from(receipt.wal_frames),
                (receipt.wal_end - WAL_HEADER as u64) / frame
            );
            assert!(receipt.real_frames <= receipt.wal_frames);
            if n > 0 {
                assert!(
                    receipt.wal_end > receipts[n - 1].wal_end,
                    "positions only grow"
                );
            }
        }
        assert_eq!(receipts[3].wal_end, vfs.committed_wal_end());
    }
    // A veto fails BEFORE the commit: no approval event, no receipt, nothing pending, no fault.
    sink.veto.store(true, Ordering::SeqCst);
    assert!(conn
        .execute(
            "INSERT INTO messages VALUES ('vetoed','general','v needle')",
            []
        )
        .is_err());
    assert_eq!(
        (
            sink.count(),
            vfs.local_commit_sequence(),
            vfs.unreceipted_commit()
        ),
        (4, 4, None)
    );
    assert_eq!(vfs.receipt_failed_after_commit(), None);
    assert!(!vfs.is_poisoned());
    sink.veto.store(false, Ordering::SeqCst);
    conn.execute(
        "INSERT INTO messages VALUES ('b6','general','b6 needle')",
        [],
    )
    .unwrap();
    assert_eq!((vfs.commit_sequence(), vfs.local_commit_sequence()), (5, 5));
    assert_eq!(sink.events.lock().unwrap().len(), 10);
    drop(conn);
    drop(vfs);
    let text = dump(&db).join("\n");
    assert!(text.contains("b6|") && !text.contains("vetoed") && !text.contains("gone|"));
}

#[test]
fn a_failed_or_panicking_receipt_after_a_real_commit_poisons_but_the_commit_survives() {
    let (dir, db) = fixture();
    let sink = Recorder::new(64 << 10);
    for (panics, id) in [(false, "r1"), (true, "r2")] {
        let vfs = WalVfs::register(&db, Config::DEFAULT, sink.clone()).unwrap();
        let conn = vfs.open_connection(READ_WRITE).unwrap();
        let approved = sink.count();
        if panics {
            sink.panic_receipt.store(true, Ordering::SeqCst);
        } else {
            sink.fail_receipt.store(true, Ordering::SeqCst);
        }
        let result = conn.execute(
            "INSERT INTO messages VALUES (?1, 'general', 'receipt needle')",
            [id],
        );
        // The caller sees an error, but this was NOT a rollback or a veto: the commit happened
        // and the failure is reported as such.
        assert!(result.is_err());
        assert_eq!(sink.count(), approved + 1);
        assert_eq!(vfs.receipt_failed_after_commit(), Some(1));
        assert_eq!(
            vfs.local_commit_sequence(),
            0,
            "no receipt was ever accepted"
        );
        assert!(vfs.is_poisoned());
        // No applied checkpoint is manufactured and no new work is served.
        assert!(conn
            .execute(
                "INSERT INTO messages VALUES ('after','general','after needle')",
                []
            )
            .is_err());
        assert_eq!(
            sink.count(),
            approved + 1,
            "the refused write never reached the sink"
        );
        assert!(vfs
            .snapshot(&conn, &dir.path().join("never.sqlite3"))
            .is_err());
        assert!(!dir.path().join("never.sqlite3").exists());
        assert!(vfs.open_connection(READ_WRITE).is_err());
        drop(conn);
        drop(vfs);
        sink.fail_receipt.store(false, Ordering::SeqCst);
        sink.panic_receipt.store(false, Ordering::SeqCst);
        // The committed row is visible after reopening with a plain connection (which also
        // recovers and checkpoints the WAL); the refused one is not.
        let text = dump(&db).join("\n");
        assert!(
            text.contains(&format!("{id}|")),
            "{id} was really committed"
        );
        assert!(!text.contains("after|"));
    }
}

#[test]
fn automatic_passive_checkpoints_continue_while_the_owned_hook_stays_installed() {
    let (_dir, db) = fixture();
    let sink = Recorder::new(64 << 10);
    let config = Config {
        autocheckpoint_frames: 8,
        ..Config::DEFAULT
    };
    let vfs = WalVfs::register(&db, config, sink.clone()).unwrap();
    let conn = vfs.open_connection(READ_WRITE).unwrap();
    for i in 0..60 {
        conn.execute(
            "INSERT INTO messages VALUES (?1, 'general', 'auto needle')",
            [format!("a{i:02}")],
        )
        .unwrap();
    }
    assert!(!vfs.is_poisoned());
    // The hook replaced SQLite's default one yet reproduces its PASSIVE checkpoint: whole
    // committed pages reached the main database while commits went on, and a fully
    // checkpointed WAL restarted (a later commit begins a new generation at frame 1).
    assert!(vfs.checkpointed_pages() > 0);
    {
        let recorded = sink.transactions.lock().unwrap();
        assert!(recorded
            .iter()
            .skip(1)
            .any(|r| r.first_frame == 1 && r.generation != recorded[0].generation));
    }
    // A checkpoint is never a commit receipt: exactly one receipt per commit.
    assert_eq!(
        (
            vfs.commit_sequence(),
            vfs.local_commit_sequence(),
            sink.receipts.lock().unwrap().len()
        ),
        (60, 60, 60)
    );
    // The pragma that would replace the hook is denied while armed; reading it is not.
    assert!(conn
        .execute_batch("PRAGMA wal_autocheckpoint = 5;")
        .is_err());
    assert!(conn
        .query_row("PRAGMA wal_autocheckpoint", [], |r| r.get::<_, i64>(0))
        .is_ok());
    drop(conn);
    drop(vfs);

    // A zero threshold disables automatic checkpoints, as wal_autocheckpoint(0) would.
    let quiet = Recorder::new(64 << 10);
    let config = Config {
        autocheckpoint_frames: 0,
        ..Config::DEFAULT
    };
    let vfs = WalVfs::register(&db, config, quiet.clone()).unwrap();
    let conn = vfs.open_connection(READ_WRITE).unwrap();
    for i in 0..30 {
        conn.execute(
            "INSERT INTO messages VALUES (?1, 'general', 'quiet needle')",
            [format!("q{i:02}")],
        )
        .unwrap();
    }
    assert_eq!(vfs.checkpointed_pages(), 0);
    assert_eq!(vfs.local_commit_sequence(), 30);
    let recorded = quiet.transactions.lock().unwrap();
    let first = recorded[0].generation;
    assert!(recorded.iter().all(|r| r.generation == first));
}
