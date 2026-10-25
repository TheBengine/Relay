use super::*;
use crate::tests::{create_test_channel, new_invite, request, signup};
use serde_json::{json, Value};

struct Fixture {
    data: tempfile::TempDir,
    state: AppState,
    app: Router,
    a: Session,
    b: Session,
    call: PrivateCall,
}

impl Fixture {
    async fn new() -> Self {
        let data = tempfile::tempdir().unwrap();
        let state = AppState::open(data.path(), "private-call-recovery", 0).unwrap();
        let app = router(state.clone());
        let a = signup(&app, "Caller", "").await;
        let b = signup(&app, "Recipient", &new_invite(&app, &a).await).await;
        for (session, peer, device) in [(&a, &b, "recovery-a"), (&b, &a, "recovery-b")] {
            assert_eq!(
                request(
                    &app,
                    "POST",
                    &format!("/api/friends/{}", peer.user.id),
                    Some(&session.token),
                    Value::Null
                )
                .await
                .0,
                StatusCode::OK
            );
            assert_eq!(request(&app, "PUT", "/api/e2ee/device", Some(&session.token), json!({
                "device_id":device,"identity":"ab".repeat(32),"key_package":STANDARD.encode(format!("owned-public:{device}"))
            })).await.0, StatusCode::OK);
        }
        let (status, call) = request(
            &app,
            "POST",
            "/api/private-calls",
            Some(&a.token),
            json!({"peer_id":b.user.id}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{call}");
        Self {
            data,
            state,
            app,
            a,
            b,
            call: serde_json::from_value(call).unwrap(),
        }
    }

    fn restart(&mut self) {
        let old = self.state.0.instance_id.clone();
        self.state = AppState::open(self.data.path(), "private-call-recovery", 0).unwrap();
        self.app = router(self.state.clone());
        assert_ne!(old, self.state.0.instance_id);
        assert!(lock(&self.state.0.private_calls).is_empty());
    }

    async fn action(&self, session: &Session, call: &str, action: &str) -> (StatusCode, Value) {
        request(
            &self.app,
            "POST",
            &format!("/api/private-calls/{call}"),
            Some(&session.token),
            json!({"action":action}),
        )
        .await
    }

    async fn accept(&self) {
        assert_eq!(
            self.action(&self.b, &self.call.id, "accept").await.0,
            StatusCode::OK
        );
    }

    async fn join(&self, session: &Session, device: &str) -> VoiceTicket {
        let (status, body) = request(
            &self.app,
            "POST",
            "/api/voice/join",
            Some(&session.token),
            json!({"channel_id":self.call.channel_id,"device_id":device}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let ticket: VoiceTicket = serde_json::from_value(body).unwrap();
        assert_eq!(
            lock(&self.state.0.voice)[&ticket.session_id]
                .private_call_id
                .as_deref(),
            Some(self.call.id.as_str())
        );
        ticket
    }

    async fn admitted(&self) -> (VoiceTicket, VoiceTicket) {
        self.accept().await;
        (
            self.join(&self.a, "recovery-a").await,
            self.join(&self.b, "recovery-b").await,
        )
    }

    async fn resume(
        &self,
        session: &Session,
        device: &str,
        ticket: &VoiceTicket,
    ) -> (StatusCode, Value) {
        request(
            &self.app,
            "POST",
            "/api/voice/resume",
            Some(&session.token),
            json!({
                "channel_id":self.call.channel_id,"device_id":device,
                "resume_token":ticket.resume.as_ref().unwrap().token
            }),
        )
        .await
    }

    async fn visible(&self, session: &Session) -> PrivateCall {
        let (status, social) = request(
            &self.app,
            "GET",
            "/api/social",
            Some(&session.token),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{social}");
        serde_json::from_value(
            social["calls"]
                .as_array()
                .unwrap()
                .iter()
                .find(|c| c["id"] == self.call.id)
                .unwrap()
                .clone(),
        )
        .unwrap()
    }

    fn leases(&self) -> u32 {
        lock(&self.state.0.db)
            .query_row(
                "SELECT COUNT(*) FROM voice_resume_leases WHERE private_call_id=?1",
                [&self.call.id],
                |r| r.get(0),
            )
            .unwrap()
    }

    fn durable_state(&self) -> String {
        lock(&self.state.0.db)
            .query_row(
                "SELECT state FROM private_call_recovery WHERE id=?1",
                [&self.call.id],
                |r| r.get(0),
            )
            .unwrap()
    }
}

#[tokio::test]
async fn restart_resumes_both_peers_with_original_id_and_hangup_invalidates_both() {
    let mut f = Fixture::new().await;
    let (a, b) = f.admitted().await;
    let original = f.visible(&f.a).await;
    f.restart();
    for peer in [&f.a, &f.b] {
        assert_eq!(f.visible(peer).await, original);
    }
    let (status, repeated) = f.action(&f.b, &f.call.id, "accept").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        repeated["expires_at"],
        json!(original.expires_at),
        "acceptance retry does not renew the original join deadline"
    );
    for (session, device, ticket) in [(&f.a, "recovery-a", &a), (&f.b, "recovery-b", &b)] {
        let (status, value) = f.resume(session, device, ticket).await;
        assert_eq!(status, StatusCode::OK, "{value}");
        let ticket: VoiceTicket = serde_json::from_value(value).unwrap();
        assert_eq!(
            lock(&f.state.0.voice)[&ticket.session_id]
                .private_call_id
                .as_deref(),
            Some(f.call.id.as_str())
        );
    }
    assert_eq!(f.action(&f.a, &f.call.id, "end").await.0, StatusCode::OK);
    assert_eq!(f.durable_state(), "ended");
    assert_eq!(f.leases(), 0);
    assert!(lock(&f.state.0.voice).is_empty());
    f.restart();
    for peer in [&f.a, &f.b] {
        assert_eq!(f.visible(peer).await.state, "ended");
    }
    assert_eq!(
        f.resume(&f.b, "recovery-b", &b).await.0,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn offline_leave_and_committed_end_survive_restart_without_pair_inference() {
    for end_endpoint in [false, true] {
        let mut f = Fixture::new().await;
        let (a, b) = f.admitted().await;
        f.restart();
        let status = if end_endpoint {
            f.action(&f.b, &f.call.id, "end").await.0
        } else {
            request(
                &f.app,
                "POST",
                "/api/voice/leave",
                Some(&f.a.token),
                Value::Null,
            )
            .await
            .0
        };
        assert!(status.is_success());
        assert_eq!(f.durable_state(), "ended");
        assert_eq!(f.leases(), 0);
        f.restart();
        assert_eq!(f.visible(&f.a).await.state, "ended");
        assert_eq!(
            f.resume(&f.a, "recovery-a", &a).await.0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            f.resume(&f.b, "recovery-b", &b).await.0,
            StatusCode::FORBIDDEN
        );
    }
}

#[tokio::test]
async fn ended_old_id_cannot_end_new_call_reusing_the_same_channel() {
    let f = Fixture::new().await;
    let (old_a, _) = f.admitted().await;
    assert_eq!(f.action(&f.a, &f.call.id, "end").await.0, StatusCode::OK);
    let (status, next) = request(
        &f.app,
        "POST",
        "/api/private-calls",
        Some(&f.a.token),
        json!({"peer_id":f.b.user.id}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{next}");
    let next: PrivateCall = serde_json::from_value(next).unwrap();
    assert_ne!(next.id, f.call.id);
    assert_eq!(next.channel_id, f.call.channel_id);
    assert_eq!(f.action(&f.b, &next.id, "accept").await.0, StatusCode::OK);
    for (peer, device) in [(&f.a, "recovery-a"), (&f.b, "recovery-b")] {
        assert_eq!(
            request(
                &f.app,
                "POST",
                "/api/voice/join",
                Some(&peer.token),
                json!({"channel_id":next.channel_id,"device_id":device})
            )
            .await
            .0,
            StatusCode::OK
        );
    }
    assert_eq!(f.action(&f.b, &f.call.id, "end").await.0, StatusCode::OK);
    assert_eq!(
        f.action(&f.b, &f.call.id, "accept").await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        f.resume(&f.a, "recovery-a", &old_a).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(lock(&f.state.0.voice).len(), 2);
    assert!(lock(&f.state.0.voice)
        .values()
        .all(|v| v.private_call_id.as_deref() == Some(next.id.as_str())));
    let db = lock(&f.state.0.db);
    assert_eq!(
        db.query_row(
            "SELECT COUNT(*) FROM voice_resume_leases WHERE private_call_id=?1",
            [&next.id],
            |r| r.get::<_, u32>(0)
        )
        .unwrap(),
        2
    );
}

#[tokio::test]
async fn reconnect_preserves_deadlines_and_expiry_ends_the_original_call() {
    let mut f = Fixture::new().await;
    let (a, b) = f.admitted().await;
    let deadline = now() + 20;
    {
        let db = lock(&f.state.0.db);
        db.execute("UPDATE voice_resume_leases SET expires=?1", [deadline])
            .unwrap();
        db.execute(
            "UPDATE private_call_recovery SET join_expires=?1",
            [now().saturating_sub(1)],
        )
        .unwrap();
    }
    f.restart();
    let (status, value) = f.resume(&f.a, "recovery-a", &a).await;
    assert_eq!(status, StatusCode::OK, "{value}");
    let new_a: VoiceTicket = serde_json::from_value(value).unwrap();
    assert_eq!(
        request(
            &f.app,
            "GET",
            "/api/bootstrap",
            Some(&f.a.token),
            Value::Null
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        lock(&f.state.0.db)
            .query_row(
                "SELECT expires FROM voice_resume_leases WHERE user_id=?1",
                [&f.a.user.id],
                |r| r.get::<_, u64>(0)
            )
            .unwrap(),
        deadline
    );
    f.restart();
    assert_eq!(f.resume(&f.a, "recovery-a", &new_a).await.0, StatusCode::OK);
    {
        let db = lock(&f.state.0.db);
        db.execute(
            "UPDATE voice_resume_leases SET expires=?1",
            [now().saturating_sub(1)],
        )
        .unwrap();
        for v in lock(&f.state.0.voice).values_mut() {
            v.recovery_deadline = Some(now().saturating_sub(1));
        }
    }
    assert_eq!(f.visible(&f.b).await.state, "ended");
    assert_eq!(f.leases(), 0);
    assert!(lock(&f.state.0.voice).is_empty());
    assert_eq!(
        f.resume(&f.b, "recovery-b", &b).await.0,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn missing_peer_keeps_exact_lease_grace_and_resumes_before_its_deadline() {
    let mut f = Fixture::new().await;
    let (a, b) = f.admitted().await;
    lock(&f.state.0.db)
        .execute(
            "UPDATE private_call_recovery SET join_expires=?1",
            [now().saturating_sub(1)],
        )
        .unwrap();
    f.restart();
    assert!(
        busy(&f.state, &lock(&f.state.0.db), &f.a.user.id).unwrap(),
        "an exact offline recovery lease reserves its seat even before social hydrates the map"
    );
    assert_eq!(
        request(
            &f.app,
            "POST",
            "/api/private-calls",
            Some(&f.a.token),
            json!({"peer_id":f.b.user.id})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let (status, value) = f.resume(&f.a, "recovery-a", &a).await;
    assert_eq!(status, StatusCode::OK, "{value}");
    let resumed_a: VoiceTicket = serde_json::from_value(value).unwrap();
    assert_eq!(lock(&f.state.0.voice).len(), 1);
    assert_eq!(
        f.visible(&f.a).await.state,
        "accepted",
        "the absent recipient still owns its exact valid lease"
    );
    let (status, value) = f.resume(&f.b, "recovery-b", &b).await;
    assert_eq!(status, StatusCode::OK, "{value}");
    let resumed_b: VoiceTicket = serde_json::from_value(value).unwrap();
    assert_eq!(
        f.visible(&f.a).await.state,
        "accepted",
        "the peer resumed within its original exact deadline"
    );
    {
        let db = lock(&f.state.0.db);
        db.execute(
            "UPDATE voice_resume_leases SET expires=?1 WHERE user_id=?2",
            params![now().saturating_sub(1), f.b.user.id],
        )
        .unwrap();
        let mut seats = lock(&f.state.0.voice);
        seats.get_mut(&resumed_b.session_id).unwrap().last_seen = now().saturating_sub(60);
        assert!(seats[&resumed_a.session_id].alive_at(now()));
        assert!(!seats[&resumed_b.session_id].alive_at(now()));
    }
    assert_eq!(
        f.visible(&f.a).await.state,
        "ended",
        "one live peer cannot retain a call after the other loses both seat freshness and lease"
    );
    assert_eq!(f.leases(), 0);
    assert!(lock(&f.state.0.voice).is_empty());
}

#[tokio::test]
async fn absent_peer_expiry_ends_the_surviving_exact_lease_after_restart() {
    let mut f = Fixture::new().await;
    let (a, b) = f.admitted().await;
    lock(&f.state.0.db)
        .execute(
            "UPDATE private_call_recovery SET join_expires=?1",
            [now().saturating_sub(1)],
        )
        .unwrap();
    f.restart();
    {
        let db = lock(&f.state.0.db);
        assert!(bound_lease(&db, &f.call.id, &f.a.user.id, now()).unwrap());
        assert!(bound_lease(&db, &f.call.id, &f.b.user.id, now()).unwrap());
        db.execute(
            "UPDATE voice_resume_leases SET expires=?1 WHERE user_id=?2",
            params![now().saturating_sub(1), f.b.user.id],
        )
        .unwrap();
        assert!(bound_lease(&db, &f.call.id, &f.a.user.id, now()).unwrap());
        assert!(!bound_lease(&db, &f.call.id, &f.b.user.id, now()).unwrap());
    }
    assert_eq!(f.visible(&f.a).await.state, "ended");
    assert_eq!(f.leases(), 0);
    assert_eq!(
        f.resume(&f.a, "recovery-a", &a).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        f.resume(&f.b, "recovery-b", &b).await.0,
        StatusCode::FORBIDDEN
    );
    f.restart();
    assert_eq!(f.visible(&f.a).await.state, "ended");
}

#[tokio::test]
async fn missing_peer_end_failure_preserves_both_runtime_seats_and_leases() {
    let f = Fixture::new().await;
    let (a, b) = f.admitted().await;
    {
        let db = lock(&f.state.0.db);
        db.execute(
            "UPDATE private_call_recovery SET join_expires=?1",
            [now().saturating_sub(1)],
        )
        .unwrap();
        db.execute(
            "UPDATE voice_resume_leases SET expires=?1 WHERE user_id=?2",
            params![now().saturating_sub(1), f.b.user.id],
        )
        .unwrap();
        lock(&f.state.0.voice)
            .get_mut(&b.session_id)
            .unwrap()
            .last_seen = now().saturating_sub(60);
        db.execute_batch("CREATE TRIGGER owned_absent_peer_end_failure BEFORE UPDATE OF state ON private_call_recovery WHEN new.state='ended' BEGIN SELECT RAISE(ABORT,'owned fixture'); END;").unwrap();
        assert!(reconcile(&f.state, &db).is_err());
    }
    assert_eq!(f.durable_state(), "accepted");
    assert_eq!(f.leases(), 2);
    assert_eq!(lock(&f.state.0.private_calls)[&f.call.id].state, "accepted");
    {
        let seats = lock(&f.state.0.voice);
        assert_eq!(seats.len(), 2);
        assert!(seats[&a.session_id].alive_at(now()));
        assert!(!seats[&b.session_id].alive_at(now()));
        assert!(seats[&a.session_id].key.as_slice() == STANDARD.decode(&a.key).unwrap().as_slice());
        assert!(seats[&b.session_id].key.as_slice() == STANDARD.decode(&b.key).unwrap().as_slice());
    }
    {
        let db = lock(&f.state.0.db);
        db.execute_batch("DROP TRIGGER owned_absent_peer_end_failure;")
            .unwrap();
        reconcile(&f.state, &db).unwrap();
    }
    assert_eq!(f.durable_state(), "ended");
    assert_eq!(f.leases(), 0);
    assert!(lock(&f.state.0.voice).is_empty());
}

#[tokio::test]
async fn original_join_window_admits_a_peer_without_adding_an_absence_timer() {
    let f = Fixture::new().await;
    f.accept().await;
    f.join(&f.a, "recovery-a").await;
    assert_eq!(
        f.visible(&f.a).await.state,
        "accepted",
        "the original join window still gives the recipient time to join"
    );
    lock(&f.state.0.db)
        .execute(
            "UPDATE private_call_recovery SET join_expires=?1",
            [now().saturating_sub(1)],
        )
        .unwrap();
    assert_eq!(f.visible(&f.a).await.state,"ended","there is no extra timer once the original window has ended and the recipient has neither seat nor lease");
    assert_eq!(f.leases(), 0);
    assert!(lock(&f.state.0.voice).is_empty());
}

#[tokio::test]
async fn ordinary_group_voice_join_after_end_does_not_resurrect_private_call() {
    let mut f = Fixture::new().await;
    let (old_a, _) = f.admitted().await;
    assert_eq!(f.action(&f.a, &f.call.id, "end").await.0, StatusCode::OK);
    let (status, value) = request(
        &f.app,
        "POST",
        "/api/voice/join",
        Some(&f.a.token),
        json!({"channel_id":f.call.channel_id,"device_id":"recovery-a"}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a fresh authorized group-room admission remains supported: {value}"
    );
    let fresh: VoiceTicket = serde_json::from_value(value).unwrap();
    assert!(lock(&f.state.0.voice)[&fresh.session_id]
        .private_call_id
        .is_none());
    assert_eq!(f.durable_state(), "ended");
    assert_eq!(
        f.leases(),
        0,
        "the ordinary lease is not attached to the ended call"
    );
    f.restart();
    assert_eq!(f.resume(&f.a, "recovery-a", &fresh).await.0, StatusCode::OK);
    assert!(lock(&f.state.0.voice)
        .values()
        .all(|v| v.private_call_id.is_none()));
    assert_eq!(f.visible(&f.a).await.state, "ended");
    assert_eq!(
        f.resume(&f.a, "recovery-a", &old_a).await.0,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn only_authenticated_voice_transport_confirms_a_recovered_private_seat() {
    use chacha20poly1305::{
        aead::{Aead, Payload},
        ChaCha20Poly1305, KeyInit, Nonce,
    };
    let mut f = Fixture::new().await;
    let (a, _) = f.admitted().await;
    let deadline = now() + 20;
    lock(&f.state.0.db)
        .execute("UPDATE voice_resume_leases SET expires=?1", [deadline])
        .unwrap();
    f.restart();
    let (status, value) = f.resume(&f.a, "recovery-a", &a).await;
    assert_eq!(status, StatusCode::OK, "{value}");
    let ticket: VoiceTicket = serde_json::from_value(value).unwrap();
    let source = "127.0.0.1:30001".parse().unwrap();
    let mut header = Vec::from(ticket.session_id.to_le_bytes());
    header.extend(1u64.to_le_bytes());
    let mut nonce = [0u8; 12];
    nonce[4..].copy_from_slice(&1u64.to_le_bytes());
    let key = STANDARD.decode(&ticket.key).unwrap();
    let cipher = ChaCha20Poly1305::new_from_slice(&key).unwrap();
    let encrypted = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: &[0],
                aad: &header,
            },
        )
        .unwrap();
    let mut packet = header;
    packet.extend(encrypted);
    let mut forged = packet.clone();
    *forged.last_mut().unwrap() ^= 1;
    voice::relay_packet(&f.state, source, &forged);
    assert_eq!(
        lock(&f.state.0.voice)[&ticket.session_id].recovery_deadline,
        Some(deadline)
    );
    assert_eq!(
        request(
            &f.app,
            "GET",
            "/api/bootstrap",
            Some(&f.a.token),
            Value::Null
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        lock(&f.state.0.db)
            .query_row(
                "SELECT expires FROM voice_resume_leases WHERE user_id=?1",
                [&f.a.user.id],
                |r| r.get::<_, u64>(0)
            )
            .unwrap(),
        deadline
    );
    voice::relay_packet(&f.state, source, &packet);
    assert_eq!(
        lock(&f.state.0.voice)[&ticket.session_id].recovery_deadline,
        None
    );
    assert_eq!(
        request(
            &f.app,
            "GET",
            "/api/bootstrap",
            Some(&f.a.token),
            Value::Null
        )
        .await
        .0,
        StatusCode::OK
    );
    assert!(
        lock(&f.state.0.db)
            .query_row(
                "SELECT expires FROM voice_resume_leases WHERE user_id=?1",
                [&f.a.user.id],
                |r| r.get::<_, u64>(0)
            )
            .unwrap()
            > deadline
    );
}

#[tokio::test]
async fn revocation_and_restore_cannot_resurrect_either_exact_call_lease() {
    for operation in ["unfriend", "block", "permission", "device", "session"] {
        let mut f = Fixture::new().await;
        let (a, b) = f.admitted().await;
        f.restart();
        match operation {
            "unfriend" => {
                assert_eq!(
                    request(
                        &f.app,
                        "DELETE",
                        &format!("/api/friends/{}", f.b.user.id),
                        Some(&f.a.token),
                        Value::Null
                    )
                    .await
                    .0,
                    StatusCode::OK
                );
            }
            "block" => {
                assert_eq!(
                    request(
                        &f.app,
                        "POST",
                        &format!("/api/blocks/{}", f.b.user.id),
                        Some(&f.a.token),
                        Value::Null
                    )
                    .await
                    .0,
                    StatusCode::OK
                );
                assert_eq!(
                    request(
                        &f.app,
                        "DELETE",
                        &format!("/api/blocks/{}", f.b.user.id),
                        Some(&f.a.token),
                        Value::Null
                    )
                    .await
                    .0,
                    StatusCode::OK
                );
            }
            "permission" => {
                // The public community ACL route deliberately excludes private
                // conversations. Exercise the durable permission trigger with
                // owned DB writes, including restoration before reconciliation.
                {
                    let db = lock(&f.state.0.db);
                    db.execute(
                        "INSERT INTO channel_permissions VALUES(?1,'user',?2,?3)",
                        params![f.call.channel_id, f.b.user.id, READ | SEND],
                    )
                    .unwrap();
                    db.execute(
                        "UPDATE channel_permissions SET permissions=?3 WHERE channel_id=?1 AND subject_kind='user' AND subject_id=?2",
                        params![f.call.channel_id, f.b.user.id, READ | SEND | JOIN | STREAM],
                    )
                    .unwrap();
                }
                f.state.enforce_voice_permissions().unwrap();
            }
            "device" => {
                assert_eq!(
                    request(
                        &f.app,
                        "DELETE",
                        "/api/e2ee/device/recovery-a",
                        Some(&f.a.token),
                        Value::Null
                    )
                    .await
                    .0,
                    StatusCode::NO_CONTENT
                );
                // Even a restored device row cannot restore an old capability.
                lock(&f.state.0.db)
                    .execute(
                        "UPDATE crypto_devices SET active=1 WHERE device_id='recovery-a'",
                        [],
                    )
                    .unwrap();
            }
            _ => {
                assert_eq!(
                    request(&f.app, "POST", "/api/logout", Some(&f.a.token), Value::Null)
                        .await
                        .0,
                    StatusCode::NO_CONTENT
                );
            }
        }
        if matches!(operation, "unfriend" | "block") {
            for (peer, other) in [(&f.a, &f.b), (&f.b, &f.a)] {
                assert_eq!(
                    request(
                        &f.app,
                        "POST",
                        &format!("/api/friends/{}", other.user.id),
                        Some(&peer.token),
                        Value::Null
                    )
                    .await
                    .0,
                    StatusCode::OK
                );
            }
        }
        assert_eq!(f.durable_state(), "ended", "{operation}");
        assert_eq!(f.leases(), 0, "{operation}");
        f.restart();
        assert!(
            !f.resume(&f.a, "recovery-a", &a).await.0.is_success(),
            "{operation}"
        );
        assert_eq!(
            f.resume(&f.b, "recovery-b", &b).await.0,
            StatusCode::FORBIDDEN,
            "{operation}"
        );
        assert!(lock(&f.state.0.voice).is_empty());
    }
}

#[tokio::test]
async fn failed_acceptance_or_end_keeps_runtime_and_both_leases_unchanged() {
    let f = Fixture::new().await;
    lock(&f.state.0.db).execute_batch("CREATE TRIGGER owned_accept_failure BEFORE INSERT ON private_call_recovery BEGIN SELECT RAISE(ABORT,'owned fixture'); END;").unwrap();
    assert_eq!(
        f.action(&f.b, &f.call.id, "accept").await.0,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(lock(&f.state.0.private_calls)[&f.call.id].state, "ringing");
    assert!(lock(&f.state.0.voice).is_empty());
    lock(&f.state.0.db)
        .execute_batch("DROP TRIGGER owned_accept_failure;")
        .unwrap();
    let (a, b) = f.admitted().await;
    lock(&f.state.0.db).execute_batch("CREATE TRIGGER owned_end_failure BEFORE UPDATE OF state ON private_call_recovery WHEN new.state='ended' BEGIN SELECT RAISE(ABORT,'owned fixture'); END;").unwrap();
    assert_eq!(
        f.action(&f.a, &f.call.id, "end").await.0,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(
        request(
            &f.app,
            "POST",
            "/api/voice/leave",
            Some(&f.a.token),
            Value::Null
        )
        .await
        .0,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(f.durable_state(), "accepted");
    assert_eq!(lock(&f.state.0.private_calls)[&f.call.id].state, "accepted");
    assert_eq!(f.leases(), 2);
    {
        let seats = lock(&f.state.0.voice);
        assert!(seats.contains_key(&a.session_id) && seats.contains_key(&b.session_id));
    }
    {
        let db = lock(&f.state.0.db);
        db.execute_batch("DROP TRIGGER owned_end_failure;").unwrap();
        // The IDs are generated UUIDs in this owned fixture. Fail during the
        // cascading invalidation, after the attempted accepted -> ended write.
        db.execute_batch(&format!("CREATE TRIGGER owned_lease_delete_failure BEFORE DELETE ON voice_resume_leases WHEN old.user_id='{}' BEGIN SELECT RAISE(ABORT,'owned fixture'); END;",f.b.user.id)).unwrap();
    }
    assert_eq!(
        f.action(&f.a, &f.call.id, "end").await.0,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(f.durable_state(), "accepted");
    assert_eq!(f.leases(), 2);
    assert_eq!(lock(&f.state.0.voice).len(), 2);
    let revision = protection::current(&lock(&f.state.0.db)).unwrap().revision;
    assert_eq!(
        request(
            &f.app,
            "PUT",
            "/api/protection",
            Some(&f.a.token),
            json!({"level":"open","expected_revision":revision})
        )
        .await
        .0,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    let mode = protection::current(&lock(&f.state.0.db)).unwrap();
    assert_eq!(mode.level, ProtectionLevel::Protected);
    assert_eq!(mode.revision, revision);
    assert_eq!(f.durable_state(), "accepted");
    assert_eq!(f.leases(), 2);
    assert_eq!(lock(&f.state.0.voice).len(), 2);
}

#[tokio::test]
async fn selected_device_revocation_ends_live_call_even_after_its_own_lease_expired() {
    let f = Fixture::new().await;
    f.admitted().await;
    lock(&f.state.0.db)
        .execute(
            "DELETE FROM voice_resume_leases WHERE user_id=?1",
            [&f.a.user.id],
        )
        .unwrap();
    assert_eq!(f.leases(), 1);
    assert_eq!(
        request(
            &f.app,
            "DELETE",
            "/api/e2ee/device/recovery-a",
            Some(&f.a.token),
            Value::Null
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(f.durable_state(), "ended");
    assert_eq!(f.leases(), 0);
    assert!(lock(&f.state.0.voice).is_empty());
}

#[tokio::test]
async fn open_private_call_recovery_keeps_mode_revision_and_exact_call_binding() {
    let mut f = Fixture::new().await;
    let expected = protection::current(&lock(&f.state.0.db)).unwrap().revision;
    let (status, mode) = request(
        &f.app,
        "PUT",
        "/api/protection",
        Some(&f.a.token),
        json!({"level":"open","expected_revision":expected}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{mode}");
    let revision = mode["revision"].as_u64().unwrap();
    f.accept().await;
    let device = protection::open_device(&token_hash(&f.a.token));
    let (status, ticket) = request(
        &f.app,
        "POST",
        "/api/voice/join",
        Some(&f.a.token),
        json!({"channel_id":f.call.channel_id,"device_id":device,"open_revision":revision}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{ticket}");
    let ticket: VoiceTicket = serde_json::from_value(ticket).unwrap();
    f.restart();
    let body = json!({"channel_id":f.call.channel_id,"device_id":device,"resume_token":ticket.resume.as_ref().unwrap().token,"open_revision":revision});
    let mut wrong = body.clone();
    wrong["open_revision"] = json!(revision + 1);
    assert_eq!(
        request(&f.app, "POST", "/api/voice/resume", Some(&f.a.token), wrong)
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    let (status, value) =
        request(&f.app, "POST", "/api/voice/resume", Some(&f.a.token), body).await;
    assert_eq!(status, StatusCode::OK, "{value}");
    assert_eq!(
        request(
            &f.app,
            "GET",
            "/api/bootstrap",
            Some(&f.a.token),
            Value::Null
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(f.visible(&f.a).await.state, "accepted");
    assert!(lock(&f.state.0.voice)
        .values()
        .all(|v| v.private_call_id.as_deref() == Some(f.call.id.as_str())));
    assert_eq!(f.action(&f.a, &f.call.id, "end").await.0, StatusCode::OK);
    assert_eq!(f.leases(), 0);
}

#[tokio::test]
async fn leaving_for_another_room_ends_only_the_replaced_exact_call() {
    let f = Fixture::new().await;
    f.admitted().await;
    let other = create_test_channel(&f.app, &f.a, "voice", false).await;
    assert_eq!(
        request(
            &f.app,
            "POST",
            "/api/voice/join",
            Some(&f.a.token),
            json!({"channel_id":other,"device_id":"recovery-a"})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(f.durable_state(), "ended");
    assert_eq!(f.leases(), 0);
    let seats = lock(&f.state.0.voice);
    assert_eq!(seats.len(), 1);
    assert!(seats
        .values()
        .all(|v| v.user.id == f.a.user.id && v.channel_id == other && v.private_call_id.is_none()));
}

#[tokio::test]
async fn call_tombstone_retention_is_bounded_and_call_fk_cascades_exact_leases() {
    let f = Fixture::new().await;
    f.admitted().await;
    {
        let db = lock(&f.state.0.db);
        db.execute(
            "DELETE FROM private_call_recovery WHERE id=?1",
            [&f.call.id],
        )
        .unwrap();
        assert_eq!(
            db.query_row("SELECT COUNT(*) FROM voice_resume_leases", [], |r| r
                .get::<_, u32>(0))
                .unwrap(),
            0,
            "FK deletion cascades, never makes private bindings NULL"
        );
        reconcile(&f.state, &db).unwrap();
        assert!(lock(&f.state.0.voice).is_empty());
        for index in 0..MAX_CALLS {
            db.execute(
                "INSERT INTO private_call_recovery VALUES(?1,?2,?3,?4,'ended',?5,?6)",
                params![
                    format!("owned-ended-{index}"),
                    f.call.channel_id,
                    f.a.user.id,
                    f.b.user.id,
                    now().saturating_sub(1),
                    now() + 30
                ],
            )
            .unwrap();
        }
        assert!(db
            .execute(
                "INSERT INTO private_call_recovery VALUES('over-budget',?1,?2,?3,'ended',?4,?4)",
                params![f.call.channel_id, f.a.user.id, f.b.user.id, now() + 30]
            )
            .is_err());
    }
    assert_eq!(
        request(
            &f.app,
            "POST",
            "/api/private-calls",
            Some(&f.a.token),
            json!({"peer_id":f.b.user.id})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    lock(&f.state.0.db)
        .execute(
            "UPDATE private_call_recovery SET retained_until=?1",
            [now().saturating_sub(1)],
        )
        .unwrap();
    assert_eq!(
        request(
            &f.app,
            "POST",
            "/api/private-calls",
            Some(&f.a.token),
            json!({"peer_id":f.b.user.id})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        lock(&f.state.0.db)
            .query_row("SELECT COUNT(*) FROM private_call_recovery", [], |r| r
                .get::<_, u32>(0))
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn schema_23_migration_is_atomic_and_interrupts_only_unbound_legacy_private_leases() {
    let mut f = Fixture::new().await;
    // No acceptance: these represent the unbound schema-22 seats, not a guessed
    // historical call. An ordinary community channel keeps its valid lease.
    assert_eq!(
        request(
            &f.app,
            "POST",
            "/api/voice/join",
            Some(&f.a.token),
            json!({"channel_id":f.call.channel_id,"device_id":"recovery-a"})
        )
        .await
        .0,
        StatusCode::OK
    );
    let ordinary = create_test_channel(&f.app, &f.a, "voice", false).await;
    assert_eq!(
        request(
            &f.app,
            "POST",
            "/api/voice/join",
            Some(&f.b.token),
            json!({"channel_id":ordinary,"device_id":"recovery-b"})
        )
        .await
        .0,
        StatusCode::OK
    );
    {
        let mut db = lock(&f.state.0.db);
        crate::tests::discard_private_call_schema(&db);
        db.execute_batch("DROP INDEX private_call_lease;ALTER TABLE voice_resume_leases DROP COLUMN private_call_id;DELETE FROM schema_migrations WHERE version>=23;PRAGMA user_version=22;CREATE TABLE private_call_recovery(owned_conflict TEXT);").unwrap();
        assert!(migrate_database(&mut db).is_err());
        assert_eq!(
            db.pragma_query_value::<u32, _>(None, "user_version", |r| r.get(0))
                .unwrap(),
            22
        );
        assert_eq!(
            db.query_row("SELECT COUNT(*) FROM voice_resume_leases", [], |r| r
                .get::<_, u32>(0))
                .unwrap(),
            2
        );
        db.execute_batch("DROP TABLE private_call_recovery;")
            .unwrap();
        migrate_database(&mut db).unwrap();
        assert_eq!(
            db.pragma_query_value::<u32, _>(None, "user_version", |r| r.get(0))
                .unwrap(),
            crate::SCHEMA_VERSION
        );
        assert_eq!(
            db.query_row("SELECT COUNT(*) FROM voice_resume_leases", [], |r| r
                .get::<_, u32>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            db.query_row("SELECT channel_id FROM voice_resume_leases", [], |r| r
                .get::<_, String>(
                0
            ))
            .unwrap(),
            ordinary
        );
    }
    f.restart();
    assert!(lock(&f.state.0.voice).is_empty());
}
