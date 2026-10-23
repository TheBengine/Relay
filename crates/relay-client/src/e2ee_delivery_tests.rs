//! Delivery budgets for saved public operations; no private key exports.
use super::*;
use relay_protocol::{AppendCryptoEvents, CryptoEventBody, CryptoTarget};

#[test]
fn consumption_confirmation_matches_complete_saved_receipt_and_survives_reopen() {
    let root = std::env::var_os("RELAY_TEST_WORK_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("relay-tests"))
        .join(format!("exact-consumption-{}", uuid::Uuid::new_v4()));
    let server = uuid::Uuid::new_v4().to_string();
    let mut actor = CryptoSession::open_for_test(root.clone(), &server, "owned-user").unwrap();
    let first = relay_protocol::ConsumeCryptoKeyPackage {
        target: CryptoTarget {
            channel_id: Some("owned-channel-a".into()),
            peer_id: None,
        },
        device_id: actor.device.identity().device_id.clone(),
        package_hash: "ab".repeat(32),
        welcome_sequence: 3,
    };
    // Synthetic outbox variants isolate every binding field. The opt-in HTTP
    // test separately creates and joins 32 real MLS groups with sequence 3.
    let mut variants = vec![first.clone(); 6];
    variants[1].target.channel_id = Some("owned-channel-b".into());
    variants[2].target = CryptoTarget {
        channel_id: None,
        peer_id: Some("owned-peer".into()),
    };
    variants[3].device_id = "other-device".into();
    variants[4].package_hash = "cd".repeat(32);
    variants[5].welcome_sequence = 4;
    actor
        .mutate(|_, state| {
            state.consumption_outbox = variants.clone();
            Ok(())
        })
        .unwrap();
    actor.committed_consumption(&first).unwrap();
    assert_eq!(
        serde_json::to_value(actor.pending_consumptions()).unwrap(),
        serde_json::to_value(&variants[1..]).unwrap()
    );
    drop(actor);
    let mut actor = CryptoSession::open_for_test(root, &server, "owned-user").unwrap();
    assert_eq!(
        serde_json::to_value(actor.pending_consumptions()).unwrap(),
        serde_json::to_value(&variants[1..]).unwrap()
    );
    actor.committed_consumption(&first).unwrap();
    assert_eq!(
        actor.pending_consumptions().len(),
        5,
        "old duplicate ACK cannot retire a newer package or another scope"
    );
    for expected in variants.iter().skip(1) {
        actor.committed_consumption(expected).unwrap();
    }
    assert!(actor.pending_consumptions().is_empty());
    protected::remove_head(&format!("Relay/crypto/v1/{server}/owned-user"));
}

fn batch(sizes: &[usize]) -> AppendCryptoEvents {
    AppendCryptoEvents {
        target: CryptoTarget {
            channel_id: Some("owned-general".into()),
            peer_id: None,
        },
        device_id: "owned-device".into(),
        group_id: "owned-group".into(),
        operation_id: "owned-operation".into(),
        expected_sequence: 0,
        epoch: 1,
        events: sizes
            .iter()
            .map(|size| CryptoEventBody {
                kind: "commit".into(),
                recipient_device_id: None,
                body: STANDARD.encode(vec![7; *size]),
            })
            .collect(),
    }
}

#[test]
fn saved_control_delivery_enforces_http_and_decoded_budgets() {
    let event_limit = relay_protocol::CRYPTO_MAX_EVENT_DECODED_BYTES;
    assert!(check_control_delivery(&batch(&[event_limit])).is_ok());
    assert!(check_control_delivery(&batch(&[event_limit + 1])).is_err());
    assert!(check_control_delivery(&batch(&[event_limit, event_limit])).is_ok());
    assert!(check_control_delivery(&batch(&[event_limit, event_limit, 1])).is_err());
    assert!(check_control_delivery(&batch(&[1; 66])).is_ok());
    assert!(check_control_delivery(&batch(&[1; 67])).is_err());
    let mut oversized_json = batch(&[1]);
    oversized_json.operation_id = "x".repeat(relay_protocol::CRYPTO_MAX_OPERATION_JSON_BYTES);
    assert!(check_control_delivery(&oversized_json).is_err());
    let mut invalid = batch(&[1]);
    invalid.events[0].body = "invalid%%".into();
    assert!(check_control_delivery(&invalid).is_err());
}

#[test]
fn oversized_saved_operation_survives_reopen_without_early_application_release() {
    let root = std::env::var_os("RELAY_TEST_WORK_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("relay-tests"))
        .join(format!("delivery-budget-{}", uuid::Uuid::new_v4()));
    let server = uuid::Uuid::new_v4().to_string();
    let scope = format!("{server}/channel/owned-general");
    let mut actor = CryptoSession::open_for_test(root.clone(), &server, "owned-user").unwrap();
    let identity = actor.device.identity().clone();
    let initial = actor
        .stage_membership(
            &scope,
            batch(&[]).target,
            "owned-group".into(),
            0,
            &[],
            &[],
            true,
        )
        .unwrap();
    actor
        .committed_control(&scope, initial.events.len() as u64)
        .unwrap();
    // Establish a working application ratchet before installing the synthetic
    // legacy outbox; a missing group must not be the reason sending is refused.
    let baseline = actor
        .encrypted_content(Envelope {
            version: 1,
            scope: scope.clone(),
            client_id: "owned-working-send".into(),
            content: "owned baseline".into(),
            reply_to_id: None,
            mentions: Vec::new(),
            attachments: Vec::new(),
            action: None,
        })
        .unwrap();
    assert!(baseline.starts_with(MESSAGE_PREFIX));
    actor.committed("owned-working-send").unwrap();
    let mut operation = batch(&[relay_protocol::CRYPTO_MAX_EVENT_DECODED_BYTES + 1]);
    operation.device_id = identity.device_id.clone();
    // Synthetic legacy outbox shape tests preservation, not a historical ratchet.
    actor
        .mutate(|_, state| {
            state
                .control_outbox
                .insert(scope.clone(), operation.clone());
            Ok(())
        })
        .unwrap();
    let saved = std::fs::read(&actor.path).unwrap();
    let revision = actor.state.revision;
    drop(actor);
    let mut reopened = CryptoSession::open_for_test(root, &server, "owned-user").unwrap();
    let pending = reopened.pending_control(&scope).unwrap();
    assert_eq!(
        serde_json::to_vec(&pending).unwrap(),
        serde_json::to_vec(&operation).unwrap()
    );
    assert!(check_control_delivery(&pending)
        .unwrap_err()
        .contains("preserved"));
    assert_eq!(reopened.device.identity(), &identity);
    assert_eq!(reopened.state.revision, revision);
    assert_eq!(std::fs::read(&reopened.path).unwrap(), saved);
    assert_eq!(
        reopened
            .encrypted_content(Envelope {
                version: 1,
                scope: scope.clone(),
                client_id: "owned-working-send".into(),
                content: "owned baseline".into(),
                reply_to_id: None,
                mentions: Vec::new(),
                attachments: Vec::new(),
                action: None,
            })
            .unwrap(),
        baseline
    );
    assert!(reopened
        .encrypted_content(Envelope {
            version: 1,
            scope,
            client_id: "owned-blocked-send".into(),
            content: "private body".into(),
            reply_to_id: None,
            mentions: Vec::new(),
            attachments: Vec::new(),
            action: None,
        })
        .is_err());
    assert!(reopened.state.outbox.is_empty());
    assert_eq!(std::fs::read(&reopened.path).unwrap(), saved);
    protected::remove_head(&format!("Relay/crypto/v1/{server}/owned-user"));
}

#[test]
fn real_pending_membership_blocks_new_and_edited_content_only_in_its_scope() {
    let root = std::env::var_os("RELAY_TEST_WORK_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("relay-tests"))
        .join(format!("pending-delivery-{}", uuid::Uuid::new_v4()));
    let server = uuid::Uuid::new_v4().to_string();
    let mut actor = CryptoSession::open_for_test(root, &server, "owned-user").unwrap();
    let pending_scope = format!("{server}/channel/pending");
    let ready_scope = format!("{server}/channel/ready");
    let pending = actor
        .stage_membership(
            &pending_scope,
            batch(&[]).target,
            "owned-pending".into(),
            0,
            &[],
            &[],
            true,
        )
        .unwrap();
    let ready = actor
        .stage_membership(
            &ready_scope,
            batch(&[]).target,
            "owned-ready".into(),
            0,
            &[],
            &[],
            true,
        )
        .unwrap();
    actor
        .committed_control(&ready_scope, ready.events.len() as u64)
        .unwrap();
    let saved = std::fs::read(&actor.path).unwrap();
    let envelope = |scope: &str| Envelope {
        version: 1,
        scope: scope.into(),
        client_id: format!("owned-{scope}"),
        content: "owned body".into(),
        reply_to_id: None,
        mentions: Vec::new(),
        attachments: Vec::new(),
        action: None,
    };
    assert!(actor
        .encrypted_content(envelope(&pending_scope))
        .unwrap_err()
        .contains("confirmation"));
    assert!(actor
        .encrypted_edit_content(envelope(&pending_scope))
        .unwrap_err()
        .contains("confirmation"));
    assert!(actor
        .call_keys(&pending_scope, &["owned-user".into()])
        .err()
        .unwrap()
        .contains("confirmation"));
    assert_eq!(std::fs::read(&actor.path).unwrap(), saved);
    assert_eq!(
        serde_json::to_vec(&actor.pending_control(&pending_scope)).unwrap(),
        serde_json::to_vec(&Some(&pending)).unwrap()
    );
    assert!(actor
        .encrypted_content(envelope(&ready_scope))
        .unwrap()
        .starts_with(MESSAGE_PREFIX));
    assert!(actor
        .call_keys(&ready_scope, &["owned-user".into()])
        .is_ok());
    actor
        .committed_control(&pending_scope, pending.events.len() as u64)
        .unwrap();
    assert!(actor
        .encrypted_content(envelope(&pending_scope))
        .unwrap()
        .starts_with(MESSAGE_PREFIX));
    protected::remove_head(&format!("Relay/crypto/v1/{server}/owned-user"));
}
