use super::*;
use std::{
    fs,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

#[derive(Serialize, Deserialize)]
struct Input {
    root: Vec<u8>,
    release: Release,
    objects: BTreeMap<String, Vec<u8>>,
}
fn capture(fixture: &Fixture, path: &Path) {
    storage::guard(path).unwrap();
    let input = Input {
        root: fixture.root.clone(),
        release: fixture.release.clone(),
        objects: fixture.memory.objects.lock().unwrap().clone(),
    };
    fs::write(path, serde_json::to_vec(&input).unwrap()).unwrap();
}
fn child(input: &Path, root: &Path, point: &str, output: &Path) {
    storage::guard(input).unwrap();
    storage::guard(root).unwrap();
    storage::guard(output).unwrap();
    let exe = std::env::current_exe().unwrap();
    storage::guard(&exe).unwrap();
    let log = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)
        .unwrap();
    let mut command = Command::new(exe);
    command
        .args([
            "--ignored",
            "--exact",
            "tests::storage_recovery::storage_child",
            "--nocapture",
        ])
        .env("RELAY_UPDATER_FIXTURE_BREAK", point)
        .env("RELAY_UPDATER_FIXTURE_INPUT", input)
        .env("RELAY_UPDATER_FIXTURE_ROOT", root)
        .stdin(Stdio::null())
        .stdout(log.try_clone().unwrap())
        .stderr(log);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let mut process = command.spawn().unwrap();
    let end = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = process.try_wait().unwrap() {
            assert_eq!(
                status.code(),
                Some(77),
                "Expected owned controlled interruption at {point}"
            );
            return;
        }
        assert!(
            Instant::now() < end,
            "Owned fixture still running; inspect it, do not replay or force-stop"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
#[tokio::test]
#[ignore = "Entry point for a bounded owned subprocess; never a real client/installer"]
async fn storage_child() {
    let input = PathBuf::from(std::env::var_os("RELAY_UPDATER_FIXTURE_INPUT").unwrap());
    storage::guard(&input).unwrap();
    let fixture: Input =
        serde_json::from_slice(&storage::read(&input, 256 * 1024).unwrap()).unwrap();
    let directory = PathBuf::from(std::env::var_os("RELAY_UPDATER_FIXTURE_ROOT").unwrap());
    storage::guard(&directory).unwrap();
    let publisher = Publisher::from_bundled_root(
        fixture.root,
        "https://publisher.fixture.invalid/metadata/",
        "https://publisher.fixture.invalid/targets/",
        "stable",
    )
    .unwrap();
    let engine = Engine::new(
        publisher,
        InstalledRelease {
            version: "0.10.0".into(),
            sequence: "20261009034055".into(),
        },
        directory,
    )
    .unwrap();
    let memory = Memory {
        objects: Arc::new(Mutex::new(fixture.objects)),
        broken: None,
        requested: Arc::new(Mutex::new(vec![])),
    };
    let point = std::env::var("RELAY_UPDATER_FIXTURE_BREAK").unwrap();
    if point.starts_with("download-") {
        engine
            .download_with(&fixture.release, memory)
            .await
            .unwrap();
    } else {
        engine.check_with(memory).await.unwrap();
    }
    panic!("Requested fixture checkpoint was not reached");
}
#[tokio::test]
async fn actual_process_interruption_at_every_initial_owner_stage_recovers_without_adopting_foreign_files(
) {
    for point in [
        "bootstrap-intent",
        "owner-partial",
        "owner-synced",
        "owner-published",
    ] {
        let base = directory().keep();
        let root = base.join("updates");
        let fixture = Fixture::new();
        let input = base.join("public-signed-fixture.json");
        capture(&fixture, &input);
        child(&input, &root, point, &base.join("child.log"));
        let boot = root.join(format!("bootstrap-{}.owner", fixture.publisher().identity));
        let preserved: BTreeMap<_, _> = fs::read_dir(&boot)
            .unwrap()
            .filter_map(|entry| {
                let path = entry.unwrap().path();
                if path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("owner-")
                {
                    Some((path.clone(), fs::read(path).unwrap()))
                } else {
                    None
                }
            })
            .collect();
        assert!(matches!(
            fixture
                .engine(&root)
                .check_with(fixture.memory.clone())
                .await
                .unwrap(),
            Check::Available(_)
        ));
        let owner: Value =
            serde_json::from_slice(&fs::read(root.join("publisher.json")).unwrap()).unwrap();
        assert_eq!(
            owner,
            json!({"schema":1,"publisher":fixture.publisher().identity})
        );
        for (path, bytes) in preserved {
            assert_eq!(fs::read(path).unwrap(), bytes);
        }
        let profile = base.join("separate-profile-sentinel");
        fs::write(&profile, b"not an actor key").unwrap();
        fixture
            .engine(&root)
            .check_with(fixture.memory.clone())
            .await
            .unwrap();
        assert_eq!(fs::read(profile).unwrap(), b"not an actor key");
        fs::write(base.join("proof.json"),serde_json::to_vec(&json!({"checkpoint":point,"child_exit":77,"restarted_verified_check":true,"partial_owner_files_preserved":true,"real_actors":false})).unwrap()).unwrap();
    }
}
#[tokio::test]
async fn actual_download_process_interruption_recovers_and_cannot_roll_back_observed_metadata() {
    for point in [
        "download-created",
        "download-journaled",
        "download-partial",
        "download-published",
        "download-receipted",
    ] {
        let base = directory().keep();
        let root = base.join("updates");
        let fixture = Fixture::new();
        fixture
            .engine(&root)
            .check_with(fixture.memory.clone())
            .await
            .unwrap();
        let old = Memory {
            objects: Arc::new(Mutex::new(fixture.memory.objects.lock().unwrap().clone())),
            broken: None,
            requested: Arc::new(Mutex::new(vec![])),
        };
        fixture.publish(2);
        let input = base.join("public-signed-fixture.json");
        capture(&fixture, &input);
        child(&input, &root, point, &base.join("child.log"));
        let head = fs::read(root.join("head.json")).unwrap();
        assert!(fixture.engine(&root).check_with(old).await.is_err());
        assert_eq!(fs::read(root.join("head.json")).unwrap(), head);
        let downloads = root.join("downloads");
        let unknown = downloads.join("keep-user-file");
        fs::write(&unknown, b"keep").unwrap();
        let orphan_names: Vec<_> = fs::read_dir(&downloads)
            .unwrap()
            .filter_map(|e| {
                let p = e.unwrap().path();
                if p.extension().is_some_and(|v| v == "part") {
                    Some(p)
                } else {
                    None
                }
            })
            .collect();
        let ready = fixture
            .engine(&root)
            .download_with(&fixture.release, fixture.memory.clone())
            .await
            .unwrap();
        ready.revalidate().unwrap();
        assert_eq!(fs::read(ready.path()).unwrap(), fixture.installer);
        assert_eq!(fs::read(unknown).unwrap(), b"keep");
        assert!(!downloads.join("download-intent.json").exists());
        for path in orphan_names {
            assert_eq!(path.exists(), point == "download-created");
        }
        if matches!(point, "download-published" | "download-receipted") {
            assert!(!fixture
                .memory
                .requested
                .lock()
                .unwrap()
                .iter()
                .any(|p| p == &format!("/targets/{}", fixture.release.installer_target)));
        }
        fs::write(base.join("proof.json"),serde_json::to_vec(&json!({"checkpoint":point,"child_exit":77,"newer_metadata_retained":true,"signed_rollback_refused":true,"verified_restart_download":true,"unreceipted_zero_partial_preserved":point=="download-created","installer_executed":false})).unwrap()).unwrap();
    }
}
#[test]
fn legacy_partial_owner_and_foreign_bootstrap_contents_are_never_repaired_or_deleted() {
    let fixture = Fixture::new();
    for mode in [
        "legacy",
        "foreign-policy",
        "unexpected-file",
        "locked-bootstrap",
    ] {
        let base = directory();
        let root = base.path().join("updates");
        fs::create_dir(&root).unwrap();
        let publisher = fixture.publisher();
        let boot = root.join(format!("bootstrap-{}.owner", publisher.identity));
        let mut held = None;
        let sentinel = match mode {
            "legacy" => root.join("publisher.json"),
            "foreign-policy" => {
                let path = root.join(format!("bootstrap-{}.owner", "f".repeat(64)));
                fs::create_dir(&path).unwrap();
                path.join("keep")
            }
            "unexpected-file" => {
                fs::create_dir(&boot).unwrap();
                boot.join("user-file")
            }
            _ => {
                fs::create_dir(&boot).unwrap();
                let path = boot.join("engine.lock");
                let file = fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create_new(true)
                    .open(&path)
                    .unwrap();
                fs2::FileExt::try_lock_exclusive(&file).unwrap();
                held = Some(file);
                base.path().join("profile-sentinel")
            }
        };
        fs::write(&sentinel, b"partial-or-unknown").unwrap();
        let error = Store::open(&root, &publisher.identity).err().unwrap();
        if mode == "locked-bootstrap" {
            assert!(error.contains("initialization is already running"));
        }
        assert_eq!(fs::read(sentinel).unwrap(), b"partial-or-unknown");
        drop(held);
        if mode == "locked-bootstrap" {
            drop(Store::open(&root, &publisher.identity).unwrap());
        }
    }
}
#[tokio::test]
async fn interrupted_newer_payload_durably_advances_only_authenticated_metadata_not_installed_version(
) {
    let fixture = Fixture::new();
    let base = directory();
    let engine = fixture.engine(base.path());
    engine.check_with(fixture.memory.clone()).await.unwrap();
    let old = Memory {
        objects: Arc::new(Mutex::new(fixture.memory.objects.lock().unwrap().clone())),
        broken: None,
        requested: Arc::new(Mutex::new(vec![])),
    };
    fixture.publish(2);
    let mut broken = fixture.memory.clone();
    broken.broken = Some(format!("/targets/{}", fixture.release.installer_target));
    assert!(engine
        .download_with(&fixture.release, broken)
        .await
        .is_err());
    assert!(engine.check_with(old).await.is_err());
    assert_eq!(engine.installed.sequence, "20261009034055");
    assert_eq!(engine.installed.version, "0.10.0");
    let head: Value =
        serde_json::from_slice(&fs::read(base.path().join("head.json")).unwrap()).unwrap();
    assert_eq!(head["highest"]["sequence"], fixture.release.sequence);
    assert_eq!(generations(base.path()), 1);
    assert_eq!(
        fs::read_dir(base.path().join("downloads")).unwrap().count(),
        0
    );
}
