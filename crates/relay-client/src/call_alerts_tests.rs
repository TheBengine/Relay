use super::*;

#[test]
fn call_notice_authority_ends_with_the_exact_call_and_cannot_be_revived() {
    for terminal in [
        "accepted",
        "declined",
        "cancelled",
        "ended",
        "missing",
        "blocked",
    ] {
        let (mut state, origin, generation, mut b, now) = fixture(true);
        state.observe(&origin, generation, &b, now, 100);
        let notice = state.step(now).notifications.pop().unwrap();
        let guard = notice.guard.unwrap();
        assert!(guard.valid());
        let stale = b.clone();
        match terminal {
            "missing" => b.social.calls.clear(),
            "blocked" => b.social.blocked.push("friend".into()),
            value => b.social.calls[0].state = value.into(),
        }
        state.observe(&origin, generation, &b, now + Duration::from_secs(1), 101);
        let effect = state.step(now + Duration::from_secs(1));
        assert!(!guard.valid(), "{terminal}");
        assert!(
            effect.guards_changed && effect.notifications.is_empty(),
            "{terminal}"
        );
        state.observe(
            &origin,
            generation,
            &stale,
            now + Duration::from_secs(2),
            102,
        );
        assert!(state
            .step(now + Duration::from_secs(2))
            .notifications
            .is_empty());
        assert!(!guard.valid());
    }
}

#[test]
fn call_notice_expiry_revokes_without_a_new_snapshot_or_painted_frame() {
    let (mut state, origin, generation, b, now) = fixture(true);
    state.observe(&origin, generation, &b, now, 100);
    let guard = state.step(now).notifications.pop().unwrap().guard.unwrap();
    let expired = state.step(now + Duration::from_secs(30));
    assert!(expired.guards_changed);
    assert!(!guard.valid());
    assert!(expired.notifications.is_empty() && expired.sound.is_none());
    assert!(!state.step(now + Duration::from_secs(31)).guards_changed);
}

#[test]
fn call_notice_same_origin_rebind_revokes_old_authority_without_replaying_it() {
    let (mut state, origin, generation, mut b, now) = fixture(true);
    state.observe(&origin, generation, &b, now, 100);
    let old = state.step(now).notifications.pop().unwrap().guard.unwrap();
    let current = state.bind(origin.clone()).unwrap();
    assert_ne!(generation, current);
    assert!(!old.valid());
    b.social.calls[0].id = "9cfe560c-361b-487e-882c-0645f22e9684".into();
    state.observe(&origin, current, &b, now + Duration::from_secs(1), 101);
    let effect = state.step(now + Duration::from_secs(1));
    assert_eq!(effect.notifications.len(), 1);
    let fresh = effect.notifications[0].guard.as_ref().unwrap();
    assert!(fresh.valid());
    b.social.calls.clear();
    state.observe(&origin, generation, &b, now + Duration::from_secs(2), 102);
    state.step(now + Duration::from_secs(2));
    assert!(
        fresh.valid(),
        "a stale subscriber cannot revoke the new call"
    );
}

#[test]
fn call_notice_terminal_state_preserves_another_call_from_the_same_peer() {
    let (mut state, origin, generation, mut b, now) = fixture(true);
    let mut second = b.social.calls[0].clone();
    second.id = "9cfe560c-361b-487e-882c-0645f22e9684".into();
    b.social.calls.push(second);
    state.observe(&origin, generation, &b, now, 100);
    let notices = state.step(now).notifications;
    assert_eq!(notices.len(), 2);
    b.social.calls[0].state = "cancelled".into();
    state.observe(&origin, generation, &b, now + Duration::from_secs(1), 101);
    state.step(now + Duration::from_secs(1));
    assert!(!notices[0].guard.as_ref().unwrap().valid());
    assert!(notices[1].guard.as_ref().unwrap().valid());
}

