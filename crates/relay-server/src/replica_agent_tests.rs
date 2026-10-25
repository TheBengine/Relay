use super::*;
use crate::{
    lock, router,
    tests::{new_invite, request, signup},
    AppState,
};

fn config(url: &str, server: &str, user: &str, replica: &str, token: &str) -> Config {
    Config {
        schema: 1,
        source_url: url.into(),
        transport_url: url.into(),
        server_id: server.into(),
        recipient_user_id: user.into(),
        replica_id: replica.into(),
        token: Zeroizing::new(token.into()),
    }
}

#[test]
fn routes_and_persistent_binding_refuse_foreign_storage_and_destinations() {
    let server = uuid::Uuid::new_v4().to_string();
    let replica = uuid::Uuid::new_v4().to_string();
    let mut c = config(
        "https://example.invalid",
        &server,
        "member",
        &replica,
        &"a".repeat(43),
    );
    assert!(c.validate().is_ok());
    for url in [
        "http://192.168.1.5:4580",
        "https://user:password@example.invalid",
        "https://example.invalid/?redirect=1",
    ] {
        c.source_url = url.into();
        c.transport_url = url.into();
        assert_eq!(c.validate().unwrap_err(), binding());
    }
    c.source_url = format!("http://{}.iroh:4580", "a".repeat(64));
    c.transport_url = "http://127.0.0.1:12345".into();
    assert!(c.validate().is_ok());
    c.transport_url = "http://192.168.1.5:12345".into();
    assert!(c.validate().is_err());
    c.source_url = "http://[::1]:4580".into();
    c.transport_url = c.source_url.clone();
    assert!(c.validate().is_ok());
    c.source_url = "http://localhost:4580".into();
    c.transport_url = "http://127.0.0.1:4580".into();
    assert!(c.validate().is_ok());
    c.transport_url = "http://127.0.0.1:4581".into();
    assert!(c.validate().is_err());
    c.source_url = "https://example.invalid".into();
    c.transport_url = c.source_url.clone();
    let temp = tempfile::tempdir().unwrap();
    let directory = temp.path().join("copy");
    let source = c.validate().unwrap().0;
    let (store, lease) = open_store(&directory, &c, &source).unwrap();
    assert!(store.cursor().unwrap().is_none());
    drop(store);
    assert_eq!(open_store(&directory, &c, &source).err(), Some(busy()));
    drop(lease);
    c.recipient_user_id = "another-member".into();
    assert_eq!(open_store(&directory, &c, &source).err(), Some(binding()));
    c.recipient_user_id = "member".into();
    assert!(open_store(&directory, &c, &source).is_ok());
    let ordinary = temp.path().join("ordinary");
    fs::create_dir(&ordinary).unwrap();
    fs::write(ordinary.join("relay.sqlite3"), b"owner sentinel").unwrap();
    assert_eq!(open_store(&ordinary, &c, &source).err(), Some(storage()));
    assert_eq!(
        fs::read(ordinary.join("relay.sqlite3")).unwrap(),
        b"owner sentinel"
    );
    assert!(!ordinary.join("copy.lock").exists());
    let interrupted = temp.path().join("interrupted-first-start");
    fs::create_dir(&interrupted).unwrap();
    drop(Connection::open(interrupted.join("copy.sqlite3")).unwrap());
    fs::write(interrupted.join("copy.sqlite3-journal"), []).unwrap();
    assert!(
        open_store(&interrupted, &c, &source).is_ok(),
        "an interrupted empty first database is resumable"
    );
    let foreign = temp.path().join("foreign-populated");
    fs::create_dir(&foreign).unwrap();
    let db = Connection::open(foreign.join("copy.sqlite3")).unwrap();
    db.execute_batch("CREATE TABLE owner_data(value TEXT); INSERT INTO owner_data VALUES('keep')")
        .unwrap();
    drop(db);
    assert_eq!(open_store(&foreign, &c, &source).err(), Some(binding()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn actual_http_agent_restarts_catches_up_and_stops_on_revocation_without_losing_its_copy() {
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open(&temp.path().join("source"), "Copy source", 0).unwrap();
    let app = router(state.clone());
    let admin = signup(&app, "Owner", "").await;
    let member = signup(&app, "CopyMember", &new_invite(&app, &admin).await).await;
    for (path, body, token) in [
        (
            "/api/hosting/offers",
            json!({"user_id":member.user.id,"roles":["replica"]}),
            &admin.token,
        ),
        (
            "/api/hosting/answer",
            json!({"accepted":["replica"]}),
            &member.token,
        ),
    ] {
        let (s, b) = request(&app, "POST", path, Some(token), body).await;
        assert_eq!(s, StatusCode::OK, "{b}");
    }
    let (s, issued) = request(
        &app,
        "POST",
        "/api/replica/token",
        Some(&member.token),
        Value::Null,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{issued}");
    {
        let db = lock(&state.0.db);
        db.execute("INSERT INTO channels(id,name,kind,topic,visibility) VALUES('copy-room','copy','text','','public')",[]).unwrap();
        db.execute("INSERT INTO messages(id,channel_id,author_id,content,created_at,created) VALUES('first','copy-room',?1,'first message','t',1)",[&admin.user.id]).unwrap();
        db.execute("INSERT INTO uploads(id,owner_id,message_id,name,mime,size,created,data) VALUES('file',?1,'first','sample.txt','text/plain',5,1,CAST('HELLO' AS BLOB))",[&admin.user.id]).unwrap();
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let deny_file = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = deny_file.clone();
    let source_state = state.clone();
    let service = app.clone().layer(axum::middleware::from_fn(move |req: axum::extract::Request, next: axum::middleware::Next| {
        let flag = flag.clone(); let state = source_state.clone();
        async move {
            if req.uri().path().ends_with("/files/uploads/restricted-file")
                && flag.swap(false, std::sync::atomic::Ordering::SeqCst) {
                // The listed attachment becomes private after the export was
                // read. This changes file access without withdrawing the role.
                lock(&state.0.db).execute("UPDATE uploads SET message_id='secret-message' WHERE id='restricted-file'", []).unwrap();
            }
            next.run(req).await
        }
    }));
    let server = tokio::spawn(async move {
        axum::serve(listener, service).await.unwrap();
    });
    let replica = uuid::Uuid::new_v4().to_string();
    let server_id = issued["server_id"].as_str().unwrap();
    let token = issued["token"].as_str().unwrap();
    let directory = temp.path().join("owned-copy");
    let first = run(
        config(&url, server_id, &member.user.id, &replica, token),
        &directory,
    )
    .await
    .unwrap();
    assert_eq!(first.state, "current");
    assert_eq!(first.files_pending, 0);
    assert!(!first.promotion_eligible);
    assert!(first.source_confirmed);
    {
        let db = Connection::open(directory.join("copy.sqlite3")).unwrap();
        let bytes: Vec<u8> = db
            .query_row("SELECT data FROM replica_files WHERE id='file'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(bytes, b"HELLO");
        let secrets:i64=db.query_row("SELECT COUNT(*) FROM sqlite_master WHERE name IN('sessions','replica_tokens','authenticator_credentials')",[],|r|r.get(0)).unwrap();
        assert_eq!(secrets, 0);
    }
    lock(&state.0.db)
        .execute(
            "UPDATE messages SET content='updated message' WHERE id='first'",
            [],
        )
        .unwrap();
    let second = run(
        config(&url, server_id, &member.user.id, &replica, token),
        &directory,
    )
    .await
    .unwrap();
    assert!(second.head > first.head);
    assert!(second.rows_applied < first.rows_applied);
    if let Some(exe) = std::env::var_os("RELAY_REPLICA_AGENT_EXE") {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let mut command = Command::new(exe);
        command
            .arg("--replica-pull")
            .arg(&directory)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        let mut child = command.spawn().unwrap();
        let input = Zeroizing::new(
            serde_json::to_vec(&json!({"schema":1,
            "source_url":url,"transport_url":url,"server_id":server_id,
            "recipient_user_id":member.user.id,"replica_id":replica,"token":token}))
            .unwrap(),
        );
        let mut pipe = child.stdin.take().unwrap();
        pipe.write_all(&input).unwrap();
        drop(pipe);
        let output = tokio::task::spawn_blocking(move || child.wait_with_output().unwrap())
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "companion failed: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["server_id"], server_id);
        assert_eq!(report["head"], second.head);
        assert_eq!(report["files_pending"], 0);
        assert_eq!(report["promotion_eligible"], false);
    }
    {
        let db = Connection::open(directory.join("copy.sqlite3")).unwrap();
        let text: String = db
            .query_row("SELECT content FROM messages WHERE id='first'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(text, "updated message");
    }
    // Losing one file's visibility must not revoke the whole copy token.
    tokio::time::sleep(Duration::from_secs(2)).await;
    lock(&state.0.db).execute_batch("INSERT INTO channels(id,name,kind,topic,visibility) VALUES('secret-room','secret','text','','private');
        INSERT INTO messages(id,channel_id,author_id,content,created_at,created) SELECT 'secret-message','secret-room',author_id,'private fixture','t',2 FROM messages WHERE id='first';
        INSERT INTO uploads(id,owner_id,message_id,name,mime,size,created,data) SELECT 'restricted-file',author_id,'first','private.txt','text/plain',5,2,CAST('BYTES' AS BLOB) FROM messages WHERE id='first';").unwrap();
    deny_file.store(true, std::sync::atomic::Ordering::SeqCst);
    let pending = run(
        config(&url, server_id, &member.user.id, &replica, token),
        &directory,
    )
    .await
    .unwrap();
    assert_eq!(pending.files_pending, 1);
    let refreshed = run(
        config(&url, server_id, &member.user.id, &replica, token),
        &directory,
    )
    .await
    .unwrap();
    assert_eq!(refreshed.files_pending, 0);
    assert!(refreshed.head > second.head);
    // The authenticated report is visible as a claim, never takeover authority.
    let (_, view) = request(&app, "GET", "/api/hosting", Some(&admin.token), Value::Null).await;
    assert!(view.to_string().contains("copy reports"));
    assert_eq!(view["abilities"]["planned_handover"], false);
    let (status, _) = request(
        &app,
        "DELETE",
        &format!("/api/hosting/candidates/{}", member.user.id),
        Some(&admin.token),
        Value::Null,
    )
    .await;
    // Route spelling is deliberately checked against the actual public router.
    assert_eq!(status, StatusCode::OK);
    let failure = run(
        config(&url, server_id, &member.user.id, &replica, token),
        &directory,
    )
    .await
    .unwrap_err();
    assert_eq!(failure.code, "access_denied");
    let db = Connection::open(directory.join("copy.sqlite3")).unwrap();
    let head: i64 = db
        .query_row("SELECT head FROM replica_cursor", [], |r| r.get(0))
        .unwrap();
    assert_eq!(head, refreshed.head);
    server.abort();
}
