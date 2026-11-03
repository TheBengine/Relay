use super::*;
#[test]
fn history_transfer_survives_restart_without_importing_live_ratchets_or_counters() {
    let mut pair = Pair::new();
    let scope = pair.scope.clone();
    let envelope = pair.envelope("transfer-one", "owned prior message");
    let wire = pair
        .alice
        .as_mut()
        .unwrap()
        .encrypted_content(envelope)
        .unwrap();
    let old = pair.bob.as_mut().unwrap();
    old.decrypt_content(&scope, &wire).unwrap();
    old.bind_delivery(&wire, "message-1", 17).unwrap();
    let origin = crate::network::CommandOrigin {
        url: "http://127.0.0.1".into(),
        server_id: pair.server.clone(),
        user_id: "bob".into(),
    };
    let snapshot = old.transfer_snapshot(&origin).unwrap();
    let code = crate::history_transfer::Code::generate().unwrap();
    let sealed = crate::history_transfer::seal(&snapshot, &code, &origin).unwrap();
    let old_identity = old.device.identity().clone();
    drop(pair.bob.take());
    let mut new = CryptoSession::reset_at(pair.root.clone(), &pair.server, "bob").unwrap();
    assert_ne!(new.device.identity(), &old_identity);
    let device = new.device.checkpoint().unwrap().to_vec();
    let cursors = new.state.cursors.clone();
    let clock = new.state.delivery_clock;
    assert!(new.decrypt_content(&scope, &wire).is_err());
    assert_eq!(
        new.import_transferred_history(
            crate::history_transfer::open(&sealed, &code, &origin).unwrap(),
            &origin
        )
        .unwrap(),
        1
    );
    assert_eq!(
        new.device.checkpoint().unwrap().as_slice(),
        device.as_slice()
    );
    assert_eq!(new.state.delivery_clock, clock);
    assert_eq!(new.state.cursors, cursors);
    assert!(new.state.outbox.is_empty());
    assert_eq!(new.cached_envelopes(&scope).count(), 0);
    assert_eq!(
        new.recovered_content(&scope, &wire)
            .unwrap()
            .unwrap()
            .envelope
            .content,
        "owned prior message"
    );
    assert!(new.recovered_content("other", &wire).is_err());
    assert!(new
        .recovered_content(&scope, &format!("{wire}x"))
        .unwrap()
        .is_none());
    let revision = new.state.revision;
    assert_eq!(
        new.import_transferred_history(
            crate::history_transfer::open(&sealed, &code, &origin).unwrap(),
            &origin
        )
        .unwrap(),
        0
    );
    assert_eq!(new.state.revision, revision);
    let mut conflict = crate::history_transfer::open(&sealed, &code, &origin).unwrap();
    conflict
        .entries
        .values_mut()
        .next()
        .unwrap()
        .envelope
        .content = "substituted".into();
    assert!(new.import_transferred_history(conflict, &origin).is_err());
    assert_eq!(new.state.revision, revision);
    drop(new);
    pair.bob = Some(CryptoSession::open_for_test(pair.root.clone(), &pair.server, "bob").unwrap());
    let reopened = pair.bob.as_mut().unwrap();
    assert!(reopened.device.group_state(&scope).is_none());
    assert!(reopened.decrypt_content(&scope, &wire).is_err());
    assert_eq!(
        reopened
            .recovered_content(&scope, &wire)
            .unwrap()
            .unwrap()
            .message_id
            .as_deref(),
        Some("message-1")
    );
    assert!(reopened.state.outbox.is_empty());
}

