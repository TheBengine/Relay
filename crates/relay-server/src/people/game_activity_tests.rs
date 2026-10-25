//! Opt-in game activity: private to current friends, short-lived, never stored.
use super::*;
use crate::tests::request;
use serde_json::{json, Value};

const GOOD_LINK: &str = "steam://joinlobby/730/109775241234567890/76561198000000000";

struct World {
    data: tempfile::TempDir,
    state: AppState,
    app: Router,
}

impl World {
    fn new() -> Self {
        let data = tempfile::tempdir().unwrap();
        let state = AppState::open_with_config(data.path(), "Relay friends", 0, config()).unwrap();
        let app = router(state.clone());
        Self { data, state, app }
    }
    async fn befriend(&self, x: &Session, y: &Session) {
        for (from, to) in [(x, y), (y, x)] {
            let (status, _) = request(
                &self.app,
                "POST",
                &format!("/api/friends/{}", to.user.id),
                Some(&from.token),
                Value::Null,
            )
            .await;
            assert_eq!(status, StatusCode::OK);
        }
    }
    async fn put(&self, who: &Session, body: Value) -> (StatusCode, Value) {
        request(&self.app, "PUT", "/api/activity", Some(&who.token), body).await
    }
    /// What `viewer` is shown about `person` in their own bootstrap.
    async fn seen(&self, viewer: &Session, person: &Session) -> Option<Value> {
        let (_, boot) = request(
            &self.app,
            "GET",
            "/api/bootstrap",
            Some(&viewer.token),
            Value::Null,
        )
        .await;
        boot["presence"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["user_id"] == person.user.id)
            .cloned()
    }
    async fn activity(&self, viewer: &Session, person: &Session) -> Option<Value> {
        self.seen(viewer, person)
            .await
            .and_then(|p| p.get("activity").filter(|a| !a.is_null()).cloned())
    }
}

