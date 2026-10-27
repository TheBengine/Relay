//! A real host on loopback with two co-host computers (folders and threads)
//! following its live changes over plain HTTP, as the app does.
use super::*;
use crate::takeover_cohost::{CohostKey, Follower};
use relay_protocol::{hosting::TakeoverState, takeover::AuthoritySet};
use serde_json::{json, Value};
use std::{
    net::SocketAddr,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    thread::JoinHandle,
};

struct Host {
    url: String,
    state: AppState,
    runtime: tokio::runtime::Runtime,
}

/// Serves `data` on a fresh loopback port, with protection allowed.
fn serve(data: &std::path::Path) -> Host {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    let state = AppState::open(data, "Protected", 0).unwrap();
    state.enable_takeover_protection();
    let listener = runtime
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let routes = router(state.clone());
    runtime.spawn(async move {
        axum::serve(listener, routes.into_make_service_with_connect_info::<SocketAddr>())
            .await
            .unwrap();
    });
    Host {
        url,
        state,
        runtime,
    }
}

/// A plain HTTP call from a test thread (the server crate has no blocking client).
fn send(method: reqwest::Method, url: String, token: Option<&str>, body: Option<Vec<u8>>) -> Result<(u16, Vec<u8>), String> {
    let token = token.map(str::to_owned);
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    runtime.block_on(async move {
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(40))
            .build()
            .unwrap();
        let mut request = client.request(method, url).header("content-type", "application/json");
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        if let Some(body) = body {
            request = request.body(body);
        }
        let response = request.send().await.map_err(|e| e.to_string())?;
        let status = response.status().as_u16();
        Ok((status, response.bytes().await.map_err(|e| e.to_string())?.to_vec()))
    })
}

fn call(h: &Host, method: reqwest::Method, path: &str, token: Option<&str>, body: Value) -> (u16, Value) {
    let (status, raw) = send(method, format!("{}{path}", h.url), token, Some(body.to_string().into_bytes())).unwrap();
    (status, serde_json::from_slice(&raw).unwrap_or(Value::Null))
}

struct Member {
    token: String,
    id: String,
}

fn join(h: &Host, name: &str, invite: &str) -> Member {
    let (code, session) = call(
        h,
        reqwest::Method::POST,
        "/api/register",
        None,
        json!({"username": name, "password": "a-long-test-password", "invite": invite}),
    );
    assert_eq!(code, 200, "{session}");
    Member {
        token: session["token"].as_str().unwrap().into(),
        id: session["user"]["id"].as_str().unwrap().into(),
    }
}

/// One co-host computer following the feed until stopped.
struct Following {
    stop: Arc<AtomicBool>,
    applied: Arc<AtomicU64>,
    thread: JoinHandle<Follower>,
}

fn follow(url: String, mut follower: Follower, feed_key: String) -> Following {
    let stop = Arc::new(AtomicBool::new(false));
    let applied = Arc::new(AtomicU64::new(follower.applied_index()));
    let (s, a) = (stop.clone(), applied.clone());
    let thread = std::thread::spawn(move || {
        while !s.load(Ordering::Acquire) {
            let Ok((200, reply)) = send(reqwest::Method::POST, format!("{url}/api/hosting/takeover/feed"), None, Some(follower.request(&feed_key))) else {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            };
            if let Ok(answers) = follower.apply(&reply, &feed_key) {
                let _ = send(reqwest::Method::POST, format!("{url}/api/hosting/takeover/answers"), None, Some(answers));
            }
            a.store(follower.applied_index(), Ordering::Release);
        }
        follower
    });
    Following {
        stop,
        applied,
        thread,
    }
}

impl Following {
    fn stop(self) -> Follower {
        self.stop.store(true, Ordering::Release);
        self.thread.join().unwrap()
    }
}

