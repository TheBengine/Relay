//! `POST /api/hosting/measurements/upload`: a co-host's app measures its own
//! upload by timing this one request (#19). The body is read and dropped;
//! nothing is stored. One probe per member a minute, bounded in members tracked.
//!
//! `GET /api/hosting/measurements/download`: a member's app times how fast it
//! receives `DOWNLOAD_BYTES` from the host. The host's upload is at least the
//! fastest such rate, so the running host gets a measured upload too. The bytes
//! are random so no compression on the way can shorten the test.
use super::*;
use std::collections::HashMap;
use std::time::{Duration as StdDuration, Instant};

/// The largest probe body. The app sends 256 KiB.
pub(crate) const MAX_BYTES: usize = 512 * 1024;
const EVERY: StdDuration = StdDuration::from_secs(60);
const MAX_TRACKED: usize = 1024;

/// The download probe the app times: as large as its upload probe.
pub(crate) const DOWNLOAD_BYTES: usize = 256 * 1024;

fn last_probes() -> &'static Mutex<HashMap<String, Instant>> {
    static LAST: std::sync::OnceLock<Mutex<HashMap<String, Instant>>> = std::sync::OnceLock::new();
    LAST.get_or_init(Default::default)
}

fn last_downloads() -> &'static Mutex<HashMap<String, Instant>> {
    static LAST: std::sync::OnceLock<Mutex<HashMap<String, Instant>>> = std::sync::OnceLock::new();
    LAST.get_or_init(Default::default)
}

fn admit(last: &Mutex<HashMap<String, Instant>>, user: &str) -> ApiResult<()> {
    let mut last = lock(last);
    let now = Instant::now();
    last.retain(|_, at| now.saturating_duration_since(*at) < EVERY);
    if last.contains_key(user) || last.len() >= MAX_TRACKED {
        return Err(ApiError::new(StatusCode::TOO_MANY_REQUESTS, "rate_limited"));
    }
    last.insert(user.to_owned(), now);
    Ok(())
}

pub(crate) async fn download(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<axum::response::Response> {
    let auth = state.auth(&headers)?;
    {
        let db = lock(&state.0.db);
        active_user(&db, &auth.user.id)?;
    }
    admit(last_downloads(), &auth.user.id)?;
    let mut body = vec![0u8; DOWNLOAD_BYTES];
    rand::Rng::fill(&mut rand::thread_rng(), body.as_mut_slice());
    Ok((
        [
            (axum::http::header::CONTENT_TYPE, "application/octet-stream"),
            (axum::http::header::CACHE_CONTROL, "no-store"),
        ],
        body,
    )
        .into_response())
}

pub(crate) async fn upload(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> ApiResult<StatusCode> {
    let auth = state.auth(&headers)?;
    {
        let db = lock(&state.0.db);
        active_user(&db, &auth.user.id)?;
    }
    admit(last_probes(), &auth.user.id)?;
    drop(body);
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use crate::tests::{request, signup};
    use axum::http::StatusCode;

    #[tokio::test]
    async fn a_member_times_one_upload_a_minute_and_nothing_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let state = crate::AppState::open(dir.path(), "Upload", 0).unwrap();
        let app = crate::router(state);
        let admin = signup(&app, "Ulla", "").await;
        let body = serde_json::Value::String("x".repeat(200 * 1024));
        let path = "/api/hosting/measurements/upload";
        let (status, _) = request(&app, "POST", path, Some(&admin.token), body.clone()).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (status, _) = request(&app, "POST", path, Some(&admin.token), body.clone()).await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "one probe a minute");
        let (status, _) = request(&app, "POST", path, None, body).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let too_big = serde_json::Value::String("x".repeat(super::MAX_BYTES + 1));
        let other = signup(&app, "Uffe", &crate::tests::new_invite(&app, &admin).await).await;
        let (status, _) = request(&app, "POST", path, Some(&other.token), too_big).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn a_member_downloads_one_random_probe_a_minute() {
        use axum::body::Body;
        use tower::ServiceExt;
        let dir = tempfile::tempdir().unwrap();
        let state = crate::AppState::open(dir.path(), "Download", 0).unwrap();
        let app = crate::router(state);
        let admin = signup(&app, "Dora", "").await;
        let path = "/api/hosting/measurements/download";
        let get = |token: Option<&str>| {
            let mut builder = axum::http::Request::builder().method("GET").uri(path);
            if let Some(token) = token {
                builder = builder.header("authorization", format!("Bearer {token}"));
            }
            builder.body(Body::empty()).unwrap()
        };
        let response = app.clone().oneshot(get(Some(&admin.token))).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(bytes.len(), super::DOWNLOAD_BYTES);
        assert!(bytes.iter().filter(|b| **b == 0).count() < bytes.len() / 64, "random, not zeros");
        let again = app.clone().oneshot(get(Some(&admin.token))).await.unwrap();
        assert_eq!(again.status(), StatusCode::TOO_MANY_REQUESTS);
        let anonymous = app.clone().oneshot(get(None)).await.unwrap();
        assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
    }
}
