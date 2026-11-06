use super::*;
use crate::{
    e2ee::{CachedEnvelope, Envelope, MessageProtection},
    history_transfer::Snapshot,
};
use sha2::Digest;
#[test]
fn history_transfer_page_is_read_only_and_rejects_delivery_substitution() {
    let mut test = TestWorker::new();
    test.connection("http://127.0.0.1".into());
    let server = uuid::Uuid::new_v4().to_string();
    let root = PathBuf::from(std::env::var_os("RELAY_TEST_WORK_ROOT").unwrap())
        .join(format!("transfer-page-{server}"));
    let c = test.worker.connection.as_mut().unwrap();
    c.server_id = server.clone();
    let mut actor =
        crate::e2ee::CryptoSession::open_for_test(root, &server, &c.session.user.id).unwrap();
    let target = Target::Channel("general".into());
    let scope = format!("{server}/channel/general");
    let wire = "relay-e2ee:v1:synthetic-owned-wire";
    let hash = format!("{:x}", sha2::Sha256::digest(wire.as_bytes()));
    let row = CachedEnvelope {
        author: relay_crypto::Identity {
            server_id: server.clone(),
            user_id: "peer".into(),
            device_id: "old-peer".into(),
        },
        envelope: Envelope {
            version: 1,
            scope: scope.clone(),
            client_id: "send-1".into(),
            content: "Recovered private text".into(),
            reply_to_id: None,
            mentions: vec![],
            attachments: vec![],
            action: None,
        },
        message_id: Some("message-1".into()),
        sequence: 17,
        delivery_revision: 0,
    };
    let origin = CommandOrigin::from(&*c);
    actor
        .import_transferred_history(
            Snapshot {
                version: 1,
                server_id: server,
                user_id: c.session.user.id.clone(),
                source_device: "old-owner".into(),
                entries: HashMap::from([(hash, row)]),
            },
            &origin,
        )
        .unwrap();
    let revision = actor.checkpoint_revision_for_test();
    c.crypto = Some(actor);
    let message = Message {
        id: "message-1".into(),
        author: User {
            id: "peer".into(),
            ..Default::default()
        },
        channel_id: Some("general".into()),
        client_id: Some("send-1".into()),
        sequence: 17,
        content: wire.into(),
        ..Default::default()
    };
    let mut rows = vec![message.clone()];
    let protections = encrypted::decrypt_in_connection(c, &target, &mut rows);
    assert_eq!(rows[0].content, "Recovered private text");
    assert!(matches!(protections[0].1, MessageProtection::Recovered));
    assert_eq!(
        c.crypto.as_ref().unwrap().checkpoint_revision_for_test(),
        revision
    );
    for variant in 0..5 {
        let mut changed = message.clone();
        match variant {
            0 => changed.id = "different".into(),
            1 => changed.sequence += 1,
            2 => changed.author.id = "different".into(),
            3 => changed.client_id = Some("different".into()),
            _ => changed.channel_id = Some("different".into()),
        }
        let mut rows = vec![changed];
        let protections = encrypted::decrypt_in_connection(c, &target, &mut rows);
        assert!(matches!(
            protections[0].1,
            MessageProtection::Unavailable(_)
        ));
        assert_eq!(rows[0].content, "Encrypted message unavailable");
        assert_eq!(
            c.crypto.as_ref().unwrap().checkpoint_revision_for_test(),
            revision
        );
    }
    assert_eq!(
        c.crypto.as_ref().unwrap().cached_envelopes(&scope).count(),
        0
    );
    c.crypto.as_ref().unwrap().remove_test_checkpoint();
}

// "Update this server before transferring history between PCs": the client checks the
// server's history-transfer-v1 capability before it sends or requests anything.
#[test]
fn history_transfer_refuses_a_server_without_the_transfer_capability() {
    let mut test = TestWorker::new();
    test.connection("http://127.0.0.1".into());
    let c = test.worker.connection.as_ref().unwrap();
    assert!(!c.chat.supports(crate::history_transfer::CAPABILITY));
    let origin = CommandOrigin::from(c);
    let code = crate::history_transfer::Code::generate().unwrap().display();
    for send in [true, false] {
        let error = test
            .worker
            .start_history_transfer(
                origin.clone(),
                1,
                zeroize::Zeroizing::new(code.to_string()),
                send,
                std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            )
            .unwrap_err();
        assert_eq!(error, "Update this server before transferring history between PCs.");
    }
    assert!(test.worker.history_transfer_job.is_none());
}