#[test]
fn call_notice_privacy_and_silence_revoke_already_queued_authority() {
    for operation in [
        "previews",
        "notifications",
        "disabled",
        "silence",
        "forget",
        "stop",
    ] {
        let (state, origin, _, mut b, _) = fixture(true);
        b.social.calls[0].expires_at = unix_now() + 40;
        let alerts = Alerts::new(Notifications::new(eframe::egui::Context::default()), None);
        alerts.configure(state.policy);
        let generation = alerts.bind(origin.clone()).unwrap();
        alerts.observe(&origin, generation, &b);
        let guard = alerts.0.lock().unwrap().state.accounts[0].rings[0]
            .notification_guard
            .clone()
            .unwrap();
        assert!(guard.valid());
        match operation {
            "silence" => alerts.silence(&origin, &b.social.calls[0].id),
            "forget" => alerts.forget_url(&origin.url),
            "stop" => alerts.stop(),
            _ => {
                let mut policy = alerts.0.lock().unwrap().state.policy.clone();
                match operation {
                    "previews" => policy.previews = false,
                    "notifications" => policy.notifications = false,
                    "disabled" => policy.disabled = true,
                    _ => unreachable!(),
                }
                alerts.configure(policy);
            }
        }
        assert!(!guard.valid(), "{operation}");
        alerts.stop();
    }
}

#[test]
fn call_notice_dnd_revokes_queued_authority_without_restarting_delivery() {
    let (mut state, origin, generation, mut b, now) = fixture(true);
    state.observe(&origin, generation, &b, now, 100);
    let guard = state.step(now).notifications.pop().unwrap().guard.unwrap();
    b.presence.push(relay_protocol::UserPresence {
        user_id: "self".into(),
        status: "dnd".into(),
        ..Default::default()
    });
    state.observe(&origin, generation, &b, now + Duration::from_secs(1), 101);
    let quiet = state.step(now + Duration::from_secs(1));
    assert!(quiet.guards_changed && !guard.valid());
    assert!(quiet.notifications.is_empty() && quiet.sound.is_none());
    b.presence.clear();
    state.observe(&origin, generation, &b, now + Duration::from_secs(2), 102);
    assert!(state
        .step(now + Duration::from_secs(2))
        .notifications
        .is_empty());
}
use relay_protocol::{PrivateCall, User};

fn fixture(incoming: bool) -> (State, CommandOrigin, u64, Bootstrap, Instant) {
    let origin = CommandOrigin {
        url: "https://accounts.invalid/people".into(),
        server_id: "owned-friends".into(),
        user_id: "self".into(),
    };
    let mut state = State::default();
    state.policy.notifications = true;
    state.policy.previews = true;
    state.policy.audio.sounds_enabled = true;
    state.policy.audio.sound_volume = 0.5;
    state.policy.active = Some(origin.clone());
    let generation = state.bind(origin.clone()).unwrap();
    let mut b = Bootstrap {
        server_id: origin.server_id.clone(),
        user: User {
            id: "self".into(),
            ..Default::default()
        },
        ..Default::default()
    };
    b.members.push(User {
        id: "friend".into(),
        username: "Alex".into(),
        ..Default::default()
    });
    b.social.friends.push("friend".into());
    b.social.calls.push(PrivateCall {
        id: "063bcf24-3b22-4e6c-9898-24204b58f450".into(),
        channel_id: "pair-room".into(),
        caller_id: if incoming { "friend" } else { "self" }.into(),
        recipient_id: if incoming { "self" } else { "friend" }.into(),
        state: "ringing".into(),
        expires_at: 130,
    });
    (state, origin, generation, b, Instant::now())
}

#[test]
fn incoming_call_alert_is_once_across_foreground_background_and_duplicate_snapshots() {
    let (mut state, origin, generation, b, now) = fixture(true);
    state.observe(&origin, generation, &b, now, 100);
    let first = state.step(now);
    assert_eq!(first.notifications.len(), 1);
    assert_eq!(first.notifications[0].origin, origin);
    assert_eq!(
        first.notifications[0].target,
        Target::Direct("friend".into())
    );
    assert_eq!(first.sound, Some(AudioCue::Ring));
    // Paint/visibility is deliberately absent from this shared decision path.
    state.policy.active = None;
    state.observe(&origin, generation, &b, now + Duration::from_secs(1), 101);
    let second = state.step(now + Duration::from_secs(1));
    assert!(second.notifications.is_empty());
    assert!(second.sound.is_none());
    let next = state.step(now + Duration::from_secs(3));
    assert!(next.notifications.is_empty());
    assert_eq!(next.sound, Some(AudioCue::Ring));
}

