//! A sender's chosen video quality reaches the others only for a shared
//! source and only within what any sender can choose.
use super::*;
use crate::tests::{create_test_channel, new_invite, request, signup};
use serde_json::json;

async fn members(app: &Router, token: &str) -> Vec<VoiceMember> {
    let (status, body) = request(app, "GET", "/api/bootstrap", Some(token), serde_json::Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    serde_json::from_value(body["voice_members"].clone()).unwrap()
}

#[tokio::test]
async fn chosen_quality_is_forwarded_for_shared_sources_and_bounded() {
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open(temp.path(), "chosen-video", 0).unwrap();
    let app = router(state);
    let admin = signup(&app, "Admin", "").await;
    let viewer = signup(&app, "Viewer", &new_invite(&app, &admin).await).await;
    let voice = create_test_channel(&app, &admin, "voice", false).await;
    let (status, _) = request(&app, "POST", "/api/voice/join", Some(&admin.token), json!({"channel_id":voice})).await;
    assert_eq!(status, StatusCode::OK);
    let choice = json!({"max_width":1920,"max_height":1080,"fps":60,"bitrate_kbps":8000});
    let post = |body: serde_json::Value| request(&app, "POST", "/api/voice/state", Some(&admin.token), body);

    assert_eq!(post(json!({"muted":false,"deafened":false,"sharing":true,"camera":false,
        "chosen_camera":choice,"chosen_screen":choice})).await.0, StatusCode::NO_CONTENT);
    let seen = members(&app, &viewer.token).await;
    let sender = seen.iter().find(|m| m.user.id == admin.user.id).unwrap();
    assert_eq!(sender.chosen_screen, Some(ChosenVideo { max_width: 1920, max_height: 1080, fps: 60, bitrate_kbps: Some(8000) }));
    assert_eq!(sender.chosen_camera, None, "the camera is not shared, so no choice is forwarded");

    let absurd = json!({"max_width":100000,"max_height":1080,"fps":60});
    assert_eq!(post(json!({"muted":false,"deafened":false,"sharing":true,"camera":true,
        "chosen_camera":absurd,"chosen_screen":{"max_width":1280,"max_height":720,"fps":0}})).await.0, StatusCode::NO_CONTENT);
    let sender = members(&app, &viewer.token).await.into_iter().find(|m| m.user.id == admin.user.id).unwrap();
    assert_eq!((sender.chosen_camera, sender.chosen_screen), (None, None));

    // An older client sends no choice at all; that still works.
    assert_eq!(post(json!({"muted":true,"deafened":false,"sharing":true,"camera":true})).await.0, StatusCode::NO_CONTENT);
    let sender = members(&app, &viewer.token).await.into_iter().find(|m| m.user.id == admin.user.id).unwrap();
    assert!(sender.sharing && sender.chosen_screen.is_none());
}