fn wait_until(what: &str, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(40);
    while !ok() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Anna hosts; Bo and Clara joined, consented and installed the arming copy.
struct Protected {
    data: tempfile::TempDir,
    computers: tempfile::TempDir,
    host: Host,
    anna: Member,
    set: AuthoritySet,
    keys: Vec<CohostKey>,
    feed_keys: Vec<String>,
    channel: String,
}

fn protected() -> Protected {
    let data = tempfile::tempdir().unwrap();
    let h = serve(data.path());
    let anna = join(&h, "Anna", "");
    let invite = || call(&h, reqwest::Method::POST, "/api/invites", Some(&anna.token), Value::Null).1["code"]
        .as_str()
        .unwrap()
        .to_owned();
    let members = [join(&h, "Bo", &invite()), join(&h, "Clara", &invite())];
    for m in &members {
        call(&h, reqwest::Method::POST, "/api/hosting/offers", Some(&anna.token), json!({"user_id": m.id, "roles": ["replica"]}));
        call(&h, reqwest::Method::POST, "/api/hosting/answer", Some(&m.token), json!({"accepted": ["replica"]}));
    }
    let computers = tempfile::tempdir().unwrap();
    let server_id = h.state.server_id().unwrap();
    let pin = h.state.0.handover.route_pin();
    let keys: Vec<CohostKey> = ["bo", "clara"]
        .iter()
        .map(|n| CohostKey::open(&computers.path().join(n)).unwrap())
        .collect();
    for (m, k) in members.iter().zip(&keys) {
        let (code, body) = call(&h, reqwest::Method::POST, "/api/hosting/takeover/enroll", Some(&m.token), serde_json::to_value(k.enroll(&server_id, &m.id).unwrap()).unwrap());
        assert_eq!(code, 200, "{body}");
    }
    let mut accepts = Vec::new();
    let mut set = None;
    for (m, k) in members.iter().zip(&keys) {
        let (_, cohort) = call(&h, reqwest::Method::GET, "/api/hosting/takeover/cohort", Some(&m.token), Value::Null);
        let s: AuthoritySet = serde_json::from_value(cohort["set"].clone()).unwrap();
        let accept = k.consent(&s, &pin).unwrap();
        let (code, body) = call(&h, reqwest::Method::POST, "/api/hosting/takeover/consent", Some(&m.token), json!({ "accept": accept }));
        assert_eq!(code, 200, "{body}");
        accepts.push(accept);
        set = Some(s);
    }
    let set = set.unwrap();
    let (_, bootstrap) = call(&h, reqwest::Method::GET, "/api/bootstrap", Some(&anna.token), Value::Null);
    let channel = bootstrap["channels"][0]["id"].as_str().unwrap().to_owned();

    // Turning protection on stops changes until both copies are installed.
    let (code, manifest) = call(&h, reqwest::Method::POST, "/api/hosting/takeover/arm", Some(&anna.token), Value::Null);
    assert_eq!(code, 200, "{manifest}");
    let manifest: Manifest = serde_json::from_value(manifest).unwrap();
    let (code, _) = call(&h, reqwest::Method::POST, "/api/messages", Some(&anna.token), json!({"channel_id": channel, "content": "during arming"}));
    assert_eq!(code, 503, "no change may slip in between the copy and protection");
    let mut feed_keys = Vec::new();
    for ((m, k), accept) in members.iter().zip(&keys).zip(&accepts) {
        let (code, shared) = call(&h, reqwest::Method::POST, "/api/hosting/takeover/snapshot", Some(&m.token), Value::Null);
        assert_eq!(code, 200, "{shared}");
        assert_eq!(shared["snapshot_id"], manifest.snapshot_id.as_str(), "one shared copy");
        let image = k
            .pull(&manifest, |i| {
                send(reqwest::Method::GET, format!("{}/api/hosting/takeover/snapshot/{}/parts/{i}", h.url, manifest.snapshot_id), Some(&m.token), None)
                    .map(|(_, raw)| raw)
            })
            .unwrap();
        k.install(&set, accept, &manifest, &image).unwrap();
        let (code, reply) = call(&h, reqwest::Method::POST, "/api/hosting/takeover/verified", Some(&m.token), json!({"snapshot_id": manifest.snapshot_id, "sha256": manifest.sha256}));
        assert_eq!(code, 200, "{reply}");
        feed_keys.push(reply["feed_key"].as_str().unwrap().to_owned());
    }
    wait_until("protection to be on", || lock(&h.state.0.takeover).is_some());
    Protected {
        data,
        computers,
        host: h,
        anna,
        set,
        keys,
        feed_keys,
        channel,
    }
}

fn start(p: &Protected, followers: Vec<Follower>) -> Vec<Following> {
    followers
        .into_iter()
        .zip(&p.feed_keys)
        .map(|(f, key)| follow(p.host.url.clone(), f, key.clone()))
        .collect()
}

fn write(p: &Protected, text: &str) -> u16 {
    call(&p.host, reqwest::Method::POST, "/api/messages", Some(&p.anna.token), json!({"channel_id": p.channel, "content": text})).0
}

fn host_tip(p: &Protected) -> u64 {
    let source = lock(&p.host.state.0.takeover).as_ref().unwrap().clone();
    source.applied_tip().unwrap().index
}

/// Anna is gone: Bo takes over with Clara's vote and starts as the new host.
fn take_over(p: &Protected, followers: Vec<Follower>) -> AppState {
    let mut replicas: Vec<crate::takeover_runtime::Replica> =
        followers.into_iter().map(Follower::into_replica).collect();
    let mut clara = replicas.pop().unwrap();
    let mut bo = replicas.pop().unwrap();
    let long_ago = Instant::now().checked_sub(Duration::from_secs(300)).unwrap();
    bo.pretend_silent_since(long_ago);
    clara.pretend_silent_since(long_ago);
    struct Direct<'a>(&'a mut crate::takeover_runtime::Replica);
    impl crate::takeover_failover::VotePeers for Direct<'_> {
        fn ask(&mut self, request: &relay_protocol::takeover::VoteRequest) -> Vec<Option<crate::takeover_voter::VoteAnswer>> {
            vec![crate::takeover_failover::answer_vote(self.0, request, Instant::now()).ok(), None]
        }
    }
    let (status, mandate) = crate::takeover_failover::attempt(&mut bo, &p.set, &mut Direct(&mut clara), Duration::ZERO, Instant::now());
    assert_eq!(status.state, TakeoverState::TookOver, "{status:?}");
    let standby = p.computers.path().join("bo");
    crate::takeover_failover::promote(&mut bo, &mandate.unwrap(), &standby).unwrap();
    let new_host = p.computers.path().join("bo-host");
    crate::takeover_failover::install(&standby, &new_host).unwrap();
    AppState::open(&new_host, "Bo continues", 0).unwrap()
}

