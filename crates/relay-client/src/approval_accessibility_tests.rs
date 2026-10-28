use super::*;

struct FieldFrame {
    output: egui::FullOutput,
    field: egui::Id,
    changed: bool,
    approve: egui::Id,
    approved: bool,
    other: egui::Id,
}

fn context() -> egui::Context {
    let ctx = egui::Context::default();
    configure(&ctx);
    ctx.enable_accesskit();
    ctx
}

fn peer() -> CryptoPeerStatus {
    CryptoPeerStatus {
        user_id: "person".into(),
        name: "Person".into(),
        device_id: "device".into(),
        fingerprint: "12ab".repeat(16),
        verified: false,
        trusted: true,
        changed: false,
        needs_review: false,
    }
}

fn frame(
    ctx: &egui::Context,
    entered: &mut String,
    scope: (&str, &str, &str),
    peer: &CryptoPeerStatus,
    enabled: bool,
    events: Vec<egui::Event>,
) -> FieldFrame {
    let mut field = None;
    let output = ctx.run(
        egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(940.0, 650.0))),
            events,
            ..Default::default()
        },
        |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.set_max_width(440.0);
                ui.add_enabled_ui(enabled, |ui| {
                    let response =
                        approval_code_input(ui, entered, scope, &DeviceIdentity::from(peer));
                    let approve = approve_device_button(
                        ui,
                        approval_matches(peer, scope.2, entered),
                        response.id,
                    );
                    let other = ui.button("Other control");
                    field = Some((
                        response.id,
                        response.changed(),
                        approve.id,
                        approve.clicked(),
                        other.id,
                    ));
                });
            });
        },
    );
    let (field, changed, approve, approved, other) = field.unwrap();
    FieldFrame {
        output,
        field,
        changed,
        approve,
        approved,
        other,
    }
}

fn node(frame: &FieldFrame, id: egui::Id) -> &egui::accesskit::Node {
    &frame
        .output
        .platform_output
        .accesskit_update
        .as_ref()
        .unwrap()
        .nodes
        .iter()
        .find(|(node_id, _)| *node_id == egui::accesskit::NodeId(id.value()))
        .unwrap()
        .1
}

fn action(
    target: egui::Id,
    action: egui::accesskit::Action,
    data: Option<egui::accesskit::ActionData>,
) -> egui::Event {
    egui::Event::AccessKitActionRequest(egui::accesskit::ActionRequest {
        target: egui::accesskit::NodeId(target.value()),
        action,
        data,
    })
}

fn set(target: egui::Id, text: &str) -> egui::Event {
    action(
        target,
        egui::accesskit::Action::SetValue,
        Some(egui::accesskit::ActionData::Value(text.into())),
    )
}

const SCOPE: (&str, &str, &str) = ("http://test-community", "own-account", "community-id");

#[test]
fn public_approval_value_is_readable_and_setvalue_does_not_approve_or_move_focus() {
    let ctx = context();
    let peer = peer();
    let mut entered = String::new();
    let first = frame(&ctx, &mut entered, SCOPE, &peer, true, vec![]);
    let n = node(&first, first.field);
    assert_eq!(n.name(), Some("Security code or verification card"));
    assert_eq!(n.role(), egui::accesskit::Role::MultilineTextInput);
    assert_eq!(n.value(), Some(""));
    assert!(n.supports_action(egui::accesskit::Action::SetValue));
    assert!(node(&first, first.approve).is_disabled());
    let other_focus = first.other;
    ctx.memory_mut(|memory| memory.request_focus(other_focus));
    let second = frame(
        &ctx,
        &mut entered,
        SCOPE,
        &peer,
        true,
        vec![set(first.field, &peer.fingerprint)],
    );
    assert_eq!(entered, peer.fingerprint);
    assert_eq!(node(&second, second.field).value(), Some(entered.as_str()));
    assert!(!node(&second, second.approve).is_disabled());
    assert!(second.changed);
    assert!(!second.approved);
    assert!(!ctx.memory(|memory| memory.has_focus(second.field)));
    assert!(ctx.memory(|memory| memory.has_focus(second.other)));
    let repeated = frame(
        &ctx,
        &mut entered,
        SCOPE,
        &peer,
        true,
        vec![set(second.field, &peer.fingerprint)],
    );
    assert!(!repeated.changed);
    assert!(!repeated.approved);
}

#[test]
fn stale_approval_targets_cannot_follow_host_account_community_or_device_changes() {
    let ctx = context();
    let peer = peer();
    let mut entered = String::new();
    let old = frame(&ctx, &mut entered, SCOPE, &peer, true, vec![]).field;
    for scope in [
        ("http://other-host", SCOPE.1, SCOPE.2),
        (SCOPE.0, "other-account", SCOPE.2),
        (SCOPE.0, SCOPE.1, "other-community"),
    ] {
        let f = frame(
            &ctx,
            &mut entered,
            scope,
            &peer,
            true,
            vec![set(old, &peer.fingerprint)],
        );
        assert_ne!(f.field, old);
        assert!(entered.is_empty());
        assert!(!f.changed);
        assert!(!f.approved);
        let disabled = frame(
            &ctx,
            &mut entered,
            SCOPE,
            &peer,
            true,
            vec![action(f.approve, egui::accesskit::Action::Default, None)],
        );
        assert!(!disabled.approved);
    }
    for altered in [
        CryptoPeerStatus {
            user_id: "other-person".into(),
            ..peer.clone()
        },
        CryptoPeerStatus {
            device_id: "other-device".into(),
            ..peer.clone()
        },
        CryptoPeerStatus {
            fingerprint: "12ac".repeat(16),
            ..peer.clone()
        },
    ] {
        let f = frame(
            &ctx,
            &mut entered,
            SCOPE,
            &altered,
            true,
            vec![set(old, &peer.fingerprint)],
        );
        assert_ne!(f.field, old);
        assert!(entered.is_empty());
        assert!(!f.changed);
        assert!(!f.approved);
    }
}

