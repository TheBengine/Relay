use super::*;
use relay_protocol::{Message, User};

fn fixture(cursor: u64) -> (State, CommandOrigin, u64, Bootstrap, Instant) {
    let origin = CommandOrigin {
        url: "https://owned.invalid/people".into(),
        server_id: "friends".into(),
        user_id: "self".into(),
    };
    let mut state = State::default();
    state.policy.notifications = true;
    state.policy.previews = true;
    state.policy.audio.sounds_enabled = true;
    state.policy.audio.sound_volume = 0.5;
    let generation = state.bind(origin.clone()).unwrap();
    let mut b = Bootstrap {
        server_id: origin.server_id.clone(),
        user: User {
            id: origin.user_id.clone(),
            ..Default::default()
        },
        protocol_version: relay_protocol::API_PROTOCOL_VERSION,
        capabilities: vec!["people-v1".into()],
        latest_change_sequence: cursor,
        ..Default::default()
    };
    b.members.push(User {
        id: "friend".into(),
        username: "Alex".into(),
        ..Default::default()
    });
    b.social.friends.push("friend".into());
    let now = Instant::now();
    state.observe_with_cursor(
        &origin,
        generation,
        &b,
        now,
        1000,
        Some(MessageCursor::baseline(Some(cursor))),
    );
    (state, origin, generation, b, now)
}
fn change(sequence: u64, id: &str) -> MessageChange {
    MessageChange {
        sequence,
        kind: "created".into(),
        message_id: id.into(),
        user_id: "friend".into(),
        recipient_id: Some("self".into()),
        message: Some(Message {
            id: id.into(),
            recipient_id: Some("self".into()),
            author: User {
                id: "friend".into(),
                ..Default::default()
            },
            content: "ciphertext-must-never-be-previewed".into(),
            ..Default::default()
        }),
        ..Default::default()
    }
}
fn page(changes: Vec<MessageChange>, next: u64) -> ChangePage {
    ChangePage {
        changes,
        next_cursor: next,
        has_more: false,
    }
}

#[test]
fn new_friends_dm_is_generic_and_exactly_once_in_both_ui_background_orders() {
    for ui_first in [true, false] {
        let (mut state, origin, generation, _, now) = fixture(100);
        let c = change(101, "new");
        if ui_first {
            let claims = state.ui_message_claims(&origin, std::slice::from_ref(&c), now);
            assert!(
                claims[0].as_ref().unwrap().as_ref().unwrap().valid(),
                "UI fallback retains its own generation guard"
            );
        }
        assert!(state.message_page(&origin, generation, 100, &page(vec![c.clone()], 101), now));
        assert!(state.ui_message_claims(&origin, std::slice::from_ref(&c), now)[0].is_err());
        let effects = state.message_effects(now, 1000, false);
        assert_eq!(effects.len(), usize::from(!ui_first));
        if let Some(delivery) = effects.first() {
            assert_eq!(delivery.notification.origin, origin);
            assert_eq!(
                delivery.notification.target,
                Target::Direct("friend".into())
            );
            assert!(delivery.notification.body.contains("Alex"));
            assert!(!delivery.notification.body.contains("ciphertext"));
            assert!(delivery.notification.guard.as_ref().unwrap().valid());
            assert!(delivery.notification.background_only);
        }
        assert!(state
            .message_effects(now + Duration::from_secs(1), 1001, false)
            .is_empty());
        assert!(!state.message_page(&origin, generation, 100, &page(vec![c], 101), now));
    }
}

