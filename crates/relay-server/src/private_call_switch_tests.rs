use super::*;
use crate::tests::{create_test_channel, new_invite, request, signup};
use serde_json::{json, Value};

struct Fixture {
    _data: tempfile::TempDir,
    state: AppState,
    app: Router,
    caller: Session,
    recipient: Session,
    call: PrivateCall,
    old: VoiceTicket,
    old_room: String,
}
impl Fixture {
    async fn new() -> Self {
        let data = tempfile::tempdir().unwrap();
        let state = AppState::open(data.path(), "switch-call", 0).unwrap();
        let app = router(state.clone());
        let caller = signup(&app, "Caller", "").await;
        let recipient = signup(&app, "Recipient", &new_invite(&app, &caller).await).await;
        for (actor, peer) in [(&caller, &recipient), (&recipient, &caller)] {
            assert_eq!(
                request(
                    &app,
                    "POST",
                    &format!("/api/friends/{}", peer.user.id),
                    Some(&actor.token),
                    Value::Null
                )
                .await
                .0,
                StatusCode::OK
            );
        }
        assert_eq!(request(&app,"PUT","/api/e2ee/device",Some(&recipient.token),json!({"device_id":"switch-device","identity":"ab".repeat(32),"key_package":STANDARD.encode("owned-public:switch-device")})).await.0,StatusCode::OK);
        let (status, call) = request(
            &app,
            "POST",
            "/api/private-calls",
            Some(&caller.token),
            json!({"peer_id":recipient.user.id}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let old_room = create_test_channel(&app, &caller, "voice", false).await;
        let (status, old) = request(
            &app,
            "POST",
            "/api/voice/join",
            Some(&recipient.token),
            json!({"channel_id":old_room,"device_id":"switch-device"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{old}");
        Self {
            _data: data,
            state,
            app,
            caller,
            recipient,
            call: serde_json::from_value(call).unwrap(),
            old: serde_json::from_value(old).unwrap(),
            old_room,
        }
    }
    fn body(&self) -> Value {
        json!({"instance_id":self.state.0.instance_id,"join":{"channel_id":self.call.channel_id,"device_id":"switch-device"},"replace_session_id":self.old.session_id,"state":{"muted":true,"deafened":true,"sharing":true,"camera":true}})
    }
    async fn answer(&self, body: Value) -> (StatusCode, Value) {
        request(
            &self.app,
            "POST",
            &format!("/api/private-calls/{}/join", self.call.id),
            Some(&self.recipient.token),
            body,
        )
        .await
    }
    fn kept(&self) {
        let seats = lock(&self.state.0.voice);
        let old = seats
            .get(&self.old.session_id)
            .expect("original admission retained");
        assert_eq!(old.channel_id, self.old_room);
        assert_eq!(old.key, STANDARD.decode(&self.old.key).unwrap().as_slice());
        drop(seats);
        assert_eq!(
            lock(&self.state.0.private_calls)[&self.call.id].state,
            "ringing"
        );
        assert_eq!(
            lock(&self.state.0.db)
                .query_row(
                    "SELECT COUNT(*) FROM private_call_recovery WHERE id=?1",
                    [&self.call.id],
                    |r| r.get::<_, u32>(0)
                )
                .unwrap(),
            0
        );
    }
}
#[tokio::test]
async fn atomic_answer_validates_before_replacing_exact_seat_and_preserves_mute_deafen() {
    let f = Fixture::new().await;
    let (status, body) = f.answer(f.body()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let result: PrivateCallAdmission = serde_json::from_value(body).unwrap();
    assert_eq!(result.call.id, f.call.id);
    assert_eq!(result.call.state, "accepted");
    {
        let seats = lock(&f.state.0.voice);
        assert!(!seats.contains_key(&f.old.session_id));
        let new = &seats[&result.ticket.session_id];
        assert_eq!(new.private_call_id.as_deref(), Some(f.call.id.as_str()));
        assert!(new.state.muted && new.state.deafened);
        assert!(!new.state.sharing && !new.state.camera);
    }
    let (status, again) = f.answer(f.body()).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "retry must use the same call identity: {again}"
    );
    assert_eq!(again["call"]["id"], f.call.id);
    assert_eq!(
        lock(&f.state.0.private_calls)[&f.call.id].expires_at,
        result.call.expires_at
    );
    // A delayed release of the replaced seat must not revoke the successor's
    // already issued video membership, even when the login token is unchanged.
    lock(&f.state.0.rtc_members).insert(
        f.recipient.user.id.clone(),
        crate::rtc::RtcMembership {
            channel_id: f.call.channel_id.clone(),
            room: "successor-room".into(),
            token_hash: token_hash(&f.recipient.token),
            can_publish: true,
        },
    );
    assert_eq!(
        request(
            &f.app,
            "POST",
            "/api/voice/leave-exact",
            Some(&f.recipient.token),
            json!({"instance_id":f.state.0.instance_id,"session_id":f.old.session_id,"channel_id":f.old_room,"device_id":"switch-device","open_revision":null})
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        lock(&f.state.0.voice).len(),
        1,
        "stale release keeps successor"
    );
    assert_eq!(
        lock(&f.state.0.rtc_members)[&f.recipient.user.id].room,
        "successor-room"
    );
}

#[tokio::test]
async fn exact_release_rejects_other_device_protection_or_instance_without_revoking_media() {
    let f = Fixture::new().await;
    assert_eq!(request(&f.app,"PUT","/api/e2ee/device",Some(&f.recipient.token),json!({"device_id":"another-valid-device","identity":"cd".repeat(32),"key_package":STANDARD.encode("owned-public:another-valid-device")})).await.0,StatusCode::OK);
    lock(&f.state.0.rtc_members).insert(
        f.recipient.user.id.clone(),
        crate::rtc::RtcMembership {
            channel_id: f.old_room.clone(),
            room: "current-room".into(),
            token_hash: token_hash(&f.recipient.token),
            can_publish: true,
        },
    );
    for failure in ["device", "protection", "instance"] {
        let mut body = json!({"instance_id":f.state.0.instance_id,"session_id":f.old.session_id,
            "channel_id":f.old_room,"device_id":"switch-device","open_revision":null});
        match failure {
            "device" => body["device_id"] = json!("another-valid-device"),
            "protection" => body["open_revision"] = json!(1),
            _ => body["instance_id"] = json!("previous-server-instance"),
        }
        assert_eq!(
            request(
                &f.app,
                "POST",
                "/api/voice/leave-exact",
                Some(&f.recipient.token),
                body
            )
            .await
            .0,
            StatusCode::NO_CONTENT
        );
        f.kept();
        assert_eq!(
            lock(&f.state.0.rtc_members)[&f.recipient.user.id].room,
            "current-room"
        );
    }
}
#[tokio::test]
async fn stale_seat_device_or_transaction_failure_does_not_accept_or_leave() {
    for failure in [
        "seat",
        "device",
        "other-device",
        "protection",
        "instance",
        "write",
    ] {
        let f = Fixture::new().await;
        let mut body = f.body();
        match failure {
            "seat" => body["replace_session_id"] = json!(f.old.session_id.wrapping_add(1)),
            "device" => body["join"]["device_id"] = json!("revoked-or-unknown"),
            "other-device" => {
                assert_eq!(request(&f.app,"PUT","/api/e2ee/device",Some(&f.recipient.token),json!({"device_id":"another-valid-device","identity":"cd".repeat(32),"key_package":STANDARD.encode("owned-public:another-valid-device")})).await.0,StatusCode::OK);
                body["join"]["device_id"] = json!("another-valid-device");
            }
            "protection" => body["join"]["open_revision"] = json!(1),
            "instance" => body["instance_id"] = json!("stale-instance"),
            _ => {
                lock(&f.state.0.db).execute_batch("CREATE TRIGGER fail_switch BEFORE INSERT ON private_call_recovery BEGIN SELECT RAISE(ABORT,'owned failure'); END;").unwrap();
            }
        }
        assert!(!f.answer(body).await.0.is_success(), "{failure}");
        f.kept();
    }
}
#[tokio::test]
async fn expired_cancelled_blocked_or_caller_moved_invitation_cannot_replace_seat() {
    for failure in ["expired", "cancelled", "blocked", "caller-moved"] {
        let f = Fixture::new().await;
        match failure {
            "expired" => {
                lock(&f.state.0.private_calls)
                    .get_mut(&f.call.id)
                    .unwrap()
                    .expires_at = now().saturating_sub(1)
            }
            "cancelled" => {
                lock(&f.state.0.private_calls)
                    .get_mut(&f.call.id)
                    .unwrap()
                    .state = "cancelled".into()
            }
            "blocked" => {
                lock(&f.state.0.db)
                    .execute(
                        "INSERT INTO user_blocks(owner,peer) VALUES(?1,?2)",
                        params![f.caller.user.id, f.recipient.user.id],
                    )
                    .unwrap();
            }
            _ => {
                assert_eq!(
                    request(
                        &f.app,
                        "POST",
                        "/api/voice/join",
                        Some(&f.caller.token),
                        json!({"channel_id":f.old_room})
                    )
                    .await
                    .0,
                    StatusCode::OK
                );
            }
        }
        assert!(!f.answer(f.body()).await.0.is_success(), "{failure}");
        assert!(
            lock(&f.state.0.voice).contains_key(&f.old.session_id),
            "{failure}"
        );
    }
}

#[tokio::test]
async fn unanswered_switch_cannot_replace_a_live_recovery_lease_but_expired_retention_is_idle() {
    for expired in [false, true] {
        let f = Fixture::new().await;
        // Represent a lost/restarted transport without deleting its durable
        // admission. Only the real resume capability may reclaim a live lease.
        lock(&f.state.0.voice).remove(&f.old.session_id);
        if expired {
            lock(&f.state.0.db)
                .execute(
                    "UPDATE voice_resume_leases SET expires=?1 WHERE user_id=?2",
                    params![now().saturating_sub(1), f.recipient.user.id],
                )
                .unwrap();
        }
        let mut body = f.body();
        body["replace_session_id"] = Value::Null;
        let (status, reply) = f.answer(body).await;
        assert_eq!(status.is_success(), expired, "expired={expired}: {reply}");
        if !expired {
            assert_eq!(lock(&f.state.0.private_calls)[&f.call.id].state, "ringing");
            assert!(lock(&f.state.0.voice).is_empty());
            assert_eq!(
                lock(&f.state.0.db)
                    .query_row(
                        "SELECT COUNT(*) FROM voice_resume_leases WHERE user_id=?1",
                        [&f.recipient.user.id],
                        |r| r.get::<_, u32>(0)
                    )
                    .unwrap(),
                1
            );
            // Supplying the vanished session ID cannot bypass lease recovery.
            assert!(!f.answer(f.body()).await.0.is_success());
        }
    }
}

#[tokio::test]
async fn expired_runtime_seat_and_lease_do_not_block_an_idle_answer() {
    let f = Fixture::new().await;
    lock(&f.state.0.voice)
        .get_mut(&f.old.session_id)
        .unwrap()
        .expires = now().saturating_sub(1);
    lock(&f.state.0.db)
        .execute(
            "UPDATE voice_resume_leases SET expires=?1 WHERE user_id=?2",
            params![now().saturating_sub(1), f.recipient.user.id],
        )
        .unwrap();
    let mut body = f.body();
    body["replace_session_id"] = Value::Null;
    let (status, reply) = f.answer(body).await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    assert!(!lock(&f.state.0.voice).contains_key(&f.old.session_id));
}
