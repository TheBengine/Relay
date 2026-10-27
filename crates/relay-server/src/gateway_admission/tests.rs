//! Admission and revocation of local voice gateways, through the real router.
use super::*;
use crate::tests::{new_invite, request, signup};
use serde_json::{json, Value};

const PATH: &str = "/api/hosting/voice-gateways";
/// The instance id of a gateway worker, and of a restarted one.
const INSTANCE: &str = "abababababababababababababababab";
const RESTARTED: &str = "cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd";

struct World {
    _dir: tempfile::TempDir,
    state: AppState,
    app: Router,
    admin: Session,
    gateway: Session,
    client: Session,
    other: Session,
}

/// Put a member into the voice map the way an admitted session would be.
fn seat(state: &AppState, who: &Session, id: u64, channel: &str) {
    lock(&state.0.voice).insert(
        id,
        voice::VoiceSession {
            user: who.user.clone(),
            channel_id: channel.into(),
            device_id: None,
            token_hash: crate::session_hash(&who.token),
            token_digest: token_hash(&who.token),
            private_call_id: None,
            recovery_deadline: None,
            key: [7; 32],
            state: Default::default(),
            remote: None,
            tunnel_sender: None,
            tunnel_id: None,
            in_sequence: None,
            replay: Default::default(),
            out_sequence: 0,
            last_seen: now(),
            expires: now() + 3600,
            direct: Default::default(),
            can_speak: true,
            can_stream: true,
            whisper_channels: Default::default(),
        },
    );
}

async fn world() -> World {
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::open(dir.path(), "Gateways", 0).unwrap();
    let app = router(state.clone());
    let admin = signup(&app, "Anna", "").await;
    let gateway = signup(&app, "Gus", &new_invite(&app, &admin).await).await;
    let client = signup(&app, "Cleo", &new_invite(&app, &admin).await).await;
    let other = signup(&app, "Otto", &new_invite(&app, &admin).await).await;
    lock(&state.0.db)
        .execute_batch(
            "INSERT INTO channels(id,name,kind,topic,visibility) VALUES
                ('vc1','Room one','voice','','public'),('vc2','Room two','voice','','public');",
        )
        .unwrap();
    World {
        _dir: dir,
        state,
        app,
        admin,
        gateway,
        client,
        other,
    }
}