#[test]
fn baseline_missing_cursor_and_restored_cursor_never_replay_history() {
    let (mut state, origin, generation, mut b, now) = fixture(0);
    state.accounts[0].messages = None;
    state.observe_with_cursor(
        &origin,
        generation,
        &b,
        now,
        1000,
        Some(MessageCursor::baseline(None)),
    );
    assert!(state.ui_message_claims(&origin, &[change(1, "old")], now)[0].is_err());
    assert!(state.message_page(
        &origin,
        generation,
        0,
        &page(vec![change(1, "old")], 1),
        now
    ));
    assert!(state.message_effects(now, 1000, false).is_empty());
    assert!(state.message_page(
        &origin,
        generation,
        1,
        &page(vec![change(2, "live")], 2),
        now
    ));
    assert_eq!(state.message_effects(now, 1000, false).len(), 1);
    b.latest_change_sequence = 0;
    state.observe_with_cursor(
        &origin,
        generation,
        &b,
        now + Duration::from_secs(1),
        1001,
        Some(MessageCursor::polled(Some(0), now + Duration::from_secs(1))),
    );
    assert!(state.message_page(
        &origin,
        generation,
        0,
        &page(vec![change(1, "old"), change(2, "live")], 2),
        now + Duration::from_secs(1)
    ));
    assert!(state
        .message_effects(now + Duration::from_secs(1), 1001, false)
        .is_empty());
    assert!(state.message_page(
        &origin,
        generation,
        2,
        &page(vec![change(3, "after-restore")], 3),
        now + Duration::from_secs(2)
    ));
    assert_eq!(
        state
            .message_effects(now + Duration::from_secs(2), 1002, false)
            .len(),
        1
    );
}

#[test]
fn malformed_or_unstarted_background_feed_keeps_guarded_ui_fallback() {
    for broken in ["backwards", "unordered", "beyond-cursor", "no-progress"] {
        let (mut state, origin, generation, _, now) = fixture(100);
        let mut p = page(vec![change(101, "new")], 101);
        match broken {
            "backwards" => p.next_cursor = 99,
            "unordered" => p.changes.push(change(100, "earlier")),
            "beyond-cursor" => p.changes[0].sequence = 102,
            _ => {
                p.next_cursor = 100;
                p.has_more = true;
                p.changes.clear();
            }
        }
        assert!(
            !state.message_page(&origin, generation, 100, &p, now),
            "{broken}"
        );
        assert!(
            state.ui_message_claims(&origin, &[change(101, "fallback")], now)[0]
                .as_ref()
                .unwrap()
                .is_some(),
            "{broken}"
        );
    }
}

#[test]
fn self_history_edits_deleted_unrelated_and_spoofed_dm_metadata_do_not_alert() {
    for invalid in [
        "self",
        "history",
        "edited",
        "deleted",
        "recipient",
        "author",
        "id",
        "channel",
        "not-friend",
        "blocked",
    ] {
        let (mut state, origin, generation, mut b, now) = fixture(100);
        let mut c = change(101, "candidate");
        match invalid {
            "self" => {
                c.user_id = "self".into();
                c.message.as_mut().unwrap().author.id = "self".into();
            }
            "history" => c.sequence = 100,
            "edited" => c.kind = "edited".into(),
            "deleted" => c.deleted = true,
            "recipient" => c.message.as_mut().unwrap().recipient_id = Some("someone-else".into()),
            "author" => c.message.as_mut().unwrap().author.id = "someone-else".into(),
            "id" => c.message.as_mut().unwrap().id = "different".into(),
            "channel" => {
                c.channel_id = Some("public".into());
                c.message.as_mut().unwrap().channel_id = Some("public".into());
            }
            "not-friend" => b.social.friends.clear(),
            _ => b.social.blocked.push("friend".into()),
        }
        state.observe(&origin, generation, &b, now, 1000);
        if invalid == "history" {
            assert!(state.ui_message_claims(&origin, &[c], now)[0].is_err());
        } else {
            assert!(state.message_page(&origin, generation, 100, &page(vec![c], 101), now));
        }
        assert!(
            state.message_effects(now, 1000, false).is_empty(),
            "{invalid}"
        );
    }
}

