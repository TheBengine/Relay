use super::*;
use crate::tests::{new_invite, request, signup};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

const CONTROL: &str = "fixture-control-0123456789-0123456789";
const TOKEN: &str = "fixture-bot-token-never-sent-to-discord";

pub(crate) async fn host_request(
    state: &AppState,
    session: &str,
    method: &str,
    body: Value,
) -> (StatusCode, Value) {
    call(
        state,
        session,
        method,
        body.to_string(),
        Some("127.0.0.1:1"),
        &[],
    )
    .await
}

async fn call(
    state: &AppState,
    session: &str,
    method: &str,
    body: String,
    peer: Option<&str>,
    changes: &[(&str, &str)],
) -> (StatusCode, Value) {
    let mut headers = vec![
        ("x-relay-local-control".into(), CONTROL.into()),
        ("x-relay-server-id".into(), state.server_id().unwrap()),
        ("x-relay-instance-id".into(), state.instance_id().to_owned()),
    ];
    for (name, value) in changes {
        headers.retain(|(n, _): &(String, String)| n != name);
        if !value.is_empty() {
            headers.push(((*name).into(), (*value).into()));
        }
    }
    let mut req = axum::http::Request::builder()
        .method(method)
        .uri("/api/local/discord-credentials")
        .header("host", "127.0.0.1:4580")
        .header("Authorization", format!("Bearer {session}"))
        .header("Content-Type", "application/json");
    for (name, value) in headers {
        req = req.header(name, value);
    }
    if let Some(peer) = peer {
        req = req.extension(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
    }
    let result = router(state.clone())
        .oneshot(req.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let code = result.status();
    let bytes = result.into_body().collect().await.unwrap().to_bytes();
    (code, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

fn state(temp: &tempfile::TempDir) -> AppState {
    AppState::open_with_config(
        temp.path(),
        "fixture",
        0,
        ServerConfig {
            local_control_token: Some(CONTROL.into()),
            ..Default::default()
        },
    )
    .unwrap()
}

#[tokio::test]
async fn secret_api_requires_direct_host_and_all_proofs_before_decoding() {
    let temp = tempfile::tempdir().unwrap();
    let state = state(&temp);
    let admin = signup(&router(state.clone()), "Admin", "").await;
    for (peer, changed, expected) in [
        (Some("192.0.2.2:1"), vec![], StatusCode::FORBIDDEN),
        (None, vec![], StatusCode::FORBIDDEN),
        (
            Some("127.0.0.1:1"),
            vec![("x-relay-local-control", "")],
            StatusCode::FORBIDDEN,
        ),
        (
            Some("127.0.0.1:1"),
            vec![("x-relay-local-control", "wrong")],
            StatusCode::FORBIDDEN,
        ),
        (
            Some("127.0.0.1:1"),
            vec![("origin", "http://localhost")],
            StatusCode::FORBIDDEN,
        ),
        (
            Some("127.0.0.1:1"),
            vec![("forwarded", "for=127.0.0.1")],
            StatusCode::FORBIDDEN,
        ),
        (
            Some("127.0.0.1:1"),
            vec![("x-forwarded-for", "127.0.0.1")],
            StatusCode::FORBIDDEN,
        ),
        (
            Some("127.0.0.1:1"),
            vec![("x-relay-instance-id", "old")],
            StatusCode::CONFLICT,
        ),
        (
            Some("127.0.0.1:1"),
            vec![("x-relay-server-id", "other")],
            StatusCode::CONFLICT,
        ),
    ] {
        assert_eq!(
            call(
                &state,
                &admin.token,
                "PUT",
                "invalid JSON secret".into(),
                peer,
                &changed
            )
            .await
            .0,
            expected
        );
    }
    assert_eq!(
        host_request(&state, &admin.token, "GET", Value::Null)
            .await
            .0,
        StatusCode::OK
    );
    assert!(matches!(
        lock(&state.0.discord_credential).loaded,
        Loaded::Absent
    ));
}

#[tokio::test]
async fn host_proof_does_not_replace_current_admin_or_session() {
    let temp = tempfile::tempdir().unwrap();
    let state = state(&temp);
    let app = router(state.clone());
    let admin = signup(&app, "Admin", "").await;
    let member = signup(&app, "Member", &new_invite(&app, &admin).await).await;
    assert_eq!(
        call(
            &state,
            &member.token,
            "PUT",
            "bad JSON".into(),
            Some("127.0.0.1:1"),
            &[]
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let auth = state.authenticate_token(&admin.token).unwrap();
    request(&app, "POST", "/api/logout", Some(&admin.token), json!({})).await;
    assert_eq!(
        host_request(&state, &admin.token, "PUT", json!({"token":TOKEN}))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert!(current_admin(&lock(&state.0.db), &HostAccess { auth }).is_err());
}

#[tokio::test]
async fn saving_replacing_and_removing_changes_revision_without_sharing_or_secret_response() {
    let temp = tempfile::tempdir().unwrap();
    let state = state(&temp);
    let app = router(state.clone());
    let admin = signup(&app, "Admin", "").await;
    let mut changes = state.0.discord_changes.subscribe();
    let (_, status) = host_request(&state, &admin.token, "PUT", json!({"token":TOKEN})).await;
    assert_eq!(status["source"], "saved");
    assert!(!status.to_string().contains(TOKEN));
    assert!(changes.has_changed().unwrap());
    changes.borrow_and_update();
    let epoch = lock(&state.0.discord_credential).revision();
    let next = "replacement-fixture-token-never-external";
    assert_eq!(
        host_request(&state, &admin.token, "PUT", json!({"token":next}))
            .await
            .0,
        StatusCode::OK
    );
    assert!(lock(&state.0.discord_credential).revision() > epoch);
    assert_eq!(
        lock(&state.0.discord_credential)
            .current()
            .0
            .unwrap()
            .as_str(),
        next
    );
    let (_, policy) = request(&app, "GET", "/api/discord", Some(&admin.token), Value::Null).await;
    assert_eq!(policy["settings"]["enabled"], false);
    assert!(!policy.to_string().contains(next));
    let metadata: String = lock(&state.0.db)
        .prepare("SELECT value FROM metadata")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert!(!metadata.contains(TOKEN) && !metadata.contains(next));
    assert_eq!(
        host_request(&state, &admin.token, "DELETE", Value::Null)
            .await
            .1["source"],
        "removed"
    );
    assert!(lock(&state.0.discord_credential).current().0.is_none());
}

#[tokio::test]
async fn oversized_unknown_or_invalid_secret_input_is_not_stored() {
    let temp = tempfile::tempdir().unwrap();
    let state = state(&temp);
    let app = router(state.clone());
    let admin = signup(&app, "Admin", "").await;
    assert_eq!(
        host_request(
            &state,
            &admin.token,
            "PUT",
            json!({"token":TOKEN,"extra":true})
        )
        .await
        .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        host_request(
            &state,
            &admin.token,
            "PUT",
            json!({"token":"Bot token with spaces"})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        host_request(
            &state,
            &admin.token,
            "PUT",
            json!({"token":"a".repeat(6000)})
        )
        .await
        .0,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    assert!(matches!(
        lock(&state.0.discord_credential).loaded,
        Loaded::Unloaded
    ));
}

#[test]
fn exact_vault_record_binding_and_tombstone_override_legacy_environment_after_reload() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let id = Uuid::new_v4().to_string();
    let mut store = Store::new(a.path(), id.clone()).unwrap();
    assert!(store.target.starts_with("Relay/discord-host/v1/"));
    assert_ne!(
        store.target,
        Store::new(b.path(), id.clone()).unwrap().target
    );
    assert_ne!(
        store.target,
        Store::new(a.path(), Uuid::new_v4().to_string())
            .unwrap()
            .target
    );
    store.environment(Some(TOKEN.into()));
    assert_eq!(store.status().source, "environment");
    store.write(None).unwrap();
    let tombstone = store.backing.clone();
    assert!(!std::str::from_utf8(tombstone.as_ref().unwrap())
        .unwrap()
        .contains(TOKEN));
    let mut reopened = Store::new(a.path(), id).unwrap();
    reopened.backing = tombstone;
    reopened.environment(Some(TOKEN.into()));
    assert_eq!(reopened.status().source, "removed");
    assert!(reopened.current().0.is_none());
    reopened.backing = Some(Zeroizing::new(b"invalid vault record".to_vec()));
    reopened.retry();
    assert!(reopened.status().error.is_some());
    assert!(reopened.current().0.is_none());
    reopened.backing = store.backing.clone();
    reopened.path_hash = "different".into();
    reopened.retry();
    assert!(reopened.status().error.is_some());
    assert!(reopened.current().0.is_none());
}