#[test]
fn ringing_cadence_stops_on_every_terminal_state_expiry_revocation_and_shutdown() {
    for terminal in [
        "accepted",
        "declined",
        "cancelled",
        "missed",
        "ended",
        "expired",
        "blocked",
        "removed",
        "shutdown",
    ] {
        let (mut state, origin, generation, mut b, now) = fixture(true);
        state.observe(&origin, generation, &b, now, 100);
        assert_eq!(state.step(now).sound, Some(AudioCue::Ring));
        let at = if terminal == "expired" {
            now + Duration::from_secs(30)
        } else {
            now + Duration::from_secs(1)
        };
        match terminal {
            "expired" => {}
            "blocked" => b.social.blocked.push("friend".into()),
            "removed" => b.social.calls.clear(),
            "shutdown" => state.stopped = true,
            state => b.social.calls[0].state = state.into(),
        }
        if terminal != "expired" {
            state.observe(&origin, generation, &b, at, 101);
        }
        let stopped = state.step(at);
        assert!(stopped.changed, "{terminal}");
        assert!(stopped.sound.is_none(), "{terminal}");
        assert!(
            state.step(at + Duration::from_secs(3)).sound.is_none(),
            "{terminal}"
        );
        if terminal == "accepted" {
            b.social.calls[0].state = "ringing".into();
            state.observe(&origin, generation, &b, at + Duration::from_secs(3), 104);
            assert!(state
                .step(at + Duration::from_secs(3))
                .notifications
                .is_empty());
            assert!(state.step(at + Duration::from_secs(6)).sound.is_none());
        }
    }
}

#[test]
fn dnd_deafen_toast_privacy_and_sound_preferences_have_separate_effects() {
    for option in [
        "dnd",
        "deafened",
        "notifications-off",
        "sounds-off",
        "disabled",
        "no-previews",
    ] {
        let (mut state, origin, generation, b, now) = fixture(true);
        state.observe(&origin, generation, &b, now, 100);
        match option {
            "dnd" => state.accounts[0].quiet = true,
            "deafened" => state.policy.deafened = true,
            "notifications-off" => state.policy.notifications = false,
            "sounds-off" => state.policy.audio.sounds_enabled = false,
            "disabled" => state.policy.disabled = true,
            _ => state.policy.previews = false,
        }
        let effect = state.step(now);
        assert_eq!(
            effect.notifications.is_empty(),
            matches!(option, "dnd" | "notifications-off" | "disabled"),
            "{option}"
        );
        assert_eq!(
            effect.sound.is_none(),
            matches!(option, "dnd" | "deafened" | "sounds-off" | "disabled"),
            "{option}"
        );
        if option == "no-previews" {
            assert!(!effect.notifications[0].body.contains("Alex"));
        }
    }
}

#[test]
fn origin_account_and_watcher_generation_cannot_share_or_replay_call_alerts() {
    let (mut state, origin, generation, b, now) = fixture(true);
    state.observe(&origin, generation, &b, now, 100);
    assert_eq!(state.step(now).notifications.len(), 1);
    // Same call ID on another authenticated service is distinct.
    let other = CommandOrigin {
        url: "https://other.invalid/people".into(),
        ..origin.clone()
    };
    let other_generation = state.bind(other.clone()).unwrap();
    state.observe(&other, other_generation, &b, now, 100);
    assert_eq!(state.step(now).notifications.len(), 1);
    // Replacing the user at the same URL invalidates the old watch and identity.
    let new = CommandOrigin {
        user_id: "replacement".into(),
        ..origin.clone()
    };
    let new_generation = state.bind(new.clone()).unwrap();
    state.observe(&origin, generation, &b, now, 100);
    state.observe(&new, new_generation, &b, now, 100);
    assert!(state.step(now).notifications.is_empty());
    assert!(!state.accounts.iter().any(|a| a.origin == origin));
}

