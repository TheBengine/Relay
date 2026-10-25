//! Own temporary SQLite files only (fresh folders under the system temp directory): no
//! Relay profile, key, device, network or running server. Written for the later focused run;
//! not executed when written.
use super::*;
use std::fs;

/// `Some((index, bytes))` with the usual slice coercions applied to the second argument.
fn entry(index: u64, bytes: &[u8]) -> Option<(u64, &[u8])> {
    Some((index, bytes))
}
fn record(seed: u8, len: usize) -> Vec<u8> {
    (0..len).map(|i| seed.wrapping_add(i as u8)).collect()
}
fn fresh() -> (tempfile::TempDir, PathBuf) {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("takeover");
    (root, dir)
}
/// Damages the SQLite header of an existing store file in place.
fn damage(file: &Path) {
    use std::io::{Seek, SeekFrom, Write};
    let mut handle = fs::OpenOptions::new().write(true).open(file).unwrap();
    handle.seek(SeekFrom::Start(0)).unwrap();
    handle.write_all(&[0xAB; 100]).unwrap();
    handle.sync_all().unwrap();
}

#[test]
fn the_full_entry_and_metadata_survive_a_reopen_as_the_exact_bytes() {
    let (_root, dir) = fresh();
    let mut store = TakeoverStore::create_new(&dir, Quota::DEFAULT).unwrap();
    assert_eq!(store.load_state().unwrap(), None);
    assert_eq!(store.load_entry(1).unwrap(), None);
    let first = record(1, 300_000);
    let largest = record(2, MAX_ENTRY_JSON);
    store.commit(entry(1, &first), b"state-one").unwrap();
    store
        .commit(entry(2, &largest), &record(9, MAX_STATE))
        .unwrap();
    store.commit(None, b"vote-only-state").unwrap();
    let used = ((first.len() + largest.len()) as u64, 2);
    assert_eq!(store.usage(), (used.1, used.0));
    drop(store);

    let mut store = TakeoverStore::open_existing(&dir, Quota::DEFAULT).unwrap();
    assert_eq!(store.usage(), (used.1, used.0));
    assert_eq!(store.load_state().unwrap().unwrap(), b"vote-only-state");
    assert_eq!(store.load_entry(1).unwrap().unwrap(), first);
    assert_eq!(store.load_entry(2).unwrap().unwrap(), largest);
    // Nothing is ever synthesized for what is not there.
    assert_eq!(store.load_entry(3).unwrap(), None);
    assert_eq!(store.load_entry(0).unwrap(), None);
    assert_eq!(store.load_entry(u64::MAX).unwrap(), None);
    let debug = format!("{store:?}");
    assert!(!debug.contains("state") && !debug.contains("takeover"));
}

#[test]
fn an_identical_commit_is_idempotent_and_a_conflict_writes_nothing_not_even_the_metadata() {
    let (_root, dir) = fresh();
    let mut store = TakeoverStore::create_new(&dir, Quota::DEFAULT).unwrap();
    let kept = record(3, 5_000);
    store.commit(entry(1, &kept), b"s1").unwrap();
    store.commit(entry(1, &kept), b"s2").unwrap();
    assert_eq!(store.usage(), (1, kept.len() as u64));
    let mut same_length = kept.clone();
    same_length[0] ^= 1;
    assert_eq!(
        store.commit(entry(1, &same_length), b"s3").err(),
        Some(StoreError::Conflict)
    );
    assert_eq!(
        store.commit(entry(1, &record(7, 10)), b"s3").err(),
        Some(StoreError::Conflict)
    );
    // Neither the record nor the metadata changed, and the store is still usable.
    assert_eq!(store.load_state().unwrap().unwrap(), b"s2");
    assert_eq!(store.load_entry(1).unwrap().unwrap(), kept);
    assert_eq!(store.usage(), (1, kept.len() as u64));
    store.commit(None, b"s4").unwrap();
    drop(store);
    let mut store = TakeoverStore::open_existing(&dir, Quota::DEFAULT).unwrap();
    assert_eq!(store.load_state().unwrap().unwrap(), b"s4");
    assert_eq!(store.load_entry(1).unwrap().unwrap(), kept);
}

