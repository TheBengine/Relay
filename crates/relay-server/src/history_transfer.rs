//! Temporary owner-only opaque relay. Never writes snapshots to DB, disk or replicas.
use super::*;
use axum::{extract::OriginalUri, Extension};
use std::{
    sync::atomic::{AtomicUsize, Ordering},
    time::Instant,
};
pub const MAX_BYTES: usize = 32 * 1024 * 1024;
pub(super) const ROUTE: &str = "/api/history-transfer/{id}";
const MAX_TOTAL: usize = 128 * 1024 * 1024;
const TTL: Duration = Duration::from_secs(15 * 60);
#[derive(Default)]
pub(super) struct Store {
    entries: Mutex<HashMap<String, Entry>>,
    used: Arc<AtomicUsize>,
}

impl Store {
    /// Called under the DB lock as soon as a restore replaces the database.
    pub(super) fn clear_after_restore(&self) {
        lock(&self.entries).clear();
    }
}
/// Dispatch holds maintenance through authentication and rate limiting. The
/// upload takes the guard out before awaiting a potentially slow request body.
#[derive(Clone)]
pub(super) struct UploadAdmission(Arc<Mutex<Option<tokio::sync::OwnedRwLockReadGuard<()>>>>);

impl UploadAdmission {
    pub(super) fn new(guard: tokio::sync::OwnedRwLockReadGuard<()>) -> Self {
        Self(Arc::new(Mutex::new(Some(guard))))
    }