#[tokio::test]
async fn nothing_is_shared_by_default_and_a_community_has_no_such_route() {
    let w = World::new();
    let (a, b) = (account(&w.state, "alice"), account(&w.state, "bob"));
    w.befriend(&a, &b).await;
    // Friends alone share nothing, even though both are online.
    assert!(w.seen(&b, &a).await.is_some());
    assert!(w.activity(&b, &a).await.is_none());
    let (_, boot) = request(&w.app, "GET", "/api/bootstrap", Some(&b.token), Value::Null).await;
    assert!(boot["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c == relay_protocol::game_activity::CAPABILITY));
    // An ordinary community neither advertises nor accepts activity.
    let temp = tempfile::tempdir().unwrap();
    let community = AppState::open(temp.path(), "community", 0).unwrap();
    let app = router(community);
    let owner = crate::tests::signup(&app, "Owner", "").await;
    let (_, boot) = request(
        &app,
        "GET",
        "/api/bootstrap",
        Some(&owner.token),
        Value::Null,
    )
    .await;
    assert!(!boot["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c == relay_protocol::game_activity::CAPABILITY));
    let (status, _) = request(
        &app,
        "PUT",
        "/api/activity",
        Some(&owner.token),
        json!({"game_id":"terraria"}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn only_current_unblocked_online_friends_see_it() {
    let w = World::new();
    let (a, b, c, d, e) = (
        account(&w.state, "alice"),
        account(&w.state, "bob"),
        account(&w.state, "cara"),
        account(&w.state, "dave"),
        account(&w.state, "erin"),
    );
    w.befriend(&a, &b).await;
    w.befriend(&a, &e).await;
    // Dave has only asked to be Alice's friend; Cara is a stranger.
    request(
        &w.app,
        "POST",
        &format!("/api/friends/{}", a.user.id),
        Some(&d.token),
        Value::Null,
    )
    .await;
    let (status, shared) = w.put(&a, json!({"game_id":"counter-strike-2"})).await;
    assert_eq!(status, StatusCode::OK, "{shared}");
    assert_eq!(shared["name"], "Counter-Strike 2");
    let seen = w.activity(&b, &a).await.unwrap();
    assert_eq!(seen["game_id"], "counter-strike-2");
    assert_eq!(seen["name"], "Counter-Strike 2");
    assert!(
        w.activity(&a, &a).await.is_some(),
        "the sharer sees their own"
    );
    // A stranger and a pending request see no trace of it, not even that Alice exists.
    for outsider in [&c, &d] {
        assert!(w.seen(outsider, &a).await.is_none());
    }
    // Going invisible hides it at once and showing up again brings it back.
    for (status, visible) in [("invisible", false), ("online", true)] {
        let (code, _) = request(
            &w.app,
            "PUT",
            "/api/presence",
            Some(&a.token),
            json!({"status":status,"text":""}),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(w.activity(&b, &a).await.is_some(), visible, "{status}");
    }
    // A presence that went stale (Alice is no longer around) carries no activity.
    lock(&w.state.0.db)
        .execute(
            "UPDATE user_presence SET last_seen=last_seen-1000 WHERE user_id=?1",
            [&a.user.id],
        )
        .unwrap();
    assert_eq!(w.seen(&e, &a).await.unwrap()["status"], "offline");
    assert!(w.activity(&e, &a).await.is_none());
    // Renewing makes her present again; then Bob blocks her and sees nothing.
    assert_eq!(
        w.put(&a, json!({"game_id":"counter-strike-2"})).await.0,
        StatusCode::OK
    );
    let (code, _) = request(
        &w.app,
        "POST",
        &format!("/api/blocks/{}", a.user.id),
        Some(&b.token),
        Value::Null,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert!(w.activity(&b, &a).await.is_none());
    // The friend who did not block still sees it.
    assert_eq!(
        w.put(&a, json!({"game_id":"counter-strike-2"})).await.0,
        StatusCode::OK
    );
    assert!(w.activity(&e, &a).await.is_some());
}

#[tokio::test]
async fn it_expires_by_itself_is_withdrawn_on_request_and_is_never_stored() {
    let w = World::new();
    let (a, b) = (account(&w.state, "alice"), account(&w.state, "bob"));
    w.befriend(&a, &b).await;
    let (_, shared) = w.put(&a, json!({"game_id":"terraria"})).await;
    let valid_for = shared["expires_at"].as_u64().unwrap() - now();
    assert!(
        valid_for <= relay_protocol::game_activity::TTL_SECS && valid_for > 100,
        "{valid_for}"
    );
    assert!(w.activity(&b, &a).await.is_some());
    // Nobody renews it: it disappears without any request.
    activity::age(
        &w.state,
        &a.user.id,
        relay_protocol::game_activity::TTL_SECS + 1,
    );
    assert!(w.activity(&b, &a).await.is_none());
    // Renewing starts a fresh validity; withdrawing ends it immediately and twice is fine.
    w.put(&a, json!({"game_id":"terraria"})).await;
    assert!(w.activity(&b, &a).await.is_some());
    for _ in 0..2 {
        let (status, _) = request(
            &w.app,
            "DELETE",
            "/api/activity",
            Some(&a.token),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(w.activity(&b, &a).await.is_none());
    }
    // It lives only in memory: a restarted service knows nothing of it, and it is not in the database.
    w.put(&a, json!({"game_id":"terraria"})).await;
    let tables: Vec<String> = {
        let db = lock(&w.state.0.db);
        let mut q = db
            .prepare("SELECT name FROM sqlite_master WHERE sql LIKE '%terraria%' OR name LIKE '%activity%'")
            .unwrap();
        q.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    assert!(tables.is_empty(), "{tables:?}");
    let path = w.data.path().to_path_buf();
    drop(w.app);
    drop(w.state);
    let restarted = AppState::open_with_config(&path, "Relay friends", 0, config()).unwrap();
    assert!(lock(&restarted.0.activity).is_empty());
}

#[tokio::test]
async fn only_catalog_games_and_a_matching_steam_link_are_accepted() {
    let w = World::new();
    let (a, b) = (account(&w.state, "alice"), account(&w.state, "bob"));
    w.befriend(&a, &b).await;
    // The game must be in the catalog, and nothing but a game id and a link is read.
    assert_eq!(
        w.put(&a, json!({"game_id":"my-secret-game"})).await.0,
        StatusCode::BAD_REQUEST
    );
    for body in [
        json!({"game_id":"terraria","name":"Anything I like"}),
        json!({"game_id":"terraria","process":"secret.exe"}),
        json!({"game_id":"terraria","window_title":"Inbox - Mail"}),
    ] {
        assert_eq!(w.put(&a, body).await.0, StatusCode::UNPROCESSABLE_ENTITY);
    }
    assert!(w.activity(&b, &a).await.is_none());
    // Links: the same game's Steam lobby only.
    for bad in [
        "steam://joinlobby/570/1/2",
        "steam://run/730",
        "https://example.com/join",
        "javascript:alert(1)",
        "file:///C:/Windows/System32/calc.exe",
        "steam://joinlobby/730/1/2 && calc",
        "steam://joinlobby/730/0/2",
        "steam://joinlobby/730/1/18446744073709551616",
    ] {
        assert_eq!(
            w.put(&a, json!({"game_id":"counter-strike-2","join":bad}))
                .await
                .0,
            StatusCode::BAD_REQUEST,
            "{bad}"
        );
    }
    // A game with no Steam id cannot carry a link.
    assert_eq!(
        w.put(&a, json!({"game_id":"fortnite","join":GOOD_LINK}))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    assert!(w.activity(&b, &a).await.is_none());
    let (status, shared) = w
        .put(
            &a,
            json!({"game_id":"counter-strike-2","join":format!("  {GOOD_LINK}\n")}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{shared}");
    assert_eq!(w.activity(&b, &a).await.unwrap()["join"], GOOD_LINK);
    // Renewing without a link removes it: a stale lobby never lingers.
    w.put(&a, json!({"game_id":"counter-strike-2"})).await;
    assert!(w.activity(&b, &a).await.unwrap().get("join").is_none());
    // A status update cannot smuggle activity in either.
    let (_, sneaky) = request(
        &w.app,
        "PUT",
        "/api/presence",
        Some(&a.token),
        json!({"status":"online","text":"","activity":{"game_id":"dota-2","name":"Fake","expires_at":99999999999u64}}),
    )
    .await;
    assert!(sneaky.get("activity").is_none_or(Value::is_null));
    assert_eq!(
        w.activity(&b, &a).await.unwrap()["game_id"],
        "counter-strike-2"
    );
}

#[tokio::test]
async fn the_store_is_bounded_prunes_expired_entries_and_never_evicts_a_current_person() {
    let w = World::new();
    let (a, b) = (account(&w.state, "alice"), account(&w.state, "bob"));
    w.befriend(&a, &b).await;
    assert_eq!(
        w.put(&a, json!({"game_id":"terraria"})).await.0,
        StatusCode::OK
    );
    // The store is full of people who renewed just now.
    activity::fill(&w.state, activity::MAX_USERS - 1);
    assert_eq!(activity::count(&w.state), activity::MAX_USERS);
    // Someone who already has an announcement can still renew and change it.
    let (status, renewed) = w.put(&a, json!({"game_id":"counter-strike-2"})).await;
    assert_eq!(status, StatusCode::OK, "{renewed}");
    assert_eq!(activity::count(&w.state), activity::MAX_USERS);
    // A new person is turned away; nobody is evicted to make room.
    let (status, _) = w.put(&b, json!({"game_id":"terraria"})).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(activity::count(&w.state), activity::MAX_USERS);
    assert!(w.activity(&b, &a).await.is_some());
    assert!(w.activity(&a, &b).await.is_none());
    // Once the others stop renewing they are pruned and the newcomer fits.
    activity::lapse_all_but(&w.state, &a.user.id);
    assert_eq!(
        w.put(&b, json!({"game_id":"terraria"})).await.0,
        StatusCode::OK
    );
    assert_eq!(activity::count(&w.state), 2);
    assert!(w.activity(&a, &b).await.is_some());
}
