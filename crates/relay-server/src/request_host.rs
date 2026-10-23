//! Reject browser DNS rebinding before request bodies, passwords or first-owner setup.
use super::*;

/// IP literals cannot be rebound by DNS. Named public hosts must be configured
/// explicitly; a forwarded header is never evidence that a host is ours.
fn allowed(value: &str, configured: &[String]) -> bool {
    if value.len() > 300 || value.contains(['@', '/', '\\', '%', '?', '#']) {
        return false;
    }
    if value.ends_with(':') {
        return false;
    }
    let Ok(url) = url::Url::parse(&format!("http://{value}/")) else {
        return false;
    };
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return !ip.is_unspecified() && !ip.is_multicast();
    }
    configured
        .iter()
        .any(|name| host.eq_ignore_ascii_case(name))
}

pub(super) async fn check(
    State(state): State<AppState>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    // This extension can only be installed by the capability-authenticated peer
    // listener. Its own guard has already stripped browser/forwarding claims.
    if request.extensions().get::<NetworkIngress>().is_some() {
        return next.run(request).await;
    }
    let mut hosts = request.headers().get_all(header::HOST).iter();
    let host = hosts.next();
    let valid = hosts.next().is_none()
        && match host {
            Some(host) => host
                .to_str()
                .is_ok_and(|host| allowed(host, &state.0.config.allowed_hosts)),
            // In-process router calls have no authority. HTTP/2 uses :authority;
            // actual HTTP listeners always add ConnectInfo, including HTTP/1.0.
            None => match request.uri().authority() {
                Some(host) => allowed(host.as_str(), &state.0.config.allowed_hosts),
                None => {
                    request
                        .extensions()
                        .get::<ConnectInfo<SocketAddr>>()
                        .is_none()
                        && !request.headers().contains_key(header::ORIGIN)
                }
            },
        };
    if !valid {
        return ApiError::new(
            StatusCode::MISDIRECTED_REQUEST,
            "This server address is not allowed",
        )
        .into_response();
    }
    next.run(request).await
}

/// Hostnames only, not wildcard patterns, URLs or forwarded authorities.
pub fn validate_names(names: &[String]) -> Result<(), &'static str> {
    if names.len() > 16
        || names.iter().any(|name| {
            name.len() > 253
                || !name.is_ascii()
                || name.is_empty()
                || name.split('.').any(|label| {
                    label.is_empty()
                        || label.len() > 63
                        || label.starts_with('-')
                        || label.ends_with('-')
                        || !label
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                })
        })
    {
        return Err("Allowed hosts must be at most 16 exact DNS names (no URL, port or wildcard)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower::ServiceExt;

    #[test]
    fn names_are_exact_and_literal_lan_loopback_ipv6_and_public_ips_still_work() {
        let configured = vec!["chat.example.org".into()];
        for host in [
            "127.0.0.1:4580",
            "localhost:4580",
            "[::1]:4580",
            "192.168.111.6:4580",
            "100.64.0.7",
            "203.0.113.1:8443",
            "CHAT.example.org:8443",
        ] {
            assert!(allowed(host, &configured), "{host}");
        }
        for host in [
            "evil.example:4580",
            "chat.example.org.evil.example",
            "evil@127.0.0.1",
            "0.0.0.0:4580",
            "[::]:4580",
            "localhost:bad",
            "localhost/evil",
            "localhost\\evil",
            "localhost#evil",
            "localhost%00",
        ] {
            assert!(!allowed(host, &configured), "{host}");
        }
        assert!(validate_names(&configured).is_ok());
        for name in [
            "*",
            "*.example.org",
            "https://chat.example.org",
            "chat.example.org:8443",
            "evil..example",
            "-evil.example",
        ] {
            assert!(validate_names(&[name.into()]).is_err());
        }
    }

    #[tokio::test]
    async fn rebinding_headers_cannot_create_first_owner_and_forwarding_claims_do_not_bypass() {
        let data = tempfile::tempdir().unwrap();
        let state = AppState::open(data.path(), "host guard", 0).unwrap();
        let app = router(state.clone());
        for host in ["evil.example", "localhost.evil.example"] {
            let response = app
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .method("POST")
                        .uri("/api/register")
                        .header("host", host)
                        .header("x-forwarded-host", "127.0.0.1:4580")
                        .header("x-relay-network-ingress", "fake-proof")
                        .header("content-type", "application/json")
                        .extension(ConnectInfo(
                            "127.0.0.1:54321".parse::<SocketAddr>().unwrap(),
                        ))
                        .body(Body::from(
                            r#"{"username":"Attacker","password":"not-used-password"}"#,
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::MISDIRECTED_REQUEST);
        }
        assert!(!state.has_users().unwrap());
        for host in [
            "127.0.0.1:4580",
            "localhost:4580",
            "[::1]:4580",
            "192.168.111.6:4580",
        ] {
            let response = app
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .uri("/api/health")
                        .header("host", host)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
        }
    }
}
