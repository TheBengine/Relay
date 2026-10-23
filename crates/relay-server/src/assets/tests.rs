use super::*;
use crate::tests::{new_invite, request, signup};
use serde_json::Value;
fn png() -> Vec<u8> {
    let mut out = std::io::Cursor::new(vec![]);
    image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
        400,
        200,
        image::Rgba([200, 40, 70, 255]),
    ))
    .write_to(&mut out, image::ImageFormat::Png)
    .unwrap();
    out.into_inner()
}
#[tokio::test]
async fn bounded_presentation_assets_authorization_crop_quota_and_restart() {
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open(temp.path(), "assets", 0).unwrap();
    let app = router(state.clone());
    let a = signup(&app, "Admin", "").await;
    let b = signup(&app, "Bob", &new_invite(&app, &a).await).await;
    let body = json!({"image_base64":STANDARD.encode(png()),"crop":{"x":0.25,"y":0.0,"width":0.5,"height":1.0}});
    assert_eq!(
        request(
            &app,
            "PUT",
            "/api/assets/server_icon/community",
            Some(&b.token),
            body.clone()
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let (status, icon) = request(
        &app,
        "PUT",
        "/api/assets/server_icon/community",
        Some(&a.token),
        body.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{icon}");
    let (status, banner) = request(
        &app,
        "PUT",
        &format!("/api/assets/profile_banner/{}", b.user.id),
        Some(&b.token),
        body.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{banner}");
    assert_eq!(
        request(
            &app,
            "PUT",
            &format!("/api/assets/profile_banner/{}", a.user.id),
            Some(&b.token),
            body.clone()
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            &app,
            "PUT",
            "/api/assets/emoji/hello_world",
            Some(&a.token),
            body.clone()
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        request(
            &app,
            "PUT",
            "/api/assets/sticker/hello",
            Some(&a.token),
            body.clone()
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(list(&lock(&state.0.db)).unwrap().len(), 4);
    let bytes: Vec<u8> = lock(&state.0.db)
        .query_row(
            "SELECT data FROM presentation_assets WHERE kind='server_icon'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let image = image::load_from_memory(&bytes).unwrap();
    assert_eq!((image.width(), image.height()), (200, 200));
    assert_eq!(
        request(
            &app,
            "PUT",
            "/api/assets/emoji/invalid-name",
            Some(&a.token),
            body.clone()
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(request(&app,"PUT","/api/assets/server_banner/community",Some(&a.token),json!({"image_base64":STANDARD.encode(png()),"crop":{"x":0.8,"y":0.0,"width":0.8,"height":1.0}})).await.0,StatusCode::BAD_REQUEST);
    assert_eq!(
        request(
            &app,
            "PUT",
            "/api/assets/server_banner/community",
            Some(&a.token),
            json!({"image_base64":STANDARD.encode(b"damaged image")})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let (total, owned) = profiles::storage_usage(&lock(&state.0.db), &b.user.id).unwrap();
    assert!(total > owned && owned > 0);
    // Legacy profile updates do not mutate the separate banner storage.
    request(
        &app,
        "PUT",
        "/api/profile",
        Some(&b.token),
        json!({"display_name":"Bob","bio":"Old client"}),
    )
    .await;
    assert!(list(&lock(&state.0.db))
        .unwrap()
        .iter()
        .any(|i| i.id == banner["id"]));
    drop(app);
    drop(state);
    let state = AppState::open_with_config(
        temp.path(),
        "restart",
        0,
        ServerConfig {
            storage_limit_bytes: 1,
            ..Default::default()
        },
    )
    .unwrap();
    let app = router(state.clone());
    assert_eq!(
        request(
            &app,
            "PUT",
            "/api/assets/emoji/new_image",
            Some(&a.token),
            body
        )
        .await
        .0,
        StatusCode::INSUFFICIENT_STORAGE
    );
    assert_eq!(list(&lock(&state.0.db)).unwrap().len(), 4);
    assert_eq!(
        request(
            &app,
            "DELETE",
            "/api/assets/emoji/hello_world",
            Some(&a.token),
            Value::Null
        )
        .await
        .0,
        StatusCode::OK
    );
}