#[test]
fn message_policies_dnd_focus_and_preview_privacy_are_applied_without_replay() {
    for quiet in [
        "dnd",
        "disabled",
        "off",
        "focus",
        "community-muted",
        "peer-none",
    ] {
        let (mut state, origin, generation, mut b, now) = fixture(100);
        let scope =
            crate::conversation_state::ConversationMemory::scope(&origin.url, &origin.user_id);
        match quiet {
            "dnd" => {
                b.presence.push(relay_protocol::UserPresence {
                    user_id: "self".into(),
                    status: "dnd".into(),
                    ..Default::default()
                });
                state.observe(&origin, generation, &b, now, 1000);
            }
            "disabled" => state.policy.disabled = true,
            "off" => state.policy.notifications = false,
            "community-muted" => {
                state.policy.message_policies.insert(
                    format!("{scope}|community"),
                    NotificationPolicy {
                        muted_until: 1001,
                        ..Default::default()
                    },
                );
            }
            "peer-none" => {
                state.policy.message_policies.insert(
                    format!("{scope}|{:?}", Target::Direct("friend".into())),
                    NotificationPolicy {
                        mode: "none".into(),
                        ..Default::default()
                    },
                );
            }
            _ => {}
        }
        assert!(state.message_page(
            &origin,
            generation,
            100,
            &page(vec![change(101, "quiet")], 101),
            now
        ));
        assert!(
            state
                .message_effects(now, 1000, quiet == "focus")
                .is_empty(),
            "{quiet}"
        );
        state.policy.disabled = false;
        state.policy.notifications = true;
        state.policy.message_policies.clear();
        state.accounts[0].quiet = false;
        assert!(
            state
                .message_effects(now + Duration::from_secs(2), 1002, false)
                .is_empty(),
            "old quiet message must not reappear: {quiet}"
        );
    }
    let (mut state, origin, generation, _, now) = fixture(100);
    state.policy.previews = false;
    assert!(state.message_page(
        &origin,
        generation,
        100,
        &page(vec![change(101, "private")], 101),
        now
    ));
    let delivery = state.message_effects(now, 1000, false).pop().unwrap();
    assert!(!delivery.notification.body.contains("Alex"));
    assert!(!delivery.notification.title.contains("Alex"));
    assert!(!delivery.notification.body.contains("ciphertext"));
    assert_eq!(
        message_cue(&state.policy, true, false),
        Some(AudioCue::Message)
    );
    state.policy.deafened = true;
    assert!(message_cue(&state.policy, true, false).is_none());
    state.policy.deafened = false;
    assert!(
        message_cue(&state.policy, true, true).is_none(),
        "message sound must not replace a ring"
    );
    state.policy.audio.sounds_enabled = false;
    assert!(message_cue(&state.policy, true, false).is_none());
}

#[test]
fn logout_relogin_other_origin_and_revoked_generation_cannot_reuse_message_authority() {
    let (mut state, origin, generation, b, now) = fixture(100);
    let guard = state.accounts[0].message_guard.clone();
    let next = state.bind(origin.clone()).unwrap();
    assert!(!guard.valid());
    state.observe_with_cursor(
        &origin,
        next,
        &b,
        now,
        1000,
        Some(MessageCursor::baseline(Some(b.latest_change_sequence))),
    );
    assert!(!state.message_page(
        &origin,
        generation,
        100,
        &page(vec![change(101, "old-generation")], 101),
        now
    ));
    let other = CommandOrigin {
        url: "https://different.invalid/people".into(),
        ..origin.clone()
    };
    assert!(!state.message_page(
        &other,
        next,
        100,
        &page(vec![change(101, "other-origin")], 101),
        now
    ));
    state.retire_message_account(&origin);
    state.accounts.clear();
    assert!(state.ui_message_claims(&origin, &[change(101, "queued-ui")], now)[0].is_err());
    assert!(!state.message_page(
        &origin,
        next,
        100,
        &page(vec![change(101, "logged-out")], 101),
        now
    ));
    let current = state.bind(origin.clone()).unwrap();
    let mut fresh = b;
    fresh.latest_change_sequence = 101;
    state.observe_with_cursor(
        &origin,
        current,
        &fresh,
        now,
        1000,
        Some(MessageCursor::baseline(Some(fresh.latest_change_sequence))),
    );
    assert!(state.ui_message_claims(&origin, &[change(101, "queued-ui")], now)[0].is_err());
    assert!(state.message_effects(now, 1000, false).is_empty());
}

