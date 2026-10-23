//! Which requests a friend's Relay may send through, and how they are rewritten.
//!
//! This is the same fixed-target policy as the Tailscale gateway
//! (`network-helper/proxy.go`): a friend reaches the community's public API and
//! nothing else. It is not a general proxy, and local management and host secret
//! paths are never reachable. The vectors in the tests are the Go ones.
use hyper::{
    header::{HeaderName, CONNECTION, ORIGIN, UPGRADE},
    HeaderMap, Method, Uri,
};
use sha2::{Digest, Sha256};
use std::net::Ipv4Addr;

/// May this request be forwarded to the community?
pub fn request_allowed(method: &Method, uri: &Uri, headers: &HeaderMap) -> bool {
    if headers.contains_key(ORIGIN) {
        return false;
    }
    if headers
        .get("sec-fetch-site")
        .is_some_and(|value| !value.is_empty())
        || *method == Method::CONNECT
    {
        return false;
    }
    // Only origin-form requests ("/api/..."), never an absolute address to proxy to.
    if uri.authority().is_some() || uri.scheme().is_some() {
        return false;
    }
    let path = uri.path();
    if path.contains(['\\', '\0', '\r', '\n', '%']) || !path.starts_with('/') {
        return false;
    }
    if path.split('/').any(|part| part == "." || part == "..") {
        return false;
    }
    let lower = path.to_ascii_lowercase();
    let parts: Vec<&str> = lower.split('/').filter(|part| !part.is_empty()).collect();
    if matches!(
        parts.first().copied(),
        Some("host-secret" | "local-management" | "localapi")
    ) {
        return false;
    }
    if parts.first() == Some(&"api")
        && matches!(
            parts.get(1).copied(),
            Some("local" | "host-secret" | "local-management")
        )
    {
        return false;
    }
    path == "/health"
        || path == "/api/health"
        || path.starts_with("/api/")
        || path == "/rtc"
        || path.starts_with("/rtc/")
}

/// Headers a friend must never be able to set, because the server reads them as
/// proof of local or private-network identity.
pub fn is_ingress_header(name: &HeaderName) -> bool {
    let name = name.as_str();
    name == "forwarded"
        || name.starts_with("x-forwarded-")
        || name.starts_with("x-relay-network-")
        || name == "x-relay-local-control"
        || name == "x-relay-instance-id"
        || name == "x-relay-server-id"
        || name == "proxy-authorization"
}

const HOP_BY_HOP: [&str; 7] = [
    "connection",
    "keep-alive",
    "proxy-connection",
    "proxy-authenticate",
    "te",
    "trailer",
    "transfer-encoding",
];

/// Copies the headers which may go on, without anything a friend could abuse.
/// An upgrade (a WebSocket) keeps the headers which describe it.
pub fn forwardable_headers(from: &HeaderMap) -> HeaderMap {
    let upgrading = is_upgrade(from);
    let mut out = HeaderMap::new();
    for (name, value) in from {
        if is_ingress_header(name) || name == hyper::header::HOST {
            continue;
        }
        let hop = HOP_BY_HOP.contains(&name.as_str());
        if hop && !(upgrading && name == CONNECTION) {
            continue;
        }
        if name == UPGRADE && !upgrading {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    out
}

pub fn is_upgrade(headers: &HeaderMap) -> bool {
    headers
        .get(CONNECTION)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(',')
                .any(|t| t.trim().eq_ignore_ascii_case("upgrade"))
        })
        && headers.contains_key(UPGRADE)
}