#[test]
fn accessible_pasted_cards_still_require_exact_identity_and_code() {
    let ctx = context();
    let peer = peer();
    let mut entered = String::new();
    let id = frame(&ctx, &mut entered, SCOPE, &peer, true, vec![]).field;
    let card = VerificationCard {
        relay_device_check: 1,
        community: SCOPE.2.into(),
        account: peer.user_id.clone(),
        device: peer.device_id.clone(),
        security_code: peer.fingerprint.clone(),
    };
    let valid = serde_json::to_string_pretty(&card).unwrap();
    let f = frame(
        &ctx,
        &mut entered,
        SCOPE,
        &peer,
        true,
        vec![set(id, &valid)],
    );
    assert_eq!(entered, valid);
    assert!(!node(&f, f.approve).is_disabled());
    assert!(!f.approved);
    for (key, value) in [
        ("community", serde_json::json!("other")),
        ("account", serde_json::json!("other")),
        ("device", serde_json::json!("other")),
        ("security_code", serde_json::json!("12ac".repeat(16))),
        ("relay_device_check", serde_json::json!(2)),
        ("private_key", serde_json::json!("untrusted-extra-field")),
    ] {
        let mut card = serde_json::to_value(&card).unwrap();
        card[key] = value;
        let text = card.to_string();
        let f = frame(&ctx, &mut entered, SCOPE, &peer, true, vec![set(id, &text)]);
        assert_eq!(node(&f, id).value(), Some(text.as_str()));
        assert!(node(&f, f.approve).is_disabled());
        assert!(!f.approved);
        let attempted = frame(
            &ctx,
            &mut entered,
            SCOPE,
            &peer,
            true,
            vec![action(f.approve, egui::accesskit::Action::Default, None)],
        );
        assert_eq!(entered, text);
        assert!(!attempted.approved);
    }
}

#[test]
fn setvalue_limits_are_atomic_and_disabled_or_malformed_actions_are_ignored() {
    let ctx = context();
    let peer = peer();
    let mut entered = peer.fingerprint.clone();
    let id = frame(&ctx, &mut entered, SCOPE, &peer, true, vec![]).field;
    for text in ["a".repeat(4097), "é".repeat(2049)] {
        let f = frame(&ctx, &mut entered, SCOPE, &peer, true, vec![set(id, &text)]);
        assert_eq!(entered, peer.fingerprint);
        assert!(!f.changed);
        assert!(!f.approved);
    }
    for event in [
        action(id, egui::accesskit::Action::SetValue, None),
        action(
            id,
            egui::accesskit::Action::SetValue,
            Some(egui::accesskit::ActionData::NumericValue(1.0)),
        ),
        action(
            id,
            egui::accesskit::Action::Focus,
            Some(egui::accesskit::ActionData::Value("other".into())),
        ),
    ] {
        let f = frame(&ctx, &mut entered, SCOPE, &peer, true, vec![event]);
        assert_eq!(entered, peer.fingerprint);
        assert!(!f.changed);
        assert!(!f.approved);
    }
    let f = frame(
        &ctx,
        &mut entered,
        SCOPE,
        &peer,
        false,
        vec![set(id, "other")],
    );
    assert_eq!(entered, peer.fingerprint);
    assert!(node(&f, id).is_disabled());
    assert!(!node(&f, id).supports_action(egui::accesskit::Action::SetValue));
    assert!(!f.changed);
    let max = "a".repeat(4096);
    let f = frame(&ctx, &mut entered, SCOPE, &peer, true, vec![set(id, &max)]);
    assert_eq!(entered, max);
    assert!(f.changed);
    assert!(node(&f, f.approve).is_disabled());
    // An oversized last replacement cannot silently fall back to an earlier one.
    let f = frame(
        &ctx,
        &mut entered,
        SCOPE,
        &peer,
        true,
        vec![set(id, "other"), set(id, &"a".repeat(4097))],
    );
    assert_eq!(entered, max);
    assert!(!f.changed);
    let f = frame(&ctx, &mut entered, SCOPE, &peer, true, vec![set(id, "")]);
    assert!(entered.is_empty());
    assert_eq!(node(&f, id).value(), Some(""));
    assert!(f.changed);
    assert!(node(&f, f.approve).is_disabled());
    assert!(!f.approved);
}

#[test]
fn normal_keyboard_editing_and_separate_explicit_approval_remain_required() {
    let ctx = context();
    let peer = peer();
    let mut entered = String::new();
    let id = frame(&ctx, &mut entered, SCOPE, &peer, true, vec![]).field;
    let f = frame(
        &ctx,
        &mut entered,
        SCOPE,
        &peer,
        true,
        vec![set(id, &peer.fingerprint[..32])],
    );
    assert!(node(&f, f.approve).is_disabled());
    assert!(!f.approved);
    ctx.memory_mut(|memory| memory.request_focus(id));
    let f = frame(
        &ctx,
        &mut entered,
        SCOPE,
        &peer,
        true,
        vec![egui::Event::Text(peer.fingerprint[32..].into())],
    );
    assert_eq!(entered, peer.fingerprint);
    assert!(!node(&f, f.approve).is_disabled());
    assert!(!f.approved);
    let f = frame(
        &ctx,
        &mut entered,
        SCOPE,
        &peer,
        true,
        vec![action(f.approve, egui::accesskit::Action::Default, None)],
    );
    assert!(f.approved);
}
