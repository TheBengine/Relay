//! The pull routes through the real router: tokens, binding, staging, limits,
//! invalidation, revocation and a complete pull applied to a replica store.
use super::export::{assemble, parse_part, Export, FileRef, Manifest};
use super::store::Store;
use super::*;
use crate::tests::{new_invite, request, signup};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use tower::ServiceExt;

struct Hub {
    _dir: tempfile::TempDir,
    state: AppState,
    app: Router,
    admin: Session,
    bo: Session,
    cy: Session,
    server_id: String,
}

async fn hub() -> Hub {
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::open(dir.path(), "Replica", 0).unwrap();
    let app = router(state.clone());
    let admin = signup(&app, "Anna", "").await;
    let bo = signup(&app, "Bo", &new_invite(&app, &admin).await).await;
    let cy = signup(&app, "Cy", &new_invite(&app, &admin).await).await;
    let server_id = community::server_id(&lock(&state.0.db)).unwrap();
    {
        let db = lock(&state.0.db);
        db.execute_batch(&format!(
            "INSERT INTO channels(id,name,kind,topic,visibility) VALUES
                ('pub','general','text','','public'),('priv','secret','text','','private');
             INSERT INTO messages(id,channel_id,author_id,content,created_at,created) VALUES
                ('m1','pub','{a}','hello world','t',1),('m2','priv','{a}','private talk','t',2);
             INSERT INTO message_order(message_id) VALUES('m1'),('m2');
             INSERT INTO uploads(id,owner_id,message_id,name,mime,size,created,data) VALUES
                ('u1','{a}','m1','a.txt','text/plain',5,1,CAST('HELLO' AS BLOB)),
                ('u2','{a}','m2','b.txt','text/plain',6,1,CAST('SECRET' AS BLOB));",
            a = admin.user.id
        ))
        .unwrap();
    }
    Hub {
        _dir: dir,
        state,
        app,
        admin,
        bo,
        cy,
        server_id,
    }
}

