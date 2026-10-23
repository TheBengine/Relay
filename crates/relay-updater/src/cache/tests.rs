use super::*;
use crate::digest;
use std::io::Write;

fn open(root: &Path) -> Cache {
    storage::guard(root).unwrap();
    fs::create_dir_all(root).unwrap();
    Cache::open(root.into(), &"a".repeat(64)).unwrap()
}
fn directory() -> tempfile::TempDir {
    let root = PathBuf::from(
        std::env::var_os("RELAY_TEST_WORK_ROOT").expect("Explicit owned fixture root required"),
    );
    storage::guard(&root).unwrap();
    fs::create_dir_all(&root).unwrap();
    tempfile::tempdir_in(root).unwrap()
}
fn fixture_release() -> Release {
    Release {
        schema: 1,
        app_id: crate::APP_ID.into(),
        platform: "windows-x64".into(),
        channel: "stable".into(),
        version: "0.10.1".into(),
        sequence: "20261010000000".into(),
        installer_target: "windows-x64/stable/Relay-0.10.1-20261010000000-Setup.exe".into(),
        installer_sha256: digest(b"fixture"),
        installer_bytes: 7,
        notes: String::new(),
        handoff: None,
    }
}
fn complete(cache: &mut Cache, mut release: Release, bytes: &[u8]) -> PathBuf {
    release.installer_bytes = bytes.len() as u64;
    release.installer_sha256 = digest(bytes);
    let destination = cache.directory.join(Receipt::from_release(&release).name);
    let mut stage = cache.stage(&release).unwrap();
    stage.file().write_all(bytes).unwrap();
    stage.persist(&destination).unwrap();
    cache.record(&release, &destination).unwrap();
    drop(stage);
    destination
}

#[test]
fn only_verified_old_installer_is_pruned_and_unknown_files_survive() {
    let root = directory();
    let mut cache = open(root.path());
    let mut release = fixture_release();
    let unknown = root.path().join("my-file.txt");
    fs::write(&unknown, b"keep").unwrap();
    let first = complete(&mut cache, release.clone(), b"first");
    release.sequence = "20261011000000".into();
    let second = complete(&mut cache, release, b"second");
    assert!(!first.exists());
    assert_eq!(fs::read(second).unwrap(), b"second");
    assert_eq!(fs::read(unknown).unwrap(), b"keep");
    assert_eq!(cache.ledger.files.len(), 1);
}
#[test]
fn quota_counts_unknown_nested_files_and_rejects_before_creating_partial() {
    let root = directory();
    let mut cache = open(root.path());
    cache.limit = 128 * 1024 + 256;
    let nested = root.path().join("unknown");
    fs::create_dir(&nested).unwrap();
    let unknown = nested.join("keep.bin");
    fs::write(&unknown, vec![1; 257]).unwrap();
    let release = fixture_release();
    assert!(cache.stage(&release).err().unwrap().contains("full"));
    assert_eq!(fs::read(unknown).unwrap().len(), 257);
    assert!(!cache.journal().exists());
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
}
#[test]
fn corrupt_receipted_installer_is_never_deleted_or_replaced_during_cleanup() {
    let root = directory();
    let mut cache = open(root.path());
    let mut release = fixture_release();
    let first = complete(&mut cache, release.clone(), b"owned installer");
    fs::write(&first, b"changed contents").unwrap();
    release.sequence = "20261011000000".into();
    release.installer_sha256 = digest(b"new");
    release.installer_bytes = 3;
    let next = root.path().join(Receipt::from_release(&release).name);
    fs::write(&next, b"new").unwrap();
    assert!(cache
        .record(&release, &next)
        .unwrap_err()
        .contains("preserved"));
    assert_eq!(fs::read(first).unwrap(), b"changed contents");
    assert_eq!(fs::read(next).unwrap(), b"new");
}
#[test]
fn valid_journal_recovers_only_its_exact_bounded_partial_and_missing_part_is_safe() {
    let root = directory();
    let mut cache = open(root.path());
    let release = fixture_release();
    let unknown = root.path().join("download-unknown.part");
    fs::write(&unknown, b"unknown").unwrap();
    let mut stage = cache.stage(&release).unwrap();
    stage.file().write_all(b"partial").unwrap();
    let partial = stage.path().to_path_buf();
    let temp = stage.temporary.take().unwrap();
    temp.keep().unwrap();
    std::mem::forget(stage);
    let recovered = open(root.path());
    assert!(!partial.exists());
    assert!(!recovered.journal().exists());
    assert_eq!(fs::read(&unknown).unwrap(), b"unknown");
    let missing = Intent {
        schema: 1,
        publisher: "a".repeat(64),
        name: "download-missing.part".into(),
        maximum: 3,
        installer_sha256: digest(b"new"),
    };
    storage::atomic_json(&recovered.journal(), &missing).unwrap();
    let recovered = open(root.path());
    assert!(!recovered.journal().exists());
    assert!(unknown.exists());
}
#[test]
fn malformed_foreign_or_changed_pending_records_are_preserved() {
    for mode in ["broken", "foreign", "traversal", "oversize"] {
        let root = directory();
        let cache = open(root.path());
        let part = root.path().join("download-owned.part");
        fs::write(&part, b"partial").unwrap();
        let mut intent = Intent {
            schema: 1,
            publisher: "a".repeat(64),
            name: "download-owned.part".into(),
            maximum: 7,
            installer_sha256: digest(b"installer"),
        };
        match mode {
            "foreign" => intent.publisher = "b".repeat(64),
            "traversal" => intent.name = "../user-file.part".into(),
            "oversize" => intent.maximum = 1,
            _ => {}
        }
        let bytes = if mode == "broken" {
            b"{half".to_vec()
        } else {
            serde_json::to_vec(&intent).unwrap()
        };
        fs::write(cache.journal(), &bytes).unwrap();
        assert!(Cache::open(root.path().into(), &"a".repeat(64)).is_err());
        assert_eq!(fs::read(cache.journal()).unwrap(), bytes);
        assert_eq!(fs::read(part).unwrap(), b"partial");
    }
}
#[test]
fn dropped_normal_download_removes_partial_before_intent_and_does_not_touch_unknown() {
    let root = directory();
    let mut cache = open(root.path());
    let release = fixture_release();
    let unknown = root.path().join("keep");
    fs::write(&unknown, b"keep").unwrap();
    let mut staged = cache.stage(&release).unwrap();
    staged.file().write_all(b"partial").unwrap();
    let path = staged.path().to_path_buf();
    drop(staged);
    assert!(!path.exists());
    assert!(!cache.journal().exists());
    assert_eq!(fs::read(unknown).unwrap(), b"keep");
}
#[test]
fn reservation_reclaims_verified_owned_file_but_protects_accepted_current_file() {
    let root = directory();
    let mut cache = open(root.path());
    let release = fixture_release();
    let owned = complete(&mut cache, release.clone(), &vec![1; 8000]);
    cache.limit = cache.inventory().unwrap() + 128 * 1024 + release.installer_bytes - 1;
    let staged = cache.stage(&release).unwrap();
    assert!(!owned.exists());
    drop(staged);
    cache.limit = MAX_CACHE;
    let owned = complete(&mut cache, release, &vec![1; 8000]);
    cache.limit = cache.inventory().unwrap() + 128 * 1024 - 1;
    let name = owned.file_name().unwrap().to_str().unwrap();
    assert!(cache.make_room(0, Some(name)).is_err());
    assert!(owned.exists());
}
#[test]
fn cache_entry_count_limit_is_enforced_without_deleting_unknown_entries() {
    let root = directory();
    for n in 0..MAX_ENTRIES + 1 {
        fs::write(root.path().join(format!("keep-{n}")), b"").unwrap();
    }
    assert!(Cache::open(root.path().into(), &"a".repeat(64))
        .err()
        .unwrap()
        .contains("entries"));
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), MAX_ENTRIES + 1);
}