async fn grant_role(w: &World, who: &Session) {
    let (status, body) = request(
        &w.app,
        "POST",
        "/api/hosting/offers",
        Some(&w.admin.token),
        json!({"user_id":who.user.id,"roles":["voice_gateway"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = request(
        &w.app,
        "POST",
        "/api/hosting/answer",
        Some(&who.token),
        json!({"accepted":["voice_gateway"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

/// The member's live voice session id as the server holds it now.
fn session_of(w: &World, who: &Session) -> u64 {
    lock(&w.state.0.voice)
        .iter()
        .filter(|(_, session)| session.user.id == who.user.id)
        .map(|(id, _)| *id)
        .max()
        .unwrap_or(0)
}

async fn register_session(
    w: &World,
    who: &Session,
    instance: &str,
    session_id: u64,
    ip: &str,
    port: u16,
) -> (StatusCode, Value) {
    request(
        &w.app,
        "POST",
        &format!("{PATH}/registration"),
        Some(&who.token),
        json!({"instance":instance,"session_id":session_id,"ip":ip,"port":port}),
    )
    .await
}

async fn register_as(
    w: &World,
    who: &Session,
    instance: &str,
    ip: &str,
    port: u16,
) -> (StatusCode, Value) {
    register_session(w, who, instance, session_of(w, who), ip, port).await
}

async fn register(w: &World, who: &Session, ip: &str, port: u16) -> (StatusCode, Value) {
    register_as(w, who, INSTANCE, ip, port).await
}

async fn lease_session(
    w: &World,
    who: &Session,
    gateway: &str,
    instance: &str,
    session_id: u64,
) -> (StatusCode, Value) {
    request(
        &w.app,
        "POST",
        &format!("{PATH}/leases"),
        Some(&who.token),
        json!({"gateway_id":gateway,"instance":instance,"session_id":session_id}),
    )
    .await
}

async fn lease_instance(
    w: &World,
    who: &Session,
    gateway: &str,
    instance: &str,
) -> (StatusCode, Value) {
    lease_session(w, who, gateway, instance, session_of(w, who)).await
}

async fn drop_registration(
    w: &World,
    who: &Session,
    instance: &str,
    session_id: u64,
) -> (StatusCode, Value) {
    request(
        &w.app,
        "DELETE",
        &format!("{PATH}/registration?instance={instance}&session_id={session_id}"),
        Some(&who.token),
        Value::Null,
    )
    .await
}

async fn drop_lease(
    w: &World,
    who: &Session,
    session_id: u64,
    lease_id: &str,
) -> (StatusCode, Value) {
    request(
        &w.app,
        "DELETE",
        &format!("{PATH}/leases?session_id={session_id}&lease_id={lease_id}"),
        Some(&who.token),
        Value::Null,
    )
    .await
}

async fn lease(w: &World, who: &Session, gateway: &str) -> (StatusCode, Value) {
    lease_instance(w, who, gateway, INSTANCE).await
}

async fn grants_for(w: &World, who: &Session, instance: &str) -> (StatusCode, Value) {
    request(
        &w.app,
        "GET",
        &format!("{PATH}/grants?instance={instance}"),
        Some(&who.token),
        Value::Null,
    )
    .await
}

async fn grants(w: &World, who: &Session) -> (StatusCode, Value) {
    grants_for(w, who, INSTANCE).await
}

async fn list(w: &World, who: &Session) -> (StatusCode, Value) {
    request(&w.app, "GET", PATH, Some(&who.token), Value::Null).await
}

/// A gateway with its role and a registered address in room one, and a client in the same room.
async fn ready() -> World {
    let w = world().await;
    grant_role(&w, &w.gateway).await;
    seat(&w.state, &w.gateway, 11, "vc1");
    seat(&w.state, &w.client, 22, "vc1");
    let (status, body) = register(&w, &w.gateway, "192.168.1.20", 47000).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    w
}

#[tokio::test]
async fn only_a_gateway_with_a_live_session_registers_a_numeric_private_address() {
    let w = world().await;
    // No role: refused, however live the session is.
    seat(&w.state, &w.gateway, 11, "vc1");
    assert_eq!(
        register(&w, &w.gateway, "192.168.1.20", 47000).await.0,
        StatusCode::FORBIDDEN
    );
    grant_role(&w, &w.gateway).await;
    // An offered-and-accepted role but no live voice session: refused.
    lock(&w.state.0.voice).clear();
    assert_eq!(
        register(&w, &w.gateway, "192.168.1.20", 47000).await.0,
        StatusCode::FORBIDDEN
    );
    seat(&w.state, &w.gateway, 11, "vc1");
    // Only a numeric private IPv4 address with a real port.
    for (ip, port) in [
        ("gateway.local", 47000),
        ("8.8.8.8", 47000),
        ("127.0.0.1", 47000),
        ("100.64.0.9", 47000),
        ("192.168.1.20", 80),
        ("http://192.168.1.20", 47000),
        ("", 47000),
    ] {
        assert_eq!(
            register(&w, &w.gateway, ip, port).await.0,
            StatusCode::BAD_REQUEST,
            "{ip}:{port}"
        );
    }
    let (status, body) = register(&w, &w.gateway, "192.168.1.20", 47000).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["channel_id"], "vc1",
        "the channel is the caller's own live session's"
    );
    assert_eq!(body["instance"], INSTANCE);
    assert_eq!(body["expires_in_secs"], 120);
    assert_eq!(body["max_grants"], 32);
    assert_eq!(w.state.0.gateways.gateway_count(), 1);
    // A member without the role cannot register, and a registration expires.
    seat(&w.state, &w.other, 33, "vc1");
    assert_eq!(
        register(&w, &w.other, "192.168.1.21", 47001).await.0,
        StatusCode::FORBIDDEN
    );
    w.state.0.gateways.expire_registrations();
    assert_eq!(
        list(&w, &w.client).await.0,
        StatusCode::FORBIDDEN,
        "client has no seat yet"
    );
    seat(&w.state, &w.client, 22, "vc1");
    assert_eq!(list(&w, &w.client).await.1["gateways"], json!([]));
    // Memory only: nothing about gateways is stored in the database.
    let tables: i64 = lock(&w.state.0.db)
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name LIKE '%gateway%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(tables, 0);
}

#[tokio::test]
async fn a_client_leases_only_its_own_live_session_in_the_same_channel() {
    let w = ready().await;
    let gateway = w.gateway.user.id.clone();
    // No live session, or a session in another room: no lease and no gateway list.
    assert_eq!(lease(&w, &w.other, &gateway).await.0, StatusCode::FORBIDDEN);
    seat(&w.state, &w.other, 33, "vc2");
    assert_eq!(lease(&w, &w.other, &gateway).await.0, StatusCode::NOT_FOUND);
    assert_eq!(list(&w, &w.other).await.1["gateways"], json!([]));
    // The gateway itself, unknown ids and malformed ids are refused.
    assert_eq!(
        lease(&w, &w.gateway, &gateway).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        lease(&w, &w.client, "someone-else").await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        lease(&w, &w.client, "bad id!").await.0,
        StatusCode::FORBIDDEN
    );
    // The same room: listed, then leased for the client's own session.
    let (status, listed) = list(&w, &w.client).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        listed["gateways"],
        json!([{"gateway_id":gateway,"instance":INSTANCE,"ip":"192.168.1.20","port":47000}])
    );
    let (status, got) = lease(&w, &w.client, &gateway).await;
    assert_eq!(status, StatusCode::OK, "{got}");
    assert_eq!(
        got["session_id"], 22,
        "the lease is bound to the caller's own session"
    );
    assert_eq!(got["gateway_id"], gateway);
    assert_eq!(got["instance"], INSTANCE);
    assert_eq!(
        (got["ip"].as_str(), got["port"].as_u64()),
        (Some("192.168.1.20"), Some(47000))
    );
    assert!(got["ttl_ms"].as_u64().unwrap() <= 2000 && got["ttl_ms"].as_u64().unwrap() > 0);
    let key = URL_SAFE_NO_PAD
        .decode(got["key"].as_str().unwrap())
        .unwrap();
    assert_eq!(key.len(), 32);
    assert_ne!(
        key,
        vec![7u8; 32],
        "the hop key is independent of the voice session key"
    );
    assert!(relay_protocol::gateway_broker::decode_id(got["lease_id"].as_str().unwrap()).is_some());
    // The request cannot name another session (here the gateway's): 409, nothing changes.
    let before = w.state.0.gateways.lease_count();
    let (status, _) = lease_session(&w, &w.client, &gateway, INSTANCE, 11).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(w.state.0.gateways.lease_count(), before);
    // Unknown fields are refused.
    let (status, _) = request(
        &w.app,
        "POST",
        &format!("{PATH}/leases"),
        Some(&w.client.token),
        json!({"gateway_id":gateway,"instance":INSTANCE,"session_id":22,"x":1}),
    )
    .await;
    assert!(status.is_client_error());
    // Nobody else sees the key: not the list, not another member's grants poll.
    assert!(!listed.to_string().contains("key"));
    assert_eq!(
        grants(&w, &w.client).await.0,
        StatusCode::FORBIDDEN,
        "a client is not a gateway"
    );
    assert_eq!(grants(&w, &w.other).await.0, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_live_renewal_keeps_the_grant_and_expiry_or_revocation_issue_a_new_one() {
    let w = ready().await;
    let gateway = w.gateway.user.id.clone();
    let (_, first) = lease(&w, &w.client, &gateway).await;
    tokio::time::sleep(std::time::Duration::from_millis(120)).await;
    let (status, renewed) = lease(&w, &w.client, &gateway).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        (&renewed["lease_id"], &renewed["key"]),
        (&first["lease_id"], &first["key"]),
        "renewal while live keeps id and key"
    );
    assert!(
        renewed["ttl_ms"].as_u64().unwrap() > first["ttl_ms"].as_u64().unwrap() - 120,
        "validity restarts"
    );
    assert_eq!(w.state.0.gateways.lease_count(), 1);
    // After expiry the next lease is a new admission: fresh id and key.
    w.state.0.gateways.expire_leases();
    let (_, after_expiry) = lease(&w, &w.client, &gateway).await;
    assert_ne!(after_expiry["lease_id"], first["lease_id"]);
    assert_ne!(after_expiry["key"], first["key"]);
    // After the client lets go (revocation by the holder), also new.
    let (status, dropped) = drop_lease(
        &w,
        &w.client,
        22,
        after_expiry["lease_id"].as_str().unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(dropped["dropped"], true);
    assert_eq!(w.state.0.gateways.lease_count(), 0);
    let (_, after_release) = lease(&w, &w.client, &gateway).await;
    assert_ne!(after_release["lease_id"], after_expiry["lease_id"]);
    assert_ne!(after_release["key"], after_expiry["key"]);
}

#[tokio::test]
async fn the_gateway_polls_its_own_grants_and_polling_never_renews() {
    let w = ready().await;
    let gateway = w.gateway.user.id.clone();
    assert_eq!(grants(&w, &w.gateway).await.1["grants"], json!([]));
    let (_, leased) = lease(&w, &w.client, &gateway).await;
    let (status, polled) = grants(&w, &w.gateway).await;
    assert_eq!(status, StatusCode::OK);
    let list = polled["grants"].as_array().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["id"], leased["lease_id"]);
    assert_eq!(list[0]["session"], 22);
    assert_eq!(
        list[0]["key"], leased["key"],
        "the gateway gets the same hop key as that client"
    );
    let first_ttl = list[0]["ttl_ms"].as_u64().unwrap();
    assert!(first_ttl > 0 && first_ttl <= 2000);
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    let second = grants(&w, &w.gateway).await.1["grants"][0]["ttl_ms"]
        .as_u64()
        .unwrap();
    assert!(
        second < first_ttl,
        "polling must not extend the lease ({second} !< {first_ttl})"
    );
    // Once it is not renewed it is gone from the poll.
    w.state.0.gateways.expire_leases();
    assert_eq!(grants(&w, &w.gateway).await.1["grants"], json!([]));
    // Another gateway sees none of this client's grants.
    grant_role(&w, &w.other).await;
    seat(&w.state, &w.other, 33, "vc1");
    assert_eq!(
        register(&w, &w.other, "192.168.1.30", 47002).await.0,
        StatusCode::OK
    );
    lease(&w, &w.client, &gateway).await;
    assert_eq!(grants(&w, &w.other).await.1["grants"], json!([]));
    assert_eq!(
        grants(&w, &w.gateway).await.1["grants"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn role_session_channel_and_permission_changes_revoke_at_once() {
    // The client leaves voice: its grant leaves the poll and the lease is dropped.
    let w = ready().await;
    let gateway = w.gateway.user.id.clone();
    lease(&w, &w.client, &gateway).await;
    lock(&w.state.0.voice).remove(&22);
    assert_eq!(grants(&w, &w.gateway).await.1["grants"], json!([]));
    assert_eq!(w.state.0.gateways.lease_count(), 0);

    // Another room for the client: it can no longer renew into room one.
    seat(&w.state, &w.client, 23, "vc2");
    assert_eq!(
        lease(&w, &w.client, &gateway).await.0,
        StatusCode::NOT_FOUND
    );

    // The client's permission to join the channel is removed.
    let w = ready().await;
    let gateway = w.gateway.user.id.clone();
    lease(&w, &w.client, &gateway).await;
    lock(&w.state.0.db)
        .execute(
            "INSERT INTO channel_permissions VALUES('vc1','user',?1,1)",
            [&w.client.user.id],
        )
        .unwrap();
    assert_eq!(
        lease(&w, &w.client, &gateway).await.0,
        StatusCode::FORBIDDEN,
        "renewal re-checks permission"
    );
    assert_eq!(grants(&w, &w.gateway).await.1["grants"], json!([]));

    // The client's sign-in ends.
    let w = ready().await;
    let gateway = w.gateway.user.id.clone();
    lease(&w, &w.client, &gateway).await;
    lock(&w.state.0.db)
        .execute(
            "DELETE FROM sessions WHERE token_hash=?1",
            [crate::session_hash(&w.client.token)],
        )
        .unwrap();
    assert_eq!(grants(&w, &w.gateway).await.1["grants"], json!([]));

    // The gateway's own voice session ends: registration, lease and polling all stop.
    let w = ready().await;
    let gateway = w.gateway.user.id.clone();
    lease(&w, &w.client, &gateway).await;
    lock(&w.state.0.voice).remove(&11);
    assert_eq!(grants(&w, &w.gateway).await.0, StatusCode::FORBIDDEN);
    assert_eq!(w.state.0.gateways.gateway_count(), 0);
    assert_eq!(w.state.0.gateways.lease_count(), 0);
    assert_eq!(
        lease(&w, &w.client, &gateway).await.0,
        StatusCode::NOT_FOUND
    );

    // The administrator withdraws the role: the gateway and its leases go immediately.
    let w = ready().await;
    let gateway = w.gateway.user.id.clone();
    lease(&w, &w.client, &gateway).await;
    let (status, _) = request(
        &w.app,
        "DELETE",
        &format!("/api/hosting/candidates/{gateway}"),
        Some(&w.admin.token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(w.state.0.gateways.gateway_count(), 0);
    assert_eq!(w.state.0.gateways.lease_count(), 0);
    assert_eq!(grants(&w, &w.gateway).await.0, StatusCode::FORBIDDEN);

    // Stepping down does the same.
    let w = ready().await;
    let gateway = w.gateway.user.id.clone();
    lease(&w, &w.client, &gateway).await;
    request(
        &w.app,
        "POST",
        "/api/hosting/answer",
        Some(&w.gateway.token),
        json!({"accepted":[]}),
    )
    .await;
    assert_eq!(w.state.0.gateways.gateway_count(), 0);
    assert_eq!(
        lease(&w, &w.client, &gateway).await.0,
        StatusCode::NOT_FOUND
    );

    // A registration that is not renewed expires with its leases.
    let w = ready().await;
    let gateway = w.gateway.user.id.clone();
    lease(&w, &w.client, &gateway).await;
    w.state.0.gateways.expire_registrations();
    assert_eq!(
        lease(&w, &w.client, &gateway).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(w.state.0.gateways.lease_count(), 0);
}

#[tokio::test]
async fn private_call_seats_never_use_the_broker() {
    let w = ready().await;
    // A private-call seat is not a community seat.
    lock(&w.state.0.voice).get_mut(&22).unwrap().private_call_id = Some("call".into());
    assert_eq!(list(&w, &w.client).await.0, StatusCode::FORBIDDEN);
    assert_eq!(
        lease(&w, &w.client, &w.gateway.user.id).await.0,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn quotas_and_rates_bound_registrations_and_requests() {
    let w = ready().await;
    // At most four gateways per channel: the fifth registration is refused.
    let mut extra = Vec::new();
    for name in ["G2", "G3", "G4", "G5"] {
        let member = signup(&w.app, name, &new_invite(&w.app, &w.admin).await).await;
        grant_role(&w, &member).await;
        extra.push(member);
    }
    for (n, member) in extra.iter().enumerate() {
        seat(&w.state, member, 100 + n as u64, "vc1");
        let ip = format!("192.168.1.{}", 40 + n);
        let (status, _) = register(&w, member, &ip, 47100).await;
        if n < 3 {
            assert_eq!(status, StatusCode::OK, "{n}");
        } else {
            assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        }
    }
    assert_eq!(w.state.0.gateways.gateway_count(), 4);
    // Re-registering renews without counting twice.
    assert_eq!(
        register(&w, &w.gateway, "192.168.1.21", 47001).await.0,
        StatusCode::OK
    );
    assert_eq!(w.state.0.gateways.gateway_count(), 4);
    // A burst of requests from one member is slowed down.
    let mut limited = false;
    for _ in 0..60 {
        if list(&w, &w.client).await.0 == StatusCode::TOO_MANY_REQUESTS {
            limited = true;
            break;
        }
    }
    assert!(limited);
}

#[tokio::test]
async fn a_restarted_gateway_never_inherits_an_old_id_or_key() {
    let w = ready().await;
    let gateway = w.gateway.user.id.clone();
    let (_, first) = lease(&w, &w.client, &gateway).await;
    // The same instance registering again (a renewal) keeps the live lease, id and key.
    assert_eq!(
        register(&w, &w.gateway, "192.168.1.20", 47000).await.0,
        StatusCode::OK
    );
    assert_eq!(w.state.0.gateways.lease_count(), 1);
    let (_, renewed) = lease(&w, &w.client, &gateway).await;
    assert_eq!(
        (&renewed["lease_id"], &renewed["key"]),
        (&first["lease_id"], &first["key"])
    );
    // A new worker instance at the very same address and port cancels the old leases.
    let (status, body) = register_as(&w, &w.gateway, RESTARTED, "192.168.1.20", 47000).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["instance"], RESTARTED);
    assert_eq!(w.state.0.gateways.lease_count(), 0);
    // The offer names the new instance only; the old instance can no longer be leased or polled.
    assert_eq!(
        list(&w, &w.client).await.1["gateways"][0]["instance"],
        RESTARTED
    );
    assert_eq!(
        lease_instance(&w, &w.client, &gateway, INSTANCE).await.0,
        StatusCode::NOT_FOUND
    );
    let (status, stale) = grants_for(&w, &w.gateway, INSTANCE).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(!stale.to_string().contains("key"));
    // A fresh admission for the new instance: new id, new key, and only the new worker sees it.
    let (status, fresh) = lease_instance(&w, &w.client, &gateway, RESTARTED).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fresh["instance"], RESTARTED);
    assert_ne!(fresh["lease_id"], first["lease_id"]);
    assert_ne!(fresh["key"], first["key"]);
    let polled = grants_for(&w, &w.gateway, RESTARTED).await.1;
    assert_eq!(polled["grants"][0]["id"], fresh["lease_id"]);
    // A different address, or a different voice session, also starts over.
    assert_eq!(
        register_as(&w, &w.gateway, RESTARTED, "192.168.1.20", 47001)
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        w.state.0.gateways.lease_count(),
        0,
        "a new port is a new registration"
    );
    let (_, again) = lease_instance(&w, &w.client, &gateway, RESTARTED).await;
    assert_ne!(again["key"], fresh["key"]);
    seat(&w.state, &w.gateway, 12, "vc1");
    lock(&w.state.0.voice).remove(&11);
    assert_eq!(
        register_as(&w, &w.gateway, RESTARTED, "192.168.1.20", 47001)
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        w.state.0.gateways.lease_count(),
        0,
        "a new voice session is a new registration"
    );
    // The instance is required and must be 32 lowercase hex digits that are not all zero.
    for bad in ["", "AB", &"00".repeat(16), &"AB".repeat(16)] {
        assert_eq!(
            register_as(&w, &w.gateway, bad, "192.168.1.20", 47001)
                .await
                .0,
            StatusCode::BAD_REQUEST,
            "{bad:?}"
        );
        assert_eq!(
            grants_for(&w, &w.gateway, bad).await.0,
            StatusCode::BAD_REQUEST,
            "{bad:?}"
        );
    }
    let (status, _) = request(
        &w.app,
        "POST",
        &format!("{PATH}/registration"),
        Some(&w.gateway.token),
        json!({"session_id":12,"ip":"192.168.1.20","port":47001}),
    )
    .await;
    assert!(
        status.is_client_error(),
        "a registration without an instance is refused"
    );
}

fn registration(instance: u8, port: u16) -> Registration {
    Registration {
        instance: [instance; 16],
        session_id: 11,
        channel_id: "vc1".into(),
        call_id: None,
        token_hash: "token".into(),
        address: SocketAddrV4::new("192.168.1.20".parse().unwrap(), port),
        expires: Instant::now() + REGISTRATION,
    }
}

fn client() -> Client {
    Client {
        session_id: 22,
        user: "client".into(),
        token: "client-token".into(),
        channel: "vc1".into(),
        call: None,
    }
}

#[test]
fn a_request_validated_against_an_old_instance_issues_and_publishes_nothing() {
    // The race: a lease or poll validated gateway instance A, then the worker restarted
    // as instance B before the request took the lock to issue keys.
    let mut book = Book::default();
    book.register("gw", registration(1, 47000)).unwrap();
    let stale = book.gateways["gw"].seen();
    let now = Instant::now();
    let issued = book.issue_lease(now, "gw", &stale, &client()).unwrap();
    let candidates = vec![Probe {
        id: issued.id,
        session: 22,
        user: "client".into(),
        token: "client-token".into(),
        channel: "vc1".into(),
        call: None,
    }];
    book.register("gw", registration(2, 47000)).unwrap();
    assert!(
        book.leases.is_empty(),
        "the restart cancelled the old lease"
    );
    // A stale lease request is refused and creates no lease.
    assert!(book.issue_lease(now, "gw", &stale, &client()).is_err());
    assert!(book.leases.is_empty() && book.by_session.is_empty());
    // A stale poll for A gets no grants, even if its candidate list still names the old lease.
    assert!(book
        .collect_grants(now, "gw", &[1; 16], &candidates, &[issued.id])
        .is_none());
    // The new instance can be leased, and A's poll still publishes nothing of B's grants.
    let current = book.gateways["gw"].seen();
    let fresh = book.issue_lease(now, "gw", &current, &client()).unwrap();
    assert_ne!(fresh.id, issued.id);
    assert_ne!(fresh.key, issued.key);
    assert!(book
        .collect_grants(now, "gw", &[1; 16], &[], &[fresh.id])
        .is_none());
    let grants = book
        .collect_grants(now, "gw", &[2; 16], &[], &[fresh.id])
        .unwrap();
    assert_eq!(grants.len(), 1);
    assert_eq!(grants[0].id, wire::encode_id(&fresh.id));
    // A lease of an older instance that somehow survived is never listed for the new one.
    book.leases.get_mut(&fresh.id).unwrap().gateway_instance = [1; 16];
    assert!(book
        .collect_grants(now, "gw", &[2; 16], &[], &[fresh.id])
        .unwrap()
        .is_empty());
}

#[test]
fn only_the_same_instance_address_and_session_keep_their_leases() {
    let mut book = Book::default();
    book.register("gw", registration(1, 47000)).unwrap();
    let seen = book.gateways["gw"].seen();
    let issued = book
        .issue_lease(Instant::now(), "gw", &seen, &client())
        .unwrap();
    // A renewal of the same instance keeps the lease id and key.
    book.register("gw", registration(1, 47000)).unwrap();
    let renewed = book
        .issue_lease(Instant::now(), "gw", &seen, &client())
        .unwrap();
    assert_eq!(
        (renewed.id, renewed.key.as_str()),
        (issued.id, issued.key.as_str())
    );
    for changed in [registration(2, 47000), registration(1, 47001)] {
        let mut book = Book::default();
        book.register("gw", registration(1, 47000)).unwrap();
        let seen = book.gateways["gw"].seen();
        book.issue_lease(Instant::now(), "gw", &seen, &client())
            .unwrap();
        book.register("gw", changed).unwrap();
        assert!(book.leases.is_empty());
    }
}

#[test]
fn the_request_window_table_is_bounded_at_admission() {
    let broker = Broker::default();
    for n in 0..MAX_WINDOWS {
        broker.rate(&format!("member-{n}")).unwrap();
    }
    assert_eq!(broker.window_count(), MAX_WINDOWS);
    // A new member is refused while the table is full of recent windows; it does not grow.
    let refused = broker.rate("one-too-many").unwrap_err();
    assert_eq!(refused.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(broker.window_count(), MAX_WINDOWS);
    // A tracked member keeps working (within its own budget).
    broker.rate("member-0").unwrap();
}

#[tokio::test]
async fn the_capability_is_community_only() {
    let w = world().await;
    let (status, boot) = request(
        &w.app,
        "GET",
        "/api/bootstrap",
        Some(&w.admin.token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let has = |value: &Value| {
        value["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c == "gateway-broker-v1")
    };
    assert!(has(&boot), "a community with the broker advertises it");
    // The Friends service strips it, so a client does not poll a consent-only server.
    let mut bootstrap = Bootstrap::default();
    bootstrap.capabilities = vec!["gateway-broker-v1".into(), "history".into()];
    people::snapshot(&lock(&w.state.0.db), &w.admin.user.id, &mut bootstrap);
    assert!(!bootstrap
        .capabilities
        .iter()
        .any(|c| c == "gateway-broker-v1"));
}

#[tokio::test]
async fn late_requests_of_a_finished_call_cannot_touch_the_new_seat() {
    let w = ready().await;
    let gateway = w.gateway.user.id.clone();
    let (_, old_lease) = lease(&w, &w.client, &gateway).await;
    let old_lease_id = old_lease["lease_id"].as_str().unwrap().to_string();

    // The client switches calls with the same login: seat 22 ends, seat 23 begins.
    lock(&w.state.0.voice).remove(&22);
    seat(&w.state, &w.client, 23, "vc1");
    let (status, new_lease) = lease(&w, &w.client, &gateway).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(new_lease["session_id"], 23);
    let new_lease_id = new_lease["lease_id"].as_str().unwrap().to_string();
    // The finished call's lease is still held until it expires or the gateway polls.
    assert_eq!(w.state.0.gateways.lease_count(), 2);

    // A late POST of the old call is refused and changes nothing.
    let (status, late) = lease_session(&w, &w.client, &gateway, INSTANCE, 22).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(!late.to_string().contains("key"));
    assert_eq!(w.state.0.gateways.lease_count(), 2);
    // The old call's own DELETE (old session, old lease id) removes its own lease only.
    let (status, gone) = drop_lease(&w, &w.client, 22, &old_lease_id).await;
    assert_eq!((status, &gone["dropped"]), (StatusCode::OK, &json!(true)));
    assert_eq!(w.state.0.gateways.lease_count(), 1);
    // Mixed pairs drop nothing: the new session with the old id, the old session with the new id.
    assert_eq!(
        drop_lease(&w, &w.client, 23, &old_lease_id).await.1["dropped"],
        false
    );
    assert_eq!(
        drop_lease(&w, &w.client, 22, &new_lease_id).await.1["dropped"],
        false
    );
    // Another member can never drop it either.
    assert_eq!(
        drop_lease(&w, &w.other, 23, &new_lease_id).await.1["dropped"],
        false
    );
    assert_eq!(w.state.0.gateways.lease_count(), 1);
    assert_eq!(
        grants(&w, &w.gateway).await.1["grants"][0]["id"],
        json!(new_lease_id),
        "the new call's lease survived all of it"
    );
    // Only the matching pair drops it; the id must be well formed.
    assert_eq!(
        drop_lease(&w, &w.client, 23, &new_lease_id).await.1["dropped"],
        true
    );
    assert_eq!(w.state.0.gateways.lease_count(), 0);
    assert_eq!(
        drop_lease(&w, &w.client, 23, "not-an-id").await.0,
        StatusCode::BAD_REQUEST
    );

    // The gateway switches calls the same way: seat 11 ends, seat 12 begins.
    lock(&w.state.0.voice).remove(&11);
    seat(&w.state, &w.gateway, 12, "vc1");
    // A late register of the old call is refused and changes nothing.
    let (status, _) = register_session(&w, &w.gateway, RESTARTED, 11, "192.168.1.99", 47999).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(w.state.0.gateways.gateway_count(), 1);
    // The new call registers a new worker.
    let (status, _) = register_session(&w, &w.gateway, RESTARTED, 12, "192.168.1.21", 47001).await;
    assert_eq!(status, StatusCode::OK);
    let (_, held) = lease_instance(&w, &w.client, &gateway, RESTARTED).await;
    assert_eq!(held["instance"], RESTARTED);
    // The old worker's late cleanup (old instance, old session) drops nothing.
    for (instance, session) in [(INSTANCE, 11), (INSTANCE, 12), (RESTARTED, 11)] {
        let (status, body) = drop_registration(&w, &w.gateway, instance, session).await;
        assert_eq!(
            (status, &body["dropped"]),
            (StatusCode::OK, &json!(false)),
            "{instance} {session}"
        );
    }
    assert_eq!(w.state.0.gateways.gateway_count(), 1);
    assert_eq!(
        w.state.0.gateways.lease_count(),
        1,
        "the lease on the current worker is intact"
    );
    // Another member's DELETE for the same pair drops nothing of the gateway's.
    assert_eq!(
        drop_registration(&w, &w.other, RESTARTED, 12).await.1["dropped"],
        false
    );
    assert_eq!(w.state.0.gateways.gateway_count(), 1);
    // The matching pair drops the registration and its leases.
    assert_eq!(
        drop_registration(&w, &w.gateway, RESTARTED, 12).await.1["dropped"],
        true
    );
    assert_eq!(w.state.0.gateways.gateway_count(), 0);
    assert_eq!(w.state.0.gateways.lease_count(), 0);
    // A request without the session id is not a request.
    let (status, _) = request(
        &w.app,
        "DELETE",
        &format!("{PATH}/registration?instance={RESTARTED}"),
        Some(&w.gateway.token),
        Value::Null,
    )
    .await;
    assert!(status.is_client_error());
    let (status, _) = request(
        &w.app,
        "DELETE",
        &format!("{PATH}/leases"),
        Some(&w.client.token),
        Value::Null,
    )
    .await;
    assert!(status.is_client_error(), "no blanket delete");
}

// ---------------------------------------------------------------------------
// The Friends service: call-scoped volunteers, no roles

/// A Friends service with two friends in a live, accepted private call (both seated in it),
/// and a third friend of the caller who is not in it. `gateway` is the caller, `client` the
/// friend she called, `other` the outsider; `admin` is the caller again (nobody has a role).
struct Friends {
    world: World,
    call: String,
    room: String,
}

fn friends_provider() -> account_identity::Provider {
    account_identity::Provider::new(
        "https://accounts.example.test/realms/relay-accounts",
        "relay-desktop",
        "https://people.example.test",
    )
    .unwrap()
}

fn friends_account(state: &AppState, handle: &str) -> Session {
    let db = lock(&state.0.db);
    let user = User {
        id: id(),
        username: handle.into(),
        role: "member".into(),
        ..Default::default()
    };
    db.execute(
        "INSERT INTO users VALUES(?1,?2,?3,'member')",
        params![user.id, handle, account_identity::login::ACCOUNT_ONLY],
    )
    .unwrap();
    db.execute("INSERT INTO membership(user_id) VALUES(?1)", [&user.id])
        .unwrap();
    db.execute("INSERT INTO user_profiles(user_id) VALUES(?1)", [&user.id])
        .unwrap();
    db.execute(
        "INSERT INTO linked_identities VALUES(?1,?2,?3,?4,?5)",
        params![
            user.id,
            friends_provider().public().issuer,
            format!("subject-{handle}"),
            handle,
            now()
        ],
    )
    .unwrap();
    new_session(&db, &crate::credential_seal::Seal::fixed(crate::credential_seal::TEST_KEY), user).unwrap()
}

async fn befriend(app: &Router, x: &Session, y: &Session) {
    for (from, to) in [(x, y), (y, x)] {
        let (status, _) = request(
            app,
            "POST",
            &format!("/api/friends/{}", to.user.id),
            Some(&from.token),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }
}

/// Put a participant into the voice map as the seat of a private call.
fn call_seat(state: &AppState, who: &Session, id: u64, channel: &str, call: &str) {
    seat(state, who, id, channel);
    lock(&state.0.voice).get_mut(&id).unwrap().private_call_id = Some(call.into());
}

/// Make the call accepted and durable, as an accepted call is after the answer.
fn accept_call(state: &AppState, call: &str, channel: &str, caller: &Session, recipient: &Session) {
    lock(&state.0.db)
        .execute(
            "INSERT OR REPLACE INTO private_call_recovery(id,channel_id,caller_id,recipient_id,state,join_expires,retained_until) VALUES(?1,?2,?3,?4,'accepted',?5,?5)",
            params![call, channel, caller.user.id, recipient.user.id, now() + 3600],
        )
        .unwrap();
}

async fn friends() -> Friends {
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::open_with_config(
        dir.path(),
        "Relay friends",
        0,
        ServerConfig {
            people: true,
            account_provider: Some(friends_provider()),
            ..Default::default()
        },
    )
    .unwrap();
    let app = router(state.clone());
    let ann = friends_account(&state, "ann");
    let ben = friends_account(&state, "ben");
    let cara = friends_account(&state, "cara");
    befriend(&app, &ann, &ben).await;
    befriend(&app, &ann, &cara).await;
    let (status, started) = request(
        &app,
        "POST",
        "/api/private-calls",
        Some(&ann.token),
        json!({"peer_id": ben.user.id}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{started}");
    let call = started["id"].as_str().unwrap().to_owned();
    let room = started["channel_id"].as_str().unwrap().to_owned();
    accept_call(&state, &call, &room, &ann, &ben);
    call_seat(&state, &ann, 11, &room, &call);
    call_seat(&state, &ben, 22, &room, &call);
    Friends {
        call,
        room,
        world: World {
            _dir: dir,
            state,
            app,
            admin: ann.clone(),
            gateway: ann,
            client: ben,
            other: cara,
        },
    }
}

/// The gateway (caller) registers her own seat of the call.
async fn register_call(f: &Friends) -> (StatusCode, Value) {
    register(&f.world, &f.world.gateway, "192.168.1.20", 47000).await
}

#[tokio::test]
async fn the_friends_of_a_live_call_register_lease_and_poll_without_any_role() {
    let f = friends().await;
    let w = &f.world;
    assert!(!hosting::holds_role(
        &lock(&w.state.0.db),
        &w.gateway.user.id,
        HostRole::VoiceGateway
    ));
    // Her own seat of the call registers; no offer, answer or administrator is involved.
    let (status, registered) = register_call(&f).await;
    assert_eq!(status, StatusCode::OK, "{registered}");
    assert_eq!(registered["channel_id"], f.room);
    // The friend who has also joined that call finds it and leases it.
    let (status, listed) = list(w, &w.client).await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    assert_eq!(listed["gateways"].as_array().unwrap().len(), 1);
    assert_eq!(listed["gateways"][0]["gateway_id"], w.gateway.user.id);
    let (status, first) = lease(w, &w.client, &w.gateway.user.id).await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(first["session_id"], 22);
    assert!(first["ttl_ms"].as_u64().unwrap() <= 2000);
    // A live renewal keeps id and key; the gateway sees exactly that grant and polling renews nothing.
    let (_, again) = lease(w, &w.client, &w.gateway.user.id).await;
    assert_eq!(
        (&again["lease_id"], &again["key"]),
        (&first["lease_id"], &first["key"])
    );
    let (status, polled) = grants(w, &w.gateway).await;
    assert_eq!(status, StatusCode::OK, "{polled}");
    assert_eq!(polled["grants"].as_array().unwrap().len(), 1);
    assert_eq!(polled["grants"][0]["session"], 22);
    // Registering granted her nothing: still no role.
    assert!(!hosting::holds_role(
        &lock(&w.state.0.db),
        &w.gateway.user.id,
        HostRole::VoiceGateway
    ));
    // She cannot lease her own gateway, and the client cannot register one for the caller's seat.
    assert_eq!(
        lease(w, &w.gateway, &w.gateway.user.id).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        register_session(w, &w.client, INSTANCE, 11, "192.168.1.21", 47001)
            .await
            .0,
        StatusCode::CONFLICT
    );
}

#[tokio::test]
async fn outsiders_unaccepted_or_ended_calls_and_blocked_friends_get_and_keep_nothing() {
    let f = friends().await;
    let w = &f.world;
    assert_eq!(register_call(&f).await.0, StatusCode::OK);
    // A third friend, seated in the same room as that call, is no participant of it.
    call_seat(&w.state, &w.other, 33, &f.room, &f.call);
    assert_eq!(list(w, &w.other).await.0, StatusCode::FORBIDDEN);
    assert_eq!(
        lease(w, &w.other, &w.gateway.user.id).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        register_session(w, &w.other, RESTARTED, 33, "192.168.1.30", 47002)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    // Nor is a member of the conversation or its room without any call: a seat with no call.
    lock(&w.state.0.voice).get_mut(&33).unwrap().private_call_id = None;
    assert_eq!(list(w, &w.other).await.0, StatusCode::FORBIDDEN);
    // A ringing or unknown call has no accepted admission behind it.
    lock(&w.state.0.voice).get_mut(&22).unwrap().private_call_id = Some("ringing-call".into());
    assert_eq!(list(w, &w.client).await.0, StatusCode::FORBIDDEN);
    assert_eq!(
        lease(w, &w.client, &w.gateway.user.id).await.0,
        StatusCode::NOT_FOUND
    );
    lock(&w.state.0.voice).get_mut(&22).unwrap().private_call_id = Some(f.call.clone());
    assert_eq!(
        lease(w, &w.client, &w.gateway.user.id).await.0,
        StatusCode::OK
    );
    assert_eq!(w.state.0.gateways.lease_count(), 1);
    // The call ends: nothing can be asked, the poll drops the registration and its lease.
    lock(&w.state.0.db)
        .execute(
            "UPDATE private_call_recovery SET state='ended' WHERE id=?1",
            [&f.call],
        )
        .unwrap();
    assert_eq!(
        lease(w, &w.client, &w.gateway.user.id).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(list(w, &w.client).await.0, StatusCode::FORBIDDEN);
    assert_eq!(grants(w, &w.gateway).await.0, StatusCode::FORBIDDEN);
    assert_eq!(
        (
            w.state.0.gateways.gateway_count(),
            w.state.0.gateways.lease_count()
        ),
        (0, 0)
    );

    // Blocking ends it too, even with the call still marked accepted.
    let f = friends().await;
    let w = &f.world;
    assert_eq!(register_call(&f).await.0, StatusCode::OK);
    assert_eq!(
        lease(w, &w.client, &w.gateway.user.id).await.0,
        StatusCode::OK
    );
    let (status, _) = request(
        &w.app,
        "POST",
        &format!("/api/blocks/{}", w.client.user.id),
        Some(&w.gateway.token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    accept_call(&w.state, &f.call, &f.room, &w.gateway, &w.client);
    assert_eq!(list(w, &w.client).await.0, StatusCode::FORBIDDEN);
    assert_eq!(
        lease(w, &w.client, &w.gateway.user.id).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(grants(w, &w.gateway).await.0, StatusCode::FORBIDDEN);
    assert_eq!(
        (
            w.state.0.gateways.gateway_count(),
            w.state.0.gateways.lease_count()
        ),
        (0, 0)
    );
}

#[tokio::test]
async fn a_new_call_in_the_same_room_inherits_no_registration_lease_or_grant() {
    let f = friends().await;
    let w = &f.world;
    assert_eq!(register_call(&f).await.0, StatusCode::OK);
    let (_, old) = lease(w, &w.client, &w.gateway.user.id).await;
    // The first call ends and a new one begins in the very same room, with new seats.
    lock(&w.state.0.db)
        .execute(
            "UPDATE private_call_recovery SET state='ended' WHERE id=?1",
            [&f.call],
        )
        .unwrap();
    let second = uuid::Uuid::new_v4().to_string();
    accept_call(&w.state, &second, &f.room, &w.gateway, &w.client);
    lock(&w.state.0.voice).clear();
    call_seat(&w.state, &w.gateway, 51, &f.room, &second);
    call_seat(&w.state, &w.client, 52, &f.room, &second);
    // The old registration is not offered to, nor leasable by, the new call.
    let (status, listed) = list(w, &w.client).await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    assert!(listed["gateways"].as_array().unwrap().is_empty());
    assert_eq!(
        lease(w, &w.client, &w.gateway.user.id).await.0,
        StatusCode::NOT_FOUND
    );
    // Registering again for the new call starts clean: the old lease and its key are gone.
    assert_eq!(register_call(&f).await.0, StatusCode::OK);
    assert_eq!(w.state.0.gateways.lease_count(), 0);
    assert_eq!(
        grants(w, &w.gateway).await.1["grants"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    let (status, fresh) = lease(w, &w.client, &w.gateway.user.id).await;
    assert_eq!(status, StatusCode::OK, "{fresh}");
    assert_ne!(fresh["lease_id"], old["lease_id"]);
    assert_ne!(fresh["key"], old["key"]);
    assert_eq!(fresh["session_id"], 52);
    // The first call's late cleanup cannot touch the second one's lease.
    let (_, dropped) = drop_lease(w, &w.client, 22, old["lease_id"].as_str().unwrap()).await;
    assert_eq!(dropped["dropped"], false);
    assert_eq!(w.state.0.gateways.lease_count(), 1);
}

#[tokio::test]
async fn friends_leases_bind_the_exact_login_instance_session_and_renew_only_themselves() {
    let f = friends().await;
    let w = &f.world;
    assert_eq!(register_call(&f).await.0, StatusCode::OK);
    // Another sign-in of the same account has no seat: it cannot discover or lease.
    let second = {
        let db = lock(&w.state.0.db);
        let user = w.client.user.clone();
        new_session(&db, &crate::credential_seal::Seal::fixed(crate::credential_seal::TEST_KEY), user).unwrap()
    };
    assert_eq!(list(w, &second).await.0, StatusCode::FORBIDDEN);
    assert_eq!(
        lease(w, &second, &w.gateway.user.id).await.0,
        StatusCode::FORBIDDEN
    );
    // The wrong instance or a stale session is refused; the right ones are leased.
    assert_eq!(
        lease_instance(w, &w.client, &w.gateway.user.id, RESTARTED)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        lease_session(w, &w.client, &w.gateway.user.id, INSTANCE, 999)
            .await
            .0,
        StatusCode::CONFLICT
    );
    let (_, first) = lease(w, &w.client, &w.gateway.user.id).await;
    // A restart of the worker (a new instance) cancels the lease and never reuses an id or key.
    let (status, _) = register_as(w, &w.gateway, RESTARTED, "192.168.1.20", 47000).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(w.state.0.gateways.lease_count(), 0);
    assert_eq!(
        grants_for(w, &w.gateway, INSTANCE).await.0,
        StatusCode::CONFLICT
    );
    let (_, next) = lease_instance(w, &w.client, &w.gateway.user.id, RESTARTED).await;
    assert_ne!(next["lease_id"], first["lease_id"]);
    assert_ne!(next["key"], first["key"]);
    // The gateway's poll of the new instance lists exactly that one lease.
    let (status, polled) = grants_for(w, &w.gateway, RESTARTED).await;
    assert_eq!(status, StatusCode::OK, "{polled}");
    assert_eq!(polled["grants"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn community_roles_are_unchanged_and_the_friends_routes_and_capability_stay_narrow() {
    // A community still needs the accepted role, and a call seat never counts there.
    let w = world().await;
    seat(&w.state, &w.gateway, 11, "vc1");
    assert_eq!(
        register_session(&w, &w.gateway, INSTANCE, 11, "192.168.1.20", 47000)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    grant_role(&w, &w.gateway).await;
    assert_eq!(
        register_session(&w, &w.gateway, INSTANCE, 11, "192.168.1.20", 47000)
            .await
            .0,
        StatusCode::OK
    );
    let (_, boot) = request(
        &w.app,
        "GET",
        "/api/bootstrap",
        Some(&w.admin.token),
        Value::Null,
    )
    .await;
    let has = |value: &Value, name: &str| {
        value["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c == name)
    };
    assert!(has(&boot, "gateway-broker-v1") && !has(&boot, FRIENDS_CAPABILITY));

    // The Friends service announces only its own capability, and only these exact routes.
    let f = friends().await;
    let (_, boot) = request(
        &f.world.app,
        "GET",
        "/api/bootstrap",
        Some(&f.world.gateway.token),
        Value::Null,
    )
    .await;
    assert!(has(&boot, "friends-gateway-broker-v1") && !has(&boot, "gateway-broker-v1"));
    for (method, path, allowed) in [
        ("GET", "/api/hosting/voice-gateways", true),
        ("POST", "/api/hosting/voice-gateways/registration", true),
        ("DELETE", "/api/hosting/voice-gateways/registration", true),
        ("GET", "/api/hosting/voice-gateways/grants", true),
        ("POST", "/api/hosting/voice-gateways/leases", true),
        ("DELETE", "/api/hosting/voice-gateways/leases", true),
        ("PUT", "/api/hosting/voice-gateways/registration", false),
        ("POST", "/api/hosting/voice-gateways", false),
        ("GET", "/api/hosting/voice-gateways/leases", false),
        ("POST", "/api/hosting/voice-gateways/grants", false),
        ("GET", "/api/hosting/voice-gateways/other", false),
        ("GET", "/api/hosting/voice-gateways/", false),
        ("POST", "/api/hosting/offers", false),
        ("POST", "/api/hosting/answer", false),
        ("GET", "/api/hosting/handover", false),
        ("POST", "/api/hosting/handover/route", false),
        ("GET", "/api/hosting/hosts", false),
    ] {
        assert_eq!(
            friends_route(
                path,
                &axum::http::Method::from_bytes(method.as_bytes()).unwrap()
            ),
            allowed,
            "{method} {path}"
        );
        // Through the real guard: the ones which are not admitted are refused outright.
        if !allowed {
            let (status, _) = request(
                &f.world.app,
                method,
                path,
                Some(&f.world.gateway.token),
                json!({}),
            )
            .await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path}");
        }
    }
}

// ---------------------------------------------------------------------------
// The voice mesh between homes

const ENDPOINT_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const ENDPOINT_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

async fn mesh_report(
    w: &World,
    who: &Session,
    endpoint: &str,
    upload: Option<u32>,
    applied: Option<u64>,
    peer: Option<&Session>,
) -> (StatusCode, Value) {
    let links = match peer {
        Some(peer) => json!([{"gateway_id":peer.user.id,
            "quality":{"rtt_ms":30,"loss_permille":0,"jitter_ms":2},"up":true}]),
        None => json!([]),
    };
    request(
        &w.app,
        "POST",
        &format!("{PATH}/mesh"),
        Some(&who.token),
        json!({"instance":INSTANCE,"session_id":session_of(w, who),"port":48000,
            "endpoint_id":endpoint,"upload_kbps":upload,"host_rtt_ms":40,
            "applied_epoch":applied,"links":links}),
    )
    .await
}

async fn mesh_member(w: &World, who: &Session, healthy: bool) -> (StatusCode, Value) {
    request(
        &w.app,
        "POST",
        &format!("{PATH}/mesh/member"),
        Some(&who.token),
        json!({"session_id":session_of(w, who),"healthy":healthy}),
    )
    .await
}

/// Two homes in room one: Gus's (with Cleo on his LAN lease) and Otto's (with
/// Anna on his), both with measured upload and a working link to each other.
async fn two_homes() -> World {
    let w = ready().await;
    grant_role(&w, &w.other).await;
    seat(&w.state, &w.other, 33, "vc1");
    seat(&w.state, &w.admin, 44, "vc1");
    let (status, body) = register(&w, &w.other, "192.168.2.20", 47000).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = lease(&w, &w.client, &w.gateway.user.id).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = lease(&w, &w.admin, &w.other.user.id).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    w
}

/// Both gateways report with the plan applied and everyone reports a working hop.
async fn carried(w: &World) -> u64 {
    mesh_report(w, &w.gateway, ENDPOINT_A, Some(4000), None, Some(&w.other)).await;
    let (_, b) = mesh_report(w, &w.other, ENDPOINT_B, Some(4000), None, Some(&w.gateway)).await;
    let epoch = b["plan"]["epoch"].as_u64().unwrap();
    for who in [&w.gateway, &w.client, &w.other, &w.admin] {
        let (status, body) = mesh_member(w, who, true).await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    mesh_report(
        w,
        &w.gateway,
        ENDPOINT_A,
        Some(4000),
        Some(epoch),
        Some(&w.other),
    )
    .await;
    let (_, b) = mesh_report(
        w,
        &w.other,
        ENDPOINT_B,
        Some(4000),
        Some(epoch),
        Some(&w.gateway),
    )
    .await;
    assert_eq!(b["plan"]["epoch"].as_u64(), Some(epoch), "the plan settled");
    epoch
}

#[tokio::test]
async fn two_homes_get_one_bound_plan_a_shared_link_key_and_their_own_members() {
    let w = two_homes().await;
    // Gus alone first: a plan of his home only.
    let (status, alone) = mesh_report(&w, &w.gateway, ENDPOINT_A, Some(4000), None, None).await;
    assert_eq!(status, StatusCode::OK, "{alone}");
    assert_eq!(alone["plan"]["nodes"].as_array().unwrap().len(), 1);
    let (_, b) = mesh_report(&w, &w.other, ENDPOINT_B, Some(4000), None, Some(&w.gateway)).await;
    let (_, a) = mesh_report(&w, &w.gateway, ENDPOINT_A, Some(4000), None, Some(&w.other)).await;
    let (_, b2) = mesh_report(&w, &w.other, ENDPOINT_B, Some(4000), None, Some(&w.gateway)).await;
    assert_eq!(a["plan"]["nodes"].as_array().unwrap().len(), 2, "{a}");
    assert_eq!(a["plan"], b2["plan"], "one plan per channel");
    let _ = b;
    let b = b2;
    let community = server_id(&lock(&w.state.0.db)).unwrap();
    for x in [&a, &b] {
        let binding = &x["approval"]["binding"];
        assert_eq!(binding["community_id"], community.as_str());
        assert_eq!(binding["channel_id"], "vc1");
        assert_ne!(binding["media_session"], 0);
        assert_eq!(x["links"].as_array().unwrap().len(), 1);
    }
    // Both ends of the link get the same id and key and each other's iroh endpoint.
    assert_eq!(a["links"][0]["id"], b["links"][0]["id"]);
    assert_eq!(a["links"][0]["key"], b["links"][0]["key"]);
    assert_eq!(a["links"][0]["endpoint_id"], ENDPOINT_B);
    assert_eq!(b["links"][0]["endpoint_id"], ENDPOINT_A);
    assert_ne!(a["approval"]["node"], b["approval"]["node"]);
    // Cleo, on Gus's LAN lease, gets her place at Gus's gateway and her own key,
    // and Gus then serves exactly that lease.
    let (status, cleo) = mesh_member(&w, &w.client, true).await;
    assert_eq!(status, StatusCode::OK, "{cleo}");
    assert_eq!(cleo["gateway_id"], w.gateway.user.id.as_str());
    assert_eq!(cleo["ip"], "192.168.1.20");
    assert_eq!(cleo["port"], 48000);
    assert_eq!(cleo["node"], a["approval"]["node"]);
    let (_, a) = mesh_report(&w, &w.gateway, ENDPOINT_A, Some(4000), None, Some(&w.other)).await;
    assert!(a["members"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["id"] == cleo["lease_id"]
            && m["key"] == cleo["key"]
            && m["stream"] == cleo["stream"]));
    let (_, b) = mesh_report(&w, &w.other, ENDPOINT_B, Some(4000), None, Some(&w.gateway)).await;
    assert!(
        b["members"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["id"] != cleo["lease_id"]),
        "a gateway never receives another home's member keys"
    );
}

fn sealed_voice(session: u64, sequence: u64, payload: &[u8]) -> Vec<u8> {
    use chacha20poly1305::{
        aead::{Aead, Payload},
        ChaCha20Poly1305, KeyInit, Nonce,
    };
    let mut header = session.to_le_bytes().to_vec();
    header.extend(sequence.to_le_bytes());
    let mut nonce = [0u8; 12];
    nonce[4..].copy_from_slice(&sequence.to_le_bytes());
    let mut out = header.clone();
    out.extend(
        ChaCha20Poly1305::new_from_slice(&[7; 32])
            .unwrap()
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: payload,
                    aad: &header,
                },
            )
            .unwrap(),
    );
    out
}

#[tokio::test]
async fn the_host_skips_a_pair_only_while_the_whole_mesh_carries_it() {
    let w = two_homes().await;
    assert!(!w.state.0.gateways.mesh_carries(22, 44));
    // Plans and keys alone are not enough: the epoch must be applied and the hops healthy.
    mesh_report(&w, &w.gateway, ENDPOINT_A, Some(4000), None, Some(&w.other)).await;
    mesh_report(&w, &w.other, ENDPOINT_B, Some(4000), None, Some(&w.gateway)).await;
    mesh_member(&w, &w.client, true).await;
    mesh_member(&w, &w.admin, true).await;
    assert!(!w.state.0.gateways.mesh_carries(22, 44));
    carried(&w).await;
    assert!(w.state.0.gateways.mesh_carries(22, 44));
    assert!(w.state.0.gateways.mesh_carries(11, 33));
    // The voice relay sends Cleo's frame only to people outside the mesh.
    let lena = signup(&w.app, "Lena", &new_invite(&w.app, &w.admin).await).await;
    seat(&w.state, &lena, 55, "vc1");
    let address = |port: u16| std::net::SocketAddr::from(([10, 0, 0, 1], 40000 + port));
    {
        let mut seats = lock(&w.state.0.voice);
        for (id, port) in [(11u64, 1u16), (22, 2), (33, 3), (44, 4), (55, 5)] {
            seats.get_mut(&id).unwrap().remote = Some(address(port));
        }
    }
    let mut frame = vec![relay_protocol::AUDIO_PACKET_V2];
    frame.extend([0; relay_protocol::AUDIO_V2_HEADER_LEN + 20]);
    let out = voice::relay_packet(&w.state, address(2), &sealed_voice(22, 900, &frame));
    let to: Vec<u16> = out.iter().map(|(a, _)| a.port() - 40000).collect();
    assert_eq!(
        to,
        vec![5],
        "only Lena, who is not in the mesh, gets it from the host"
    );
    // Cleo's hop stops working: the host delivers to everyone again at once.
    mesh_member(&w, &w.client, false).await;
    assert!(!w.state.0.gateways.mesh_carries(22, 44));
    let out = voice::relay_packet(&w.state, address(2), &sealed_voice(22, 901, &frame));
    assert_eq!(out.len(), 4);
}

#[tokio::test]
async fn revoking_a_role_or_a_silent_gateway_ends_the_mesh_at_once() {
    let w = two_homes().await;
    carried(&w).await;
    assert!(w.state.0.gateways.mesh_carries(22, 44));
    w.state.0.gateways.cancel_user(&w.other.user.id);
    assert!(
        !w.state.0.gateways.mesh_carries(22, 44),
        "revocation is immediate"
    );
    assert!(
        !w.state.0.gateways.mesh_carries(11, 22),
        "an incomplete tree carries nothing"
    );
    let (status, _) =
        mesh_report(&w, &w.other, ENDPOINT_B, Some(4000), None, Some(&w.gateway)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (_, a) = mesh_report(&w, &w.gateway, ENDPOINT_A, Some(4000), None, Some(&w.other)).await;
    assert_eq!(a["plan"]["nodes"].as_array().unwrap().len(), 1);
    assert!(a["links"].as_array().unwrap().is_empty());
    let (_, anna) = mesh_member(&w, &w.admin, true).await;
    assert!(anna.is_null(), "Anna's home is gone; the host carries her");
    // A gateway that stops reporting lapses too.
    let w = two_homes().await;
    carried(&w).await;
    w.state.0.gateways.age_mesh_reports();
    mesh_member(&w, &w.client, true).await;
    assert!(!w.state.0.gateways.mesh_carries(22, 44));
}

#[tokio::test]
async fn other_rooms_unmeasured_upload_and_stale_instances_get_no_mesh() {
    let w = two_homes().await;
    // Without measured upload no link between homes is used.
    mesh_report(&w, &w.gateway, ENDPOINT_A, None, None, Some(&w.other)).await;
    let (_, b) = mesh_report(&w, &w.other, ENDPOINT_B, None, None, Some(&w.gateway)).await;
    assert!(b["links"].as_array().unwrap().is_empty(), "{b}");
    assert!(b["plan"]["edges"].as_array().is_none_or(|e| e.is_empty()));
    // A report for another instance or a privileged port is refused.
    let (status, _) = request(
        &w.app,
        "POST",
        &format!("{PATH}/mesh"),
        Some(&w.gateway.token),
        json!({"instance":RESTARTED,"session_id":11,"port":48000}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = request(
        &w.app,
        "POST",
        &format!("{PATH}/mesh"),
        Some(&w.gateway.token),
        json!({"instance":INSTANCE,"session_id":11,"port":80}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // Someone in another room never gets a place in room one's mesh.
    let lena = signup(&w.app, "Lena", &new_invite(&w.app, &w.admin).await).await;
    seat(&w.state, &lena, 66, "vc2");
    let (status, other_room) = mesh_member(&w, &lena, true).await;
    assert_eq!(status, StatusCode::OK);
    assert!(other_room.is_null());
    assert!(!w.state.0.gateways.mesh_carries(66, 22));
    // A member who is not a registered gateway cannot report as one.
    let (status, _) = mesh_report(&w, &w.client, ENDPOINT_A, Some(4000), None, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}