#[test]
fn outgoing_ringback_yields_to_incoming_and_stops_when_the_selected_account_changes() {
    let (mut state, origin, generation, b, now) = fixture(false);
    state.observe(&origin, generation, &b, now, 100);
    let first = state.step(now);
    assert!(first.notifications.is_empty());
    assert_eq!(first.sound, Some(AudioCue::Ringback));
    state.policy.active = None;
    assert!(state.step(now + Duration::from_secs(1)).changed);
    assert!(state.step(now + Duration::from_secs(4)).sound.is_none());
    state.policy.active = Some(origin.clone());
    let mut incoming = b;
    incoming.social.calls[0].caller_id = "friend".into();
    incoming.social.calls[0].recipient_id = "self".into();
    incoming.social.calls[0].id = uuid::Uuid::new_v4().to_string();
    state.observe(
        &origin,
        generation,
        &incoming,
        now + Duration::from_secs(5),
        105,
    );
    assert_eq!(
        state.step(now + Duration::from_secs(5)).sound,
        Some(AudioCue::Ring)
    );
}

#[test]
fn generated_ring_and_ringback_are_distinct_enveloped_bounded_and_quiet() {
    let ring = crate::sound_cues::samples(AudioCue::Ring, 1.0);
    let back = crate::sound_cues::samples(AudioCue::Ringback, 1.0);
    assert_ne!(ring, back);
    assert_eq!(ring.len(), 19200);
    for samples in [&ring, &back] {
        assert!(samples.iter().all(|v| v.is_finite() && v.abs() <= 0.15));
        assert_eq!(samples[0], 0.0);
        assert_eq!(*samples.last().unwrap(), 0.0);
    }
    let peak = |values: &[f32]| values.iter().fold(0.0_f32, |a, b| a.max(b.abs()));
    assert!(peak(&back) < peak(&ring) * 0.4);
    assert!(crate::sound_cues::samples(AudioCue::Ring, f32::NAN)
        .iter()
        .all(|v| *v == 0.0));
}

#[test]
fn logout_cancels_its_authenticated_source_and_late_snapshots_cannot_restart_it() {
    let (_, origin, _, mut bootstrap, _) = fixture(true);
    bootstrap.social.calls[0].expires_at = unix_now() + 40;
    let alerts = Alerts::new(Notifications::new(eframe::egui::Context::default()), None);
    let generation = alerts.bind(origin.clone()).unwrap();
    alerts.observe(&origin, generation, &bootstrap);
    assert_eq!(alerts.active_rings(), 1);
    alerts.forget_url(&origin.url);
    assert_eq!(alerts.active_rings(), 0);
    alerts.observe(&origin, generation, &bootstrap);
    assert_eq!(alerts.active_rings(), 0);
    let current = alerts.bind(origin.clone()).unwrap();
    alerts.observe(&origin, current, &bootstrap);
    assert_eq!(alerts.active_rings(), 1);
    alerts.remove(&origin, generation);
    assert_eq!(
        alerts.active_rings(),
        1,
        "old subscriber cannot revoke replacement"
    );
    alerts.stop();
    assert_eq!(alerts.active_rings(), 0);
    assert!(alerts.bind(origin).is_none());
}

#[test]
fn server_call_clock_ignores_positive_and_negative_pc_offset_at_first_receipt() {
    for local_unix in [40, 160] {
        let (mut state, origin, generation, mut bootstrap, received) = fixture(true);
        bootstrap.social.server_time = 100;
        state.observe(&origin, generation, &bootstrap, received, local_unix);
        let clock = state.accounts[0].reading(received, local_unix);
        assert_eq!(clock.unix_seconds, 100);
        assert_eq!(clock.server_sample_age, Some(Duration::ZERO));
        assert!(state.accounts[0].ringing(&bootstrap.social.calls[0].id, received));
        assert_eq!(state.step(received).sound, Some(AudioCue::Ring));
    }
}