#[test]
fn entry_reservation_rejects_a_full_cache_before_writing_its_journal() {
    let root = directory();
    for n in 0..MAX_ENTRIES {
        fs::write(root.path().join(format!("keep-{n}")), b"").unwrap();
    }
    let mut cache = open(root.path());
    assert!(cache
        .stage(&fixture_release())
        .err()
        .unwrap()
        .contains("entries"));
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), MAX_ENTRIES);
    assert!(!cache.journal().exists());
}

#[test]
fn active_download_cannot_be_overwritten_and_changed_journal_is_preserved_on_drop() {
    let root = directory();
    let mut cache = open(root.path());
    let staged = cache.stage(&fixture_release()).unwrap();
    let original = fs::read(cache.journal()).unwrap();
    assert!(cache
        .stage(&fixture_release())
        .err()
        .unwrap()
        .contains("active"));
    assert_eq!(fs::read(cache.journal()).unwrap(), original);
    fs::write(cache.journal(), b"foreign replacement").unwrap();
    drop(staged);
    assert_eq!(fs::read(cache.journal()).unwrap(), b"foreign replacement");
}

#[test]
fn foreign_malformed_traversing_or_hardlinked_receipt_ledger_is_refused_and_preserved() {
    for mode in [
        "foreign",
        "malformed",
        "traversal",
        "version-build",
        "hardlink",
    ] {
        let root = directory();
        let cache = open(root.path());
        let path = root.path().join("installer-receipts.json");
        let mut receipt = Receipt::from_release(&fixture_release());
        if mode == "traversal" {
            receipt.name = "../keep.exe".into();
        }
        if mode == "version-build" {
            receipt.name = format!("Relay-0.10.1+foreign-{}-Setup.exe", receipt.sequence);
        }
        let ledger = Ledger {
            schema: 1,
            publisher: if mode == "foreign" {
                "b".repeat(64)
            } else {
                cache.ledger.publisher.clone()
            },
            files: BTreeMap::from([(receipt.name.clone(), receipt)]),
        };
        let bytes = if mode == "malformed" {
            b"{broken".to_vec()
        } else {
            serde_json::to_vec(&ledger).unwrap()
        };
        let original = root.path().join("original.bin");
        if mode == "hardlink" {
            fs::write(&original, &bytes).unwrap();
            fs::hard_link(&original, &path).unwrap();
        } else {
            fs::write(&path, &bytes).unwrap();
        }
        assert!(Cache::open(root.path().into(), &"a".repeat(64)).is_err());
        assert_eq!(fs::read(path).unwrap(), bytes);
        if original.exists() {
            assert_eq!(fs::read(original).unwrap(), bytes);
        }
    }
}
