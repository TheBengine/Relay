use super::*;

fn fixture() -> (Intent, crate::central_account::Account, Bootstrap) {
    let account = crate::central_account::Account {
        issuer: "https://account.invalid/realms/qa".into(),
        subject: "owned-subject".into(),
        handle: "alice".into(),
        expires_at: 1000,
    };
    let previous = CommandOrigin {
        url: "https://community.invalid".into(),
        server_id: "previous".into(),
        user_id: "local-user".into(),
    };
    let origin = CommandOrigin {
        url: "https://account.invalid/people".into(),
        server_id: "people".into(),
        user_id: "me".into(),
    };
    let intent = Intent {
        origin,
        call: uuid::Uuid::new_v4().to_string(),
        previous: Some(previous),
        issuer: account.issuer.clone(),
        subject: account.subject.clone(),
        handle: account.handle.clone(),
        generation: 7,
        deadline: Instant::now() + Duration::from_secs(10),
    };
    let mut b = Bootstrap {
        server_id: "people".into(),
        user: User {
            id: "me".into(),
            ..Default::default()
        },
        members: vec![User {
            id: "caller".into(),
            ..Default::default()
        }],
        capabilities: vec!["people-v1".into(), "private-calls-v1".into()],
        linked_identities: vec![relay_protocol::LinkedIdentity {
            user_id: "me".into(),
            issuer: account.issuer.clone(),
            handle: account.handle.clone(),
        }],
        channels: vec![Channel {
            id: "voice".into(),
            kind: "voice".into(),
            can_join: true,
            ..Default::default()
        }],
        conversations: vec![
            ConversationInfo {
                channel_id: "voice".into(),
                kind: "group_voice".into(),
                parent_id: Some("pair".into()),
                ..Default::default()
            },
            ConversationInfo {
                channel_id: "pair".into(),
                kind: "group".into(),
                member_ids: vec!["me".into(), "caller".into()],
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    b.social.friends.push("caller".into());
    b.social.calls.push(PrivateCall {
        id: intent.call.clone(),
        channel_id: "voice".into(),
        caller_id: "caller".into(),
        recipient_id: "me".into(),
        state: "ringing".into(),
        expires_at: 200,
    });
    (intent, account, b)
}

fn context<'a>(intent: &'a Intent, account: &'a crate::central_account::Account) -> Context<'a> {
    Context {
        previous: intent.previous.as_ref(),
        account: Some(account),
        generation: Some(7),
        ringing: true,
        idle: true,
        navigation_idle: true,
        now: Instant::now(),
    }
}

#[test]
fn exact_fresh_incoming_call_survives_only_its_own_idle_account_switch() {
    let (intent, account, b) = fixture();
    let c = context(&intent, &account);
    assert_eq!(
        intent.call(&c, &intent.origin.url, &b, 100).unwrap().id,
        intent.call
    );
    assert!(intent
        .call(&c, "https://other.invalid/people", &b, 100)
        .is_none());
    assert!(intent.call(&c, &intent.origin.url, &b, 200).is_none());
    let other = CommandOrigin {
        url: "https://third.invalid".into(),
        ..intent.previous.clone().unwrap()
    };
    assert!(!intent.current(&Context {
        previous: Some(&other),
        ..c
    }));
}

#[test]
fn logout_same_origin_relogin_subject_change_and_expired_intent_cannot_answer() {
    let (intent, account, _) = fixture();
    assert!(!intent.current(&Context {
        account: None,
        ..context(&intent, &account)
    }));
    assert!(!intent.current(&Context {
        generation: None,
        ..context(&intent, &account)
    }));
    assert!(!intent.current(&Context {
        generation: Some(8),
        ..context(&intent, &account)
    }));
    let other = crate::central_account::Account {
        subject: "replacement-subject".into(),
        ..account.clone()
    };
    assert!(!intent.current(&context(&intent, &other)));
    assert!(!intent.current(&Context {
        now: intent.deadline,
        ..context(&intent, &account)
    }));
}

#[test]
fn starting_another_call_or_navigation_or_terminal_ring_cancels_the_answer() {
    let (intent, account, _) = fixture();
    for (idle, navigation_idle, ringing) in [
        (false, true, true),
        (true, false, true),
        (true, true, false),
    ] {
        assert!(!intent.current(&Context {
            idle,
            navigation_idle,
            ringing,
            ..context(&intent, &account)
        }));
    }
}

#[test]
fn response_identity_link_friendship_pair_membership_and_permission_are_rechecked() {
    let (intent, account, original) = fixture();
    let c = context(&intent, &account);
    for change in 0..9 {
        let mut b = original.clone();
        match change {
            0 => b.server_id = "replacement".into(),
            1 => b.user.id = "different-user".into(),
            2 => b.linked_identities.clear(),
            3 => b.linked_identities[0].issuer = "https://different.invalid".into(),
            4 => b.social.friends.clear(),
            5 => b.social.blocked.push("caller".into()),
            6 => b.channels[0].can_join = false,
            7 => b.conversations[1].member_ids.push("outsider".into()),
            _ => b.capabilities.retain(|value| value != "people-v1"),
        }
        assert!(
            intent.call(&c, &intent.origin.url, &b, 100).is_none(),
            "changed field {change}"
        );
    }
}

#[test]
fn exact_call_identity_is_required_and_a_terminal_or_outgoing_call_is_not_answered() {
    let (intent, account, original) = fixture();
    let c = context(&intent, &account);
    for state in ["accepted", "declined", "cancelled", "missed", "ended"] {
        let mut b = original.clone();
        b.social.calls[0].state = state.into();
        assert!(intent.call(&c, &intent.origin.url, &b, 100).is_none());
    }
    let mut b = original.clone();
    b.social.calls[0].id = uuid::Uuid::new_v4().to_string();
    assert!(intent.call(&c, &intent.origin.url, &b, 100).is_none());
    b = original.clone();
    b.social.calls[0].recipient_id = "caller".into();
    assert!(intent.call(&c, &intent.origin.url, &b, 100).is_none());
    let mut other = original.social.calls[0].clone();
    other.id = uuid::Uuid::new_v4().to_string();
    b = original;
    b.social.calls.insert(0, other);
    assert_eq!(
        intent.call(&c, &intent.origin.url, &b, 100).unwrap().id,
        intent.call
    );
}