#[test]
fn quota_is_full_without_any_mutation_or_eviction_and_reads_and_growth_stay_bounded() {
    let (_root, dir) = fresh();
    let quota = Quota {
        max_entries: 3,
        max_entry_bytes: 1_000,
    };
    let mut store = TakeoverStore::create_new(&dir, quota).unwrap();
    store.commit(entry(1, &record(1, 400)), b"s1").unwrap();
    store.commit(entry(2, &record(2, 400)), b"s2").unwrap();
    // Too many bytes: Full, with the old state, both records and the usage untouched.
    assert_eq!(
        store.commit(entry(3, &record(3, 300)), b"s3").err(),
        Some(StoreError::Full)
    );
    assert_eq!(store.usage(), (2, 800));
    assert_eq!(store.load_state().unwrap().unwrap(), b"s2");
    assert_eq!(store.load_entry(3).unwrap(), None);
    assert_eq!(store.load_entry(1).unwrap().unwrap(), record(1, 400));
    assert_eq!(store.load_entry(2).unwrap().unwrap(), record(2, 400));
    // Full is definite: the store stays usable; a vote needs no room; a fitting entry works.
    store.commit(None, b"vote").unwrap();
    store.commit(entry(3, &record(3, 200)), b"s3").unwrap();
    // Too many entries, even for a tiny one.
    assert_eq!(
        store.commit(entry(4, &record(4, 1)), b"s4").err(),
        Some(StoreError::Full)
    );
    assert_eq!(store.usage(), (3, 1_000));

    // Repeated vote-only commits do not grow the file without bound.
    let file = TakeoverStore::file(&dir);
    let before = fs::metadata(&file).unwrap().len();
    for i in 0..300usize {
        store
            .commit(None, &record(i as u8, 1_000 + (i % 50)))
            .unwrap();
    }
    assert!(fs::metadata(&file).unwrap().len() <= before + 8 * PAGE_SIZE);
    assert!(!sidecar(&file, "-wal").exists(), "no WAL side file");

    // Out-of-contract inputs fail closed with nothing written and do not break the store.
    let before = store.usage();
    assert_eq!(
        store
            .commit(entry(9, &vec![0; MAX_ENTRY_JSON + 1]), b"x")
            .err(),
        Some(StoreError::Failed)
    );
    assert_eq!(
        store.commit(entry(0, b"x"), b"x").err(),
        Some(StoreError::Failed)
    );
    assert_eq!(
        store.commit(entry(u64::MAX, b"x"), b"x").err(),
        Some(StoreError::Failed)
    );
    assert_eq!(
        store.commit(entry(9, b""), b"x").err(),
        Some(StoreError::Failed)
    );
    assert_eq!(store.commit(None, b"").err(), Some(StoreError::Failed));
    assert_eq!(
        store.commit(None, &vec![0; MAX_STATE + 1]).err(),
        Some(StoreError::Failed)
    );
    assert_eq!(store.usage(), before);
    store.commit(None, b"still usable").unwrap();
    drop(store);

    // A row that would exceed the read bound is refused at open, by its length alone.
    let raw = Connection::open(&file).unwrap();
    raw.execute_batch(
        "PRAGMA ignore_check_constraints = ON; UPDATE entries SET body = zeroblob(1500000) WHERE idx = 1;",
    )
    .unwrap();
    drop(raw);
    assert_eq!(
        TakeoverStore::open_existing(&dir, quota).err(),
        Some(OpenError::Corrupt)
    );
}

