use super::*;
use crate::tests::{create_test_channel, new_invite, request, signup};
use serde_json::{json, Value};

struct Fixture {
    data: tempfile::TempDir,
    state: AppState,
    app: Router,
    admin: Session,
    user: Session,
    channel: String,
}
impl Fixture {
    async fn new() -> Self {
        let data = tempfile::tempdir().unwrap();
        let state = AppState::open(data.path(), "resume", 0).unwrap();
        let app = router(state.clone());
        let admin = signup(&app, "Admin", "").await;
        let user = signup(&app, "Member", &new_invite(&app, &admin).await).await;
        let channel = create_test_channel(&app, &admin, "voice", false).await;
        for id in ["selected", "spare"] {
            assert_eq!(request(&app,"PUT","/api/e2ee/device",Some(&user.token),json!({
                "device_id":id,"identity":"ab".repeat(32),"key_package":STANDARD.encode(format!("public:{id}"))
            })).await.0, StatusCode::OK);
        }
        Self {
            data,
            state,
            app,
            admin,
            user,
            channel,
        }
    }
    async fn join(&self, device: &str) -> VoiceTicket {
        let (status, body) = request(
            &self.app,
            "POST",
            "/api/voice/join",
            Some(&self.user.token),
            json!({"channel_id":self.channel,"device_id":device}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        serde_json::from_value(body).unwrap()
    }
    fn restart(&mut self) {
        let old = self.state.0.instance_id.clone();
        self.state = AppState::open(self.data.path(), "resume", 0).unwrap();
        self.app = router(self.state.clone());
        assert_ne!(self.state.0.instance_id, old);
    }
    fn body(&self, ticket: &VoiceTicket) -> Value {
        json!({"channel_id":self.channel,"device_id":"selected","resume_token":ticket.resume.as_ref().unwrap().token})
    }
    async fn resume(&self, body: Value, session: &Session) -> (StatusCode, Value) {
        request(
            &self.app,
            "POST",
            "/api/voice/resume",
            Some(&session.token),
            body,
        )
        .await
    }
    fn leases(&self) -> u32 {
        lock(&self.state.0.db)
            .query_row("SELECT COUNT(*) FROM voice_resume_leases", [], |r| r.get(0))
            .unwrap()
    }
}

#[tokio::test]
async fn restart_resume_rotates_admission_preserves_controls_and_stops_old_captures() {
    let mut f = Fixture::new().await;
    let old = f.join("selected").await;
    assert_eq!(
        f.resume(f.body(&old), &f.user).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            &f.app,
            "POST",
            "/api/voice/state",
            Some(&f.user.token),
            json!({
                "muted":true,"deafened":true,"sharing":true,"camera":true
            })
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    let token = &old.resume.as_ref().unwrap().token;
    assert!(!format!("{:?}", old.resume).contains(token));
    let hash: String = lock(&f.state.0.db)
        .query_row("SELECT capability_hash FROM voice_resume_leases", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(hash, token_hash(token));
    assert_ne!(&hash, token);
    let old_instance = old.resume.as_ref().unwrap().instance_id.clone();
    f.restart();
    let (_, bootstrap) = request(
        &f.app,
        "GET",
        "/api/bootstrap",
        Some(&f.user.token),
        Value::Null,
    )
    .await;
    assert_ne!(bootstrap["instance_id"], old_instance);
    let (status, body) = f.resume(f.body(&old), &f.user).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let new: VoiceTicket = serde_json::from_value(body).unwrap();
    assert_ne!(new.session_id, old.session_id);
    assert_ne!(new.key, old.key);
    assert_ne!(new.resume.as_ref().unwrap().token, *token);
    let controls = new.resumed_state.unwrap();
    assert!(controls.muted && controls.deafened);
    assert!(!controls.sharing && !controls.camera);
    {
        let sessions = lock(&f.state.0.voice);
        let session = &sessions[&new.session_id];
        assert!(session.in_sequence.is_none());
        assert_eq!(session.out_sequence, 0);
        assert!(session.remote.is_none() && session.tunnel_sender.is_none());
    }
    assert_eq!(
        f.resume(f.body(&old), &f.user).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(f.leases(), 1);
}

#[tokio::test]
async fn restart_resume_is_bound_to_account_login_channel_device_and_single_use() {
    let mut f = Fixture::new().await;
    let ticket = f.join("selected").await;
    let other_channel = create_test_channel(&f.app, &f.admin, "voice", false).await;
    let replacement_login = new_session(&lock(&f.state.0.db), f.user.user.clone()).unwrap();
    f.restart();
    for (key, value) in [
        ("channel_id", other_channel),
        ("device_id", "spare".into()),
        ("resume_token", URL_SAFE_NO_PAD.encode([99; 32])),
    ] {
        let mut body = f.body(&ticket);
        body[key] = Value::String(value);
        assert_eq!(f.resume(body, &f.user).await.0, StatusCode::FORBIDDEN);
        assert!(lock(&f.state.0.voice).is_empty());
        assert_eq!(f.leases(), 1);
    }
    assert_eq!(
        f.resume(f.body(&ticket), &f.admin).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        f.resume(f.body(&ticket), &replacement_login).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(f.resume(f.body(&ticket), &f.user).await.0, StatusCode::OK);
    f.restart();
    assert_eq!(
        f.resume(f.body(&ticket), &f.user).await.0,
        StatusCode::FORBIDDEN,
        "an old capability cannot become valid again on a second restart"
    );
}

#[tokio::test]
async fn restart_resume_never_replaces_an_explicit_call_on_another_device() {
    let mut f = Fixture::new().await;
    let ticket = f.join("selected").await;
    f.restart();
    let replacement = f.join("spare").await;
    assert_eq!(
        f.resume(f.body(&ticket), &f.user).await.0,
        StatusCode::FORBIDDEN
    );
    let sessions = lock(&f.state.0.voice);
    assert_eq!(sessions.len(), 1);
    assert_eq!(
        sessions[&replacement.session_id].device_id.as_deref(),
        Some("spare")
    );
}

#[tokio::test]
async fn restart_resume_is_cancelled_by_leave_kick_and_offline_device_revocation() {
    for operation in ["leave", "kick", "device"] {
        let mut f = Fixture::new().await;
        let ticket = f.join("selected").await;
        // Device revocation must also work after restart, with no live session
        // from which to discover the old selected device.
        if operation == "device" {
            f.restart();
        }
        let (method, path, body) = match operation {
            "leave" => ("POST", "/api/voice/leave".into(), json!({})),
            "kick" => (
                "PATCH",
                format!("/api/members/{}", f.user.user.id),
                json!({"status":"removed"}),
            ),
            _ => ("DELETE", "/api/e2ee/device/selected".into(), Value::Null),
        };
        let session = if operation == "kick" {
            &f.admin
        } else {
            &f.user
        };
        let (status, result) = request(&f.app, method, &path, Some(&session.token), body).await;
        assert!(status.is_success(), "{operation}: {status} {result}");
        assert_eq!(f.leases(), 0, "{operation}");
        f.restart();
        assert!(!f.resume(f.body(&ticket), &f.user).await.0.is_success());
        assert!(lock(&f.state.0.voice).is_empty());
    }
}

#[tokio::test]
async fn removed_then_restored_permissions_cannot_restore_a_resume_capability() {
    let mut f = Fixture::new().await;
    let ticket = f.join("selected").await;
    f.restart();
    for join in [false, true] {
        let(status,value)=request(&f.app,"PUT",&format!("/api/channels/{}/permissions",f.channel),Some(&f.admin.token),json!({
            "entries":[{"subject_kind":"user","subject_id":f.user.user.id,"read":true,"send":true,"join":join,"stream":join}]
        })).await;
        assert_eq!(status, StatusCode::OK, "{value}");
    }
    assert_eq!(f.leases(), 0);
    f.restart();
    assert_eq!(
        f.resume(f.body(&ticket), &f.user).await.0,
        StatusCode::FORBIDDEN
    );
    f.join("selected").await;
    assert_eq!(f.leases(), 1, "explicit selection remains possible");
}

#[tokio::test]
async fn expired_lease_never_renews_and_failed_durable_join_preserves_old_runtime() {
    let mut f = Fixture::new().await;
    let ticket = f.join("selected").await;
    lock(&f.state.0.db)
        .execute(
            "UPDATE voice_resume_leases SET expires=?1",
            [now().saturating_sub(1)],
        )
        .unwrap();
    assert_eq!(
        request(
            &f.app,
            "GET",
            "/api/bootstrap",
            Some(&f.user.token),
            Value::Null
        )
        .await
        .0,
        StatusCode::OK
    );
    let expires: u64 = lock(&f.state.0.db)
        .query_row("SELECT expires FROM voice_resume_leases", [], |r| r.get(0))
        .unwrap();
    assert!(expires < now());
    lock(&f.state.0.db).execute_batch("CREATE TRIGGER refuse_new_lease BEFORE INSERT ON voice_resume_leases BEGIN SELECT RAISE(ABORT,'owned fixture failure'); END;").unwrap();
    assert_eq!(
        request(
            &f.app,
            "POST",
            "/api/voice/join",
            Some(&f.user.token),
            json!({"channel_id":f.channel,"device_id":"spare"})
        )
        .await
        .0,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(lock(&f.state.0.voice).len(), 1);
    assert_eq!(
        lock(&f.state.0.voice)[&ticket.session_id]
            .device_id
            .as_deref(),
        Some("selected")
    );
    f.restart();
    assert_eq!(
        f.resume(f.body(&ticket), &f.user).await.0,
        StatusCode::FORBIDDEN
    );
}

#[test]
fn recovery_migration_is_transactional_and_legacy_tickets_still_parse() {
    let data = tempfile::tempdir().unwrap();
    let state = AppState::open(data.path(), "migration", 0).unwrap();
    let mut db = lock(&state.0.db);
    crate::tests::discard_voice_resume_schema(&db);
    db.execute_batch("DELETE FROM schema_migrations WHERE version>=17;PRAGMA user_version=16;CREATE TABLE voice_resume_leases(deliberate_conflict TEXT);").unwrap();
    assert!(migrate_database(&mut db).is_err());
    assert_eq!(
        db.pragma_query_value::<u32, _>(None, "user_version", |r| r.get(0))
            .unwrap(),
        16
    );
    db.execute_batch("DROP TABLE voice_resume_leases;").unwrap();
    migrate_database(&mut db).unwrap();
    assert_eq!(
        db.pragma_query_value::<u32, _>(None, "user_version", |r| r.get(0))
            .unwrap(),
        crate::SCHEMA_VERSION
    );
    let legacy: VoiceTicket =
        serde_json::from_value(json!({"session_id":1,"key":"AA==","udp_port":0,"wire_version":3}))
            .unwrap();
    assert!(legacy.resume.is_none() && legacy.resumed_state.is_none());
}

#[tokio::test]
async fn authenticated_bootstrap_renews_live_voice_without_rejoin_or_key_change() {
    use chacha20poly1305::{
        aead::{Aead, Payload},
        ChaCha20Poly1305, KeyInit, Nonce,
    };
    let f = Fixture::new().await;
    let ticket = f.join("selected").await;
    let original_expiry = now() + 2;
    let key = {
        let mut sessions = lock(&f.state.0.voice);
        let v = sessions.get_mut(&ticket.session_id).unwrap();
        v.expires = original_expiry;
        v.out_sequence = 17;
        v.state.muted = true;
        v.key
    };
    let replacement = new_session(&lock(&f.state.0.db), f.user.user.clone()).unwrap();
    for login in [&f.admin, &replacement] {
        assert_eq!(
            request(
                &f.app,
                "GET",
                "/api/bootstrap",
                Some(&login.token),
                Value::Null
            )
            .await
            .0,
            StatusCode::OK
        );
        assert_eq!(
            lock(&f.state.0.voice)[&ticket.session_id].expires,
            original_expiry,
            "another account or login must not keep this ticket alive"
        );
    }
    let (status, body) = request(
        &f.app,
        "GET",
        "/api/bootstrap",
        Some(&f.user.token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    {
        let sessions = lock(&f.state.0.voice);
        let v = &sessions[&ticket.session_id];
        assert!(
            v.expires > original_expiry + 60,
            "authenticated refresh must renew the live voice ticket"
        );
        assert!(v.expires <= now() + VOICE_LIFETIME);
        assert_eq!(v.key, key);
        assert_eq!(v.out_sequence, 17);
        assert_eq!(v.device_id.as_deref(), Some("selected"));
        assert!(v.state.muted);
    }
    assert!(body["voice_members"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v["session_id"] == ticket.session_id));
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(now() >= original_expiry);
    let source = "127.0.0.1:19218".parse().unwrap();
    let mut packet = ticket.session_id.to_le_bytes().to_vec();
    packet.extend(0_u64.to_le_bytes());
    let nonce = [0_u8; 12];
    let encrypted = ChaCha20Poly1305::new_from_slice(&key)
        .unwrap()
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: &[0],
                aad: &packet,
            },
        )
        .unwrap();
    packet.extend(encrypted);
    assert!(voice::relay_packet(&f.state, source, &packet).is_empty());
    let sessions = lock(&f.state.0.voice);
    let v = &sessions[&ticket.session_id];
    assert_eq!(
        v.remote,
        Some(source),
        "same authenticated ticket survives its old deadline"
    );
    assert_eq!(v.in_sequence, Some(0));
    assert_eq!(v.out_sequence, 17);
    assert_eq!(v.key, key);
}

#[tokio::test]
async fn authenticated_refresh_never_revives_expired_or_idle_voice() {
    let f = Fixture::new().await;
    for idle in [false, true] {
        let ticket = f.join("selected").await;
        let expiry = if idle { now() + 100 } else { now() };
        let seen = if idle {
            now().saturating_sub(60)
        } else {
            now()
        };
        {
            let mut sessions = lock(&f.state.0.voice);
            let v = sessions.get_mut(&ticket.session_id).unwrap();
            v.expires = expiry;
            v.last_seen = seen;
        }
        let (status, body) = request(
            &f.app,
            "GET",
            "/api/bootstrap",
            Some(&f.user.token),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let sessions = lock(&f.state.0.voice);
        let v = &sessions[&ticket.session_id];
        assert_eq!(v.expires, expiry);
        assert_eq!(
            v.last_seen, seen,
            "HTTP must not replace authenticated voice keepalives"
        );
        assert!(!v.alive_at(now()));
        assert!(!body["voice_members"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["session_id"] == ticket.session_id));
    }
}

#[tokio::test]
async fn live_voice_renewal_is_capped_by_login_expiry_and_requires_valid_login() {
    let f = Fixture::new().await;
    let ticket = f.join("selected").await;
    let login_expiry = now() + 90;
    lock(&f.state.0.db)
        .execute(
            "UPDATE sessions SET expires=?1 WHERE token_hash=?2",
            params![login_expiry, token_hash(&f.user.token)],
        )
        .unwrap();
    lock(&f.state.0.voice)
        .get_mut(&ticket.session_id)
        .unwrap()
        .expires = now() + 3;
    assert_eq!(
        request(
            &f.app,
            "GET",
            "/api/bootstrap",
            Some(&f.user.token),
            Value::Null
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        lock(&f.state.0.voice)[&ticket.session_id].expires,
        login_expiry,
        "voice cannot outlive its login"
    );
    lock(&f.state.0.db)
        .execute(
            "DELETE FROM sessions WHERE token_hash=?1",
            [token_hash(&f.user.token)],
        )
        .unwrap();
    assert_eq!(
        request(
            &f.app,
            "GET",
            "/api/bootstrap",
            Some(&f.user.token),
            Value::Null
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        lock(&f.state.0.voice)[&ticket.session_id].expires,
        login_expiry
    );
}

#[tokio::test]
async fn failed_durable_renewal_does_not_partially_extend_runtime_admission() {
    let f = Fixture::new().await;
    let ticket = f.join("selected").await;
    let expiry = now() + 30;
    lock(&f.state.0.voice)
        .get_mut(&ticket.session_id)
        .unwrap()
        .expires = expiry;
    lock(&f.state.0.db).execute_batch("CREATE TRIGGER refuse_renewal BEFORE UPDATE OF expires ON voice_resume_leases BEGIN SELECT RAISE(ABORT,'owned renewal failure'); END;").unwrap();
    assert_eq!(
        request(
            &f.app,
            "GET",
            "/api/bootstrap",
            Some(&f.user.token),
            Value::Null
        )
        .await
        .0,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(lock(&f.state.0.voice)[&ticket.session_id].expires, expiry);
}
