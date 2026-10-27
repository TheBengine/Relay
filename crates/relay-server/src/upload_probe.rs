//! `POST /api/hosting/measurements/upload`: a co-host's app measures its own
//! upload by timing this one request (#19). The body is read and dropped;
//! nothing is stored. One probe per member a minute, bounded in members tracked.
use super::*;
use std::collections::HashMap;
use std::time::{Duration as StdDuration, Instant};

/// The largest probe body. The app sends 256 KiB.
pub(crate) const MAX_BYTES: usize = 512 * 1024;
const EVERY: StdDuration = StdDuration::from_secs(60);
const MAX_TRACKED: usize = 1024;

fn last_probes() -> &'static Mutex<HashMap<String, Instant>> {
    static LAST: std::sync::OnceLock<Mutex<HashMap<String, Instant>>> = std::sync::OnceLock::new();
    LAST.get_or_init(Default::default)
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
    {
        let mut last = lock(last_probes());
        let now = Instant::now();
        last.retain(|_, at| now.saturating_duration_since(*at) < EVERY);
        if last.contains_key(&auth.user.id) || last.len() >= MAX_TRACKED {
            return Err(ApiError::new(StatusCode::TOO_MANY_REQUESTS, "rate_limited"));
        }
        last.insert(auth.user.id.clone(), now);
    }
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
}
