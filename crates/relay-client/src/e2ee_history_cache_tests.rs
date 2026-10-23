//! Warm-cache corruption, mutable checkpoint and actual MLS continuity regressions.
use super::*;

#[test]
fn warm_history_rechecks_ciphertext_and_manifest_before_reuse() {
    let mut pair = Pair::new();
    fill_history(pair.bob.as_mut().unwrap(), &pair.scope, 1100);
    let bob = pair.bob.as_mut().unwrap();
    let scope = pair.scope.clone();
    let read = |bob: &CryptoSession| bob.cached_envelopes(&scope).collect::<Result<Vec<_>, _>>();
    assert_eq!(read(bob).unwrap().len(), 1100);
    let path = history::archive_paths(&bob.state, &bob.path)[0].clone();
    let saved = std::fs::read(&path).unwrap();
    let mut damaged = saved.clone();
    damaged[0] ^= 1;
    std::fs::write(&path, damaged).unwrap();
    assert!(read(bob).is_err(), "warm cache hid same-length tampering");
    std::fs::write(&path, &saved).unwrap();
    assert_eq!(read(bob).unwrap().len(), 1100);
    std::fs::remove_file(&path).unwrap();
    assert!(read(bob).is_err());
    std::fs::write(&path, &saved).unwrap();
    let original = bob.state.history[0].clone();
    let mut altered = serde_json::to_value(&original).unwrap();
    altered["count"] = serde_json::json!(1);
    bob.state.history[0] = serde_json::from_value(altered).unwrap();
    assert!(read(bob).is_err(), "cached bytes bypassed changed manifest");
    bob.state.history[0] = original;
    assert_eq!(read(bob).unwrap().len(), 1100);
    pair.reopen_bob();
    assert_eq!(read(pair.bob.as_ref().unwrap()).unwrap().len(), 1100);
}

#[test]
fn warm_history_observes_new_delivery_and_isolates_scope_and_reopened_actor() {
    let mut pair = Pair::new();
    let identity = pair.bob.as_ref().unwrap().device.identity().clone();
    fill_history(pair.bob.as_mut().unwrap(), &pair.scope, 1100);
    assert_eq!(
        pair.bob
            .as_ref()
            .unwrap()
            .cached_envelopes(&pair.scope)
            .count(),
        1100
    );
    assert_eq!(
        pair.bob
            .as_ref()
            .unwrap()
            .cached_envelopes("other-scope")
            .count(),
        0
    );
    let wire = pair
        .alice
        .as_mut()
        .unwrap()
        .encrypted_content(Envelope {
            version: 1,
            scope: pair.scope.clone(),
            client_id: "actual-after-warm".into(),
            content: "Actual MLS after warm history".into(),
            reply_to_id: None,
            mentions: vec![],
            attachments: vec![],
            action: None,
        })
        .unwrap();
    let bob = pair.bob.as_mut().unwrap();
    bob.decrypt_content(&pair.scope, &wire).unwrap();
    bob.bind_delivery(&wire, "actual-after-warm", 2000).unwrap();
    let rows = bob
        .cached_envelopes(&pair.scope)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(rows.len(), 1101);
    assert_eq!(
        rows.iter()
            .filter(|row| row.message_id.as_deref() == Some("actual-after-warm")
                && row.envelope.content == "Actual MLS after warm history")
            .count(),
        1
    );
    assert!(rows.iter().all(|row| row.envelope.scope == pair.scope));
    assert_eq!(
        pair.alice
            .as_ref()
            .unwrap()
            .cached_envelopes(&pair.scope)
            .count(),
        0
    );
    pair.reopen_bob();
    let bob = pair.bob.as_ref().unwrap();
    assert_eq!(bob.device.identity(), &identity);
    assert_eq!(
        bob.cached_envelopes(&pair.scope)
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
            .len(),
        1101
    );
    assert_eq!(bob.cached_envelopes("other-scope").count(), 0);
}
