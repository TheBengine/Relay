use super::*;
use crate::tests::{new_invite, request, signup};
use axum::http::Request;
use http_body_util::BodyExt;
use tower::ServiceExt;

#[tokio::test]
async fn local_profile_effects_are_validated_preserved_for_old_clients_and_persisted() {
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open(temp.path(), "Effects", 0).unwrap();
    let app = router(state.clone());
    let admin = signup(&app, "Admin", "").await;
    for effect in ["glow", "sparkle", "wave", "none"] {
        let (status, user) = request(
            &app,
            "PUT",
            "/api/profile",
            Some(&admin.token),
            serde_json::json!({"display_name":"Alex","bio":"","appearance":{"effect":effect}}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{user}");
        assert_eq!(user["profile"]["appearance"]["effect"], effect);
    }
    assert_eq!(request(&app,"PUT","/api/profile",Some(&admin.token),serde_json::json!({"display_name":"Alex","bio":"","appearance":{"effect":"https://untrusted.example/animation"}})).await.0,StatusCode::BAD_REQUEST);
    let (_, saved) = request(
        &app,
        "PUT",
        "/api/profile",
        Some(&admin.token),
        serde_json::json!({"display_name":"Alex","bio":"","appearance":{"effect":"sparkle"}}),
    )
    .await;
    let (_, legacy)=request(&app,"PUT","/api/profile",Some(&admin.token),serde_json::json!({"display_name":"Alex","bio":"Older client","appearance":{"pronouns":"they/them","accent":0x5865f2,"banner":0x245a83}})).await;
    assert_eq!(
        legacy["profile"]["appearance"]["effect"],
        saved["profile"]["appearance"]["effect"]
    );
    drop(app);
    drop(state);
    let app = router(AppState::open(temp.path(), "Effects", 0).unwrap());
    assert_eq!(
        request(
            &app,
            "GET",
            "/api/profile",
            Some(&admin.token),
            serde_json::Value::Null
        )
        .await
        .1["profile"],
        legacy["profile"]
    );
}

#[tokio::test]
async fn extended_profiles_survive_legacy_edits_and_restart_and_reject_invalid_colors() {
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open(temp.path(), "profiles", 0).unwrap();
    let app = router(state.clone());
    let admin = signup(&app, "Admin", "").await;
    let appearance =
        serde_json::json!({"pronouns":"they/them","accent":0x5865f2,"banner":0x245a83});
    let (status, saved) = request(
        &app,
        "PUT",
        "/api/profile",
        Some(&admin.token),
        serde_json::json!({"display_name":"Alex","bio":"Hello","appearance":appearance}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["profile"]["appearance"], appearance);
    let (_, legacy) = request(
        &app,
        "PUT",
        "/api/profile",
        Some(&admin.token),
        serde_json::json!({"display_name":"Alex new","bio":"Legacy client edit"}),
    )
    .await;
    assert_eq!(legacy["profile"]["appearance"], appearance);
    for bad_appearance in [
        serde_json::json!({"accent":0x1000000}),
        serde_json::json!({"pronouns":"x".repeat(33)}),
        serde_json::json!({"pronouns":"\u{202e}spoof"}),
    ] {
        assert_eq!(
            request(
                &app,
                "PUT",
                "/api/profile",
                Some(&admin.token),
                serde_json::json!({"display_name":"bad","bio":"bad","appearance":bad_appearance})
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    }
    drop(app);
    drop(state);
    let app = router(AppState::open(temp.path(), "profiles", 0).unwrap());
    let (_, bootstrap) = request(
        &app,
        "GET",
        "/api/bootstrap",
        Some(&admin.token),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(bootstrap["user"]["profile"], legacy["profile"]);
}

#[tokio::test]
async fn server_presentation_requires_admin_and_survives_restart_without_changing_encryption_id() {
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open(temp.path(), "Original", 0).unwrap();
    let app = router(state.clone());
    let admin = signup(&app, "Admin", "").await;
    let member = signup(&app, "Member", &new_invite(&app, &admin).await).await;
    let (_, before) = request(
        &app,
        "GET",
        "/api/bootstrap",
        Some(&admin.token),
        serde_json::Value::Null,
    )
    .await;
    let settings = serde_json::json!({"name":"The Commons","description":"Private local community","accent":0x5865f2});
    assert_eq!(
        request(
            &app,
            "PATCH",
            "/api/server-settings",
            Some(&member.token),
            settings.clone()
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            &app,
            "GET",
            "/api/server-settings",
            None,
            serde_json::Value::Null
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        request(
            &app,
            "PATCH",
            "/api/server-settings",
            Some(&admin.token),
            serde_json::json!({"name":"  ","description":"","accent":0})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        request(
            &app,
            "PATCH",
            "/api/server-settings",
            Some(&admin.token),
            settings.clone()
        )
        .await
        .0,
        StatusCode::OK
    );
    let health = tokio::time::timeout(
        Duration::from_secs(2),
        request(&app, "GET", "/api/health", None, serde_json::Value::Null),
    )
    .await
    .unwrap()
    .1;
    assert_eq!(health["server_name"], "The Commons");
    assert_eq!(health["server_id"], before["server_id"]);
    drop(app);
    drop(state);
    let app = router(AppState::open(temp.path(), "CLI fallback", 0).unwrap());
    let (_, after) = request(
        &app,
        "GET",
        "/api/bootstrap",
        Some(&member.token),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(after["server_name"], settings["name"]);
    assert_eq!(after["server_id"], before["server_id"]);
    assert_eq!(
        request(
            &app,
            "GET",
            "/api/server-settings",
            Some(&member.token),
            serde_json::Value::Null
        )
        .await
        .1,
        settings
    );
}

async fn raw(
    app: &Router,
    method: &str,
    path: &str,
    token: Option<&str>,
    bytes: Vec<u8>,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut request = Request::builder().method(method).uri(path);
    if let Some(token) = token {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::from(bytes)).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    (status, headers, body)
}

fn fixture(format: ImageFormat, width: u32, height: u32) -> Vec<u8> {
    let mut output = Cursor::new(Vec::new());
    DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
        width,
        height,
        image::Rgb([31, 102, 172]),
    ))
    .write_to(&mut output, format)
    .unwrap();
    output.into_inner()
}

#[tokio::test]
async fn profiles_are_unicode_bounded_owned_and_update_all_user_surfaces() {
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open(temp.path(), "profiles", 0).unwrap();
    let app = router(state.clone());
    let admin = signup(&app, "Admin", "").await;
    let member = signup(&app, "Member", &new_invite(&app, &admin).await).await;
    let (_, message) = request(
        &app,
        "POST",
        "/api/messages",
        Some(&member.token),
        serde_json::json!({"recipient_id":admin.user.id,"content":"author profile should refresh"}),
    )
    .await;
    let mut events = state.0.events.subscribe();
    let body = serde_json::json!({"display_name":"🦊".repeat(32),"bio":"界".repeat(190)});
    let (status, saved) = request(
        &app,
        "PUT",
        "/api/profile",
        Some(&member.token),
        body.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["id"], member.user.id);
    assert_eq!(saved["username"], "Member");
    assert_eq!(saved["profile"]["version"], 1);
    assert_eq!(saved["profile"]["display_name"], "🦊".repeat(32));
    assert_eq!(events.try_recv().unwrap().event.kind, "profile");
    assert_eq!(events.try_recv().unwrap().event.kind, "members");
    assert_eq!(
        request(&app, "PUT", "/api/profile", Some(&member.token), body)
            .await
            .1,
        saved
    );
    assert!(events.try_recv().is_err(), "an unchanged save is a no-op");
    for invalid in [
        serde_json::json!({"display_name":"x".repeat(33),"bio":""}),
        serde_json::json!({"display_name":"","bio":"界".repeat(191)}),
        serde_json::json!({"display_name":"one\ntwo","bio":""}),
        serde_json::json!({"display_name":"fake\u{202e}name","bio":""}),
    ] {
        assert_eq!(
            request(&app, "PUT", "/api/profile", Some(&member.token), invalid)
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        request(
            &app,
            "PUT",
            "/api/profile",
            Some(&member.token),
            serde_json::json!({"display_name":"forged","bio":"","user_id":admin.user.id})
        )
        .await
        .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        request(
            &app,
            "PUT",
            "/api/profile",
            None,
            serde_json::json!({"display_name":"forged","bio":""})
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    let (_, bootstrap) = request(
        &app,
        "GET",
        "/api/bootstrap",
        Some(&member.token),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(bootstrap["user"], saved);
    assert!(bootstrap["members"]
        .as_array()
        .unwrap()
        .iter()
        .any(|u| u == &saved));
    assert!(bootstrap["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v == "profiles-v1"));
    let id = message["id"].as_str().unwrap();
    assert_eq!(
        get_message(&lock(&state.0.db), id)
            .unwrap()
            .author
            .profile
            .display_name,
        "🦊".repeat(32)
    );
    let (_, login) = request(
        &app,
        "POST",
        "/api/login",
        None,
        serde_json::json!({"username":"Member","password":"a-long-test-password"}),
    )
    .await;
    assert_eq!(login["user"], saved);
    let (_, own_admin) = request(
        &app,
        "GET",
        "/api/profile",
        Some(&admin.token),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(
        own_admin["profile"]["version"], 0,
        "one member cannot overwrite another"
    );
    drop(app);
    drop(state);
    let restarted = AppState::open(temp.path(), "profiles", 0).unwrap();
    assert_eq!(
        get_user(&lock(&restarted.0.db), &member.user.id)
            .unwrap()
            .profile
            .bio,
        "界".repeat(190)
    );
}

#[tokio::test]
async fn avatars_are_dedicated_normalized_versioned_and_private_to_active_members() {
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open(temp.path(), "avatars", 0).unwrap();
    let app = router(state.clone());
    let admin = signup(&app, "Admin", "").await;
    let member = signup(&app, "Member", &new_invite(&app, &admin).await).await;
    let mut jpeg = fixture(ImageFormat::Jpeg, 512, 128);
    jpeg.extend_from_slice(b"PRIVATE trailing source metadata");
    let (status, _, saved) = raw(
        &app,
        "PUT",
        "/api/profile/avatar",
        Some(&member.token),
        jpeg,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&saved)
    );
    let saved: User = serde_json::from_slice(&saved).unwrap();
    let url = saved.profile.avatar_url.unwrap();
    let (status, headers, png) = raw(&app, "GET", &url, Some(&admin.token), vec![]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "image/png");
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    assert_eq!(image::load_from_memory(&png).unwrap().width(), 256);
    assert_eq!(image::load_from_memory(&png).unwrap().height(), 64);
    assert!(!png.windows(7).any(|s| s == b"PRIVATE"));
    assert_eq!(
        raw(&app, "GET", &url, None, vec![]).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        lock(&state.0.db)
            .query_row("SELECT COUNT(*) FROM uploads", [], |r| r.get::<_, u64>(0))
            .unwrap(),
        0,
        "avatar publication never reuses chat uploads or binds a private attachment"
    );
    let (status, _, same) = raw(
        &app,
        "PUT",
        "/api/profile/avatar",
        Some(&member.token),
        png.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_slice::<User>(&same)
            .unwrap()
            .profile
            .version,
        1
    );
    let (_, changed) = request(
        &app,
        "PUT",
        "/api/profile",
        Some(&member.token),
        serde_json::json!({"display_name":"Fox","bio":""}),
    )
    .await;
    assert_eq!(changed["profile"]["version"], 2);
    assert_eq!(
        raw(&app, "GET", &url, Some(&admin.token), vec![]).await.0,
        StatusCode::NOT_FOUND
    );
    let current_url = changed["profile"]["avatar_url"].as_str().unwrap();
    assert_eq!(
        raw(&app, "GET", current_url, Some(&admin.token), vec![])
            .await
            .0,
        StatusCode::OK
    );
    let (_, cleared) = request(
        &app,
        "DELETE",
        "/api/profile/avatar",
        Some(&member.token),
        serde_json::Value::Null,
    )
    .await;
    assert!(cleared["profile"]["avatar_url"].is_null());
    assert_eq!(cleared["profile"]["version"], 3);
    assert_eq!(
        raw(&app, "GET", current_url, Some(&admin.token), vec![])
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    let (status, _, _) = raw(
        &app,
        "PUT",
        "/api/profile/avatar",
        Some(&member.token),
        fixture(ImageFormat::WebP, 8, 8),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let user = get_user(&lock(&state.0.db), &member.user.id).unwrap();
    let current_url = user.profile.avatar_url.unwrap();
    assert_eq!(
        request(
            &app,
            "PATCH",
            &format!("/api/members/{}", member.user.id),
            Some(&admin.token),
            serde_json::json!({"status":"banned"})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        raw(&app, "GET", &current_url, Some(&admin.token), vec![])
            .await
            .0,
        StatusCode::FORBIDDEN,
        "a removed profile is no longer community-visible"
    );
    assert_eq!(
        raw(
            &app,
            "GET",
            &format!("/api/profiles/{}", member.user.id),
            Some(&admin.token),
            vec![]
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        raw(&app, "PUT", "/api/profile/avatar", Some(&member.token), png)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn avatar_rejections_and_shared_quotas_are_atomic() {
    let temp = tempfile::tempdir().unwrap();
    let png = normalize_avatar(&fixture(ImageFormat::Png, 1, 1)).unwrap();
    let state = AppState::open_with_config(
        temp.path(),
        "quotas",
        0,
        ServerConfig {
            storage_limit_bytes: png.len() as u64 + 3,
            user_storage_limit_bytes: png.len() as u64 + 3,
            ..Default::default()
        },
    )
    .unwrap();
    let app = router(state.clone());
    let admin = signup(&app, "Admin", "").await;
    for bytes in [
        vec![],
        b"<svg />".to_vec(),
        fixture(ImageFormat::Png, 2049, 1),
        vec![0; MAX_AVATAR_UPLOAD_BYTES + 1],
    ] {
        let (status, _, _) = raw(
            &app,
            "PUT",
            "/api/profile/avatar",
            Some(&admin.token),
            bytes,
        )
        .await;
        assert!(
            matches!(
                status,
                StatusCode::BAD_REQUEST | StatusCode::PAYLOAD_TOO_LARGE
            ),
            "{status}"
        );
    }
    assert_eq!(
        get_user(&lock(&state.0.db), &admin.user.id)
            .unwrap()
            .profile
            .version,
        0
    );
    assert_eq!(
        raw(
            &app,
            "PUT",
            "/api/profile/avatar",
            Some(&admin.token),
            png.clone()
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        raw(
            &app,
            "POST",
            "/api/uploads?name=small&mime=text/plain",
            Some(&admin.token),
            vec![1; 3]
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        raw(
            &app,
            "POST",
            "/api/uploads?name=one&mime=text/plain",
            Some(&admin.token),
            vec![1]
        )
        .await
        .0,
        StatusCode::INSUFFICIENT_STORAGE
    );
    assert_eq!(
        raw(
            &app,
            "PUT",
            "/api/profile/avatar",
            Some(&admin.token),
            fixture(ImageFormat::Png, 256, 256)
        )
        .await
        .0,
        StatusCode::INSUFFICIENT_STORAGE
    );
    assert_eq!(
        get_user(&lock(&state.0.db), &admin.user.id)
            .unwrap()
            .profile
            .version,
        1
    );
    assert_eq!(
        raw(&app, "PUT", "/api/profile/avatar", Some(&admin.token), png)
            .await
            .0,
        StatusCode::OK,
        "an identical replacement consumes no extra quota"
    );
    let (_, usage) = request(
        &app,
        "GET",
        "/api/storage",
        Some(&admin.token),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(usage["user_bytes"], state.0.config.user_storage_limit_bytes);
    request(
        &app,
        "DELETE",
        "/api/profile/avatar",
        Some(&admin.token),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(
        raw(
            &app,
            "POST",
            "/api/uploads?name=one&mime=text/plain",
            Some(&admin.token),
            vec![1]
        )
        .await
        .0,
        StatusCode::OK
    );
}

#[test]
fn profile_migration_preserves_existing_identity_and_is_transactional() {
    let mut db = Connection::open_in_memory().unwrap();
    migrate_database(&mut db).unwrap();
    db.execute(
        "INSERT INTO users VALUES('old','OldName','hash','admin')",
        [],
    )
    .unwrap();
    db.execute("INSERT INTO membership VALUES('old','active')", [])
        .unwrap();
    db.execute("DROP TABLE user_profiles", []).unwrap();
    crate::tests::discard_post_profile_additions(&db);
    db.execute_batch("DROP TABLE crypto_call_requests;DROP TABLE crypto_key_package_consumptions;DROP TABLE crypto_welcome_packages;DROP TABLE crypto_operations;DROP TABLE crypto_events;DROP TABLE crypto_groups;DROP TABLE crypto_key_packages;DROP TABLE crypto_devices;").unwrap();
    db.execute("DELETE FROM schema_migrations WHERE version>=6", [])
        .unwrap();
    db.pragma_update(None, "user_version", 5).unwrap();
    let stable_id = server_id(&db).unwrap();
    migrate_database(&mut db).unwrap();
    let user = get_user(&db, "old").unwrap();
    assert_eq!(user.username, "OldName");
    assert_eq!(user.profile, UserProfile::default());
    assert_eq!(server_id(&db).unwrap(), stable_id);
    migrate_database(&mut db).unwrap();
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM user_profiles", [], |r| r
            .get::<_, u32>(0))
            .unwrap(),
        1
    );

    db.execute("DROP TABLE user_profiles", []).unwrap();
    crate::tests::discard_post_profile_additions(&db);
    db.execute_batch("DROP TABLE crypto_call_requests;DROP TABLE crypto_key_package_consumptions;DROP TABLE crypto_welcome_packages;DROP TABLE crypto_operations;DROP TABLE crypto_events;DROP TABLE crypto_groups;DROP TABLE crypto_key_packages;DROP TABLE crypto_devices;").unwrap();
    db.execute("DELETE FROM schema_migrations WHERE version>=6", [])
        .unwrap();
    db.pragma_update(None, "user_version", 5).unwrap();
    db.execute("CREATE TABLE user_profiles(unrelated TEXT)", [])
        .unwrap();
    assert!(migrate_database(&mut db).is_err());
    assert_eq!(
        db.pragma_query_value::<u32, _>(None, "user_version", |r| r.get(0))
            .unwrap(),
        5
    );
    assert_eq!(
        db.query_row(
            "SELECT COUNT(*) FROM schema_migrations WHERE version=6",
            [],
            |r| r.get::<_, u32>(0)
        )
        .unwrap(),
        0
    );
}

#[tokio::test]
async fn profile_and_avatar_survive_backup_restore_with_original_credentials() {
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open(temp.path(), "backup", 0).unwrap();
    let app = router(state.clone());
    let admin = signup(&app, "Admin", "").await;
    request(
        &app,
        "PUT",
        "/api/profile",
        Some(&admin.token),
        serde_json::json!({"display_name":"Stored","bio":"About me\nsecond line"}),
    )
    .await;
    raw(
        &app,
        "PUT",
        "/api/profile/avatar",
        Some(&admin.token),
        fixture(ImageFormat::Png, 32, 16),
    )
    .await;
    let expected = get_user(&lock(&state.0.db), &admin.user.id).unwrap();
    let backup = temp.path().join("profile-backup.sqlite3");
    state.backup_to(&backup).unwrap();
    request(
        &app,
        "DELETE",
        "/api/profile/avatar",
        Some(&admin.token),
        serde_json::Value::Null,
    )
    .await;
    request(
        &app,
        "PUT",
        "/api/profile",
        Some(&admin.token),
        serde_json::json!({"display_name":"Changed","bio":""}),
    )
    .await;
    state.restore_from(&backup).unwrap();
    assert_eq!(
        get_user(&lock(&state.0.db), &admin.user.id)
            .unwrap()
            .profile,
        expected.profile
    );
    let (status, login) = request(
        &app,
        "POST",
        "/api/login",
        None,
        serde_json::json!({"username":"Admin","password":"a-long-test-password"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(login["user"]["username"], "Admin");
    let token = login["token"].as_str().unwrap();
    assert_eq!(
        raw(
            &app,
            "GET",
            expected.profile.avatar_url.as_ref().unwrap(),
            Some(token),
            vec![]
        )
        .await
        .0,
        StatusCode::OK
    );
}

#[test]
fn old_user_payloads_remain_compatible() {
    let user: User =
        serde_json::from_str(r#"{"id":"old","username":"legacy","role":"member"}"#).unwrap();
    assert_eq!(user.profile, UserProfile::default());
}