fn transferred_fixture(
    pair: &mut Pair,
) -> (
    crate::network::CommandOrigin,
    Vec<u8>,
    crate::history_transfer::Code,
) {
    let envelope = pair.envelope("staging-one", "owned staging text");
    let wire = pair
        .alice
        .as_mut()
        .unwrap()
        .encrypted_content(envelope)
        .unwrap();
    let scope = pair.scope.clone();
    let old = pair.bob.as_mut().unwrap();
    old.decrypt_content(&scope, &wire).unwrap();
    old.bind_delivery(&wire, "message-one", 1).unwrap();
    let origin = crate::network::CommandOrigin {
        url: "http://127.0.0.1".into(),
        server_id: pair.server.clone(),
        user_id: "bob".into(),
    };
    let mut snapshot = old.transfer_snapshot(&origin).unwrap();
    let template = snapshot.entries.values().next().unwrap().clone();
    // Synthetic additional records exercise multi-segment staging, independently of MLS.
    for n in 2..=300 {
        let mut row = template.clone();
        row.message_id = Some(format!("message-{n}"));
        row.sequence = n;
        row.envelope.client_id = format!("send-{n}");
        snapshot.entries.insert(format!("{n:064x}"), row);
    }
    let code = crate::history_transfer::Code::generate().unwrap();
    let wire = crate::history_transfer::seal(&snapshot, &code, &origin).unwrap();
    (origin, wire, code)
}
fn staged_files(actor: &CryptoSession) -> Vec<PathBuf> {
    let directory = actor
        .path
        .with_extension(format!("{}.history", actor.device.identity().device_id));
    if !directory.exists() {
        return vec![];
    }
    std::fs::read_dir(directory)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect()
}
#[test]
fn history_transfer_cancelled_or_failed_checkpoint_cleans_staged_files() {
    let mut pair = Pair::new();
    let (origin, wire, code) = transferred_fixture(&mut pair);
    drop(pair.bob.take());
    let mut actor = CryptoSession::reset_at(pair.root.clone(), &pair.server, "bob").unwrap();
    let source = actor.transfer_source(&origin).unwrap();
    let cancel = std::sync::atomic::AtomicBool::new(false);
    let prepared = source
        .stage(
            crate::history_transfer::open(&wire, &code, &origin).unwrap(),
            &origin,
            &cancel,
        )
        .unwrap();
    assert_eq!(staged_files(&actor).len(), 2);
    let revision = actor.state.revision;
    cancel.store(true, std::sync::atomic::Ordering::Release);
    assert!(actor.commit_transferred_history(prepared, &cancel).is_err());
    assert!(staged_files(&actor).is_empty());
    assert_eq!(actor.state.revision, revision);
    assert!(actor.state.recovered_history.is_empty());
    cancel.store(false, std::sync::atomic::Ordering::Release);
    let prepared = source
        .stage(
            crate::history_transfer::open(&wire, &code, &origin).unwrap(),
            &origin,
            &cancel,
        )
        .unwrap();
    use std::os::windows::fs::OpenOptionsExt;
    // Deny replacement while permitting the cleanup's authenticated readback.
    let held = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(3)
        .open(&actor.path)
        .unwrap();
    assert!(actor.commit_transferred_history(prepared, &cancel).is_err());
    drop(held);
    assert!(staged_files(&actor).is_empty());
    assert!(actor.state.recovered_history.is_empty());
    pair.bob = Some(actor);
}
#[test]
fn history_transfer_prepared_for_an_old_device_cannot_modify_a_reset_device() {
    let mut pair = Pair::new();
    let (origin, wire, code) = transferred_fixture(&mut pair);
    drop(pair.bob.take());
    let actor = CryptoSession::reset_at(pair.root.clone(), &pair.server, "bob").unwrap();
    let source = actor.transfer_source(&origin).unwrap();
    let cancel = std::sync::atomic::AtomicBool::new(false);
    let prepared = source
        .stage(
            crate::history_transfer::open(&wire, &code, &origin).unwrap(),
            &origin,
            &cancel,
        )
        .unwrap();
    let paths = staged_files(&actor);
    assert_eq!(paths.len(), 2);
    drop(actor);
    let mut replacement = CryptoSession::reset_at(pair.root.clone(), &pair.server, "bob").unwrap();
    let before = std::fs::read(&replacement.path).unwrap();
    assert!(replacement
        .commit_transferred_history(prepared, &cancel)
        .is_err());
    assert_eq!(std::fs::read(&replacement.path).unwrap(), before);
    assert!(paths.iter().all(|p| !p.exists()));
    assert!(replacement.state.recovered_history.is_empty());
    pair.bob = Some(replacement);
}

#[test]
fn history_transfer_marker_failure_preserves_installed_segments_for_restart() {
    let mut pair = Pair::new();
    let (origin, wire, code) = transferred_fixture(&mut pair);
    drop(pair.bob.take());
    let mut actor = CryptoSession::reset_at(pair.root.clone(), &pair.server, "bob").unwrap();
    let cancel = std::sync::atomic::AtomicBool::new(false);
    let prepared = actor
        .transfer_source(&origin)
        .unwrap()
        .stage(
            crate::history_transfer::open(&wire, &code, &origin).unwrap(),
            &origin,
            &cancel,
        )
        .unwrap();
    protected::fail_next_head_write();
    assert!(actor.commit_transferred_history(prepared, &cancel).is_err());
    assert_eq!(staged_files(&actor).len(), 2);
    drop(actor);
    let reopened = CryptoSession::open_for_test(pair.root.clone(), &pair.server, "bob").unwrap();
    assert_eq!(reopened.state.recovered_history.len(), 2);
    assert_eq!(
        history::recovered_records(&reopened.state, &reopened.path, reopened.binding()).count(),
        300
    );
    assert!(reopened.state.outbox.is_empty());
    pair.bob = Some(reopened);
}
