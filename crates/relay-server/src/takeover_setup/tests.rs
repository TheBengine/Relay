//! Anna hosts; Bo and Clara are approved co-hosts on their own computers (folders).
use super::*;
use crate::takeover_cohost::CohostKey;
use crate::tests::{new_invite, request, signup};
use axum::body::Body;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

struct World {
    temp: tempfile::TempDir,
    state: AppState,
    app: Router,
    anna: Session,
    bo: Session,
    clara: Session,
}

async fn world() -> World {
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open(&temp.path().join("anna"), "Takeover setup", 0).unwrap();
    let app = router(state.clone());
    let anna = signup(&app, "Anna", "").await;
    let bo = signup(&app, "Bo", &new_invite(&app, &anna).await).await;
    let clara = signup(&app, "Clara", &new_invite(&app, &anna).await).await;
    for member in [&bo, &clara] {
        approve(&app, &anna, member).await;
    }
    // A copy large enough to travel in several parts.
    lock(&state.0.db)
        .execute(
            "INSERT INTO metadata(key,value) VALUES('takeover-test-filler',?1)",
            ["x".repeat(2_500_000)],
        )
        .unwrap();
    World {
        temp,
        state,
        app,
        anna,
        bo,
        clara,
    }
}

async fn approve(app: &Router, anna: &Session, member: &Session) {
    request(
        app,
        "POST",
        "/api/hosting/offers",
        Some(&anna.token),
        json!({"user_id":member.user.id,"roles":["replica"]}),
    )
    .await;
    let (code, body) = request(
        app,
        "POST",
        "/api/hosting/answer",
        Some(&member.token),
        json!({"accepted":["replica"]}),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
}

async fn bytes(app: &Router, token: &str, path: &str) -> (StatusCode, Vec<u8>) {
    let response = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .uri(path)
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    (
        status,
        response.into_body().collect().await.unwrap().to_bytes().to_vec(),
    )
}

fn key(w: &World, name: &str) -> CohostKey {
    CohostKey::open(&w.temp.path().join(name)).unwrap()
}

async fn enroll(w: &World, who: &Session, k: &CohostKey) -> (StatusCode, Value) {
    let server = w.state.server_id().unwrap();
    let body = serde_json::to_value(k.enroll(&server, &who.user.id).unwrap()).unwrap();
    request(&w.app, "POST", "/api/hosting/takeover/enroll", Some(&who.token), body).await
}

async fn consent(w: &World, who: &Session, k: &CohostKey) -> CandidateAccept {
    let (_, cohort) = request(
        &w.app,
        "GET",
        "/api/hosting/takeover/cohort",
        Some(&who.token),
        Value::Null,
    )
    .await;
    let set: AuthoritySet = serde_json::from_value(cohort["set"].clone()).unwrap();
    let accept = k.consent(&set, &w.state.0.handover.route_pin()).unwrap();
    let (code, body) = request(
        &w.app,
        "POST",
        "/api/hosting/takeover/consent",
        Some(&who.token),
        json!({ "accept": accept }),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    accept
}

fn step_text(w: &World, index: usize) -> (bool, String) {
    let db = lock(&w.state.0.db);
    let steps = steps(&db, |id| {
        get_user(&db, id)
            .map(|u| u.username)
            .unwrap_or_default()
    })
    .unwrap();
    (steps[index].done, steps[index].text.clone())
}

#[tokio::test]
async fn approved_co_hosts_enroll_consent_copy_resume_and_verify_up_to_date() {
    let w = world().await;
    let (bo_key, clara_key) = (key(&w, "bo-pc"), key(&w, "clara-pc"));
    // Only approved co-hosts take part, and only with proof of their own key.
    let dan = signup(&w.app, "Dan", &new_invite(&w.app, &w.anna).await).await;
    assert_eq!(
        enroll(&w, &dan, &key(&w, "dan-pc")).await.0,
        StatusCode::FORBIDDEN
    );
    let mut forged = bo_key
        .enroll(&w.state.server_id().unwrap(), &w.clara.user.id)
        .unwrap();
    forged.recipient_key = clara_key.enroll("x", "y").unwrap().recipient_key;
    let (code, _) = request(
        &w.app,
        "POST",
        "/api/hosting/takeover/enroll",
        Some(&w.bo.token),
        serde_json::to_value(forged).unwrap(),
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST);

    let (code, cohort) = enroll(&w, &w.bo, &bo_key).await;
    assert_eq!(code, StatusCode::OK, "{cohort}");
    assert!(cohort["set"].is_null(), "no agreement with a single co-host");
    let (_, cohort) = enroll(&w, &w.clara, &clara_key).await;
    let set: AuthoritySet = serde_json::from_value(cohort["set"].clone()).unwrap();
    assert_eq!(set.body.pin, w.state.0.handover.route_pin());
    assert!(set.body.is_candidate(&bo_key.identity()));
    assert!(set.body.is_candidate(&clara_key.identity()));
    assert!(step_text(&w, 0).0);
    // A third co-host is refused while two are set up.
    approve(&w.app, &w.anna, &dan).await;
    assert_eq!(
        enroll(&w, &dan, &key(&w, "dan-pc")).await.0,
        StatusCode::CONFLICT
    );
    // Consent: only for exactly this set, signed by the machine itself.
    let mut wrong = bo_key
        .consent(&set, &w.state.0.handover.route_pin())
        .unwrap();
    wrong.body.epoch += 1;
    let (code, _) = request(
        &w.app,
        "POST",
        "/api/hosting/takeover/consent",
        Some(&w.bo.token),
        json!({ "accept": wrong }),
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    let mut other = w.state.0.handover.route_pin();
    other.term += 1;
    assert!(bo_key.consent(&set, &other).is_err(), "a set from another host is refused");
    let bo_accept = consent(&w, &w.bo, &bo_key).await;
    consent(&w, &w.clara, &clara_key).await;
    assert!(step_text(&w, 1).0);

    // Changes are tracked from here, so "up to date" can be measured.
    crate::replica::enable(&lock(&w.state.0.db)).unwrap();
    let (code, manifest) = request(
        &w.app,
        "POST",
        "/api/hosting/takeover/snapshot",
        Some(&w.bo.token),
        Value::Null,
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{manifest}");
    let manifest: Manifest = serde_json::from_value(manifest).unwrap();
    assert!(manifest.parts.len() >= 3);
    let (_, text) = step_text(&w, 2);
    assert!(text.contains("Bo: copying"), "{text}");
    let part_path = |i: usize| {
        format!(
            "/api/hosting/takeover/snapshot/{}/parts/{i}",
            manifest.snapshot_id
        )
    };
    // Clara cannot read Bo's copy.
    assert_eq!(
        bytes(&w.app, &w.clara.token, &part_path(0)).await.0,
        StatusCode::NOT_FOUND
    );
    let mut parts = Vec::new();
    for i in 0..manifest.parts.len() {
        let (code, raw) = bytes(&w.app, &w.bo.token, &part_path(i)).await;
        assert_eq!(code, StatusCode::OK);
        parts.push(raw);
    }
    // The connection drops after the first part: nothing is installed.
    let interrupted = bo_key.pull(&manifest, |i| {
        if i == 0 {
            Ok(parts[0].clone())
        } else {
            Err("connection lost".into())
        }
    });
    assert!(interrupted.is_err());
    assert!(bo_key.installed().is_none());
    // A damaged part is refused.
    assert!(bo_key.pull(&manifest, |_| Ok(b"damaged".to_vec())).is_err());
    // Resuming fetches only the missing parts.
    let mut resumed = Vec::new();
    let image = bo_key
        .pull(&manifest, |i| {
            resumed.push(i);
            Ok(parts[i].clone())
        })
        .unwrap();
    assert!(!resumed.contains(&0), "the part already here is not fetched again");
    assert_eq!(resumed.len(), manifest.parts.len() - 1);
    bo_key.install(&set, &bo_accept, &manifest, &image).unwrap();
    assert_eq!(
        bo_key.installed().as_deref(),
        Some(manifest.snapshot_id.as_str())
    );
    // The installed standby holds no usable login secret.
    let installed = std::fs::read(
        w.temp
            .path()
            .join("bo-pc")
            .join("standby")
            .join(crate::takeover_standby::IMAGE_NAME),
    )
    .unwrap();
    assert!(!installed.windows(9).any(|win| win == b"$argon2id"));
    let (code, _) = request(
        &w.app,
        "POST",
        "/api/hosting/takeover/verified",
        Some(&w.bo.token),
        json!({"snapshot_id": manifest.snapshot_id, "sha256": "0".repeat(64)}),
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST, "only the prepared copy counts");
    let (code, _) = request(
        &w.app,
        "POST",
        "/api/hosting/takeover/verified",
        Some(&w.bo.token),
        json!({"snapshot_id": manifest.snapshot_id, "sha256": manifest.sha256}),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let (_, text) = step_text(&w, 2);
    assert!(text.contains("Bo: Data copy up to date"), "{text}");
    // A change after the copy makes it out of date, honestly.
    lock(&w.state.0.db)
        .execute(
            "INSERT INTO channels(id,name,kind,topic) VALUES('after-copy','after-copy','text','')",
            [],
        )
        .unwrap();
    let (_, text) = step_text(&w, 2);
    assert!(text.contains("Bo: Can't take over: copy is out of date"), "{text}");
    // "Ready to take over" stays off in this version.
    let (_, view) = request(&w.app, "GET", "/api/hosting", Some(&w.anna.token), Value::Null).await;
    let last = view["takeover_setup"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(last["done"], false);
}

#[tokio::test]
async fn revoking_a_co_host_removes_its_place_the_agreement_and_its_prepared_copy() {
    let w = world().await;
    let (bo_key, clara_key) = (key(&w, "bo-pc"), key(&w, "clara-pc"));
    enroll(&w, &w.bo, &bo_key).await;
    enroll(&w, &w.clara, &clara_key).await;
    consent(&w, &w.bo, &bo_key).await;
    consent(&w, &w.clara, &clara_key).await;
    let (_, manifest) = request(
        &w.app,
        "POST",
        "/api/hosting/takeover/snapshot",
        Some(&w.clara.token),
        Value::Null,
    )
    .await;
    let manifest: Manifest = serde_json::from_value(manifest).unwrap();
    let part = format!(
        "/api/hosting/takeover/snapshot/{}/parts/0",
        manifest.snapshot_id
    );
    assert_eq!(bytes(&w.app, &w.clara.token, &part).await.0, StatusCode::OK);

    let (code, _) = request(
        &w.app,
        "DELETE",
        &format!("/api/hosting/candidates/{}", w.clara.user.id),
        Some(&w.anna.token),
        Value::Null,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let cohort = load(&lock(&w.state.0.db)).unwrap();
    assert_eq!(cohort.members.len(), 1);
    assert!(cohort.set.is_none(), "the old agreement no longer holds");
    assert!(cohort.members[0].accept.is_none(), "Bo's consent was for the old set");
    // Clara's prepared copy is gone and she can no longer take part.
    assert_ne!(bytes(&w.app, &w.clara.token, &part).await.0, StatusCode::OK);
    assert_eq!(
        enroll(&w, &w.clara, &clara_key).await.0,
        StatusCode::FORBIDDEN
    );
    assert!(!step_text(&w, 0).0);
    // Stepping down yourself has the same effect.
    request(
        &w.app,
        "POST",
        "/api/hosting/answer",
        Some(&w.bo.token),
        json!({"accepted":[]}),
    )
    .await;
    assert!(load(&lock(&w.state.0.db)).unwrap().members.is_empty());
    // On the co-host's own computer the app removes the copy it can no longer use.
    bo_key.forget_copy();
    assert!(bo_key.installed().is_none());
}
