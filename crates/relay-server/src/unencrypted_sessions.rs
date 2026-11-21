//! Sessions signed in over plain HTTP from another machine are marked, so the
//! member can see "signed in over an unencrypted connection" next to them and
//! sign them out.
//!
//! The server only ever speaks plain HTTP itself; HTTPS ends in a proxy on this
//! PC (loopback) or in a configured trusted proxy, and Relay's private network
//! and iroh arrive through the authenticated helper listener or on loopback. So a
//! request is unencrypted on its way when it came straight from a non-loopback,
//! non-Tailscale socket address, or from a trusted proxy that says the client
//! used `http`.
//!
//! The mark lives in memory only: the database schema is unchanged, and a server
//! restart forgets it (the session itself stays valid).
use super::*;
use std::collections::HashSet;

/// Bounded: when full, marks of sessions that no longer exist are dropped first.
const MAX_MARKS: usize = 16 * 1024;

tokio::task_local! {
    static SINK: Arc<Mutex<HashSet<String>>>;
}

#[derive(Default)]
pub(crate) struct Marks(Arc<Mutex<HashSet<String>>>);

fn private_network(ip: std::net::IpAddr) -> bool {
    match ip {
        // Tailscale's address range (WireGuard between the two machines).
        std::net::IpAddr::V4(v) => v.octets()[0] == 100 && (64..=127).contains(&v.octets()[1]),
        std::net::IpAddr::V6(v) => v.segments()[..3] == [0xfd7a, 0x115c, 0xa1e0],
    }
}

pub(crate) fn unencrypted(state: &AppState, request: &axum::extract::Request) -> bool {
    if request.extensions().get::<NetworkIngress>().is_some() {
        return false;
    }
    let Some(ip) = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|peer| peer.0.ip().to_canonical())
    else {
        // In-process calls: no socket, nothing travelled.
        return false;
    };
    if ip.is_loopback() || private_network(ip) {
        return false;
    }
    if state.0.config.trusted_proxy_ips.contains(&ip) {
        return request
            .headers()
            .get("x-forwarded-proto")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.rsplit(',').next())
            .is_some_and(|proto| proto.trim().eq_ignore_ascii_case("http"));
    }
    true
}

/// Innermost layer: sessions created while handling this request are marked.
pub(crate) async fn layer(
    State(state): State<AppState>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    if unencrypted(&state, &request) {
        SINK.scope(state.0.unencrypted_sessions.0.clone(), next.run(request))
            .await
    } else {
        next.run(request).await
    }
}

/// Called with the stored verifier of every new session.
pub(crate) fn note(db: &Connection, verifier: &str) {
    let _ = SINK.try_with(|marks| {
        let mut marks = lock(marks);
        if marks.len() >= MAX_MARKS {
            marks.retain(|v| {
                db.query_row("SELECT 1 FROM sessions WHERE token_hash=?1", [v], |_| Ok(()))
                    .is_ok()
            });
        }
        if marks.len() < MAX_MARKS {
            marks.insert(verifier.to_owned());
        }
    });
}

pub(crate) fn marked(state: &AppState, verifier: &str) -> bool {
    lock(&state.0.unencrypted_sessions.0).contains(verifier)
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    async fn call(
        app: &Router,
        peer: &str,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: serde_json::Value,
        forwarded: Option<&str>,
    ) -> (StatusCode, serde_json::Value) {
        let socket: SocketAddr = peer.parse().unwrap();
        let mut request = axum::http::Request::builder()
            .method(method)
            .uri(path)
            .header(
                "host",
                match socket.ip() {
                    std::net::IpAddr::V6(ip) => format!("[{ip}]:4580"),
                    ip => format!("{ip}:4580"),
                },
            )
            .header("content-type", "application/json")
            .extension(ConnectInfo(socket));
        if let Some(token) = token {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        if let Some(proto) = forwarded {
            request = request.header("x-forwarded-proto", proto);
        }
        let response = app
            .clone()
            .oneshot(request.body(axum::body::Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap_or_default())
    }

    #[tokio::test]
    async fn sessions_signed_in_over_plain_lan_http_are_marked_and_others_are_not() {
        let data = tempfile::tempdir().unwrap();
        let state = AppState::open_with_config(
            data.path(),
            "unencrypted fixture",
            0,
            ServerConfig {
                trusted_proxy_ips: vec!["192.168.1.2".parse().unwrap()],
                ..Default::default()
            },
        )
        .unwrap();
        let app = router(state.clone());
        let login = serde_json::json!({"username":"owner","password":"a-long-test-password"});
        let (status, owner) = call(&app, "127.0.0.1:50000", "POST", "/api/register", None, login.clone(), None).await;
        assert_eq!(status, StatusCode::OK, "{owner}");
        let mut tokens = Vec::new();
        for (peer, forwarded, expected) in [
            ("192.168.1.30:50001", None, true),
            ("127.0.0.1:50002", None, false),
            ("100.101.102.103:50003", None, false),
            ("[fd7a:115c:a1e0::5]:50004", None, false),
            ("192.168.1.2:50005", Some("https"), false),
            ("192.168.1.2:50006", Some("http"), true),
        ] {
            let (status, session) = call(&app, peer, "POST", "/api/login", None, login.clone(), forwarded).await;
            assert_eq!(status, StatusCode::OK, "{peer}: {session}");
            tokens.push((session["token"].as_str().unwrap().to_owned(), expected));
        }
        for (token, expected) in &tokens {
            let (status, list) = call(&app, "127.0.0.1:50010", "GET", "/api/sessions", Some(token), serde_json::Value::Null, None).await;
            assert_eq!(status, StatusCode::OK);
            let current = list
                .as_array()
                .unwrap()
                .iter()
                .find(|s| s["current"] == true)
                .unwrap();
            assert_eq!(current["unencrypted"].as_bool().unwrap_or(false), *expected, "{current}");
        }
        // Older clients read the list unchanged: the field is only ever added.
        let (_, list) = call(&app, "127.0.0.1:50011", "GET", "/api/sessions", Some(&tokens[1].0), serde_json::Value::Null, None).await;
        let unmarked = list.as_array().unwrap().iter().filter(|s| s.get("unencrypted").is_none()).count();
        assert_eq!(unmarked, 5, "owner's loopback sign-up plus four encrypted or local logins");
    }
}
