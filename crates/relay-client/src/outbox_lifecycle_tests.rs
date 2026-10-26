use super::*;
use crate::network::SendBinding;

fn saved_send() -> durable_outbox::Entry {
    let origin = CommandOrigin {
        url: "https://relay.example".into(),
        server_id: "server-one".into(),
        user_id: "own-user".into(),
    };
    let target = Target::Channel("general".into());
    let message = Message {
        id: "local:uncertain-send".into(),
        client_id: Some("uncertain-send".into()),
        channel_id: Some("general".into()),
        author: User {
            id: origin.user_id.clone(),
            ..Default::default()
        },
        content: "private uncertain outgoing message".into(),
        attachments: vec![Attachment {
            id: "uploaded-file".into(),
            name: "private-attachment.txt".into(),
            mime: "text/plain".into(),
            size: 12,
            url: "/files/uploaded-file".into(),
        }],
        reply_to_id: Some("original-message".into()),
        mentions: vec!["reply-author".into(), "renamed-member".into()],
        created_at: "2026-10-26T12:00:00Z".into(),
        ..Default::default()
    };
    durable_outbox::Entry::from_message(
        ConversationMemory::scope(&origin.url, &origin.user_id),
        origin,
        target,
        SendBinding {
            protection: CommunityProtection::default(),
            plain: false,
            device_id: "original-device".into(),
            fingerprint: "original-fingerprint".into(),
        },
        &message,
    )
    .unwrap()
}

#[cfg(windows)]
#[test]
fn acknowledged_protected_snapshot_restores_outgoing_request_beside_newer_draft() {
    let folder = std::env::var_os("RELAY_TEST_WORK_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("relay-tests"))
        .join(format!("outbox-lifecycle-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&folder).unwrap();
    let path = folder.join("preferences.json");
    let saved = saved_send();
    let mut prefs = Preferences::default();
    prefs.outbox.insert(saved.clone()).unwrap();
    // Selecting a conversation registers its scope before draft snapshots.
    prefs.conversations.remember(&saved.scope, &saved.target);
    let newer = Draft {
        content: "private newer draft must survive".into(),
        reply: Some(ReplyDraft {
            message_id: "new-reply".into(),
            ..Default::default()
        }),
        ..Default::default()
    };
    prefs
        .conversations
        .save_drafts(&saved.scope, vec![(saved.target.clone(), newer.clone())]);
    let writer =
        preference_writer::PreferenceWriter::new(path.clone(), egui::Context::default(), true);
    let receipt = writer
        .save_confirmed(preferences_snapshot(&prefs, &AudioSettings::default()).unwrap())
        .unwrap();
    assert_eq!(
        receipt.recv_timeout(Duration::from_secs(10)).unwrap(),
        Ok(())
    );
    let restarted: Preferences = preference_writer::load(&path).unwrap();
    let wire = std::fs::read(&path).unwrap();
    assert!(wire.starts_with(b"RLYPREF1"));
    assert!(!wire
        .windows(b"private".len())
        .any(|part| part == b"private"));
    let restored = restarted
        .outbox
        .scoped(&saved.scope, &saved.origin)
        .next()
        .unwrap();
    let message = restored.message(&User {
        id: saved.origin.user_id.clone(),
        ..Default::default()
    });
    assert_eq!(message.id, "local:uncertain-send");
    assert_eq!(message.client_id.as_deref(), Some("uncertain-send"));
    assert_eq!(message.mentions, saved.mentions);
    assert_eq!(message.reply_to_id, saved.reply_to_id);
    assert_eq!(message.attachments[0].id, "uploaded-file");
    assert_eq!(restored.binding, saved.binding);
    let draft = &restarted.conversations.drafts(&saved.scope)[0].1;
    assert_eq!(draft.content, newer.content);
    assert_eq!(draft.reply, newer.reply);
    // A restored local row is not an acknowledgement and cannot consume the
    // saved retry request, even when its client ID matches exactly.
    let mut outbox = restarted.outbox;
    assert!(!outbox.acknowledge(&saved.scope, &saved.origin, &saved.target, &message));
    drop(writer);
    std::fs::remove_file(path).unwrap();
    std::fs::remove_dir(folder).unwrap();
}

#[test]
fn disabled_restart_persistence_keeps_scoped_outgoing_requests_only_in_memory() {
    let first = saved_send();
    let mut second = first.clone();
    second.scope = ConversationMemory::scope("https://other.example", &second.origin.user_id);
    second.origin.url = "https://other.example".into();
    second.origin.server_id = "server-two".into();
    second.content = "other community private outgoing message".into();
    let mut prefs = Preferences {
        remember_drafts: false,
        ..Default::default()
    };
    prefs.outbox.insert(first.clone()).unwrap();
    prefs.outbox.insert(second.clone()).unwrap();
    let snapshot = preferences_snapshot(&prefs, &AudioSettings::default()).unwrap();
    assert!(!String::from_utf8_lossy(&snapshot).contains("private outgoing message"));
    assert!(!String::from_utf8_lossy(&snapshot).contains("private uncertain outgoing message"));
    let restarted: Preferences = serde_json::from_slice(&snapshot).unwrap();
    assert!(restarted.outbox.is_empty());
    assert_eq!(prefs.outbox.scoped(&first.scope, &first.origin).count(), 1);
    assert_eq!(
        prefs.outbox.scoped(&second.scope, &second.origin).count(),
        1
    );
    assert_eq!(prefs.outbox.scoped(&first.scope, &second.origin).count(), 0);
    // Opting back in saves the existing in-memory requests with their original
    // identities; it does not create new messages or rebind their protection.
    prefs.remember_drafts = true;
    let restarted: Preferences =
        serde_json::from_slice(&preferences_snapshot(&prefs, &AudioSettings::default()).unwrap())
            .unwrap();
    assert_eq!(restarted.outbox.iter().count(), 2);
    assert_eq!(
        restarted
            .outbox
            .scoped(&first.scope, &first.origin)
            .next()
            .unwrap()
            .binding,
        first.binding
    );
}

#[test]
fn incomplete_saved_binding_is_rejected_instead_of_becoming_a_new_send() {
    let saved = saved_send();
    let mut prefs = Preferences::default();
    prefs.outbox.insert(saved).unwrap();
    let mut malformed = serde_json::to_value(&prefs).unwrap();
    malformed["outbox"]["entries"][0]
        .as_object_mut()
        .unwrap()
        .remove("binding");
    assert!(serde_json::from_value::<Preferences>(malformed).is_err());
    // Preferences predating the feature still load with an empty outbox.
    let mut older = serde_json::to_value(&prefs).unwrap();
    older.as_object_mut().unwrap().remove("outbox");
    assert!(serde_json::from_value::<Preferences>(older)
        .unwrap()
        .outbox
        .is_empty());
}