#[test]
fn server_call_clock_uses_elapsed_time_after_pc_jumps_and_reports_sample_age() {
    let (mut state, origin, generation, mut bootstrap, received) = fixture(true);
    bootstrap.social.server_time = 100;
    state.observe(&origin, generation, &bootstrap, received, 100);
    for local_unix in [0, 10000] {
        let at = received + Duration::from_secs(10);
        let clock = state.accounts[0].reading(at, local_unix);
        assert_eq!(clock.unix_seconds, 110);
        assert_eq!(clock.server_sample_age, Some(Duration::from_secs(10)));
        assert!(state.accounts[0].ringing(&bootstrap.social.calls[0].id, at));
    }
    // A later legacy reply does not pretend to refresh an existing server sample.
    bootstrap.social.server_time = 0;
    state.observe(
        &origin,
        generation,
        &bootstrap,
        received + Duration::from_secs(11),
        0,
    );
    assert_eq!(
        state.accounts[0]
            .reading(received + Duration::from_secs(12), 0)
            .server_sample_age,
        Some(Duration::from_secs(12))
    );
    assert!(!state.accounts[0].ringing(
        &bootstrap.social.calls[0].id,
        received + Duration::from_secs(30)
    ));
    assert!(state
        .step(received + Duration::from_secs(30))
        .sound
        .is_none());
}

#[test]
fn repeated_server_samples_cannot_renew_original_ring_or_lose_fractional_elapsed_time() {
    let (mut state, origin, generation, mut bootstrap, received) = fixture(true);
    bootstrap.social.server_time = 100;
    state.observe(&origin, generation, &bootstrap, received, 160);
    let deadline = state.accounts[0].rings[0].deadline;
    for millis in [1900, 3800, 5700, 29900, 31000] {
        // Even a server returning the same old second must not wind time back.
        let at = received + Duration::from_millis(millis);
        state.observe(&origin, generation, &bootstrap, at, 40);
        assert_eq!(
            state.accounts[0].reading(at, 40).unix_seconds,
            100 + millis / 1000
        );
        assert_eq!(
            state.accounts[0].reading(at, 40).server_sample_age,
            Some(Duration::from_millis(millis))
        );
        assert!(state.accounts[0]
            .rings
            .iter()
            .all(|r| r.deadline == deadline));
    }
    assert!(state.accounts[0].rings.is_empty());
    assert!(state
        .step(received + Duration::from_secs(31))
        .sound
        .is_none());
}

#[test]
fn call_clock_rejects_wrong_identity_old_receipts_and_old_generations() {
    let (mut state, origin, generation, mut bootstrap, received) = fixture(true);
    bootstrap.social.server_time = 100;
    state.observe(&origin, generation, &bootstrap, received, 160);
    for wrong_user in [false, true] {
        let mut wrong = bootstrap.clone();
        if wrong_user {
            wrong.user.id = "other-user".into();
        } else {
            wrong.server_id = "other-server".into();
        }
        wrong.social.server_time = 999;
        state.observe(
            &origin,
            generation,
            &wrong,
            received + Duration::from_secs(1),
            999,
        );
        assert_eq!(
            state.accounts[0]
                .reading(received + Duration::from_secs(1), 999)
                .unix_seconds,
            101
        );
    }
    state.observe(
        &origin,
        generation,
        &bootstrap,
        received + Duration::from_secs(3),
        999,
    );
    bootstrap.social.server_time = 999;
    state.observe(
        &origin,
        generation,
        &bootstrap,
        received + Duration::from_secs(2),
        999,
    );
    assert_eq!(
        state.accounts[0]
            .reading(received + Duration::from_secs(3), 999)
            .unix_seconds,
        103
    );
    let current = state.bind(origin.clone()).unwrap();
    state.observe(
        &origin,
        generation,
        &bootstrap,
        received + Duration::from_secs(4),
        999,
    );
    assert!(state.accounts[0].clock.is_none());
    assert!(state.accounts[0].received.is_none());
    bootstrap.social.server_time = 200;
    state.observe(
        &origin,
        current,
        &bootstrap,
        received + Duration::from_secs(4),
        0,
    );
    assert_eq!(
        state.accounts[0]
            .reading(received + Duration::from_secs(5), 0)
            .unix_seconds,
        201
    );
    let replacement = CommandOrigin {
        user_id: "replacement".into(),
        ..origin.clone()
    };
    state.bind(replacement).unwrap();
    assert!(state.accounts[0].clock.is_none());
    assert!(state.accounts[0].rings.is_empty());
    state
        .bind(CommandOrigin {
            server_id: "replacement-server".into(),
            ..origin.clone()
        })
        .unwrap();
    assert!(state.accounts[0].clock.is_none());
    let other = CommandOrigin {
        url: "https://other.invalid/people".into(),
        ..origin
    };
    state.bind(other).unwrap();
    assert!(state.accounts.iter().all(|a| a.clock.is_none()));
}