#[test]
fn a_failed_transaction_leaves_no_entry_no_metadata_and_a_full_disk_is_failed_not_full() {
    let (_root, dir) = fresh();
    let mut store = TakeoverStore::create_new(&dir, Quota::DEFAULT).unwrap();
    let kept = record(5, 2_000);
    store.commit(entry(1, &kept), b"s1").unwrap();

    // The transaction ends before COMMIT: Failed, the store stops, and a reopen shows neither.
    store.abort_before_commit = true;
    assert_eq!(
        store.commit(entry(2, &record(6, 2_000)), b"s2").err(),
        Some(StoreError::Failed)
    );
    assert!(store.broken);
    assert_eq!(store.load_state().err(), Some(StoreError::Failed));
    assert_eq!(store.load_entry(1).err(), Some(StoreError::Failed));
    assert_eq!(store.commit(None, b"s3").err(), Some(StoreError::Failed));
    drop(store);
    let mut store = TakeoverStore::open_existing(&dir, Quota::DEFAULT).unwrap();
    assert_eq!(store.load_state().unwrap().unwrap(), b"s1");
    assert_eq!(store.load_entry(1).unwrap().unwrap(), kept);
    assert_eq!(store.load_entry(2).unwrap(), None);
    assert_eq!(store.usage(), (1, kept.len() as u64));

    // A real SQLite disk-full refusal (no room to grow) is an uncertain Failed, never Full.
    let pages: i64 = store
        .conn
        .query_row("PRAGMA page_count", [], |r| r.get(0))
        .unwrap();
    store
        .conn
        .execute_batch(&format!("PRAGMA max_page_count = {pages};"))
        .unwrap();
    assert_eq!(
        store.commit(entry(2, &record(7, 800_000)), b"s4").err(),
        Some(StoreError::Failed)
    );
    assert!(store.broken);
    drop(store);
    let mut store = TakeoverStore::open_existing(&dir, Quota::DEFAULT).unwrap();
    assert_eq!(store.load_state().unwrap().unwrap(), b"s1");
    assert_eq!(store.load_entry(2).unwrap(), None);
}