#[test]
fn message_claim_memory_and_queue_retries_are_bounded_without_replaying_old_sequences() {
    let (mut state, origin, generation, _, now) = fixture(100);
    let p = page(
        (101..=300).map(|s| change(s, &format!("id-{s}"))).collect(),
        300,
    );
    assert!(state.message_page(&origin, generation, 100, &p, now));
    assert_eq!(
        state.accounts[0].messages.as_ref().unwrap().pending.len(),
        MAX_PENDING
    );
    let first = state.message_effects(now, 1000, false);
    assert_eq!(first.len(), MAX_PENDING);
    state.retry_message(&origin, &first[0].id, now);
    assert!(state
        .message_effects(now + Duration::from_millis(999), 1000, false)
        .is_empty());
    assert_eq!(
        state
            .message_effects(now + Duration::from_secs(1), 1001, false)
            .len(),
        1
    );
    assert!(state
        .message_effects(now + RETRY_LIFETIME, 1015, false)
        .is_empty());
    assert!(state.accounts[0]
        .messages
        .as_ref()
        .unwrap()
        .pending
        .is_empty());
    for sequence in 301..=1000 {
        state.ui_message_claims(&origin, &[change(sequence, &format!("id-{sequence}"))], now);
    }
    assert!(state.accounts[0].messages.as_ref().unwrap().seen.len() <= MAX_IDS);
    assert!(state.ui_message_claims(&origin, &[change(101, "id-101")], now)[0].is_err());
    // A non-Friends source still uses its normal UI notification flow.
    let other = CommandOrigin {
        url: "https://community.invalid".into(),
        ..origin
    };
    assert!(
        state.ui_message_claims(&other, &[change(1001, "community-dm")], now)[0]
            .as_ref()
            .unwrap()
            .is_none()
    );
}

#[test]
fn explicit_zero_cursor_preserves_first_new_dm_in_both_ui_background_orders() {
    for ui_first in [true, false] {
        let (mut state, origin, generation, _, now) = fixture(0);
        let c = change(1, "very-first-dm");
        if ui_first {
            assert!(
                state.ui_message_claims(&origin, std::slice::from_ref(&c), now)[0]
                    .as_ref()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .valid()
            );
        }
        assert!(state.message_page(&origin, generation, 0, &page(vec![c.clone()], 1), now));
        assert_eq!(
            state.message_effects(now, 1000, false).len(),
            usize::from(!ui_first)
        );
        assert!(state.ui_message_claims(&origin, &[c], now)[0].is_err());
    }
}

#[test]
fn failed_or_stalled_message_feed_releases_ui_fallback_without_replay_on_recovery() {
    for stalled in [false, true] {
        let (mut state, origin, generation, _, now) = fixture(100);
        assert!(state.message_page(&origin, generation, 100, &page(vec![], 100), now));
        let resumed = if stalled {
            now + OWNERSHIP_LIFETIME
        } else {
            now
        };
        if !stalled {
            state.release_message_feed(&origin, generation);
        }
        let fresh = change(101, "fallback-during-outage");
        assert!(
            state.ui_message_claims(&origin, std::slice::from_ref(&fresh), resumed)[0]
                .as_ref()
                .unwrap()
                .as_ref()
                .unwrap()
                .valid()
        );
        assert!(state.message_page(
            &origin,
            generation,
            100,
            &page(vec![fresh.clone()], 101),
            resumed
        ));
        assert!(state.message_effects(resumed, 1000, false).is_empty());
        assert!(state.ui_message_claims(&origin, &[fresh], resumed)[0].is_err());
        let next = change(102, "recovered-background");
        assert!(state.message_page(
            &origin,
            generation,
            101,
            &page(vec![next.clone()], 102),
            resumed
        ));
        assert_eq!(state.message_effects(resumed, 1000, false).len(), 1);
        assert!(state.ui_message_claims(&origin, &[next], resumed)[0].is_err());
    }
}