#[test]
fn legacy_social_reply_has_zero_time_and_explicit_local_clock_fallback() {
    let social: relay_protocol::SocialState = serde_json::from_value(serde_json::json!({
        "friends":[], "incoming":[], "outgoing":[], "blocked":[]
    }))
    .unwrap();
    assert_eq!(social.server_time, 0);
    let (mut state, origin, generation, bootstrap, received) = fixture(true);
    state.observe(&origin, generation, &bootstrap, received, 100);
    let clock = state.accounts[0].reading(received + Duration::from_secs(10), 120);
    assert_eq!(clock.unix_seconds, 120);
    assert_eq!(clock.server_sample_age, None);
}

#[test]
fn foreground_and_background_share_original_expiry_after_a_hidden_window_delay() {
    let (mut state, origin, generation, mut bootstrap, received) = fixture(true);
    bootstrap.social.server_time = 100;
    state.observe(&origin, generation, &bootstrap, received, 160);
    let id = bootstrap.social.calls[0].id.clone();
    // No observe/paint operation anchors a new clock when the UI resumes.
    for elapsed in [20, 30, 90] {
        let at = received + Duration::from_secs(elapsed);
        let foreground = state.accounts[0].ringing(&id, at);
        let background = state.step(at).sound == Some(AudioCue::Ring);
        assert_eq!(foreground, elapsed < 30);
        assert_eq!(background, foreground);
        assert_eq!(state.accounts[0].reading(at, 0).unix_seconds, 100 + elapsed);
    }
}

#[test]
fn pending_call_intent_deadline_does_not_move_with_later_pc_clock_changes() {
    let (mut state, origin, generation, mut bootstrap, received) = fixture(true);
    bootstrap.social.server_time = 100;
    state.observe(&origin, generation, &bootstrap, received, 160);
    let intent_deadline = state.accounts[0]
        .reading(received, 160)
        .deadline(160, received)
        .unwrap();
    assert_eq!(intent_deadline, received + Duration::from_secs(60));
    for local_unix in [0, 10000] {
        let at = received + Duration::from_secs(20);
        assert_eq!(
            state.accounts[0].reading(at, local_unix).deadline(160, at),
            Some(intent_deadline)
        );
    }
}

#[test]
fn forward_server_sample_shortens_foreground_and_background_expiry_without_renewal() {
    let (mut state, origin, generation, mut bootstrap, received) = fixture(true);
    bootstrap.social.server_time = 100;
    state.observe(&origin, generation, &bootstrap, received, 40);
    bootstrap.social.server_time = 120;
    state.observe(
        &origin,
        generation,
        &bootstrap,
        received + Duration::from_secs(10),
        0,
    );
    assert_eq!(
        state.accounts[0].rings[0].deadline,
        received + Duration::from_secs(20)
    );
    let id = &bootstrap.social.calls[0].id;
    assert!(state.accounts[0].ringing(id, received + Duration::from_secs(19)));
    assert_eq!(
        state.step(received + Duration::from_secs(19)).sound,
        Some(AudioCue::Ring)
    );
    assert!(!state.accounts[0].ringing(id, received + Duration::from_secs(20)));
    assert!(state
        .step(received + Duration::from_secs(20))
        .sound
        .is_none());
}

#[test]
fn first_seen_call_uses_fractional_estimate_without_a_fresh_painted_anchor() {
    let (mut state, origin, generation, mut bootstrap, received) = fixture(true);
    bootstrap.social.server_time = 100;
    let call = bootstrap.social.calls.pop().unwrap();
    state.observe(&origin, generation, &bootstrap, received, 160);
    bootstrap.social.calls.push(call);
    // Whole-second sample is older than the monotonic estimate; keep its age.
    bootstrap.social.server_time = 110;
    state.observe(
        &origin,
        generation,
        &bootstrap,
        received + Duration::from_millis(10500),
        40,
    );
    assert_eq!(
        state.accounts[0].rings[0].deadline,
        received + Duration::from_secs(30)
    );
    assert_eq!(
        state.accounts[0]
            .reading(received + Duration::from_millis(10500), 0)
            .server_sample_age,
        Some(Duration::from_millis(10500))
    );
}

