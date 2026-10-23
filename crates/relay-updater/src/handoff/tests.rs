use super::*;
use std::{fs, os::windows::fs::OpenOptionsExt, sync::Arc};

#[test]
fn signed_installer_and_parent_names_cannot_be_changed_while_pinned() {
    let (root, ready) = crate::tests::handoff_fixture();
    let signals = Signals::new().unwrap();
    let pinned = pin(&ready, &signals).unwrap();
    assert!(OpenOptions::new().write(true).open(&ready.path).is_err());
    assert!(fs::remove_file(&ready.path).is_err());
    let parent = ready.path.parent().unwrap();
    assert!(fs::rename(parent, root.path().join("renamed-downloads")).is_err());
    assert!(fs::read(&ready.path).unwrap().starts_with(b"MZ"));
    drop(pinned);
    assert!(OpenOptions::new().write(true).open(&ready.path).is_ok());
}

#[test]
fn tamper_expiry_unsupported_protocol_and_cancellation_refuse_before_launch() {
    let (_root, mut ready) = crate::tests::handoff_fixture();
    let signals = Signals::new().unwrap();
    ready.valid_until = chrono::Utc::now().timestamp() + 89;
    assert!(pin(&ready, &signals).is_err());
    ready.valid_until = chrono::Utc::now().timestamp() + 3600;
    ready.release.handoff = None;
    assert!(pin(&ready, &signals).is_err());
    ready.release.handoff = Some("wait-parent-v1".into());
    signals.cancel();
    assert!(pin(&ready, &signals).is_err());
    let signals = Signals::new().unwrap();
    fs::write(&ready.path, b"MZ-changed").unwrap();
    assert!(prepare(&ready, Arc::new(signals)).is_err());
}

#[test]
fn existing_writer_and_hardlink_are_refused_and_never_removed() {
    let (root, ready) = crate::tests::handoff_fixture();
    let signals = Signals::new().unwrap();
    let writer = OpenOptions::new()
        .write(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(&ready.path)
        .unwrap();
    assert!(pin(&ready, &signals).is_err());
    drop(writer);
    let link = root.path().join("keep-link.exe");
    fs::hard_link(&ready.path, &link).unwrap();
    assert!(pin(&ready, &signals).is_err());
    assert_eq!(fs::read(&ready.path).unwrap(), fs::read(link).unwrap());
}

#[test]
fn exact_current_parent_identity_matches_windows_creation_time() {
    let identity = parent_identity().unwrap();
    let values: Vec<u32> = identity
        .split('-')
        .map(|value| value.parse().unwrap())
        .collect();
    assert_eq!(values.len(), 3);
    assert_eq!(values[0], std::process::id());
    assert!(values[1] != 0 || values[2] != 0);
    assert_eq!(identity, parent_identity().unwrap());
    let signals = Signals::new().unwrap();
    assert!(!signals.cancelled());
    signals.cancel();
    assert!(signals.cancelled());
    assert!(signals.await_ready().is_err());
}

#[test]
#[ignore = "Requires the explicitly compiled owned installer; no personal installation or visible wizard"]
fn actual_signed_inno_handoff_becomes_ready_and_cancels_normally() {
    let build = std::path::PathBuf::from(
        std::env::var_os("RELAY_HANDOFF_FIXTURE_BUILD")
            .expect("Explicit owned build record required"),
    );
    let build: serde_json::Value =
        serde_json::from_slice(&storage::read(&build, 65536).unwrap()).unwrap();
    assert_eq!(build["app_id"], "6d0c7f57-26f9-4a46-a68d-4d3daa4c9709");
    let path = std::path::PathBuf::from(build["installer"].as_str().unwrap());
    let bytes = storage::read(&path, MAX_INSTALLER).unwrap();
    assert_eq!(
        crate::digest(&bytes),
        build["installer_sha256"].as_str().unwrap()
    );
    let (root, ready) = crate::tests::handoff_fixture_bytes(Some(bytes));
    let signals = Arc::new(Signals::new().unwrap());
    let target = root.path().join("must-not-install");
    let log = root.path().join("cancelled-inno.log");
    let extra = [
        "/VERYSILENT".into(),
        "/SUPPRESSMSGBOXES".into(),
        "/SP-".into(),
        "/NORESTART".into(),
        "/NOICONS".into(),
        format!("/DIR={}", target.display()).into(),
        format!("/LOG={}", log.display()).into(),
    ];
    let mut waiting = prepare_inner(&ready, signals.clone(), &extra).unwrap();
    assert!(waiting._child.try_wait().unwrap().is_none());
    assert!(!target.exists());
    assert!(OpenOptions::new().write(true).open(ready.path()).is_err());
    assert!(fs::rename(ready.path().parent().unwrap(), root.path().join("renamed")).is_err());
    signals.cancel();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let status = loop {
        if let Some(status) = waiting._child.try_wait().unwrap() {
            break status;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "Cancellation was not observed; owned process must not be replayed"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    assert_eq!(status.code(), Some(1));
    assert!(!target.exists());
    let text = String::from_utf8_lossy(&storage::read(&log, 65536).unwrap()).into_owned();
    assert!(text.contains("Relay update handoff was cancelled or was not approved."));
    drop(waiting);
}

#[test]
#[ignore = "Explicit owned compiled installer required; actual consent timeout and stale-result refusal"]
fn actual_signed_inno_timeout_cannot_close_relay_from_a_late_result() {
    let build = std::path::PathBuf::from(
        std::env::var_os("RELAY_HANDOFF_FIXTURE_BUILD")
            .expect("Explicit owned build record required"),
    );
    let build: serde_json::Value =
        serde_json::from_slice(&storage::read(&build, 65536).unwrap()).unwrap();
    assert_eq!(build["app_id"], "6d0c7f57-26f9-4a46-a68d-4d3daa4c9709");
    let path = std::path::PathBuf::from(build["installer"].as_str().unwrap());
    let bytes = storage::read(&path, MAX_INSTALLER).unwrap();
    assert_eq!(
        crate::digest(&bytes),
        build["installer_sha256"].as_str().unwrap()
    );
    let (root, ready) = crate::tests::handoff_fixture_bytes(Some(bytes));
    let signals = Arc::new(Signals::new().unwrap());
    let target = root.path().join("must-not-install");
    let extra = [
        "/VERYSILENT".into(),
        "/SUPPRESSMSGBOXES".into(),
        "/SP-".into(),
        "/NORESTART".into(),
        "/NOICONS".into(),
        format!("/DIR={}", target.display()).into(),
    ];
    let mut waiting = prepare_inner(&ready, signals.clone(), &extra).unwrap();
    assert_eq!(waiting._child.wait().unwrap().code(), Some(1));
    assert!(!signals.cancelled());
    assert!(!target.exists());
    assert!(waiting
        .commit()
        .unwrap_err()
        .contains("closed before Relay"));
    assert!(signals.cancelled());
}