#[test]
fn legacy_and_reset_cursor_baselines_suppress_every_initial_history_page() {
    for reset in [false, true] {
        let (mut state, origin, generation, mut b, now) = fixture(if reset { 700 } else { 0 });
        if reset {
            b.latest_change_sequence = 0;
            state.observe_with_cursor(
                &origin,
                generation,
                &b,
                now,
                1000,
                Some(MessageCursor::polled(Some(0), now)),
            );
        } else {
            state.accounts[0].messages = None;
            state.observe_with_cursor(
                &origin,
                generation,
                &b,
                now,
                1000,
                Some(MessageCursor::baseline(None)),
            );
        }
        for chunk in 0..3 {
            let after = chunk * 200;
            let next = after + 200;
            let mut p = page(
                (after + 1..=next)
                    .map(|seq| change(seq, &format!("history-{seq}")))
                    .collect(),
                next,
            );
            p.has_more = chunk < 2;
            assert!(state.message_page(&origin, generation, after, &p, now));
            assert!(state.message_effects(now, 1000, false).is_empty());
            assert!(state
                .ui_message_claims(&origin, &p.changes, now)
                .iter()
                .all(Result::is_err));
        }
        let fresh = change(601, "after-history");
        assert!(state.message_page(
            &origin,
            generation,
            600,
            &page(vec![fresh.clone()], 601),
            now
        ));
        assert_eq!(state.message_effects(now, 1000, false).len(), 1);
        assert!(state.ui_message_claims(&origin, &[fresh], now)[0].is_err());
    }
}

#[test]
fn same_origin_observer_replacement_preserves_reconnect_arrivals_and_prior_shared_claims() {
    let (mut state, origin, old, mut b, now) = fixture(100);
    let prior = change(101, "already-claimed");
    assert!(state.ui_message_claims(&origin, std::slice::from_ref(&prior), now)[0].is_ok());
    let guard = state.accounts[0].message_guard.clone();
    let generation = state.bind(origin.clone()).unwrap();
    assert_ne!(old, generation);
    assert!(!guard.valid());
    b.latest_change_sequence = 102;
    state.observe_with_cursor(
        &origin,
        generation,
        &b,
        now,
        1000,
        Some(MessageCursor::baseline(Some(102))),
    );
    assert!(!state.message_page(&origin, old, 100, &page(vec![prior.clone()], 101), now));
    assert!(state.message_page(
        &origin,
        generation,
        100,
        &page(
            vec![prior.clone(), change(102, "arrived-during-reconnect")],
            102
        ),
        now
    ));
    assert!(state.ui_message_claims(&origin, &[prior], now)[0].is_err());
    let effects = state.message_effects(now, 1000, false);
    assert_eq!(effects.len(), 1);
    assert_eq!(effects[0].id, "arrived-during-reconnect");
}

#[test]
fn same_origin_observer_rebind_retains_bounded_unsent_retry_under_fresh_guard() {
    let (mut state, origin, generation, mut b, now) = fixture(100);
    assert!(state.message_page(
        &origin,
        generation,
        100,
        &page(vec![change(101, "retry")], 101),
        now
    ));
    let previous = state.message_effects(now, 1000, false).pop().unwrap();
    state.retry_message(&origin, &previous.id, now);
    let current = state.bind(origin.clone()).unwrap();
    assert!(!previous.notification.guard.unwrap().valid());
    b.latest_change_sequence = 101;
    state.observe_with_cursor(
        &origin,
        current,
        &b,
        now,
        1000,
        Some(MessageCursor::baseline(Some(101))),
    );
    assert!(state.message_effects(now, 1000, false).is_empty());
    let retry = state.message_effects(now + Duration::from_secs(1), 1001, false);
    assert_eq!(retry.len(), 1);
    assert_eq!(retry[0].id, "retry");
    assert!(retry[0].notification.guard.as_ref().unwrap().valid());
    assert!(state
        .message_effects(now + Duration::from_secs(2), 1002, false)
        .is_empty());
}

