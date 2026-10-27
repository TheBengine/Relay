use super::*;

struct FakeProbe {
    kind: DriveKind,
    file_system: &'static str,
    free: u64,
}
impl Probe for FakeProbe {
    fn drive(&self, _: &Path) -> DriveInfo {
        DriveInfo {
            kind: self.kind,
            file_system: self.file_system.into(),
            free_bytes: self.free,
        }
    }
}
const FIXED: FakeProbe = FakeProbe {
    kind: DriveKind::Fixed,
    file_system: "NTFS",
    free: u64::MAX / 4,
};

struct World {
    _temp: tempfile::TempDir,
    base: PathBuf,
    defaults: DefaultFolders,
}

fn world() -> World {
    let parent = std::env::var_os("RELAY_TEST_WORK_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    fs::create_dir_all(&parent).unwrap();
    let temp = tempfile::tempdir_in(parent).unwrap();
    let base = temp.path().to_path_buf();
    let app = base.join("appdata");
    let defaults = DefaultFolders {
        config: app.join("Roaming").join("config"),
        local: app.join("Local").join("data"),
        server: app.join("Local").join("server-data"),
        replicas: app.join("Local").join("replica-copies"),
        bootstrap_dir: app.join("Local").join("bootstrap"),
    };
    World {
        _temp: temp,
        base,
        defaults,
    }
}

fn put(path: PathBuf, bytes: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

/// A realistic default profile: every storage area has something in it.
fn populate(d: &DefaultFolders) {
    put(d.config.join("preferences.json"), b"prefs");
    put(
        d.config.join("account-webview").join("Cookies"),
        &[7u8; 70_000],
    );
    put(d.config.join("host-moves").join("route.json"), b"moves");
    put(
        d.config.join("host-locator-abc").join("queue.json"),
        b"outbox",
    );
    put(
        d.local.join("encryption").join("a.device"),
        b"device-checkpoint",
    );
    put(
        d.local.join("encryption").join("history").join("seg-1"),
        &[1u8; 300_000],
    );
    put(d.local.join("iroh").join("key"), b"iroh");
    put(
        d.local.join("handover").join("h1").join("state"),
        b"handover",
    );
    fs::create_dir_all(d.local.join("private-network").join("empty")).unwrap();
    put(d.server.join("relay.sqlite3"), b"sqlite main");
    put(
        d.server
            .join("communities")
            .join("c1")
            .join("relay.sqlite3"),
        b"community",
    );
    put(d.replicas.join("r1").join("copy.sqlite3"), b"replica");
}

/// Expected `--profile-dir` layout for `populate`.
fn expected(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    vec![
        (
            root.join("config").join("preferences.json"),
            b"prefs".to_vec(),
        ),
        (
            root.join("config").join("account-webview").join("Cookies"),
            vec![7u8; 70_000],
        ),
        (
            root.join("host-moves").join("route.json"),
            b"moves".to_vec(),
        ),
        (
            root.join("host-locator-abc").join("queue.json"),
            b"outbox".to_vec(),
        ),
        (
            root.join("encryption").join("a.device"),
            b"device-checkpoint".to_vec(),
        ),
        (
            root.join("encryption").join("history").join("seg-1"),
            vec![1u8; 300_000],
        ),
        (root.join("iroh").join("key"), b"iroh".to_vec()),
        (
            root.join("handover").join("h1").join("state"),
            b"handover".to_vec(),
        ),
        (
            root.join("server-data").join("relay.sqlite3"),
            b"sqlite main".to_vec(),
        ),
        (
            root.join("server-data")
                .join("communities")
                .join("c1")
                .join("relay.sqlite3"),
            b"community".to_vec(),
        ),
        (
            root.join("replica-copies").join("r1").join("copy.sqlite3"),
            b"replica".to_vec(),
        ),
    ]
}

fn assert_profile(root: &Path) {
    for (path, bytes) in expected(root) {
        assert_eq!(
            fs::read(&path).unwrap_or_else(|_| panic!("missing {}", path.display())),
            bytes
        );
    }
    assert!(root.join("private-network").join("empty").is_dir());
    assert!(root.join(MARKER).is_file());
    assert!(!root.join(IN_PROGRESS).exists());
}

fn assert_defaults_intact(d: &DefaultFolders) {
    assert_eq!(
        fs::read(d.config.join("preferences.json")).unwrap(),
        b"prefs"
    );
    assert_eq!(
        fs::read(d.local.join("encryption").join("a.device")).unwrap(),
        b"device-checkpoint"
    );
    assert_eq!(
        fs::read(d.server.join("relay.sqlite3")).unwrap(),
        b"sqlite main"
    );
    assert_eq!(
        fs::read(d.replicas.join("r1").join("copy.sqlite3")).unwrap(),
        b"replica"
    );
}

fn boot(d: &DefaultFolders) -> Bootstrap {
    load(&d.bootstrap()).unwrap()
}

fn move_to_folder(w: &World) -> PathBuf {
    let root = w.base.join("D").join("Relay Data");
    schedule(&w.defaults, new_folder_place(root.clone(), false)).unwrap();
    assert_eq!(
        run_pending(&w.defaults, &FIXED, &Faults::default()).unwrap(),
        MoveResult::Moved
    );
    root
}

#[test]
fn folder_validation_rejects_unsafe_targets_and_leaves_no_trace() {
    let w = world();
    let b = Bootstrap::default();
    let check = |root: &Path, allow: bool, probe: &FakeProbe| {
        validate_folder(root, allow, 1000, &w.defaults, &b, probe, false)
    };
    let good = w.base.join("D").join("Relay Data");
    check(&good, false, &FIXED).unwrap();
    assert!(
        !good.exists() && !good.parent().unwrap().exists(),
        "a check must not leave folders"
    );

    assert!(check(Path::new("relative\\data"), false, &FIXED)
        .unwrap_err()
        .contains("complete folder path"));
    assert!(check(Path::new("C:\\"), false, &FIXED)
        .unwrap_err()
        .contains("not a whole drive"));
    let missing = FakeProbe {
        kind: DriveKind::Missing,
        ..FIXED
    };
    assert!(check(&good, false, &missing)
        .unwrap_err()
        .contains("not available"));
    for kind in [DriveKind::Removable, DriveKind::Network] {
        let probe = FakeProbe { kind, ..FIXED };
        assert!(check(&good, false, &probe)
            .unwrap_err()
            .contains("Allow a removable or network drive"));
        check(&good, true, &probe).unwrap();
    }
    let optical = FakeProbe {
        kind: DriveKind::Unsuitable,
        ..FIXED
    };
    assert!(check(&good, true, &optical).is_err());
    let fat = FakeProbe {
        file_system: "FAT32",
        ..FIXED
    };
    assert!(check(&good, false, &fat).unwrap_err().contains("FAT32"));
    let full = FakeProbe {
        free: 100 * 1024 * 1024,
        ..FIXED
    };
    assert!(check(&good, false, &full).unwrap_err().contains("free"));
    assert!(check(Path::new("\\\\server\\share\\relay"), false, &FIXED).is_err());

    // Overlaps with storage that is in use or reserved.
    assert!(check(&w.defaults.local.join("moved"), false, &FIXED)
        .unwrap_err()
        .contains("overlaps"));
    assert!(check(&w.base.join("appdata"), false, &FIXED)
        .unwrap_err()
        .contains("overlaps"));
    let active_root = w.base.join("E").join("active");
    let moved = Bootstrap {
        active: Place::Folder {
            root: active_root.clone(),
            id: "a".into(),
            allow_removable: false,
        },
        ..Bootstrap::default()
    };
    let inside_active = validate_folder(
        &active_root.join("x"),
        false,
        0,
        &w.defaults,
        &moved,
        &FIXED,
        false,
    );
    assert!(inside_active.unwrap_err().contains("overlaps"));

    // Existing data is never merged.
    let full_dir = w.base.join("full");
    put(full_dir.join("photo.jpg"), b"x");
    assert!(check(&full_dir, false, &FIXED)
        .unwrap_err()
        .contains("not empty"));
    let other = w.base.join("other-relay");
    put(other.join(MARKER), b"{}");
    assert!(check(&other, false, &FIXED)
        .unwrap_err()
        .contains("already contains Relay data"));
    let profile = w.base.join("profile");
    fs::create_dir_all(profile.join("config")).unwrap();
    fs::create_dir_all(profile.join("encryption")).unwrap();
    assert!(check(&profile.join("nested"), false, &FIXED)
        .unwrap_err()
        .contains("inside another Relay profile"));
    let file = w.base.join("a-file");
    put(file.clone(), b"x");
    assert!(check(&file, false, &FIXED)
        .unwrap_err()
        .contains("is a file"));
    // An existing empty folder is fine and is kept.
    let empty = w.base.join("empty");
    fs::create_dir_all(&empty).unwrap();
    check(&empty, false, &FIXED).unwrap();
    assert!(empty.is_dir());
}

#[cfg(windows)]
#[test]
fn read_only_target_is_rejected_without_changes() {
    let w = world();
    let locked = w.base.join("locked");
    fs::create_dir_all(&locked).unwrap();
    let user = format!(
        "{}\\{}",
        std::env::var("USERDOMAIN").unwrap(),
        std::env::var("USERNAME").unwrap()
    );
    let icacls = |args: &[&str]| {
        let status = std::process::Command::new("icacls")
            .arg(&locked)
            .args(args)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
    };
    icacls(&[
        "/inheritance:r",
        "/grant:r",
        &format!("{user}:(OI)(CI)(RX)"),
    ]);
    let b = Bootstrap::default();
    let direct = validate_folder(&locked, false, 0, &w.defaults, &b, &FIXED, false);
    let nested = validate_folder(
        &locked.join("new"),
        false,
        0,
        &w.defaults,
        &b,
        &FIXED,
        false,
    );
    icacls(&["/grant", &format!("{user}:(OI)(CI)F")]);
    assert!(direct.unwrap_err().contains("cannot write"));
    assert!(nested.unwrap_err().contains("cannot write"));
    assert_eq!(fs::read_dir(&locked).unwrap().count(), 0);
}

#[test]
fn moving_default_data_copies_verifies_switches_and_keeps_the_old_copy() {
    let w = world();
    populate(&w.defaults);
    let root = move_to_folder(&w);
    assert_profile(&root);
    assert_defaults_intact(&w.defaults);
    let b = boot(&w.defaults);
    assert!(matches!(&b.active, Place::Folder { root: r, .. } if *r == root));
    assert_eq!(b.previous, Some(Place::Default));
    assert!(b.pending.is_none() && b.last_result.as_ref().unwrap().ok);
    assert_eq!(resolve(&w.defaults).unwrap(), Startup::Folder(root));
    // Nothing else may be scheduled while the old copy is kept.
    let other = new_folder_place(w.base.join("F").join("x"), false);
    assert!(schedule(&w.defaults, other)
        .unwrap_err()
        .contains("old copy"));
}

#[test]
fn moving_a_fresh_install_creates_only_an_identified_folder() {
    let w = world();
    let root = move_to_folder(&w);
    assert!(root.join(MARKER).is_file());
    assert_eq!(resolve(&w.defaults).unwrap(), Startup::Folder(root));
}

#[test]
fn interrupted_move_is_cleaned_up_and_completed_on_next_start() {
    let w = world();
    populate(&w.defaults);
    let root = w.base.join("D").join("Relay Data");
    schedule(&w.defaults, new_folder_place(root.clone(), false)).unwrap();
    let crash = Faults {
        crash_after: Some(3),
        ..Faults::default()
    };
    assert_eq!(
        run_pending(&w.defaults, &FIXED, &crash).unwrap(),
        MoveResult::Interrupted
    );
    // As after power loss: half a copy, still using the old data.
    assert!(root.join(IN_PROGRESS).is_file());
    assert!(!root.join(MARKER).exists());
    assert_eq!(boot(&w.defaults).active, Place::Default);
    assert_eq!(resolve(&w.defaults).unwrap(), Startup::DefaultFolders);
    assert_eq!(
        run_pending(&w.defaults, &FIXED, &Faults::default()).unwrap(),
        MoveResult::Moved
    );
    assert_profile(&root);
    assert_defaults_intact(&w.defaults);
}

#[test]
fn repeated_interruptions_stop_retrying_and_keep_the_old_data() {
    let w = world();
    populate(&w.defaults);
    let root = w.base.join("D").join("Relay Data");
    schedule(&w.defaults, new_folder_place(root.clone(), false)).unwrap();
    let crash = Faults {
        crash_after: Some(1),
        ..Faults::default()
    };
    for _ in 0..MAX_ATTEMPTS {
        assert_eq!(
            run_pending(&w.defaults, &FIXED, &crash).unwrap(),
            MoveResult::Interrupted
        );
    }
    assert!(matches!(
        run_pending(&w.defaults, &FIXED, &crash).unwrap(),
        MoveResult::Failed(_)
    ));
    assert!(!root.exists());
    let b = boot(&w.defaults);
    assert!(b.pending.is_none() && b.active == Place::Default && !b.last_result.unwrap().ok);
    assert_defaults_intact(&w.defaults);
}

#[test]
fn failed_or_mismatched_copy_rolls_back_without_switching() {
    for faults in [
        Faults {
            fail_after: Some(4),
            ..Faults::default()
        },
        Faults {
            corrupt: true,
            ..Faults::default()
        },
    ] {
        let w = world();
        populate(&w.defaults);
        let root = w.base.join("D").join("Relay Data");
        schedule(&w.defaults, new_folder_place(root.clone(), false)).unwrap();
        let MoveResult::Failed(message) = run_pending(&w.defaults, &FIXED, &faults).unwrap() else {
            panic!("the move must fail");
        };
        if faults.corrupt {
            assert!(message.contains("did not match"), "{message}");
        }
        assert!(!root.exists(), "the partial copy must be removed");
        let b = boot(&w.defaults);
        assert_eq!(b.active, Place::Default);
        assert!(b.pending.is_none() && b.previous.is_none());
        assert!(!b.last_result.unwrap().ok);
        assert_defaults_intact(&w.defaults);
        assert_eq!(resolve(&w.defaults).unwrap(), Startup::DefaultFolders);
    }
}

#[test]
fn destination_filled_after_scheduling_is_never_merged() {
    let w = world();
    populate(&w.defaults);
    let root = w.base.join("D").join("Relay Data");
    schedule(&w.defaults, new_folder_place(root.clone(), false)).unwrap();
    put(root.join("someone-elses.txt"), b"keep me");
    assert!(matches!(
        run_pending(&w.defaults, &FIXED, &Faults::default()).unwrap(),
        MoveResult::Failed(_)
    ));
    assert_eq!(
        fs::read(root.join("someone-elses.txt")).unwrap(),
        b"keep me"
    );
    assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
    assert_eq!(boot(&w.defaults).active, Place::Default);
}

#[test]
fn unavailable_data_folder_never_falls_back_or_creates_a_profile() {
    let w = world();
    let unused = ('D'..='Z')
        .rev()
        .find(|letter| !Path::new(&format!("{letter}:\\")).exists())
        .expect("a free drive letter");
    for root in [
        PathBuf::from(format!("{unused}:\\Relay Data")),
        w.base.join("gone").join("Relay Data"),
    ] {
        let b = Bootstrap {
            active: Place::Folder {
                root: root.clone(),
                id: "x".into(),
                allow_removable: true,
            },
            ..Bootstrap::default()
        };
        save(&w.defaults.bootstrap(), &b).unwrap();
        let error = resolve(&w.defaults).unwrap_err();
        assert!(
            error.contains("not available") || error.contains("missing"),
            "{error}"
        );
        assert!(error.contains("empty profile"));
        assert!(!root.exists());
    }
    // A folder that exists but holds other data is refused as well.
    let foreign = w.base.join("foreign");
    put(foreign.join(MARKER), br#"{"version":1,"id":"other"}"#);
    let b = Bootstrap {
        active: Place::Folder {
            root: foreign,
            id: "mine".into(),
            allow_removable: false,
        },
        ..Bootstrap::default()
    };
    save(&w.defaults.bootstrap(), &b).unwrap();
    assert!(resolve(&w.defaults).is_err());
    // A damaged record is an error, not "use the default folders".
    fs::write(w.defaults.bootstrap(), b"{not json").unwrap();
    assert!(resolve(&w.defaults).unwrap_err().contains("damaged"));
}

#[test]
fn moving_back_copies_current_data_over_the_stale_copy() {
    let w = world();
    populate(&w.defaults);
    let root = move_to_folder(&w);
    // Relay ran from the new folder and changed data there.
    put(root.join("config").join("preferences.json"), b"newer prefs");
    put(
        root.join("server-data").join("relay.sqlite3"),
        b"newer sqlite",
    );
    schedule(&w.defaults, Place::Default).unwrap();
    assert_eq!(
        run_pending(&w.defaults, &FIXED, &Faults::default()).unwrap(),
        MoveResult::Moved
    );
    let d = &w.defaults;
    assert_eq!(
        fs::read(d.config.join("preferences.json")).unwrap(),
        b"newer prefs"
    );
    assert_eq!(
        fs::read(d.server.join("relay.sqlite3")).unwrap(),
        b"newer sqlite"
    );
    assert_eq!(
        fs::read(d.config.join("host-moves").join("route.json")).unwrap(),
        b"moves"
    );
    assert_eq!(
        fs::read(d.local.join("encryption").join("a.device")).unwrap(),
        b"device-checkpoint"
    );
    for slot in [&d.config, &d.local, &d.server, &d.replicas] {
        assert!(!slot.join(IN_PROGRESS).exists());
        let parent = slot.parent().unwrap();
        let stale = fs::read_dir(parent).unwrap().any(|e| {
            e.unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".relay-old-")
        });
        assert!(!stale, "stale copy left in {}", parent.display());
    }
    let b = boot(d);
    assert_eq!(b.active, Place::Default);
    assert!(matches!(&b.previous, Some(Place::Folder { root: r, .. }) if *r == root));
    assert_eq!(
        fs::read(root.join("config").join("preferences.json")).unwrap(),
        b"newer prefs"
    );
    assert_eq!(resolve(d).unwrap(), Startup::DefaultFolders);
}

#[test]
fn failed_move_back_restores_the_old_copy() {
    let w = world();
    populate(&w.defaults);
    let root = move_to_folder(&w);
    put(root.join("config").join("preferences.json"), b"newer prefs");
    schedule(&w.defaults, Place::Default).unwrap();
    let fail = Faults {
        fail_after: Some(2),
        ..Faults::default()
    };
    assert!(matches!(
        run_pending(&w.defaults, &FIXED, &fail).unwrap(),
        MoveResult::Failed(_)
    ));
    assert_defaults_intact(&w.defaults);
    let b = boot(&w.defaults);
    assert!(matches!(&b.active, Place::Folder { root: r, .. } if *r == root));
    assert_eq!(b.previous, Some(Place::Default));
    assert_eq!(
        fs::read(root.join("config").join("preferences.json")).unwrap(),
        b"newer prefs"
    );
}

#[test]
fn deleting_the_old_copy_touches_only_the_old_copy() {
    let w = world();
    populate(&w.defaults);
    let root = move_to_folder(&w);
    delete_previous(&w.defaults).unwrap();
    for slot in [
        &w.defaults.config,
        &w.defaults.local,
        &w.defaults.server,
        &w.defaults.replicas,
    ] {
        assert!(!slot.exists());
    }
    assert!(w.defaults.bootstrap().is_file());
    assert_profile(&root);
    assert!(boot(&w.defaults).previous.is_none());
    // A second move to another folder is allowed now; forgetting keeps files.
    let next = w.base.join("E").join("Relay");
    schedule(&w.defaults, new_folder_place(next.clone(), false)).unwrap();
    assert_eq!(
        run_pending(&w.defaults, &FIXED, &Faults::default()).unwrap(),
        MoveResult::Moved
    );
    assert_profile(&next);
    forget_previous(&w.defaults).unwrap();
    assert_profile(&root);
}

#[cfg(windows)]
#[test]
fn a_file_in_use_stops_the_move_before_switching() {
    let w = world();
    populate(&w.defaults);
    let database = w.defaults.server.join("relay.sqlite3");
    let _open = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&database)
        .unwrap();
    let root = w.base.join("D").join("Relay Data");
    schedule(&w.defaults, new_folder_place(root.clone(), false)).unwrap();
    let MoveResult::Failed(message) = run_pending(&w.defaults, &FIXED, &Faults::default()).unwrap()
    else {
        panic!("a file in use must stop the move");
    };
    assert!(message.contains("in use"), "{message}");
    assert!(!root.exists());
    assert_eq!(boot(&w.defaults).active, Place::Default);
}

#[test]
fn moved_profile_keeps_saved_login_names_but_isolated_profiles_do_not() {
    let root = OsString::from("D:\\Relay Data");
    assert_eq!(
        credential_scope(Some(root.clone()), Some(root.clone())),
        None
    );
    assert_eq!(
        credential_scope(Some(root.clone()), None),
        Some(root.clone())
    );
    let other = OsString::from("C:\\qa-profile");
    assert_eq!(
        credential_scope(Some(other.clone()), Some(root)),
        Some(other)
    );
    assert_eq!(credential_scope(None, None), None);
}

#[test]
fn path_overlap_is_case_insensitive_and_component_based() {
    assert!(within(
        Path::new("C:\\Users\\A\\Data"),
        Path::new("c:\\users\\a")
    ));
    assert!(within(
        Path::new("C:\\Users\\A"),
        Path::new("C:\\Users\\A\\")
    ));
    assert!(!within(
        Path::new("C:\\Users\\AB"),
        Path::new("C:\\Users\\A")
    ));
    assert_eq!(
        drive_root(Path::new("D:\\x\\y")),
        Some(PathBuf::from("D:\\"))
    );
}