/// The address the server sees for a friend. The server accepts only private
/// network addresses here, so the host assigns one from the friend's public key.
/// It is never taken from anything the friend sends, and it identifies a friend
/// to the server's rate limits, not to anyone else.
pub fn synthetic_peer_ip(endpoint_id: &[u8; 32]) -> Ipv4Addr {
    let digest = Sha256::digest(endpoint_id);
    let last = match digest[3] {
        0 | 255 => 1,
        other => other,
    };
    Ipv4Addr::new(100, 64 + (digest[0] & 0x3f), digest[1], last.max(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper::header::HeaderValue;

    fn allowed(method: &str, path: &str, headers: &[(&str, &str)]) -> bool {
        let mut map = HeaderMap::new();
        for (k, v) in headers {
            map.append(
                HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        request_allowed(
            &Method::from_bytes(method.as_bytes()).unwrap(),
            &path.parse().unwrap(),
            &map,
        )
    }

    #[test]
    fn the_public_api_is_reachable() {
        for path in [
            "/health",
            "/api/health",
            "/api/messages?cursor=1",
            "/api/ws",
            "/rtc",
            "/rtc/validate",
            "/api/e2ee/context",
        ] {
            assert!(allowed("GET", path, &[]), "{path}");
        }
        assert!(allowed(
            "POST",
            "/api/auth/login",
            &[("content-type", "application/json")]
        ));
        assert!(allowed(
            "GET",
            "/api/ws",
            &[("connection", "Upgrade"), ("upgrade", "websocket")]
        ));
    }

    #[test]
    fn management_secrets_and_everything_else_stay_private() {
        for path in [
            "/",
            "/index.html",
            "/host-secret",
            "/local-management/stop",
            "/localapi/v0/status",
            "/api/local",
            "/api/local/x",
            "/api/host-secret/x",
            "/api/local-management",
            "/API/LOCAL",
            "/Host-Secret",
            "//host-secret",
            "/api/../host-secret",
            "/api/./x/../local",
            "/api/%2e%2e/host-secret",
            "/api/x\\y",
            "/apix",
            "/metrics",
            "/robots.txt",
        ] {
            assert!(!allowed("GET", path, &[]), "{path}");
        }
    }

    #[test]
    fn browsers_proxies_and_absolute_targets_are_refused() {
        assert!(!allowed(
            "GET",
            "/api/health",
            &[("origin", "https://evil.example")]
        ));
        assert!(!allowed(
            "GET",
            "/api/health",
            &[("sec-fetch-site", "cross-site")]
        ));
        assert!(!allowed("CONNECT", "/api/health", &[]));
        let mut headers = HeaderMap::new();
        headers.clear();
        assert!(!request_allowed(
            &Method::GET,
            &"http://127.0.0.1:4580/api/health".parse().unwrap(),
            &headers
        ));
        assert!(!request_allowed(
            &Method::GET,
            &"127.0.0.1:4580".parse().unwrap(),
            &headers
        ));
    }

    #[test]
    fn identity_headers_are_removed_and_a_websocket_keeps_its_upgrade() {
        let mut from = HeaderMap::new();
        for (k, v) in [
            ("authorization", "Bearer user-token"),
            ("content-type", "application/json"),
            ("x-relay-network-ingress", "forged"),
            ("x-relay-network-peer", "100.64.0.1"),
            ("x-relay-local-control", "yes"),
            ("x-relay-instance-id", "i"),
            ("x-relay-server-id", "s"),
            ("x-forwarded-for", "127.0.0.1"),
            ("forwarded", "for=127.0.0.1"),
            ("proxy-authorization", "x"),
            ("connection", "keep-alive"),
            ("keep-alive", "5"),
            ("transfer-encoding", "chunked"),
            ("host", "friend.example"),
        ] {
            from.append(
                HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        let out = forwardable_headers(&from);
        let names: Vec<_> = out.keys().map(|n| n.as_str().to_owned()).collect();
        assert_eq!(names, ["authorization", "content-type"]);
        let mut socket = HeaderMap::new();
        socket.append(CONNECTION, HeaderValue::from_static("keep-alive, Upgrade"));
        socket.append(UPGRADE, HeaderValue::from_static("websocket"));
        socket.append("sec-websocket-key", HeaderValue::from_static("abc"));
        assert!(is_upgrade(&socket));
        let out = forwardable_headers(&socket);
        assert!(
            out.contains_key(CONNECTION)
                && out.contains_key(UPGRADE)
                && out.contains_key("sec-websocket-key")
        );
        // An "upgrade" header without the connection token is just removed.
        let mut sneaky = HeaderMap::new();
        sneaky.append(UPGRADE, HeaderValue::from_static("h2c"));
        assert!(!is_upgrade(&sneaky) && !forwardable_headers(&sneaky).contains_key(UPGRADE));
    }

    #[test]
    fn a_friends_synthetic_address_is_stable_private_and_never_sent_by_the_friend() {
        let a = synthetic_peer_ip(&[7u8; 32]);
        assert_eq!(a, synthetic_peer_ip(&[7u8; 32]));
        assert_ne!(a, synthetic_peer_ip(&[8u8; 32]));
        for seed in 0..=255u8 {
            let ip = synthetic_peer_ip(&[seed; 32]);
            let [first, second, _, last] = ip.octets();
            assert!(
                first == 100 && (64..=127).contains(&second) && last != 0 && last != 255,
                "{ip}"
            );
        }
    }
}