#[test]
fn delayed_external_bootstrap_and_observer_rebind_keep_cursor_retry_and_reconnect_arrivals() {
    let (mut state, origin, generation, mut stale, now) = fixture(100);
    let consumed = now + Duration::from_secs(1);
    assert!(state.message_page(
        &origin,
        generation,
        100,
        &page(vec![change(101, "pending")], 101),
        consumed
    ));
    let delivered = state.message_effects(consumed, 1001, false).pop().unwrap();
    state.retry_message(&origin, &delivered.id, consumed);
    // This external request captured sequence 100 before the page above, but
    // arrived afterwards. Its receipt order is not evidence of a feed reset.
    stale.social.server_time = 200;
    let received = now + Duration::from_secs(2);
    state.observe_with_cursor(
        &origin,
        generation,
        &stale,
        received,
        1002,
        Some(MessageCursor::baseline(Some(100))),
    );
    assert_eq!(state.message_cursor(&origin, generation), Some(101));
    assert_eq!(
        state.accounts[0].messages.as_ref().unwrap().pending.len(),
        1
    );
    assert!(
        state.accounts[0].clock.is_some(),
        "normal authenticated clock metadata still updates"
    );
    let next = state.bind(origin.clone()).unwrap();
    assert!(!delivered.notification.guard.unwrap().valid());
    // The initial bootstrap of a routine same-origin replacement can carry
    // the same delayed snapshot; pending metadata and consumed cursor survive.
    state.observe_with_cursor(
        &origin,
        next,
        &stale,
        received,
        1002,
        Some(MessageCursor::baseline(Some(100))),
    );
    assert_eq!(state.message_cursor(&origin, next), Some(101));
    let resumed = now + Duration::from_secs(3);
    assert!(state.message_page(
        &origin,
        next,
        101,
        &page(vec![change(102, "arrived-during-reconnect")], 102),
        resumed
    ));
    let effects = state.message_effects(resumed, 1003, false);
    assert_eq!(effects.len(), 2);
    assert!(effects.iter().any(|e| e.id == "pending"));
    assert!(effects.iter().any(|e| e.id == "arrived-during-reconnect"));
    assert!(effects
        .iter()
        .all(|e| e.notification.guard.as_ref().unwrap().valid()));
    assert!(state
        .ui_message_claims(
            &origin,
            &[
                change(101, "pending"),
                change(102, "arrived-during-reconnect")
            ],
            resumed
        )
        .iter()
        .all(Result::is_err));
}

#[test]
fn reset_requires_subscriber_request_started_after_the_consumed_cursor() {
    let (mut state, origin, generation, mut b, now) = fixture(100);
    let consumed = now + Duration::from_secs(1);
    assert!(state.message_page(
        &origin,
        generation,
        100,
        &page(vec![change(101, "pending")], 101),
        consumed
    ));
    b.latest_change_sequence = 0;
    state.observe_with_cursor(
        &origin,
        generation,
        &b,
        now + Duration::from_secs(2),
        1002,
        Some(MessageCursor::polled(Some(0), now)),
    );
    assert_eq!(state.message_cursor(&origin, generation), Some(101));
    assert_eq!(
        state.accounts[0].messages.as_ref().unwrap().pending.len(),
        1
    );
    state.observe_with_cursor(
        &origin,
        generation,
        &b,
        now + Duration::from_secs(4),
        1004,
        Some(MessageCursor::polled(Some(0), now + Duration::from_secs(3))),
    );
    assert_eq!(state.message_cursor(&origin, generation), Some(0));
    let messages = state.accounts[0].messages.as_ref().unwrap();
    assert!(messages.pending.is_empty());
    assert!(
        messages.baseline.is_none(),
        "a proven reset starts conservative history baselining"
    );
}

