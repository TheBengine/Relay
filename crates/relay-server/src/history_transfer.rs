//! Temporary owner-only opaque relay. Never writes snapshots to DB, disk or replicas.
use super::*;
use std::{
    sync::atomic::{AtomicUsize, Ordering},
    time::Instant,
};
pub const MAX_BYTES: usize = 32 * 1024 * 1024;
const MAX_TOTAL: usize = 128 * 1024 * 1024;
const TTL: Duration = Duration::from_secs(15 * 60);
#[derive(Default)]
pub(super) struct Store {
    entries: Mutex<HashMap<String, Entry>>,
    used: Arc<AtomicUsize>,
}
struct Entry {
    id: String,
    expires: Instant,
    data: Option<Arc<Data>>,
}
struct Data {
    bytes: Bytes,
    _budget: Reservation,
}
struct Reservation {
    used: Arc<AtomicUsize>,
    bytes: usize,
}
impl Reservation {
    fn acquire(used: &Arc<AtomicUsize>) -> ApiResult<Self> {
        used.fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
            n.checked_add(MAX_BYTES).filter(|n| *n <= MAX_TOTAL)
        })
        .map_err(|_| {
            ApiError::new(
                StatusCode::INSUFFICIENT_STORAGE,
                "History transfers are busy. Try again later.",
            )
        })?;
        Ok(Self {
            used: used.clone(),
            bytes: MAX_BYTES,
        })
    }
    fn shrink(&mut self, bytes: usize) {
        self.used.fetch_sub(self.bytes - bytes, Ordering::AcqRel);
        self.bytes = bytes;
    }
}
impl Drop for Reservation {
    fn drop(&mut self) {
        self.used.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}
// Bytes owns this lease until the last response-body reference is dropped.
struct ResponseBytes(Arc<Data>);
impl AsRef<[u8]> for ResponseBytes {
    fn as_ref(&self) -> &[u8] {
        &self.0.bytes
    }
}
fn valid(id: &str) -> ApiResult<()> {
    if id.len() != 64
        || !id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(bad("Invalid transfer identifier"));
    }
    Ok(())
}
fn clean(entries: &mut HashMap<String, Entry>) {
    entries.retain(|_, entry| entry.expires > Instant::now());
}
pub async fn put_transfer(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<StatusCode> {
    valid(&id)?;
    let auth = state.auth(&headers)?;
    let _permit = state
        .0
        .transfer_jobs
        .clone()
        .try_acquire_owned()
        .map_err(|_| {
            ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "Transfers are busy. Try again.",
            )
        })?;
    {
        clean(&mut lock(&state.0.history_transfer.entries));
    }
    // Count in-flight uploads and retained response bodies, not just stored entries.
    let mut reservation = Reservation::acquire(&state.0.history_transfer.used)?;
    let bytes = tokio::time::timeout(
        Duration::from_secs(45),
        axum::body::to_bytes(body, MAX_BYTES),
    )
    .await
    .map_err(|_| ApiError::new(StatusCode::REQUEST_TIMEOUT, "History upload timed out"))?
    .map_err(|_| {
        ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "History transfer exceeds 32 MiB",
        )
    })?;
    if bytes.len() < 48 || !bytes.starts_with(b"RLYXFER1") {
        return Err(bad("Invalid encrypted history transfer"));
    }
    reservation.shrink(bytes.len());
    let mut entries = lock(&state.0.history_transfer.entries);
    clean(&mut entries);
    if let Some(existing) = entries.get(&auth.user.id) {
        if existing.id == id
            && existing
                .data
                .as_ref()
                .is_some_and(|data| data.bytes == bytes)
        {
            return Ok(StatusCode::NO_CONTENT);
        }
        if existing.data.is_some() || existing.id == id {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "A history transfer is already pending. Finish it or wait 15 minutes.",
            ));
        }
    }
    if entries.len() >= 64 && !entries.contains_key(&auth.user.id) {
        return Err(ApiError::new(
            StatusCode::INSUFFICIENT_STORAGE,
            "History transfers are busy. Try again later.",
        ));
    }
    entries.insert(
        auth.user.id,
        Entry {
            id,
            expires: Instant::now() + TTL,
            data: Some(Arc::new(Data {
                bytes,
                _budget: reservation,
            })),
        },
    );
    Ok(StatusCode::NO_CONTENT)
}
pub async fn get_transfer(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    valid(&id)?;
    let auth = state.auth(&headers)?;
    let mut entries = lock(&state.0.history_transfer.entries);
    clean(&mut entries);
    let data = entries
        .get(&auth.user.id)
        .filter(|e| e.id == id)
        .and_then(|e| e.data.as_ref())
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "No approved transfer yet. Approve it on your original PC first.",
            )
        })?;
    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        Bytes::from_owner(ResponseBytes(data.clone())),
    )
        .into_response())
}
pub async fn delete_transfer(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<StatusCode> {
    valid(&id)?;
    let auth = state.auth(&headers)?;
    let mut entries = lock(&state.0.history_transfer.entries);
    clean(&mut entries);
    if let Some(entry) = entries.get_mut(&auth.user.id).filter(|e| e.id == id) {
        entry.data = None;
    }
    Ok(StatusCode::NO_CONTENT)
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{new_invite, signup};
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    async fn call(
        app: &Router,
        method: &str,
        id: &str,
        token: Option<&str>,
        body: Vec<u8>,
    ) -> Response {
        let mut req = Request::builder()
            .method(method)
            .uri(format!("/api/history-transfer/{id}"));
        if let Some(token) = token {
            req = req.header("Authorization", format!("Bearer {token}"));
        }
        app.clone()
            .oneshot(req.body(Body::from(body)).unwrap())
            .await
            .unwrap()
    }
    fn wire() -> Vec<u8> {
        let mut b = b"RLYXFER1".to_vec();
        b.extend([7; 80]);
        b
    }
    #[tokio::test]
    async fn owner_only_expiring_transfer_is_atomic_and_consumed() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::open(temp.path(), "Transfer", 0).unwrap();
        let app = router(state.clone());
        let alice = signup(&app, "Alice", "").await;
        let bob = signup(&app, "Bob", &new_invite(&app, &alice).await).await;
        let id = "a".repeat(64);
        let second = "b".repeat(64);
        assert_eq!(
            call(&app, "PUT", &id, None, wire()).await.status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            call(&app, "PUT", &id, Some(&alice.token), wire())
                .await
                .status(),
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            call(&app, "PUT", &id, Some(&alice.token), wire())
                .await
                .status(),
            StatusCode::NO_CONTENT
        );
        let mut changed = wire();
        changed[40] ^= 1;
        assert_eq!(
            call(&app, "PUT", &id, Some(&alice.token), changed)
                .await
                .status(),
            StatusCode::CONFLICT
        );
        assert_eq!(
            call(&app, "GET", &id, Some(&bob.token), vec![])
                .await
                .status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            call(&app, "DELETE", &id, Some(&bob.token), vec![])
                .await
                .status(),
            StatusCode::NO_CONTENT
        );
        let response = call(&app, "GET", &id, Some(&alice.token), vec![]).await;
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            wire()
        );
        assert_eq!(
            call(&app, "DELETE", &id, Some(&alice.token), vec![])
                .await
                .status(),
            StatusCode::NO_CONTENT
        );
        assert!(lock(&state.0.history_transfer.entries)[&alice.user.id]
            .data
            .is_none());
        assert_eq!(
            call(&app, "PUT", &id, Some(&alice.token), wire())
                .await
                .status(),
            StatusCode::CONFLICT
        );
        assert_eq!(
            call(&app, "PUT", &second, Some(&alice.token), wire())
                .await
                .status(),
            StatusCode::NO_CONTENT
        );
        lock(&state.0.history_transfer.entries)
            .get_mut(&alice.user.id)
            .unwrap()
            .expires = Instant::now() - Duration::from_secs(1);
        assert_eq!(
            call(&app, "GET", &second, Some(&alice.token), vec![])
                .await
                .status(),
            StatusCode::NOT_FOUND
        );
        assert!(lock(&state.0.history_transfer.entries).is_empty());
    }
    #[tokio::test]
    async fn history_transfer_slow_response_remains_charged_after_delete() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::open(temp.path(), "Leases", 0).unwrap();
        let app = router(state.clone());
        let user = signup(&app, "Alice", "").await;
        let id = "a".repeat(64);
        assert_eq!(
            call(&app, "PUT", &id, Some(&user.token), wire())
                .await
                .status(),
            StatusCode::NO_CONTENT
        );
        let response = call(&app, "GET", &id, Some(&user.token), vec![]).await;
        assert_eq!(
            call(&app, "DELETE", &id, Some(&user.token), vec![])
                .await
                .status(),
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            state.0.history_transfer.used.load(Ordering::Acquire),
            wire().len()
        );
        drop(response);
        assert_eq!(state.0.history_transfer.used.load(Ordering::Acquire), 0);
    }
    #[tokio::test]
    async fn transfer_bounds_body_and_aggregate_storage_before_accepting() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::open(temp.path(), "Limits", 0).unwrap();
        let app = router(state.clone());
        let alice = signup(&app, "Alice", "").await;
        let id = "a".repeat(64);
        assert_eq!(
            call(&app, "PUT", &id, Some(&alice.token), vec![0; MAX_BYTES + 1])
                .await
                .status(),
            StatusCode::PAYLOAD_TOO_LARGE
        );
        assert_eq!(
            call(&app, "PUT", &id, Some(&alice.token), vec![0; 48])
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
        let bytes = Bytes::from(vec![0; MAX_BYTES]);
        for n in 0..4 {
            lock(&state.0.history_transfer.entries).insert(
                format!("synthetic-{n}"),
                Entry {
                    id: id.clone(),
                    expires: Instant::now() + TTL,
                    data: Some(Arc::new(Data {
                        bytes: bytes.clone(),
                        _budget: Reservation::acquire(&state.0.history_transfer.used).unwrap(),
                    })),
                },
            );
        }
        assert_eq!(
            call(&app, "PUT", &id, Some(&alice.token), wire())
                .await
                .status(),
            StatusCode::INSUFFICIENT_STORAGE
        );
        assert!(!lock(&state.0.history_transfer.entries).contains_key(&alice.user.id));
        lock(&state.0.history_transfer.entries).clear();
        let permits = state.0.transfer_jobs.available_permits() as u32;
        let held = state
            .0
            .transfer_jobs
            .clone()
            .acquire_many_owned(permits)
            .await
            .unwrap();
        assert_eq!(
            call(&app, "PUT", &id, Some(&alice.token), wire())
                .await
                .status(),
            StatusCode::TOO_MANY_REQUESTS
        );
        drop(held);
    }
}
