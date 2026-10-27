//! The companion modes against a real host on loopback; Bo's and Clara's
//! computers are folders.
use super::*;
use crate::tests::{new_invite, request, signup};
use crate::{router, AppState};
use axum::http::StatusCode;
use std::net::SocketAddr;

async fn serve(state: &AppState) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let routes = router(state.clone());
    tokio::spawn(async move {
        axum::serve(listener, routes.into_make_service_with_connect_info::<SocketAddr>())
            .await
            .unwrap();
    });
    url
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_approved_co_hosts_get_verified_copies_and_a_revoked_one_loses_its_copy() {
    let data = tempfile::tempdir().unwrap();
    let state = AppState::open(data.path(), "Takeover copies", 0).unwrap();
    let app = router(state.clone());
    let url = serve(&state).await;
    let anna = signup(&app, "Anna", "").await;
    let bo = signup(&app, "Bo", &new_invite(&app, &anna).await).await;
    let clara = signup(&app, "Clara", &new_invite(&app, &anna).await).await;
    for member in [&bo, &clara] {
        request(&app, "POST", "/api/hosting/offers", Some(&anna.token), json!({"user_id": member.user.id, "roles": ["replica"]})).await;
        request(&app, "POST", "/api/hosting/answer", Some(&member.token), json!({"accepted": ["replica"]})).await;
    }
    let pin = state.0.handover.route_pin();
    let server_id = state.server_id().unwrap();
    let input = |who: &crate::Session| CopyInput {
        url: url.clone(),
        token: who.token.clone(),
        server_id: server_id.clone(),
        user_id: who.user.id.clone(),
        pin: pin.clone(),
    };
    let computers = tempfile::tempdir().unwrap();
    let bo_pc = computers.path().join("bo");
    let clara_pc = computers.path().join("clara");

    assert_eq!(copy(input(&bo), &bo_pc).await, Outcome::WaitingForSecondCoHost);
    assert_eq!(copy(input(&clara), &clara_pc).await, Outcome::UpToDate);
    assert_eq!(copy(input(&bo), &bo_pc).await, Outcome::UpToDate);
    assert!(bo_pc.join("standby").join("standby-image.sqlite3").is_file());
    let (_, view) = request(&app, "GET", "/api/hosting", Some(&anna.token), Value::Null).await;
    let steps = view["takeover_setup"].as_array().unwrap();
    assert!(steps[1]["done"].as_bool().unwrap(), "{steps:?}");
    assert!(steps[2]["done"].as_bool().unwrap(), "{steps:?}");
    assert_eq!(steps.last().unwrap()["done"], false, "Ready to take over stays off");

    // Revoked: Clara's next run removes the copy it can no longer use.
    let (code, _) = request(&app, "DELETE", &format!("/api/hosting/candidates/{}", clara.user.id), Some(&anna.token), Value::Null).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(copy(input(&clara), &clara_pc).await, Outcome::RoleEnded);
    assert!(!clara_pc.join("standby").exists());
    // Bo's agreement was for the old set; he waits for a new second co-host.
    assert_eq!(copy(input(&bo), &bo_pc).await, Outcome::WaitingForSecondCoHost);
}

#[tokio::test]
async fn an_unreachable_host_stops_the_run_with_a_reason_and_nothing_installed() {
    let computers = tempfile::tempdir().unwrap();
    let pin = RoutePin {
        server_id: uuid::Uuid::new_v4().to_string(),
        host: CohostKey::open(&computers.path().join("x")).unwrap().identity(),
        term: 1,
    };
    let outcome = copy(
        CopyInput {
            url: "http://127.0.0.1:9".into(),
            token: "t".into(),
            server_id: pin.server_id.clone(),
            user_id: "u".into(),
            pin,
        },
        &computers.path().join("bo"),
    )
    .await;
    assert!(matches!(outcome, Outcome::Stopped(_)));
    assert!(!computers.path().join("bo").join("standby").exists());
}

#[test]
fn the_outcome_line_is_stable_json() {
    assert_eq!(serde_json::to_string(&Outcome::Following).unwrap(), r#"{"outcome":"following"}"#);
    let stopped: Outcome = serde_json::from_str(r#"{"outcome":"stopped","reason":"x"}"#).unwrap();
    assert_eq!(stopped, Outcome::Stopped("x".into()));
}