#[test]
fn silencing_live_call_keeps_retry_timing_but_suppresses_cues_until_real_terminal_state() {
    let (_, origin, _, mut bootstrap, _) = fixture(true);
    bootstrap.social.server_time = 100;
    let id = bootstrap.social.calls[0].id.clone();
    let alerts = Alerts::new(Notifications::new(eframe::egui::Context::default()), None);
    let generation = alerts.bind(origin.clone()).unwrap();
    alerts.observe(&origin, generation, &bootstrap);
    assert!(alerts.ringing(&origin, &id));
    let deadline = alerts.0.lock().unwrap().state.accounts[0].rings[0].deadline;
    alerts.silence(&origin, &id);
    assert!(
        alerts.ringing(&origin, &id),
        "silence is not a missed/terminal call"
    );
    assert_eq!(
        alerts.0.lock().unwrap().state.accounts[0].rings[0].deadline,
        deadline
    );
    // A failed queue/HTTP action makes no server transition. The still-live
    // response remains actionable on retry, without restarting ring/toast cues.
    alerts.observe(&origin, generation, &bootstrap);
    assert!(alerts.ringing(&origin, &id));
    assert!(alerts.clock(&origin).unwrap().unix_seconds < bootstrap.social.calls[0].expires_at);
    {
        let mut inner = alerts.0.lock().unwrap();
        inner.state.policy.notifications = true;
        let effect = inner.state.step(Instant::now() + Duration::from_secs(6));
        assert!(effect.notifications.is_empty());
        assert!(effect.sound.is_none());
        assert!(!inner.state.accounts[0].rings[0].retired);
    }
    let wrong = CommandOrigin {
        user_id: "other-user".into(),
        ..origin.clone()
    };
    assert!(!alerts.ringing(&wrong, &id));
    assert!(alerts.clock(&wrong).is_none());
    bootstrap.social.calls[0].state = "declined".into();
    alerts.observe(&origin, generation, &bootstrap);
    assert!(!alerts.ringing(&origin, &id));
    bootstrap.social.calls[0].state = "ringing".into();
    alerts.observe(&origin, generation, &bootstrap);
    assert!(
        !alerts.ringing(&origin, &id),
        "terminal observations remain latched"
    );
    let current = alerts.bind(origin.clone()).unwrap();
    assert_ne!(current, generation);
    alerts.observe(&origin, generation, &bootstrap);
    assert!(
        alerts.clock(&origin).is_none(),
        "old watcher cannot authorize a new clock"
    );
    assert!(!alerts.ringing(&origin, &id));
}

#[test]
fn pre_observation_silence_requires_authenticated_live_call_and_latches_terminal_snapshots() {
    for first in ["ringing", "missing", "declined", "blocked", "expired"] {
        let (_, origin, _, mut bootstrap, _) = fixture(true);
        bootstrap.social.server_time = 100;
        let id = bootstrap.social.calls[0].id.clone();
        let live = bootstrap.clone();
        let alerts = Alerts::new(Notifications::new(eframe::egui::Context::default()), None);
        let generation = alerts.bind(origin.clone()).unwrap();
        alerts.silence(&origin, &id);
        assert!(!alerts.ringing(&origin, &id));
        assert!(alerts.clock(&origin).is_none());
        match first {
            "missing" => bootstrap.social.calls.clear(),
            "blocked" => bootstrap.social.blocked.push("friend".into()),
            "expired" => bootstrap.social.server_time = 131,
            state => bootstrap.social.calls[0].state = state.into(),
        }
        alerts.observe(&origin, generation, &bootstrap);
        assert_eq!(alerts.ringing(&origin, &id), first == "ringing", "{first}");
        alerts.observe(&origin, generation, &live);
        assert_eq!(
            alerts.ringing(&origin, &id),
            first == "ringing",
            "{first}: stale live snapshot"
        );
        let mut inner = alerts.0.lock().unwrap();
        inner.state.policy.notifications = true;
        let effect = inner.state.step(Instant::now() + Duration::from_secs(6));
        assert!(effect.notifications.is_empty(), "{first}");
        assert!(effect.sound.is_none(), "{first}");
    }
}