fn count(state: &AppState, text: &str) -> i64 {
    lock(&state.0.db)
        .query_row("SELECT COUNT(*) FROM messages WHERE content=?1", [text], |r| r.get(0))
        .unwrap()
}

#[test]
fn an_acknowledged_change_survives_the_host_disappearing_and_a_co_host_taking_over() {
    let p = protected();
    let followers = start(&p, p.keys.iter().map(|k| k.follower().unwrap()).collect());
    assert_eq!(write(&p, "kept through takeover"), 200, "a co-host confirmed it");
    let tip = host_tip(&p);
    wait_until("both copies to apply the change", || followers.iter().all(|f| f.applied.load(Ordering::Acquire) >= tip));
    let followers: Vec<Follower> = followers.into_iter().map(Following::stop).collect();
    let bo = take_over(&p, followers);
    assert_eq!(count(&bo, "kept through takeover"), 1, "the acknowledged change is on the new host");
    assert_eq!(count(&bo, "during arming"), 0);
}

#[test]
fn a_protected_host_restarts_through_its_replicating_database_and_loses_nothing() {
    let mut p = protected();
    let followers = start(&p, p.keys.iter().map(|k| k.follower().unwrap()).collect());
    assert_eq!(write(&p, "before restart"), 200);
    let tip = host_tip(&p);
    wait_until("the change to be applied", || followers.iter().all(|f| f.applied.load(Ordering::Acquire) >= tip));
    let followers: Vec<Follower> = followers.into_iter().map(Following::stop).collect();

    // The host process stops and starts again. Ordinary startup would refuse;
    // the protected start reopens through the replicating database.
    let Host { runtime, state, .. } = std::mem::replace(&mut p.host, serve_placeholder());
    drop(state);
    drop(runtime);
    p.host = serve(p.data.path());
    assert!(lock(&p.host.state.0.takeover).is_some(), "protected after restart");
    let followers = start(&p, followers);
    assert_eq!(write(&p, "after restart"), 200, "co-hosts reconnect with their feed keys");
    let tip = host_tip(&p);
    wait_until("the second change to be applied", || followers.iter().all(|f| f.applied.load(Ordering::Acquire) >= tip));
    let followers: Vec<Follower> = followers.into_iter().map(Following::stop).collect();
    let bo = take_over(&p, followers);
    assert_eq!(count(&bo, "before restart"), 1);
    assert_eq!(count(&bo, "after restart"), 1);
}