fn with_group(b: &mut Bootstrap, members: &[&str]) {
    b.capabilities
        .push(relay_protocol::PEOPLE_GROUPS_CAPABILITY.into());
    b.conversations.push(relay_protocol::ConversationInfo {
        channel_id: "trio".into(),
        kind: "group".into(),
        owner_id: "friend".into(),
        parent_id: None,
        archived: false,
        member_ids: members.iter().map(|m| m.to_string()).collect(),
    });
    b.channels.push(relay_protocol::Channel {
        id: "trio".into(),
        name: "Weekend".into(),
        can_send: true,
        ..Default::default()
    });
}
fn group_change(sequence: u64, id: &str, author: &str, channel: &str) -> MessageChange {
    MessageChange {
        sequence,
        kind: "created".into(),
        message_id: id.into(),
        user_id: author.into(),
        channel_id: Some(channel.into()),
        message: Some(Message {
            id: id.into(),
            channel_id: Some(channel.into()),
            author: User {
                id: author.into(),
                ..Default::default()
            },
            content: "ciphertext-must-never-be-previewed".into(),
            ..Default::default()
        }),
        ..Default::default()
    }
}
fn group_fixture() -> (State, CommandOrigin, u64, Bootstrap, Instant) {
    let (mut state, origin, generation, mut b, now) = fixture(100);
    b.members.push(User {
        id: "other".into(),
        username: "Sam".into(),
        ..Default::default()
    });
    b.social.friends.push("other".into());
    with_group(&mut b, &["self", "friend", "other"]);
    state.observe_with_cursor(&origin, generation, &b, now, 1000, None);
    (state, origin, generation, b, now)
}

#[test]
fn a_friends_group_message_alerts_once_for_that_group_without_its_content() {
    let (mut state, origin, generation, _, now) = group_fixture();
    let c = group_change(101, "g1", "friend", "trio");
    assert!(state.message_page(&origin, generation, 100, &page(vec![c.clone()], 101), now));
    // The foreground path stands down once the background observer owns it.
    assert!(state.ui_message_claims(&origin, std::slice::from_ref(&c), now)[0].is_err());
    let effects = state.message_effects(now, 1000, false);
    assert_eq!(effects.len(), 1);
    let note = &effects[0].notification;
    assert_eq!(note.target, Target::Channel("trio".into()));
    assert!(note.body.contains("Alex") && note.body.contains("Weekend"));
    assert!(!note.body.contains("ciphertext"));
    assert!(note.background_only && note.guard.as_ref().unwrap().valid());
    assert!(state
        .message_effects(now + Duration::from_secs(1), 1001, false)
        .is_empty());
    // With previews off neither the person nor the group is named.
    state.policy.previews = false;
    let c = group_change(102, "g2", "other", "trio");
    assert!(state.message_page(&origin, generation, 101, &page(vec![c], 102), now));
    let effects = state.message_effects(now, 1002, false);
    assert_eq!(effects.len(), 1);
    assert!(!effects[0].notification.body.contains("Sam"));
    assert!(!effects[0].notification.body.contains("Weekend"));
}

#[test]
fn only_a_current_friend_who_is_in_this_accounts_group_can_alert() {
    let (mut state, origin, generation, b, now) = group_fixture();
    let before = |state: &mut State, sequence: u64, change: MessageChange| {
        assert!(state.message_page(
            &origin,
            generation,
            sequence - 1,
            &page(vec![change], sequence),
            now
        ));
        state.message_effects(now, 1000, false).len()
    };
    // Not a member of the group, even though a friend: silent.
    let mut outsider = b.clone();
    outsider.members.push(User {
        id: "stranger".into(),
        username: "Zed".into(),
        ..Default::default()
    });
    outsider.social.friends.push("stranger".into());
    state.observe_with_cursor(&origin, generation, &outsider, now, 1000, None);
    assert_eq!(
        before(&mut state, 101, group_change(101, "x1", "stranger", "trio")),
        0
    );
    // A channel this account is not in (a community channel) keeps its own path.
    let general = group_change(102, "x2", "friend", "general");
    assert!(
        state.ui_message_claims(&origin, std::slice::from_ref(&general), now)[0]
            .as_ref()
            .is_ok_and(|claim| claim.is_none())
    );
    assert_eq!(before(&mut state, 102, general), 0);
    // My own message and a blocked friend's message never alert.
    assert_eq!(
        before(&mut state, 103, group_change(103, "x3", "self", "trio")),
        0
    );
    let mut blocked = b.clone();
    blocked.social.blocked.push("friend".into());
    state.observe_with_cursor(&origin, generation, &blocked, now, 1000, None);
    assert_eq!(
        before(&mut state, 104, group_change(104, "x4", "friend", "trio")),
        0
    );
    // A real member and friend alerts.
    state.observe_with_cursor(&origin, generation, &b, now, 1000, None);
    assert_eq!(
        before(&mut state, 105, group_change(105, "x5", "other", "trio")),
        1
    );
}

