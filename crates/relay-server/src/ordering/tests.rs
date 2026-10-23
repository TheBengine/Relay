use super::*;
use crate::tests::{new_invite, request, signup};
use serde_json::{json, Value};
#[tokio::test]
async fn atomic_moves_preserve_other_channels_validate_destination_and_persist() {
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open(temp.path(), "order", 0).unwrap();
    let app = router(state.clone());
    let a = signup(&app, "Admin", "").await;
    let b = signup(&app, "Bob", &new_invite(&app, &a).await).await;
    let (_, category) = request(
        &app,
        "POST",
        "/api/categories",
        Some(&a.token),
        json!({"name":"One"}),
    )
    .await;
    let (_, second) = request(
        &app,
        "POST",
        "/api/categories",
        Some(&a.token),
        json!({"name":"Two"}),
    )
    .await;
    let mut channels = vec![];
    for name in ["alpha", "hidden", "gamma"] {
        let (_,channel)=request(&app,"POST","/api/channels",Some(&a.token),json!({"name":name,"kind":"text","visibility":if name=="hidden"{"private"}else{"public"},"category_id":category["id"]})).await;
        channels.push(channel["id"].as_str().unwrap().to_string());
    }
    let path = format!("/api/channels/{}/move", channels[2]);
    let body = json!({"category_id":category["id"],"before_id":channels[0]});
    assert_eq!(
        request(&app, "PUT", &path, Some(&b.token), body.clone())
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(&app, "PUT", &path, Some(&a.token), body).await.0,
        StatusCode::NO_CONTENT
    );
    let positions = |state: &AppState| {
        let db = lock(&state.0.db);
        let mut q = db
            .prepare("SELECT id FROM channels WHERE category_id=?1 ORDER BY position,rowid")
            .unwrap();
        let result = q
            .query_map([category["id"].as_str().unwrap()], |r| {
                r.get::<_, String>(0)
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        result
    };
    assert_eq!(
        positions(&state),
        vec![
            channels[2].clone(),
            channels[0].clone(),
            channels[1].clone()
        ]
    );
    assert_eq!(
        request(
            &app,
            "PUT",
            &path,
            Some(&a.token),
            json!({"category_id":second["id"],"before_id":channels[0]})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(positions(&state).len(), 3);
    assert_eq!(
        request(
            &app,
            "PUT",
            &format!("/api/categories/{}/move", second["id"].as_str().unwrap()),
            Some(&a.token),
            json!({"before_id":category["id"]})
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        request(
            &app,
            "PUT",
            &format!("/api/categories/{}/move", second["id"].as_str().unwrap()),
            Some(&b.token),
            json!({})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    drop(app);
    drop(state);
    let state = AppState::open(temp.path(), "restart", 0).unwrap();
    assert_eq!(positions(&state)[0], channels[2]);
    assert_eq!(
        lock(&state.0.db)
            .query_row(
                "SELECT id FROM categories ORDER BY position,rowid LIMIT 1",
                [],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
        second["id"]
    );
    let app = router(state);
    assert_eq!(
        request(
            &app,
            "PUT",
            &path,
            Some(&a.token),
            json!({"category_id":null})
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    let (_, bootstrap) = request(&app, "GET", "/api/bootstrap", Some(&a.token), Value::Null).await;
    assert!(bootstrap["channels"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["id"] == channels[2] && c["category_id"].is_null()));
}