    fn release_for_body(self) {
        drop(lock(&self.0).take());
    }
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

fn require_live_session(db: &Connection, auth: &Auth) -> ApiResult<()> {
    let live: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sessions s JOIN membership m ON m.user_id=s.user_id
          WHERE s.token_hash=?1 AND s.user_id=?2 AND s.expires>?3 AND m.status='active')",
        params![auth.token_hash, auth.user.id, now()],
        |row| row.get(0),
    )?;
    if live {
        Ok(())
    } else {
        Err(unauthorized())
    }
}
pub(super) async fn put_transfer(
    State(state): State<AppState>,
    Path(id): Path<String>,
    OriginalUri(uri): OriginalUri,
    Extension(admission): Extension<UploadAdmission>,
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
    admission.release_for_body();
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
    let _maintenance = community::request_read_guard(&state).await?;
    community::check_request_admission(&state, uri.path(), true)?;
    // Retain the DB guard through publication. Expiry and membership changes
    // may have invalidated the initial authentication during the body read.
    let db = lock(&state.0.db);
    require_live_session(&db, &auth)?;
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
    let db = lock(&state.0.db);
    require_live_session(&db, &auth)?;
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
    let db = lock(&state.0.db);
    require_live_session(&db, &auth)?;
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
    use crate::tests::{new_invite, request, signup};
    use axum::http::Request;
    use http_body_util::BodyExt;
    use std::path::PathBuf;
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

    fn backup_for_restore(state: &AppState) -> (String, PathBuf) {
        let backup_id = id();
        let directory = state.0.data.join("backups");
        std::fs::create_dir_all(&directory).unwrap();
        let file = directory.join(format!("{backup_id}.sqlite3"));
        state.backup_to(&file).unwrap();
        (backup_id, file)
    }

    #[tokio::test]
    async fn history_upload_rechecks_session_and_membership_after_reading_the_body() {
        for change in ["logout", "revoke", "expire", "remove", "pause", "restore"] {
            let temp = tempfile::tempdir().unwrap();
            let state = AppState::open(temp.path(), "Transfer revocation", 0).unwrap();
            let app = router(state.clone());
            let alice = signup(&app, "Alice", "").await;
            let auth = state.authenticate_token(&alice.token).unwrap();
            let id = "a".repeat(64);
            let (reading, started) = tokio::sync::oneshot::channel();
            let (release, resume) = tokio::sync::oneshot::channel();
            let body = Body::from_stream(futures_util::stream::once(async move {
                reading.send(()).unwrap();
                resume.await.unwrap();
                Ok::<_, std::io::Error>(Bytes::from(wire()))
            }));
            let upload = Request::put(format!("/api/history-transfer/{id}"))
                .header(header::AUTHORIZATION, format!("Bearer {}", alice.token))
                .body(body)
                .unwrap();
            let pending = tokio::spawn(app.clone().oneshot(upload));
            tokio::time::timeout(Duration::from_secs(5), started)
                .await
                .unwrap()
                .unwrap();
            match change {
                "logout" => {
                    assert_eq!(
                        request(
                            &app,
                            "POST",
                            "/api/logout",
                            Some(&alice.token),
                            serde_json::Value::Null,
                        )
                        .await
                        .0,
                        StatusCode::NO_CONTENT
                    );
                }
                "revoke" => {
                    let other =
                        new_session(&lock(&state.0.db), &state.0.seal, alice.user.clone()).unwrap();
                    let path = format!("/api/sessions/{}", auth.token_hash);
                    assert_eq!(
                        request(
                            &app,
                            "DELETE",
                            &path,
                            Some(&other.token),
                            serde_json::Value::Null,
                        )
                        .await
                        .0,
                        StatusCode::NO_CONTENT
                    );
                }
                "expire" => {
                    lock(&state.0.db)
                        .execute(
                            "UPDATE sessions SET expires=0 WHERE token_hash=?1",
                            [&auth.token_hash],
                        )
                        .unwrap();
                }
                "remove" => {
                    lock(&state.0.db)
                        .execute(
                            "UPDATE membership SET status='removed' WHERE user_id=?1",
                            [&alice.user.id],
                        )
                        .unwrap();
                }
                "pause" => state.0.takeover_failed.store(true, Ordering::Release),
                "restore" => {
                    let (backup_id, _) = backup_for_restore(&state);
                    assert_eq!(
                        request(
                            &app,
                            "POST",
                            "/api/admin/restore",
                            Some(&alice.token),
                            serde_json::json!({
                                "backup_id":backup_id,"confirm_server_id":state.server_id().unwrap()
                            }),
                        )
                        .await
                        .0,
                        StatusCode::OK
                    );
                }
                _ => unreachable!(),
            }
            release.send(()).unwrap();
            let reply = tokio::time::timeout(Duration::from_secs(5), pending)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let expected = if change == "pause" {
                StatusCode::SERVICE_UNAVAILABLE
            } else {
                StatusCode::UNAUTHORIZED
            };
            assert_eq!(reply.status(), expected, "{change}");
            assert!(lock(&state.0.history_transfer.entries).is_empty());
            assert_eq!(state.0.history_transfer.used.load(Ordering::Acquire), 0);
        }
    }

    #[tokio::test]
    async fn history_transfer_is_discarded_only_after_successful_restore() {
        for via_http in [true, false] {
            let temp = tempfile::tempdir().unwrap();
            let state = AppState::open(temp.path(), "Transfer restore", 0).unwrap();
            let app = router(state.clone());
            let alice = signup(&app, "Alice", "").await;
            let (backup_id, backup) = backup_for_restore(&state);
            let transfer = "a".repeat(64);
            assert_eq!(
                call(&app, "PUT", &transfer, Some(&alice.token), wire())
                    .await
                    .status(),
                StatusCode::NO_CONTENT
            );
            if via_http {
                assert_eq!(
                    request(
                        &app,
                        "POST",
                        "/api/admin/restore",
                        Some(&alice.token),
                        serde_json::json!({"backup_id":backup_id,"confirm_server_id":"wrong"}),
                    )
                    .await
                    .0,
                    StatusCode::BAD_REQUEST
                );
            } else {
                assert!(state
                    .restore_from(&backup.with_extension("missing"))
                    .is_err());
            }
            let issued = call(&app, "GET", &transfer, Some(&alice.token), vec![]).await;
            assert_eq!(issued.status(), StatusCode::OK);

            if via_http {
                assert_eq!(
                    request(
                        &app,
                        "POST",
                        "/api/admin/restore",
                        Some(&alice.token),
                        serde_json::json!({
                            "backup_id":backup_id,"confirm_server_id":state.server_id().unwrap()
                        }),
                    )
                    .await
                    .0,
                    StatusCode::OK
                );
            } else {
                state.restore_from(&backup).unwrap();
            }
            assert_eq!(
                call(&app, "GET", &transfer, Some(&alice.token), vec![])
                    .await
                    .status(),
                StatusCode::UNAUTHORIZED
            );
            let fresh = new_session(&lock(&state.0.db), &state.0.seal, alice.user).unwrap();
            assert_eq!(
                call(&app, "GET", &transfer, Some(&fresh.token), vec![])
                    .await
                    .status(),
                StatusCode::NOT_FOUND
            );
            assert!(lock(&state.0.history_transfer.entries).is_empty());
            assert_eq!(
                state.0.history_transfer.used.load(Ordering::Acquire),
                wire().len()
            );
            drop(issued);
            assert_eq!(state.0.history_transfer.used.load(Ordering::Acquire), 0);
            assert_eq!(
                call(&app, "PUT", &"b".repeat(64), Some(&fresh.token), wire())
                    .await
                    .status(),
                StatusCode::NO_CONTENT
            );
        }
    }

    // docs/encryption.md and the transfer screen: the server keeps a transfer only in
    // memory, never on disk, and stops serving it after a restart.
    #[tokio::test]
    async fn history_transfer_is_never_written_to_disk_and_is_gone_after_restart() {
        let temp = tempfile::tempdir().unwrap();
        let id = "c".repeat(64);
        let marker = b"readiness-docs-unique-history-transfer-marker-20261105";
        let mut body = b"RLYXFER1".to_vec();
        body.extend_from_slice(marker);
        body.extend([9; 48]);
        let token = {
            let state = AppState::open(temp.path(), "Transfer restart", 0).unwrap();
            let app = router(state.clone());
            let alice = signup(&app, "Alice", "").await;
            assert_eq!(
                call(&app, "PUT", &id, Some(&alice.token), body.clone()).await.status(),
                StatusCode::NO_CONTENT
            );
            let served = call(&app, "GET", &id, Some(&alice.token), vec![]).await;
            assert_eq!(served.into_body().collect().await.unwrap().to_bytes(), body);
            let backup = temp.path().join("restart-check.sqlite3");
            state.backup_to(&backup).unwrap();
            for entry in walk(temp.path()) {
                let bytes = std::fs::read(&entry).unwrap();
                assert!(
                    !bytes.windows(marker.len()).any(|w| w == marker),
                    "transfer bytes reached {}",
                    entry.display()
                );
            }
            alice.token
        };
        let state = AppState::open(temp.path(), "Transfer restart", 0).unwrap();
        let app = router(state);
        assert_eq!(
            call(&app, "GET", &id, Some(&token), vec![]).await.status(),
            StatusCode::NOT_FOUND
        );
    }

    fn walk(root: &std::path::Path) -> Vec<PathBuf> {
        let mut files = Vec::new();
        let mut pending = vec![root.to_path_buf()];
        while let Some(dir) = pending.pop() {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() { pending.push(path) } else { files.push(path) }
            }
        }
        files
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