/// An administrator offers the role and the member accepts for themselves.
async fn grant(h: &Hub, member: &Session) {
    let (status, body) = request(
        &h.app,
        "POST",
        "/api/hosting/offers",
        Some(&h.admin.token),
        json!({"user_id":member.user.id,"roles":["replica"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = request(
        &h.app,
        "POST",
        "/api/hosting/answer",
        Some(&member.token),
        json!({"accepted":["replica"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

async fn mint(h: &Hub, member: &Session) -> String {
    let (status, body) = request(
        &h.app,
        "POST",
        "/api/replica/token",
        Some(&member.token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["server_id"], h.server_id);
    body["token"].as_str().unwrap().to_string()
}

async fn raw(
    h: &Hub,
    method: &str,
    path: &str,
    token: Option<&str>,
    server: Option<&str>,
    body: Value,
) -> (StatusCode, axum::http::HeaderMap, axum::body::Bytes) {
    let mut builder = axum::http::Request::builder()
        .method(method)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    if let Some(server) = server {
        builder = builder.header("x-relay-server-id", server);
    }
    let response = h
        .app
        .clone()
        .oneshot(
            builder
                .body(axum::body::Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let (status, headers) = (response.status(), response.headers().clone());
    (
        status,
        headers,
        response.into_body().collect().await.unwrap().to_bytes(),
    )
}

async fn pull(
    h: &Hub,
    method: &str,
    path: &str,
    token: &str,
    body: Value,
) -> (StatusCode, axum::http::HeaderMap, axum::body::Bytes) {
    raw(h, method, path, Some(token), Some(&h.server_id), body).await
}

fn json_of(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).unwrap_or(Value::Null)
}

async fn start(h: &Hub, token: &str, after: Option<(i64, &str)>, prev: &[String]) -> String {
    let mut body = json!({"replica_id":"rep-1","previous_eligible":prev});
    if let Some((after, lineage)) = after {
        body["after"] = json!(after);
        body["lineage"] = json!(lineage);
    }
    let (status, _, bytes) = pull(h, "POST", "/api/replica/pull/exports", token, body).await;
    assert_eq!(
        status,
        StatusCode::ACCEPTED,
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    json_of(&bytes)["export_id"].as_str().unwrap().to_string()
}

async fn ready(h: &Hub, token: &str, id: &str) -> Manifest {
    for _ in 0..500 {
        let (status, _, bytes) = pull(
            h,
            "GET",
            &format!("/api/replica/pull/exports/{id}"),
            token,
            Value::Null,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "{}",
            String::from_utf8_lossy(&bytes)
        );
        if let Ok(manifest) = serde_json::from_slice::<Manifest>(&bytes) {
            assert_eq!(manifest.export_id, id);
            return manifest;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    panic!("the export never became ready");
}

/// A whole pull as a replica agent would do it, parts verified one by one.
async fn pull_export(h: &Hub, token: &str, after: Option<(i64, &str)>, prev: &[String]) -> Export {
    let id = start(h, token, after, prev).await;
    let manifest = ready(h, token, &id).await;
    let mut parts = Vec::new();
    for index in 0..manifest.parts.len() {
        let (status, headers, bytes) = pull(
            h,
            "GET",
            &format!("/api/replica/pull/exports/{id}/parts/{index}"),
            token,
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            headers["x-relay-sha256"].to_str().unwrap(),
            manifest.parts[index].sha256
        );
        parts.push(parse_part(&manifest, index, &bytes).expect("part matches its manifest entry"));
    }
    assemble(manifest, parts).unwrap()
}

fn store(h: &Hub, user: &str) -> Store {
    Store::new(
        Connection::open_in_memory().unwrap(),
        &h.server_id,
        user,
        "rep-1",
    )
    .unwrap()
}

// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_pull_token_is_the_members_own_and_ends_with_the_role() {
    let h = hub().await;
    let enabled = || replica::enable(&lock(&h.state.0.db)).unwrap();
    enabled();
    // Nothing before the member has accepted.
    let (status, _) = request(
        &h.app,
        "POST",
        "/api/replica/token",
        Some(&h.bo.token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "no offer");
    request(
        &h.app,
        "POST",
        "/api/hosting/offers",
        Some(&h.admin.token),
        json!({"user_id":h.bo.user.id,"roles":["replica"]}),
    )
    .await;
    let (status, _) = request(
        &h.app,
        "POST",
        "/api/replica/token",
        Some(&h.bo.token),
        Value::Null,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "an offer that was not accepted grants nothing"
    );
    request(
        &h.app,
        "POST",
        "/api/hosting/answer",
        Some(&h.bo.token),
        json!({"accepted":["replica"]}),
    )
    .await;
    // Nobody can mint for someone else, and an administrator without the role cannot either.
    let (status, _) = request(
        &h.app,
        "POST",
        "/api/replica/token",
        Some(&h.admin.token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let token = mint(&h, &h.bo).await;
    assert_eq!(token.len(), 43);
    // Stored only as a hash.
    let stored: String = lock(&h.state.0.db)
        .query_row("SELECT token_hash FROM replica_tokens", [], |r| r.get(0))
        .unwrap();
    assert_eq!(stored, token_hash(&token));
    assert_ne!(stored, token);

    // Binding: the token, the named server and the route namespace.
    let (status, _, bytes) = pull(&h, "GET", "/api/replica/pull/info", &token, Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    let info = json_of(&bytes);
    assert_eq!(info["server_id"], h.server_id);
    assert_eq!(info["replica_format"], 1);
    assert_eq!(info["schema_version"], community::SCHEMA_VERSION);
    for (token, server) in [
        (None, Some(h.server_id.as_str())),
        (Some(token.as_str()), None),
        (Some(token.as_str()), Some("another-server")),
        (Some(h.bo.token.as_str()), Some(h.server_id.as_str())),
        (Some("not-a-token"), Some(h.server_id.as_str())),
    ] {
        let (status, _, _) = raw(
            &h,
            "GET",
            "/api/replica/pull/info",
            token,
            server,
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{token:?} {server:?}");
    }
    // The pull token opens no ordinary route.
    for path in ["/api/hosting", "/api/channels", "/api/replica/token"] {
        let method = if path == "/api/replica/token" {
            "POST"
        } else {
            "GET"
        };
        let (status, _, _) = raw(
            &h,
            method,
            path,
            Some(&token),
            Some(&h.server_id),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{path}");
    }

    // Rotation replaces the token at once.
    let second = mint(&h, &h.bo).await;
    assert_ne!(second, token);
    assert_eq!(
        pull(&h, "GET", "/api/replica/pull/info", &token, Value::Null)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        pull(&h, "GET", "/api/replica/pull/info", &second, Value::Null)
            .await
            .0,
        StatusCode::OK
    );

    // The member can revoke their own token.
    let (status, _) = request(
        &h.app,
        "DELETE",
        "/api/replica/token",
        Some(&h.bo.token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        pull(&h, "GET", "/api/replica/pull/info", &second, Value::Null)
            .await
            .0,
        StatusCode::FORBIDDEN
    );

    // Withdrawal by an administrator ends access and deletes the token.
    let third = mint(&h, &h.bo).await;
    let (status, _) = request(
        &h.app,
        "DELETE",
        &format!("/api/hosting/candidates/{}", h.bo.user.id),
        Some(&h.admin.token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        pull(&h, "GET", "/api/replica/pull/info", &third, Value::Null)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let rows: i64 = lock(&h.state.0.db)
        .query_row("SELECT COUNT(*) FROM replica_tokens", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 0);

    // Stepping down does the same; so does leaving the community.
    grant(&h, &h.bo).await;
    let fourth = mint(&h, &h.bo).await;
    request(
        &h.app,
        "POST",
        "/api/hosting/answer",
        Some(&h.bo.token),
        json!({"accepted":[]}),
    )
    .await;
    assert_eq!(
        pull(&h, "GET", "/api/replica/pull/info", &fourth, Value::Null)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    grant(&h, &h.bo).await;
    let fifth = mint(&h, &h.bo).await;
    lock(&h.state.0.db)
        .execute(
            "UPDATE membership SET status='left' WHERE user_id=?1",
            [&h.bo.user.id],
        )
        .unwrap();
    assert_eq!(
        pull(&h, "GET", "/api/replica/pull/info", &fifth, Value::Null)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn a_full_pull_over_http_is_applied_and_a_later_delta_equals_a_fresh_snapshot() {
    let h = hub().await;
    replica::enable(&lock(&h.state.0.db)).unwrap();
    grant(&h, &h.bo).await;
    let token = mint(&h, &h.bo).await;

    let first = pull_export(&h, &token, None, &[]).await;
    assert!(first.manifest.eligible.contains(&"pub".to_string()));
    assert!(!first.manifest.eligible.contains(&"priv".to_string()));
    assert!(!first.manifest.promotion_eligible);
    assert!(first
        .manifest
        .parts
        .iter()
        .all(|p| p.bytes <= 8 * 1024 * 1024));
    let mut live = store(&h, &h.bo.user.id);
    let applied = live.apply(&first, now_ms()).unwrap();
    assert_eq!(
        applied.files_pending, 1,
        "only the file of a readable message is listed"
    );

    // Fetch the listed file and verify it before storing it.
    let id = start(&h, &token, None, &[]).await;
    let manifest = ready(&h, &token, &id).await;
    let file = manifest.files[0].clone();
    assert_eq!((file.table.as_str(), file.id.as_str()), ("uploads", "u1"));
    let path = format!(
        "/api/replica/pull/exports/{id}/files/{}/{}?version={}&size={}",
        file.table, file.id, file.version, file.size
    );
    let (status, headers, bytes) = pull(&h, "GET", &path, &token, Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(&bytes[..], b"HELLO");
    live.accept_file(&file, headers["x-relay-sha256"].to_str().unwrap(), &bytes)
        .unwrap();
    assert!(live.files_pending().unwrap().is_empty());
    // A file of an unreadable message is neither listed nor served, even by name.
    let hidden = format!("/api/replica/pull/exports/{id}/files/uploads/u2?version=0&size=6");
    assert_eq!(
        pull(&h, "GET", &hidden, &token, Value::Null).await.0,
        StatusCode::NOT_FOUND
    );
    // A changed file is refused instead of served as another version (410).
    lock(&h.state.0.db)
        .execute(
            "UPDATE uploads SET data=CAST('CHANGED' AS BLOB),size=7 WHERE id='u1'",
            [],
        )
        .unwrap();
    assert_eq!(
        pull(&h, "GET", &path, &token, Value::Null).await.0,
        StatusCode::GONE,
        "a file changed after the manifest is refused, never served as another version"
    );

    // Changes, then a catch-up from the cursor.
    {
        let db = lock(&h.state.0.db);
        db.execute_batch(&format!(
            "INSERT INTO messages(id,channel_id,author_id,content,created_at,created) VALUES('m3','pub','{a}','news','t',3);
             INSERT INTO message_order(message_id) VALUES('m3');
             DELETE FROM messages WHERE id='m1';
             INSERT INTO channel_permissions VALUES('priv','user','{b}',3);
             INSERT INTO categories VALUES('k','Cat',0);",
            a = h.admin.user.id,
            b = h.bo.user.id
        ))
        .unwrap();
    }
    let cursor = live.cursor().unwrap().unwrap();
    let previous: Vec<String> = cursor.eligible.iter().cloned().collect();
    let delta = pull_export(&h, &token, Some((cursor.head, &cursor.lineage)), &previous).await;
    assert_eq!(delta.manifest.after, Some(cursor.head));
    assert!(["priv", "pub"]
        .iter()
        .all(|id| delta.manifest.eligible.contains(&id.to_string())));
    live.apply(&delta, now_ms()).unwrap();
    let mut fresh = store(&h, &h.bo.user.id);
    fresh
        .apply(&pull_export(&h, &token, None, &[]).await, now_ms())
        .unwrap();
    assert_eq!(
        live.dump(),
        fresh.dump(),
        "a catch-up over HTTP equals a fresh snapshot"
    );
    assert!(
        format!("{:?}", live.dump()).contains("private talk"),
        "access to the private channel arrived"
    );
}

#[tokio::test]
async fn building_an_export_never_waits_for_the_global_database_lock() {
    let h = hub().await;
    replica::enable(&lock(&h.state.0.db)).unwrap();
    grant(&h, &h.bo).await;
    let token = mint(&h, &h.bo).await;
    let id = start(&h, &token, None, &[]).await;
    // Hold the lock every ordinary request and every call needs; the build must still finish.
    let guard = lock(&h.state.0.db);
    let mut phase = None;
    for _ in 0..250 {
        phase = h.state.0.replica.phase_of(&id);
        if phase == Some("ready") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    drop(guard);
    assert_eq!(phase, Some("ready"));
}

#[tokio::test]
async fn limits_ownership_expiry_and_refusals() {
    let h = hub().await;
    let none: Vec<String> = vec![];
    grant(&h, &h.bo).await;
    let token = mint(&h, &h.bo).await;
    // The token started the log; with it stopped again the service says so
    // instead of answering with nothing.
    replica::disable(&lock(&h.state.0.db)).unwrap();
    let (status, _, bytes) = pull(
        &h,
        "POST",
        "/api/replica/pull/exports",
        &token,
        json!({"replica_id":"rep-1"}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    assert_eq!(
        pull(&h, "GET", "/api/replica/pull/info", &token, Value::Null)
            .await
            .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    replica::enable(&lock(&h.state.0.db)).unwrap();

    // Bad requests.
    for body in [
        json!({"replica_id":"has space"}),
        json!({"replica_id":""}),
        json!({"replica_id":"r","after":1}),
        json!({"replica_id":"r","lineage":"x"}),
        json!({"replica_id":"r","after":-1,"lineage":"x"}),
        json!({"replica_id":"r","extra":true}),
    ] {
        let (status, _, _) = pull(
            &h,
            "POST",
            "/api/replica/pull/exports",
            &token,
            body.clone(),
        )
        .await;
        assert!(status.is_client_error(), "{body}");
    }
    let (status, _, _) = pull(
        &h,
        "POST",
        "/api/replica/pull/exports",
        &token,
        json!({"replica_id":"r","after":0,"lineage":"another-lineage"}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "a delta names the lineage it continues"
    );

    // Ownership: another replica sees nothing of this export.
    grant(&h, &h.cy).await;
    let other = mint(&h, &h.cy).await;
    let id = start(&h, &token, None, &none).await;
    let manifest = ready(&h, &token, &id).await;
    for path in [
        format!("/api/replica/pull/exports/{id}"),
        format!("/api/replica/pull/exports/{id}/parts/0"),
        format!("/api/replica/pull/exports/{id}/done"),
    ] {
        let method = if path.ends_with("done") {
            "POST"
        } else {
            "GET"
        };
        let (status, _, _) =
            pull(&h, method, &path, &other, json!({"applied":manifest.head})).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
    }
    assert_eq!(
        pull(
            &h,
            "GET",
            "/api/replica/pull/exports/not-an-export",
            &token,
            Value::Null
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    // A part outside the manifest does not exist.
    let beyond = manifest.parts.len();
    assert_eq!(
        pull(
            &h,
            "GET",
            &format!("/api/replica/pull/exports/{id}/parts/{beyond}"),
            &token,
            Value::Null
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );

    // A new export cancels the old one, and its files are deleted.
    let dirs = h.state.0.replica.dirs();
    assert_eq!(dirs.len(), 1);
    let second = start(&h, &token, None, &none).await;
    assert_ne!(second, id);
    assert!(
        !dirs[0].exists(),
        "the cancelled export left nothing behind"
    );
    assert_eq!(
        pull(
            &h,
            "GET",
            &format!("/api/replica/pull/exports/{id}"),
            &token,
            Value::Null
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    ready(&h, &token, &second).await;

    // One creation at a time.
    h.state.0.replica.set_creating(true);
    let (status, _, _) = pull(
        &h,
        "POST",
        "/api/replica/pull/exports",
        &token,
        json!({"replica_id":"rep-1"}),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    h.state.0.replica.set_creating(false);

    // A ready export expires, together with its files.
    let dir = h.state.0.replica.dirs().remove(0);
    assert!(dir.join("manifest.json").exists());
    h.state.0.replica.expire_all();
    assert_eq!(
        pull(
            &h,
            "GET",
            &format!("/api/replica/pull/exports/{second}"),
            &token,
            Value::Null
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert!(!dir.exists());
    assert_eq!(h.state.0.replica.count(), 0);

    // A burst is slowed down.
    let mut slowed = false;
    for _ in 0..40 {
        if pull(&h, "GET", "/api/replica/pull/info", &other, Value::Null)
            .await
            .0
            == StatusCode::TOO_MANY_REQUESTS
        {
            slowed = true;
            break;
        }
    }
    assert!(slowed);
    // Exports do not survive a restart.
    let leftover = {
        let data = h.state.0.data.join("replica-exports").join("leftover");
        std::fs::create_dir_all(&data).unwrap();
        data
    };
    replica::service::startup(&h.state.0.data);
    assert!(!leftover.exists());
}

#[tokio::test]
async fn a_permission_change_or_revocation_ends_staged_exports() {
    let h = hub().await;
    replica::enable(&lock(&h.state.0.db)).unwrap();
    grant(&h, &h.bo).await;
    let token = mint(&h, &h.bo).await;
    let none: Vec<String> = vec![];

    // Any role, ACL or membership change during a pull invalidates the export.
    let id = start(&h, &token, None, &none).await;
    let manifest = ready(&h, &token, &id).await;
    let dir = h.state.0.replica.dirs().remove(0);
    lock(&h.state.0.db)
        .execute(
            "INSERT INTO channel_permissions VALUES('priv','user',?1,3)",
            [&h.cy.user.id],
        )
        .unwrap();
    let (status, _, bytes) = pull(
        &h,
        "GET",
        &format!("/api/replica/pull/exports/{id}/parts/0"),
        &token,
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(String::from_utf8_lossy(&bytes).contains("export_invalidated"));
    assert!(
        !dir.exists(),
        "no more bytes of the old eligibility, and nothing left on disk"
    );
    assert!(manifest.parts.len() >= 1);

    // Revocation deletes staged exports at once and the next request is refused.
    let id = start(&h, &token, None, &none).await;
    ready(&h, &token, &id).await;
    let dir = h.state.0.replica.dirs().remove(0);
    request(
        &h.app,
        "DELETE",
        &format!("/api/hosting/candidates/{}", h.bo.user.id),
        Some(&h.admin.token),
        Value::Null,
    )
    .await;
    assert!(!dir.exists());
    assert_eq!(h.state.0.replica.count(), 0);
    assert_eq!(
        pull(
            &h,
            "GET",
            &format!("/api/replica/pull/exports/{id}"),
            &token,
            Value::Null
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );

    // A restore gives every export and token a new history: tokens are gone.
    grant(&h, &h.bo).await;
    let _ = mint(&h, &h.bo).await;
    let backup = h._dir.path().join("restore-check.sqlite3");
    {
        let mut db = lock(&h.state.0.db);
        community::backup_database(&db, &backup).unwrap();
        community::restore_database(&mut db, &backup).unwrap();
        let tokens: i64 = db
            .query_row("SELECT COUNT(*) FROM replica_tokens", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            tokens, 0,
            "a restored file must not revive a token revoked after the backup"
        );
    }
}

#[tokio::test]
async fn done_and_status_use_the_sources_view_and_clock() {
    let h = hub().await;
    replica::enable(&lock(&h.state.0.db)).unwrap();
    grant(&h, &h.bo).await;
    let token = mint(&h, &h.bo).await;
    let started_ms = now_ms();
    let id = start(&h, &token, None, &[]).await;
    let manifest = ready(&h, &token, &id).await;
    let done = format!("/api/replica/pull/exports/{id}/done");

    // Nothing is acknowledged before every part was served.
    let (status, _, _) = pull(&h, "POST", &done, &token, json!({"applied":manifest.head})).await;
    assert_eq!(status, StatusCode::CONFLICT);
    for index in 0..manifest.parts.len() {
        pull(
            &h,
            "GET",
            &format!("/api/replica/pull/exports/{id}/parts/{index}"),
            &token,
            Value::Null,
        )
        .await;
    }
    // A cursor other than the head that was served is refused.
    let (status, _, _) = pull(
        &h,
        "POST",
        &done,
        &token,
        json!({"applied":manifest.head + 1}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _, _) = pull(
        &h,
        "POST",
        &done,
        &token,
        json!({"applied":manifest.head,"files_pending":2,"extra":1}),
    )
    .await;
    assert!(status.is_client_error(), "unknown fields are refused");
    let (status, _, _) = pull(
        &h,
        "POST",
        &done,
        &token,
        json!({"applied":manifest.head,"files_pending":2}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        pull(&h, "POST", &done, &token, json!({"applied":manifest.head}))
            .await
            .0,
        StatusCode::NOT_FOUND,
        "an export is acknowledged once"
    );
    assert_eq!(
        h.state.0.replica.count(),
        0,
        "the staged files go with the acknowledgement"
    );

    let measured = replica::service::measured(&lock(&h.state.0.db), &h.bo.user.id)
        .unwrap()
        .unwrap();
    assert_eq!(measured.served_head, manifest.head);
    assert!(measured.served_bytes > 0);
    assert_eq!(measured.claimed_head, Some(manifest.head));
    assert_eq!(measured.claimed_files_pending, Some(2));
    assert!(
        measured.claimed_ms.unwrap() >= started_ms as i64,
        "times are the source's own"
    );

    // Later progress cannot claim more than was served.
    let (status, _, _) = pull(
        &h,
        "POST",
        "/api/replica/pull/status",
        &token,
        json!({"applied":manifest.head + 1}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _, _) = pull(
        &h,
        "POST",
        "/api/replica/pull/status",
        &token,
        json!({"applied":manifest.head,"files_pending":0}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let measured = replica::service::measured(&lock(&h.state.0.db), &h.bo.user.id)
        .unwrap()
        .unwrap();
    assert_eq!(measured.claimed_files_pending, Some(0));
    let _ = FileRef {
        table: String::new(),
        id: String::new(),
        version: 0,
        size: 0,
    };
    let _ = BTreeSet::<String>::new();
}

// ---------------------------------------------------------------------------
// Consent starts the log, the last role stops it; a response is re-checked; the
// rate books follow who is active.

fn log_rows(h: &Hub) -> i64 {
    lock(&h.state.0.db)
        .query_row("SELECT COUNT(*) FROM replica_log", [], |r| r.get(0))
        .unwrap()
}

fn log_on(h: &Hub) -> bool {
    replica::state(&lock(&h.state.0.db)).unwrap().enabled
}

fn write_category(h: &Hub, id: &str) {
    lock(&h.state.0.db)
        .execute("INSERT INTO categories VALUES(?1,'x',0)", [id])
        .unwrap();
}

#[tokio::test]
async fn the_change_log_starts_with_the_members_own_token_and_stops_with_the_last_role() {
    let h = hub().await;
    // Nothing is recorded before consent, not even for an accepted role.
    assert!(!log_on(&h));
    write_category(&h, "c0");
    assert_eq!(log_rows(&h), 0);
    grant(&h, &h.bo).await;
    assert!(!log_on(&h), "accepting the role is not yet a data copy");
    write_category(&h, "c1");
    assert_eq!(log_rows(&h), 0);

    // The member's own token is the consent step, in the same transaction.
    let token = mint(&h, &h.bo).await;
    assert!(log_on(&h));
    write_category(&h, "c2");
    assert!(log_rows(&h) > 0);
    let lineage = replica::state(&lock(&h.state.0.db)).unwrap().lineage;

    // A second member joining keeps the same history.
    grant(&h, &h.cy).await;
    let cy_token = mint(&h, &h.cy).await;
    assert_eq!(
        replica::state(&lock(&h.state.0.db)).unwrap().lineage,
        lineage
    );

    // One member steps down: the other still holds the role, so the log goes on.
    let (status, _) = request(
        &h.app,
        "POST",
        "/api/hosting/answer",
        Some(&h.bo.token),
        json!({"accepted":[]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(log_on(&h));

    // The last one is revoked: the log stops, is cleared, and every token and
    // staged export is gone in the same breath.
    start(&h, &cy_token, None, &[]).await;
    request(
        &h.app,
        "DELETE",
        &format!("/api/hosting/candidates/{}", h.cy.user.id),
        Some(&h.admin.token),
        Value::Null,
    )
    .await;
    assert!(!log_on(&h));
    assert_eq!(log_rows(&h), 0);
    assert_eq!(h.state.0.replica.count(), 0);
    let tokens: i64 = lock(&h.state.0.db)
        .query_row("SELECT COUNT(*) FROM replica_tokens", [], |r| r.get(0))
        .unwrap();
    assert_eq!(tokens, 0);
    write_category(&h, "c3");
    assert_eq!(log_rows(&h), 0, "nothing is recorded for nobody");
    assert_eq!(
        pull(&h, "GET", "/api/replica/pull/info", &token, Value::Null)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn the_periodic_pass_stops_a_log_nobody_needs_even_without_a_roster_request() {
    let h = hub().await;
    grant(&h, &h.bo).await;
    let _ = mint(&h, &h.bo).await;
    assert!(log_on(&h));
    // The member simply leaves the community: no hosting request is made.
    lock(&h.state.0.db)
        .execute(
            "UPDATE membership SET status='left' WHERE user_id=?1",
            [&h.bo.user.id],
        )
        .unwrap();
    replica::service::maintain(&h.state);
    assert!(!log_on(&h));
    assert_eq!(log_rows(&h), 0);
}

/// Start an export, wait for it and return its id with a part to fetch.
async fn staged(h: &Hub, token: &str) -> (String, Manifest) {
    let id = start(h, token, None, &[]).await;
    let manifest = ready(h, token, &id).await;
    assert!(!manifest.parts.is_empty());
    (id, manifest)
}

#[tokio::test]
async fn a_response_is_refused_when_the_grant_changed_while_it_was_being_read() {
    for case in ["token_rotated", "access_changed", "role_withdrawn"] {
        let h = hub().await;
        grant(&h, &h.bo).await;
        let token = mint(&h, &h.bo).await;
        let part = |id: &str| format!("/api/replica/pull/exports/{id}/parts/0");

        // Control: the same request with nothing changing is served.
        let (id, _) = staged(&h, &token).await;
        let (status, headers, _) = pull(&h, "GET", &part(&id), &token, Value::Null).await;
        assert_eq!(status, StatusCode::OK, "{case}");
        assert!(headers.get("x-relay-sha256").is_some());

        // The grant changes after the checks and before the bytes leave.
        let (id, _) = staged(&h, &token).await;
        let state = h.state.clone();
        let (member, other) = (h.bo.user.id.clone(), h.cy.user.id.clone());
        h.state.0.replica.on_next_read(move || {
            let db = lock(&state.0.db);
            match case {
                "token_rotated" => db
                    .execute("UPDATE replica_tokens SET token_hash='rotated'", [])
                    .unwrap(),
                "access_changed" => db
                    .execute(
                        "INSERT INTO channel_permissions VALUES('priv','user',?1,3)",
                        [&other],
                    )
                    .unwrap(),
                _ => db
                    .execute(
                        "UPDATE membership SET status='left' WHERE user_id=?1",
                        [&member],
                    )
                    .unwrap(),
            };
        });
        let (status, headers, body) = pull(&h, "GET", &part(&id), &token, Value::Null).await;
        let expected = if case == "access_changed" {
            StatusCode::CONFLICT
        } else {
            StatusCode::FORBIDDEN
        };
        assert_eq!(
            status,
            expected,
            "{case}: {}",
            String::from_utf8_lossy(&body)
        );
        assert!(
            headers.get("x-relay-sha256").is_none(),
            "{case}: no part may be published with an older grant"
        );
        if case == "access_changed" {
            assert!(String::from_utf8_lossy(&body).contains("export_invalidated"));
        }
    }
}

#[tokio::test]
async fn a_file_is_refused_too_when_the_grant_changed_during_the_read() {
    let h = hub().await;
    grant(&h, &h.bo).await;
    let token = mint(&h, &h.bo).await;
    let (id, manifest) = staged(&h, &token).await;
    let file = manifest
        .files
        .iter()
        .find(|f| f.table == "uploads" && f.id == "u1")
        .expect("the public upload is listed")
        .clone();
    let url = format!(
        "/api/replica/pull/exports/{id}/files/{}/{}?version={}&size={}",
        file.table, file.id, file.version, file.size
    );
    let (status, _, body) = pull(&h, "GET", &url, &token, Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(&body[..], b"HELLO");

    let state = h.state.clone();
    h.state.0.replica.on_next_read(move || {
        lock(&state.0.db)
            .execute("UPDATE replica_tokens SET token_hash='rotated'", [])
            .unwrap();
    });
    let (status, headers, body) = pull(&h, "GET", &url, &token, Value::Null).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(headers.get("x-relay-sha256").is_none());
    assert!(!String::from_utf8_lossy(&body).contains("HELLO"));
}

#[tokio::test]
async fn the_rate_books_follow_active_replicas_not_every_one_that_ever_was() {
    let h = hub().await;
    grant(&h, &h.bo).await;
    let token = mint(&h, &h.bo).await;
    async fn info(h: &Hub, token: &str) -> StatusCode {
        pull(h, "GET", "/api/replica/pull/info", token, Value::Null)
            .await
            .0
    }
    assert_eq!(info(&h, &token).await, StatusCode::OK);
    assert_eq!(h.state.0.replica.tracked().0, 1);

    // A long history of former replicas does not stay in memory: the next pass forgets them.
    h.state.0.replica.seed_rates(2_000, false);
    assert!(h.state.0.replica.tracked().0 > 2_000);
    assert_eq!(info(&h, &token).await, StatusCode::OK);
    assert_eq!(h.state.0.replica.tracked(), (1, 0));

    // And the books cannot be filled by active entries either: a replica that is
    // not in them is turned away, while one that already is keeps being served.
    grant(&h, &h.cy).await;
    let newcomer = mint(&h, &h.cy).await;
    h.state.0.replica.seed_rates(2_000, true);
    assert_eq!(info(&h, &newcomer).await, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(info(&h, &token).await, StatusCode::OK);
}