#[test]
fn create_and_open_refuse_missing_corrupt_foreign_linked_and_unsafe_storage() {
    let (root, dir) = fresh();
    // Opening never creates: no folder, then a folder without a database.
    assert_eq!(
        TakeoverStore::open_existing(&dir, Quota::DEFAULT).err(),
        Some(OpenError::Missing)
    );
    assert!(!dir.exists());
    fs::create_dir_all(&dir).unwrap();
    assert_eq!(
        TakeoverStore::open_existing(&dir, Quota::DEFAULT).err(),
        Some(OpenError::Missing)
    );
    assert!(!TakeoverStore::file(&dir).exists());
    // An invalid quota is refused before any file is touched.
    let nothing = Quota {
        max_entries: 0,
        max_entry_bytes: 1,
    };
    assert_eq!(
        TakeoverStore::create_new(&dir, nothing).err(),
        Some(OpenError::Invalid)
    );
    assert!(!TakeoverStore::file(&dir).exists());

    // create_new never replaces what is there.
    let mut store = TakeoverStore::create_new(&dir, Quota::DEFAULT).unwrap();
    store.commit(entry(1, b"kept"), b"s1").unwrap();
    drop(store);
    let file = TakeoverStore::file(&dir);
    let before = fs::read(&file).unwrap();
    assert_eq!(
        TakeoverStore::create_new(&dir, Quota::DEFAULT).err(),
        Some(OpenError::AlreadyExists)
    );
    assert_eq!(fs::read(&file).unwrap(), before);

    // A damaged header is Corrupt and the file is left exactly as found (not repaired or reset).
    let (_other_root, damaged_dir) = fresh();
    drop(TakeoverStore::create_new(&damaged_dir, Quota::DEFAULT).unwrap());
    let damaged = TakeoverStore::file(&damaged_dir);
    damage(&damaged);
    let broken_bytes = fs::read(&damaged).unwrap();
    assert_eq!(
        TakeoverStore::open_existing(&damaged_dir, Quota::DEFAULT).err(),
        Some(OpenError::Corrupt)
    );
    assert_eq!(fs::read(&damaged).unwrap(), broken_bytes);

    // A foreign object, another application id or version and a WAL-mode file are refused
    // with the bytes left exactly as found and no side file created: nothing is converted.
    let refused_unchanged = |expected: OpenError| {
        let before = fs::read(&file).unwrap();
        assert_eq!(
            TakeoverStore::open_existing(&dir, Quota::DEFAULT).err(),
            Some(expected)
        );
        assert_eq!(fs::read(&file).unwrap(), before);
        for extra in ["-wal", "-shm", "-journal"] {
            assert!(!sidecar(&file, extra).exists());
        }
    };
    let raw = Connection::open(&file).unwrap();
    raw.execute_batch("CREATE TABLE extra (x);").unwrap();
    drop(raw);
    refused_unchanged(OpenError::Mismatch);
    let raw = Connection::open(&file).unwrap();
    raw.execute_batch("DROP TABLE extra; PRAGMA user_version = 2;")
        .unwrap();
    drop(raw);
    refused_unchanged(OpenError::Mismatch);
    let raw = Connection::open(&file).unwrap();
    raw.execute_batch("PRAGMA user_version = 1; PRAGMA application_id = 7;")
        .unwrap();
    drop(raw);
    refused_unchanged(OpenError::Mismatch);
    let raw = Connection::open(&file).unwrap();
    raw.execute_batch(&format!("PRAGMA application_id = {APPLICATION_ID};"))
        .unwrap();
    drop(raw);
    assert!(TakeoverStore::open_existing(&dir, Quota::DEFAULT).is_ok());
    // A cleanly closed WAL-mode copy of this very store (no -wal/-shm left) is not converted
    // back to the rollback journal on open: it is refused and its bytes stay as they were.
    let raw = Connection::open(&file).unwrap();
    let mode: String = raw
        .query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
    drop(raw);
    let header = fs::read(&file).unwrap();
    assert_eq!((header[18], header[19]), (2, 2));
    for extra in ["-wal", "-shm"] {
        assert!(!sidecar(&file, extra).exists());
    }
    refused_unchanged(OpenError::Mismatch);
    // Back to the rollback journal, done by the test itself: open never does this.
    let raw = Connection::open(&file).unwrap();
    raw.query_row("PRAGMA journal_mode = DELETE", [], |r| {
        r.get::<_, String>(0)
    })
    .unwrap();
    drop(raw);
    assert!(TakeoverStore::open_existing(&dir, Quota::DEFAULT).is_ok());

    // A hard link to the database, a directory in its place and a WAL file are unsafe.
    let alias = root.path().join("alias.sqlite3");
    fs::hard_link(&file, &alias).unwrap();
    assert_eq!(
        TakeoverStore::open_existing(&dir, Quota::DEFAULT).err(),
        Some(OpenError::Unsafe)
    );
    fs::remove_file(&alias).unwrap();
    fs::write(sidecar(&file, "-wal"), b"x").unwrap();
    assert_eq!(
        TakeoverStore::open_existing(&dir, Quota::DEFAULT).err(),
        Some(OpenError::Unsafe)
    );
    fs::remove_file(sidecar(&file, "-wal")).unwrap();
    let (_third_root, odd_dir) = fresh();
    fs::create_dir_all(TakeoverStore::file(&odd_dir)).unwrap();
    assert_eq!(
        TakeoverStore::open_existing(&odd_dir, Quota::DEFAULT).err(),
        Some(OpenError::Unsafe)
    );
}

