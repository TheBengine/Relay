use super::*;
use crate::tests::{new_invite, request, signup};
use serde_json::{json, Value};

async fn get(app: &Router, token: &str) -> (StatusCode, Value) {
    request(app, "GET", "/api/hosting", Some(token), Value::Null).await
}

fn state_of(view: &Value, user: &str) -> String {
    view["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["candidate"]["user_id"] == user)
        .map(|c| c["candidate"]["state"].as_str().unwrap().to_string())
        .unwrap_or_else(|| "none".into())
}

fn dimension<'a>(view: &'a Value, user: &str, task: &str, aspect: &str) -> &'a Value {
    let candidate = view["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["candidate"]["user_id"] == user)
        .unwrap();
    let assessment = candidate["assessments"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["task"] == task)
        .unwrap();
    assessment["dimensions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["aspect"] == aspect)
        .unwrap()
}

#[tokio::test]
async fn offers_need_an_administrator_and_the_members_own_answer() {
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open(temp.path(), "Hosts", 0).unwrap();
    let app = router(state.clone());
    let anna = signup(&app, "Anna", "").await;
    let bo = signup(&app, "Bo", &new_invite(&app, &anna).await).await;
    let clara = signup(&app, "Clara", &new_invite(&app, &anna).await).await;

    let (status, empty) = get(&app, &bo.token).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(empty["can_manage"], false);
    assert_eq!(empty["candidates"], json!([]));
    assert_eq!(empty["active_host"]["name"], "Hosts");
    assert_eq!(empty["abilities"]["planned_handover"], false);
    assert_eq!(get(&app, &anna.token).await.1["can_manage"], true);

    // Members cannot offer roles, to themselves or anyone else.
    let (status, _) = request(
        &app,
        "POST",
        "/api/hosting/offers",
        Some(&bo.token),
        json!({"user_id":bo.user.id,"roles":["replica"]}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, offered) = request(
        &app,
        "POST",
        "/api/hosting/offers",
        Some(&anna.token),
        json!({"user_id":bo.user.id,"roles":["replica"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{offered}");
    assert_eq!(state_of(&offered, &bo.user.id), "pending");
    assert_eq!(offered["candidates"][0]["name"], "Bo");

    // Nothing is granted yet, and the page says exactly why.
    let permission = dimension(&offered, &bo.user.id, "data_standby", "permission");
    assert_eq!(permission["verdict"], "red");
    assert_eq!(
        permission["text"],
        "The member has not answered the offer yet."
    );
    // No machine has reported a copy, so nothing is claimed about it.
    assert_eq!(
        dimension(&offered, &bo.user.id, "data_standby", "data_copy")["verdict"],
        "unknown"
    );
    // Takeover is blocked with its reason until the protocol exists.
    let takeover = dimension(&offered, &bo.user.id, "unexpected_takeover", "takeover");
    assert_eq!(takeover["verdict"], "red");
    assert!(takeover["text"]
        .as_str()
        .unwrap()
        .contains("voting protocol"));

    // Only the member can answer for themselves, and only for what was offered.
    let answer = |token: String, roles: Value| {
        let app = app.clone();
        async move {
            request(
                &app,
                "POST",
                "/api/hosting/answer",
                Some(&token),
                json!({ "accepted": roles }),
            )
            .await
        }
    };
    assert_eq!(
        answer(clara.token.clone(), json!(["replica"])).await.0,
        StatusCode::BAD_REQUEST,
        "Clara holds no offer"
    );
    assert_eq!(
        answer(bo.token.clone(), json!(["voice_gateway"])).await.0,
        StatusCode::BAD_REQUEST,
        "voice gateway was not offered"
    );
    let (status, accepted) = answer(bo.token.clone(), json!(["replica"])).await;
    assert_eq!(status, StatusCode::OK, "{accepted}");
    assert_eq!(state_of(&accepted, &bo.user.id), "approved");
    assert_eq!(
        dimension(&accepted, &bo.user.id, "data_standby", "permission")["verdict"],
        "green"
    );
    assert!(accepted["revision"].as_u64().unwrap() >= 2);

    // Adding a role waits for a new answer; the accepted one stays.
    let (_, more) = request(
        &app,
        "POST",
        "/api/hosting/offers",
        Some(&anna.token),
        json!({"user_id":bo.user.id,"roles":["replica","voice_gateway"]}),
    )
    .await;
    assert_eq!(state_of(&more, &bo.user.id), "approved");
    assert_eq!(
        dimension(&more, &bo.user.id, "voice_gateway", "permission")["verdict"],
        "red"
    );
    assert_eq!(
        dimension(&more, &bo.user.id, "data_standby", "permission")["verdict"],
        "green"
    );

    // Only an administrator withdraws a role.
    let path = format!("/api/hosting/candidates/{}", bo.user.id);
    let (status, _) = request(&app, "DELETE", &path, Some(&bo.token), Value::Null).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, revoked) = request(&app, "DELETE", &path, Some(&anna.token), Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{revoked}");
    assert_eq!(state_of(&revoked, &bo.user.id), "revoked");
    assert_eq!(
        request(&app, "DELETE", &path, Some(&anna.token), Value::Null)
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    // A revoked member cannot accept anything.
    assert_eq!(
        answer(bo.token.clone(), json!(["replica"])).await.0,
        StatusCode::BAD_REQUEST
    );

    let audit: Vec<String> = {
        let db = lock(&state.0.db);
        let mut query = db
            .prepare("SELECT action FROM audit_log WHERE action LIKE 'hosting.%' ORDER BY sequence")
            .unwrap();
        query
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    assert_eq!(
        audit,
        [
            "hosting.offered",
            "hosting.answered",
            "hosting.offered",
            "hosting.revoked"
        ]
    );
}

#[tokio::test]
async fn members_see_current_hosts_and_their_own_entry_but_not_other_peoples_offers() {
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open(temp.path(), "Hosts", 0).unwrap();
    let app = router(state.clone());
    let anna = signup(&app, "Anna", "").await;
    let bo = signup(&app, "Bo", &new_invite(&app, &anna).await).await;
    let clara = signup(&app, "Clara", &new_invite(&app, &anna).await).await;
    let dan = signup(&app, "Dan", &new_invite(&app, &anna).await).await;
    let offer = |user: &str| {
        let app = app.clone();
        let token = anna.token.clone();
        let user = user.to_string();
        async move {
            request(
                &app,
                "POST",
                "/api/hosting/offers",
                Some(&token),
                json!({"user_id":user,"roles":["replica"]}),
            )
            .await
        }
    };
    let answer = |token: &str, roles: Value| {
        let app = app.clone();
        let token = token.to_string();
        async move {
            request(
                &app,
                "POST",
                "/api/hosting/answer",
                Some(&token),
                json!({ "accepted": roles }),
            )
            .await
        }
    };
    offer(&bo.user.id).await;
    answer(&bo.token, json!(["replica"])).await; // Bo holds a role.
    offer(&clara.user.id).await; // Clara has an open offer.
    offer(&dan.user.id).await;
    answer(&dan.token, json!([])).await; // Dan declined.

    let ids = |view: &Value| -> Vec<String> {
        view["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["candidate"]["user_id"].as_str().unwrap().to_string())
            .collect()
    };
    // The administrator sees everything.
    let (_, all) = get(&app, &anna.token).await;
    assert_eq!(ids(&all).len(), 3);
    // A bystander sees only the host that holds a role.
    let eve = signup(&app, "Eve", &new_invite(&app, &anna).await).await;
    let (_, seen) = get(&app, &eve.token).await;
    assert_eq!(ids(&seen), vec![bo.user.id.clone()]);
    assert_eq!(seen["can_manage"], false);
    // A member always sees their own entry, in any state.
    assert_eq!(
        ids(&get(&app, &clara.token).await.1),
        vec![bo.user.id.clone(), clara.user.id.clone()]
    );
    assert_eq!(
        state_of(&get(&app, &dan.token).await.1, &dan.user.id),
        "declined"
    );
    // Nobody learns that Dan declined or that Clara was asked.
    assert!(!ids(&seen).contains(&dan.user.id));
    assert!(!ids(&seen).contains(&clara.user.id));
}

#[tokio::test]
async fn the_roster_survives_a_restart_and_forgets_members_who_left() {
    let temp = tempfile::tempdir().unwrap();
    let anna_token;
    let bo_id;
    {
        let state = AppState::open(temp.path(), "Hosts", 0).unwrap();
        let app = router(state.clone());
        let anna = signup(&app, "Anna", "").await;
        let bo = signup(&app, "Bo", &new_invite(&app, &anna).await).await;
        request(
            &app,
            "POST",
            "/api/hosting/offers",
            Some(&anna.token),
            json!({"user_id":bo.user.id,"roles":["replica","voice_gateway"]}),
        )
        .await;
        request(
            &app,
            "POST",
            "/api/hosting/answer",
            Some(&bo.token),
            json!({"accepted":["replica"]}),
        )
        .await;
        anna_token = anna.token;
        bo_id = bo.user.id;
    }
    let state = AppState::open(temp.path(), "Hosts", 0).unwrap();
    let app = router(state.clone());
    let (status, view) = get(&app, &anna_token).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(state_of(&view, &bo_id), "approved");
    // The roster lives in ordinary metadata, so a backup carries it.
    let stored: String = lock(&state.0.db)
        .query_row(
            "SELECT value FROM metadata WHERE key='hosting.v1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(stored.contains("\"replica\""));

    lock(&state.0.db)
        .execute(
            "UPDATE membership SET status='left' WHERE user_id=?1",
            [&bo_id],
        )
        .unwrap();
    let (_, after) = get(&app, &anna_token).await;
    assert_eq!(
        state_of(&after, &bo_id),
        "none",
        "a member who left holds no role"
    );
}

#[tokio::test]
async fn damaged_or_newer_host_settings_are_refused_instead_of_guessed() {
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open(temp.path(), "Hosts", 0).unwrap();
    let app = router(state.clone());
    let anna = signup(&app, "Anna", "").await;
    for stored in ["not json", r#"{"schema":99,"revision":1,"candidates":[]}"#] {
        lock(&state.0.db)
            .execute(
                "INSERT INTO metadata(key,value) VALUES('hosting.v1',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                [stored],
            )
            .unwrap();
        assert_eq!(get(&app, &anna.token).await.0, StatusCode::BAD_REQUEST);
    }
    assert_eq!(
        request(&app, "GET", "/api/hosting", None, Value::Null)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
}

/// Anna hosts; Bo holds an approved data-copy role; Clara is a member.
async fn approved_bo() -> (tempfile::TempDir, AppState, Router, Session, Session, Session) {
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open(temp.path(), "Hosts", 0).unwrap();
    let app = router(state.clone());
    let anna = signup(&app, "Anna", "").await;
    let bo = signup(&app, "Bo", &new_invite(&app, &anna).await).await;
    let clara = signup(&app, "Clara", &new_invite(&app, &anna).await).await;
    post(&app, &anna.token, "/api/hosting/offers", json!({"user_id":bo.user.id,"roles":["replica"]})).await;
    assert_eq!(
        post(&app, &bo.token, "/api/hosting/answer", json!({"accepted":["replica"]})).await.0,
        StatusCode::OK
    );
    (temp, state, app, anna, bo, clara)
}

async fn post(app: &Router, token: &str, path: &str, body: Value) -> (StatusCode, Value) {
    request(app, "POST", path, Some(token), body).await
}

#[tokio::test]
async fn measured_capacity_and_the_worst_participant_reach_the_overview() {
    let (_temp, _state, app, anna, bo, clara) = approved_bo().await;
    // Nothing measured: grey, never green.
    let view = get(&app, &anna.token).await.1;
    assert_eq!(dimension(&view, &bo.user.id, "data_standby", "capacity")["verdict"], "unknown");
    // Only an approved co-host reports its own machine; nobody measures themselves.
    let capacity = json!({"capacity":{"upload_kbps":20000,"cpu_free_pct":80,"free_disk_mb":50000,"participants":0}});
    let measurements = "/api/hosting/measurements";
    assert_eq!(post(&app, &clara.token, measurements, capacity.clone()).await.0, StatusCode::FORBIDDEN);
    assert_eq!(post(&app, &bo.token, measurements, capacity).await.0, StatusCode::NO_CONTENT);
    let quality = |host: &str, loss: f32| json!({"voice":[{"host_user_id":host,"quality":{"rtt_ms":30.0,"jitter_ms":5.0,"loss_pct":loss,"late_pct":0.5}}]});
    assert_eq!(post(&app, &bo.token, measurements, quality(&bo.user.id, 0.0)).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(post(&app, &clara.token, measurements, quality(&bo.user.id, 150.0)).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(post(&app, &clara.token, measurements, quality(&bo.user.id, 0.2)).await.0, StatusCode::NO_CONTENT);
    let view = get(&app, &anna.token).await.1;
    let capacity = dimension(&view, &bo.user.id, "data_standby", "capacity");
    assert_eq!(capacity["verdict"], "green", "{capacity}");
    let voice = dimension(&view, &bo.user.id, "planned_handover", "voice_quality");
    assert_eq!(voice["verdict"], "green", "{voice}");
    // The worst-off participant decides.
    assert_eq!(post(&app, &anna.token, measurements, quality(&bo.user.id, 9.0)).await.0, StatusCode::NO_CONTENT);
    let view = get(&app, &anna.token).await.1;
    let voice = dimension(&view, &bo.user.id, "planned_handover", "voice_quality");
    assert_eq!(voice["verdict"], "red", "{voice}");
    assert!(voice["text"].as_str().unwrap().contains("packet loss"));
}

#[tokio::test]
async fn the_outage_matrix_never_promises_what_this_build_cannot_do() {
    let (_temp, _state, app, _anna, bo, _clara) = approved_bo().await;
    let view = get(&app, &bo.token).await.1;
    assert_eq!(view["takeover"]["state"], "not_set_up");
    let row = |what: &str| {
        view["outage"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["what"] == what)
            .unwrap()
            .clone()
    };
    assert_eq!(row("Friends: messages and calls")["available"], true);
    assert_eq!(row("Sign in with a community password")["available"], false);
    assert_eq!(row("Keep chatting in this community")["available"], false);
    assert!(row("Keep chatting in this community")["text"].as_str().unwrap().starts_with("Not yet"));
}

#[tokio::test]
async fn host_switching_choices_are_for_administrators_and_approved_hosts_only() {
    let (temp, state, app, anna, bo, clara) = approved_bo().await;
    let switching = "/api/hosting/switching";
    let use_host = |who: &str| json!({"mode":"use_this_host","preferred":who,"backups":[]});
    assert_eq!(post(&app, &bo.token, switching, use_host(&bo.user.id)).await.0, StatusCode::FORBIDDEN);
    assert_eq!(post(&app, &anna.token, switching, use_host(&clara.user.id)).await.0, StatusCode::BAD_REQUEST);
    let (code, view) = post(&app, &anna.token, switching, json!({"mode":"automatic","preferred":null,"backups":[bo.user.id]})).await;
    assert_eq!(code, StatusCode::OK, "{view}");
    assert_eq!(view["switching"]["mode"], "automatic");
    assert!(view["switch_advice"].as_str().unwrap().contains("waits until moving a community is available"));
    // The choice survives a restart with the roster.
    drop(app);
    drop(state);
    let state = AppState::open(temp.path(), "Hosts", 0).unwrap();
    let app = router(state);
    let view = get(&app, &bo.token).await.1;
    assert_eq!(view["switching"]["backups"][0], bo.user.id.as_str());
}

#[tokio::test]
async fn the_takeover_setup_is_a_checklist_of_what_the_host_actually_measured() {
    let (_temp, _state, app, anna, bo, clara) = approved_bo().await;
    let steps = |view: &Value| {
        view["takeover_setup"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| (s["what"].as_str().unwrap().to_owned(), s["done"].as_bool().unwrap(), s["text"].as_str().unwrap().to_owned()))
            .collect::<Vec<_>>()
    };
    let view = get(&app, &anna.token).await.1;
    let first = steps(&view);
    assert_eq!(first.len(), 5, "{first:?}");
    assert!(!first[0].1 && first[0].2.starts_with("1 of 2"), "{first:?}");
    assert!(!first[1].1 && first[1].2.starts_with("0 of 2"));
    assert!(!first[2].1);
    assert!(!first[3].1 && first[3].2 == "No co-host has a copy yet.");
    assert!(!first[4].1 && first[4].2.starts_with("Not available in this version"));
    // A second approved co-host completes the first step, and only that one.
    post(&app, &anna.token, "/api/hosting/offers", json!({"user_id":clara.user.id,"roles":["replica"]})).await;
    post(&app, &clara.token, "/api/hosting/answer", json!({"accepted":["replica"]})).await;
    let view = get(&app, &bo.token).await.1;
    let second = steps(&view);
    assert!(second[0].1, "{second:?}");
    assert!(second[1..].iter().all(|step| !step.1));
}