/// A stand-in while the real host is restarted (never served).
fn serve_placeholder() -> Host {
    let data = tempfile::tempdir().unwrap();
    let state = AppState::open(data.path(), "placeholder", 0).unwrap();
    std::mem::forget(data);
    Host {
        url: String::new(),
        state,
        runtime: tokio::runtime::Builder::new_current_thread().build().unwrap(),
    }
}

#[test]
fn changes_pause_while_no_co_host_confirms_and_resume_when_one_returns() {
    let p = protected();
    let followers = start(&p, p.keys.iter().map(|k| k.follower().unwrap()).collect());
    assert_eq!(write(&p, "first"), 200);
    let tip = host_tip(&p);
    wait_until("the first change to be applied", || followers.iter().all(|f| f.applied.load(Ordering::Acquire) >= tip));
    // Both co-hosts' computers are switched off.
    let mut followers: Vec<Follower> = followers.into_iter().map(Following::stop).collect();
    wait_until("the host to pause", || p.host.state.takeover_status().state == TakeoverState::Paused);
    assert_eq!(
        p.host.state.takeover_status().text(),
        "Paused: waiting for a co-host to confirm changes"
    );
    let (code, body) = call(&p.host, reqwest::Method::POST, "/api/messages", Some(&p.anna.token), json!({"channel_id": p.channel, "content": "while away"}));
    assert_eq!(code, 503);
    assert_eq!(body["error"], "Paused: waiting for a co-host to confirm changes");
    // Nothing was left half-done: reading still works and nothing needs recovery.
    assert!(!p.host.state.takeover_blocked());
    assert_eq!(call(&p.host, reqwest::Method::GET, "/api/bootstrap", Some(&p.anna.token), Value::Null).0, 200);

    // Clara comes back alone: with the host that is 2 of 3, so changes resume.
    let clara = follow(p.host.url.clone(), followers.pop().unwrap(), p.feed_keys[1].clone());
    wait_until("changes to resume", || p.host.state.takeover_status().state == TakeoverState::ProtectedHost);
    assert_eq!(write(&p, "after return"), 200);
    let tip = host_tip(&p);
    wait_until("Clara to apply it", || clara.applied.load(Ordering::Acquire) >= tip);
    // Bo returns later and catches up on what he missed.
    let bo = follow(p.host.url.clone(), followers.pop().unwrap(), p.feed_keys[0].clone());
    wait_until("Bo to catch up", || bo.applied.load(Ordering::Acquire) >= tip);
    let followers = vec![bo.stop(), clara.stop()];
    let new_host = take_over(&p, followers);
    assert_eq!(count(&new_host, "first"), 1);
    assert_eq!(count(&new_host, "after return"), 1);
    assert_eq!(count(&new_host, "while away"), 0, "a refused change never appears");
}
