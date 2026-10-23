//! Explicit, ignored live acceptance using ONLY two disposable QA accounts.
//! No devices are captured; public HTTPS uses ordinary certificate verification.
use base64::{engine::general_purpose::STANDARD, Engine};
use relay_crypto::{Device, Identity, Received};
use reqwest::{Client, Method};
use serde_json::{json, Value};
use std::{path::Path, time::Duration};

const BASE: &str = "https://relay-accounts.tail787aee.ts.net/people";

async fn api(
    client: &Client,
    method: Method,
    path: &str,
    token: &str,
    body: Value,
    expected: u16,
) -> Value {
    let mut request = client
        .request(method, format!("{BASE}{path}"))
        .bearer_auth(token);
    if !body.is_null() {
        request = request.json(&body);
    }
    let response = request.send().await.expect("owned QA HTTPS request failed");
    assert_eq!(
        response.status().as_u16(),
        expected,
        "owned QA route {path}"
    );
    let raw = response.bytes().await.unwrap();
    if raw.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&raw).unwrap()
    }
}

#[tokio::test]
#[ignore = "requires explicit disposable live People sessions; never runs in a normal suite"]
async fn owned_live_people_encrypted_message_and_call_admission() {
    let input =
        std::env::var_os("RELAY_LIVE_PEOPLE_QA").expect("explicit owned QA fixture required");
    let input = Path::new(&input);
    assert!(input.is_absolute() && input.file_name().unwrap() == "live-sessions.json");
    for path in input.ancestors() {
        assert!(!std::fs::symlink_metadata(path)
            .unwrap()
            .file_type()
            .is_symlink());
    }
    let fixture: Value = serde_json::from_slice(&std::fs::read(input).unwrap()).unwrap();
    assert_eq!(fixture["base"], BASE);
    let sessions = fixture["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 2);
    for session in sessions {
        assert!(session["user"]["username"]
            .as_str()
            .unwrap()
            .starts_with("qaf"));
        assert_eq!(session["user"]["role"], "member");
    }
    let mut builder = Client::builder().timeout(Duration::from_secs(8)).no_proxy();
    if let Some(ip) = fixture["public_ip"].as_str() {
        builder = builder.resolve(
            "relay-accounts.tail787aee.ts.net",
            (ip.parse::<std::net::IpAddr>().unwrap(), 443).into(),
        );
    }
    let client = builder.build().unwrap();
    let a = &sessions[0];
    let b = &sessions[1];
    let at = a["token"].as_str().unwrap();
    let bt = b["token"].as_str().unwrap();
    let boot = api(&client, Method::GET, "/api/bootstrap", at, Value::Null, 200).await;
    assert!(boot["channels"].as_array().unwrap().is_empty());
    assert!(boot["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v == "people-v1"));
    assert_eq!(boot["members"].as_array().unwrap().len(), 1);
    let server = fixture["server_id"].as_str().unwrap();
    let mut alice = Device::new(Identity {
        server_id: server.into(),
        user_id: a["user"]["id"].as_str().unwrap().into(),
        device_id: "qa-live-alice".into(),
    })
    .unwrap();
    let mut bob = Device::new(Identity {
        server_id: server.into(),
        user_id: b["user"]["id"].as_str().unwrap().into(),
        device_id: "qa-live-bob".into(),
    })
    .unwrap();
    let data = tempfile::tempdir_in(input.parent().unwrap()).unwrap();
    let ap = data.path().join("alice");
    let bp = data.path().join("bob");
    let persist = |path: &Path, bytes: &[u8]| {
        std::fs::write(path, bytes).map_err(|_| "owned checkpoint".into())
    };
    let ak = alice
        .transact(|d| d.key_package(), |v| persist(&ap, v))
        .unwrap();
    let bk = bob
        .transact(|d| d.key_package(), |v| persist(&bp, v))
        .unwrap();
    // Explicit synthetic fingerprint comparison, not a production trust shortcut.
    alice
        .transact(|d| d.approve(&bk, &bk.fingerprint), |v| persist(&ap, v))
        .unwrap();
    bob.transact(|d| d.approve(&ak, &ak.fingerprint), |v| persist(&bp, v))
        .unwrap();
    let scope = "owned-live-friends-qa";
    alice
        .transact(
            |d| d.create_group(scope, "owned-live-group"),
            |v| persist(&ap, v),
        )
        .unwrap();
    let member = alice
        .transact(
            |d| d.add_members(scope, std::slice::from_ref(&bk)),
            |v| persist(&ap, v),
        )
        .unwrap();
    bob.transact(
        |d| {
            d.join(
                scope,
                "owned-live-group",
                ak.identity.clone(),
                member.welcome.as_ref().unwrap(),
            )
        },
        |v| persist(&bp, v),
    )
    .unwrap();
    for (token, key) in [(at, &ak), (bt, &bk)] {
        api(&client, Method::PUT, "/api/e2ee/device", token,
            json!({"device_id":key.identity.device_id,"identity":key.fingerprint,"key_package":STANDARD.encode(&key.key_package)}), 200).await;
    }
    let text = b"Owned QA: private message without a shared community";
    let wire = alice
        .transact(|d| d.encrypt(scope, text), |v| persist(&ap, v))
        .unwrap();
    let message = json!({"recipient_id":b["user"]["id"],"content":format!("relay-e2ee:v1:{}", STANDARD.encode(&wire))});
    api(
        &client,
        Method::POST,
        "/api/messages",
        at,
        message.clone(),
        403,
    )
    .await;
    api(
        &client,
        Method::POST,
        "/api/people/requests",
        at,
        json!({"handle":b["user"]["username"]}),
        200,
    )
    .await;
    let incoming = api(&client, Method::GET, "/api/bootstrap", bt, Value::Null, 200).await;
    assert!(incoming["social"]["incoming"]
        .as_array()
        .unwrap()
        .contains(&a["user"]["id"]));
    api(
        &client,
        Method::POST,
        &format!("/api/friends/{}", a["user"]["id"].as_str().unwrap()),
        bt,
        Value::Null,
        200,
    )
    .await;
    api(
        &client,
        Method::POST,
        "/api/messages",
        at,
        json!({"recipient_id":b["user"]["id"],"content":"plaintext must be refused"}),
        400,
    )
    .await;
    let began = std::time::Instant::now();
    api(
        &client,
        Method::POST,
        "/api/messages",
        at,
        message.clone(),
        200,
    )
    .await;
    let history = api(
        &client,
        Method::GET,
        &format!("/api/history?peer_id={}", a["user"]["id"].as_str().unwrap()),
        bt,
        Value::Null,
        200,
    )
    .await;
    assert!(began.elapsed() < Duration::from_secs(10));
    let stored = history["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["content"] == message["content"])
        .unwrap();
    let wire = STANDARD
        .decode(
            stored["content"]
                .as_str()
                .unwrap()
                .strip_prefix("relay-e2ee:v1:")
                .unwrap(),
        )
        .unwrap();
    let read = bob
        .transact(|d| d.receive(scope, &wire), |v| persist(&bp, v))
        .unwrap();
    assert!(matches!(read, Received::Application{bytes,..} if bytes == text));
    let call = api(
        &client,
        Method::POST,
        "/api/private-calls",
        at,
        json!({"peer_id":b["user"]["id"]}),
        200,
    )
    .await;
    api(
        &client,
        Method::POST,
        &format!("/api/private-calls/{}", call["id"].as_str().unwrap()),
        bt,
        json!({"action":"accept"}),
        200,
    )
    .await;
    let mut tickets = Vec::new();
    for (token, key) in [(at, &ak), (bt, &bk)] {
        tickets.push(
            api(
                &client,
                Method::POST,
                "/api/voice/join",
                token,
                json!({"channel_id":call["channel_id"],"device_id":key.identity.device_id}),
                200,
            )
            .await,
        );
    }
    std::fs::write(
        input.parent().unwrap().join("voice-tickets.json"),
        serde_json::to_vec(
            &json!({"tickets":tickets,"call_id":call["id"],"public_ip":fixture["public_ip"]}),
        )
        .unwrap(),
    )
    .unwrap();
    println!("Public HTTPS: two destination-bound accounts; request/accept; real MLS readback under 10s; plaintext refused; private call accepted and both voice seats admitted. No physical media.");
}