#[test]
fn a_group_alert_follows_the_groups_own_choice_and_mentions() {
    let (mut state, origin, generation, _, now) = group_fixture();
    let scope = crate::conversation_state::ConversationMemory::scope(&origin.url, &origin.user_id);
    let key = format!("{scope}|{:?}", Target::Channel("trio".into()));
    let choose = |state: &mut State, mode: &str| {
        state.policy.message_policies.insert(
            key.clone(),
            crate::notifications::NotificationPolicy {
                mode: mode.into(),
                ..Default::default()
            },
        );
    };
    let mut sequence = 100;
    let mut send = |state: &mut State, mention: bool| {
        sequence += 1;
        let mut c = group_change(sequence, &format!("m{sequence}"), "friend", "trio");
        if mention {
            c.message.as_mut().unwrap().mentions = vec![origin.user_id.clone()];
        }
        assert!(state.message_page(
            &origin,
            generation,
            sequence - 1,
            &page(vec![c], sequence),
            now
        ));
        state.message_effects(now, 1000, false).len()
    };
    // Unchosen: every message of a private group.
    assert_eq!(send(&mut state, false), 1);
    choose(&mut state, "none");
    assert_eq!(send(&mut state, true), 0);
    choose(&mut state, "mentions");
    assert_eq!(send(&mut state, false), 0);
    assert_eq!(send(&mut state, true), 1);
    choose(&mut state, "all");
    assert_eq!(send(&mut state, false), 1);
}

#[test]
fn leaving_the_group_drops_alerts_that_were_still_waiting() {
    let (mut state, origin, generation, mut b, now) = group_fixture();
    let c = group_change(101, "late", "friend", "trio");
    assert!(state.message_page(&origin, generation, 100, &page(vec![c], 101), now));
    // The account is removed from the group before the alert is shown.
    b.conversations.clear();
    b.channels.clear();
    state.observe_with_cursor(&origin, generation, &b, now, 1000, None);
    assert!(state.message_effects(now, 1000, false).is_empty());
}

#[test]
fn a_group_member_who_is_not_a_friend_alerts_in_the_group_but_never_for_a_private_message() {
    let (mut state, origin, generation, mut b, now) = group_fixture();
    // Invited into the group by the owner; never this account's friend.
    b.members.push(User {
        id: "guest".into(),
        username: "Gus".into(),
        ..Default::default()
    });
    b.conversations[0].member_ids.push("guest".into());
    state.observe_with_cursor(&origin, generation, &b, now, 1000, None);
    let c = group_change(101, "g1", "guest", "trio");
    assert!(state.message_page(&origin, generation, 100, &page(vec![c], 101), now));
    let effects = state.message_effects(now, 1000, false);
    assert_eq!(effects.len(), 1);
    assert!(effects[0].notification.body.contains("Gus"));
    assert!(effects[0].notification.body.contains("Weekend"));
    // Blocking that person silences them, even though they are still listed.
    b.social.blocked.push("guest".into());
    state.observe_with_cursor(&origin, generation, &b, now, 1000, None);
    let c = group_change(102, "g2", "guest", "trio");
    assert!(state.message_page(&origin, generation, 101, &page(vec![c], 102), now));
    assert!(state.message_effects(now, 1001, false).is_empty());
}