#[cfg(unix)]
#[test]
fn a_symlinked_or_loosely_permissioned_database_is_refused_on_unix() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let (root, dir) = fresh();
    drop(TakeoverStore::create_new(&dir, Quota::DEFAULT).unwrap());
    let file = TakeoverStore::file(&dir);
    assert_eq!(fs::metadata(&file).unwrap().permissions().mode() & 0o077, 0);
    fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(
        TakeoverStore::open_existing(&dir, Quota::DEFAULT).err(),
        Some(OpenError::Unsafe)
    );
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
    let target = root.path().join("elsewhere.sqlite3");
    fs::rename(&file, &target).unwrap();
    symlink(&target, &file).unwrap();
    assert_eq!(
        TakeoverStore::open_existing(&dir, Quota::DEFAULT).err(),
        Some(OpenError::Unsafe)
    );
}

#[test]
fn a_reopened_store_holds_real_exclusive_ownership_before_its_first_commit() {
    let (_root, dir) = fresh();
    let mut store = TakeoverStore::create_new(&dir, Quota::DEFAULT).unwrap();
    store.commit(entry(1, b"kept-record"), b"s1").unwrap();
    drop(store);

    // Owner A reopens and has not committed anything yet.
    let mut a = TakeoverStore::open_existing(&dir, Quota::DEFAULT).unwrap();
    let quick = Duration::from_millis(100);
    let started = std::time::Instant::now();
    assert_eq!(
        TakeoverStore::open_with_timeout(&dir, Quota::DEFAULT, quick).err(),
        Some(OpenError::Busy)
    );
    assert!(started.elapsed() < Duration::from_secs(4));
    // Not merely a mode name: an independent SQLite reader is locked out as well.
    let reader =
        Connection::open_with_flags(TakeoverStore::file(&dir), OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    reader.busy_timeout(quick).unwrap();
    let blocked = reader.query_row("SELECT count(*) FROM sqlite_master", [], |r| {
        r.get::<_, i64>(0)
    });
    assert!(matches!(
        blocked,
        Err(rusqlite::Error::SqliteFailure(code, _)) if code.code == ErrorCode::DatabaseBusy
    ));
    drop(reader);
    // A is unchanged, is still the only owner after its first commit, and B waits for it.
    assert_eq!(a.usage(), (1, 11));
    assert_eq!(a.load_state().unwrap().unwrap(), b"s1");
    a.commit(None, b"s2").unwrap();
    assert_eq!(
        TakeoverStore::open_with_timeout(&dir, Quota::DEFAULT, quick).err(),
        Some(OpenError::Busy)
    );
    drop(a);
    let mut b = TakeoverStore::open_existing(&dir, Quota::DEFAULT).unwrap();
    assert_eq!(b.load_state().unwrap().unwrap(), b"s2");
    assert_eq!(b.usage(), (1, 11));
}

#[cfg(unix)]
#[test]
fn a_loosely_permissioned_linked_or_symlinked_rollback_journal_is_refused_before_sqlite_opens_it() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let (root, dir) = fresh();
    drop(TakeoverStore::create_new(&dir, Quota::DEFAULT).unwrap());
    let journal = sidecar(&TakeoverStore::file(&dir), "-journal");
    fs::write(&journal, b"").unwrap();
    fs::set_permissions(&journal, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(
        TakeoverStore::open_existing(&dir, Quota::DEFAULT).err(),
        Some(OpenError::Unsafe)
    );
    fs::set_permissions(&journal, fs::Permissions::from_mode(0o600)).unwrap();
    let alias = root.path().join("journal-alias");
    fs::hard_link(&journal, &alias).unwrap();
    assert_eq!(
        TakeoverStore::open_existing(&dir, Quota::DEFAULT).err(),
        Some(OpenError::Unsafe)
    );
    fs::remove_file(&alias).unwrap();
    // An empty private journal is not hot, so it is accepted.
    assert!(TakeoverStore::open_existing(&dir, Quota::DEFAULT).is_ok());
    let _ = fs::remove_file(&journal);
    symlink(root.path().join("elsewhere"), &journal).unwrap();
    assert_eq!(
        TakeoverStore::open_existing(&dir, Quota::DEFAULT).err(),
        Some(OpenError::Unsafe)
    );
}
