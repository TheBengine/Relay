//! Private, bounded transport for an explicitly admitted three-host cohort.
//! No discovery, listener binding, key generation or public route activation here.
//! Setup supplies HTTPS origins or owned loopback forwards and per-peer access keys.
#![allow(dead_code)]
use crate::{
    takeover_commit::{CommitNotice, Replicate},
    takeover_runtime::Replica,
    takeover_voter::AppendAnswer,
    takeover_wal_vfs::Refusal,
};
use axum::{
    body::to_bytes,
    extract::{Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
    Router,
};
use crate::takeover_voter::VoteAnswer;
use relay_protocol::takeover::{Ack, LogEntry, Nack, Vote, VoteRequest, MAX_CONTROL_JSON, MAX_ENTRY_JSON};
use serde::{Deserialize, Serialize};
use std::{
    net::SocketAddr,
    sync::{mpsc, Arc, Mutex},
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

const BODY_LIMIT: usize = MAX_ENTRY_JSON + 16 * 1024;
const REPLY_LIMIT: usize = 16 * 1024;
const APPEND: &str = "/internal/takeover/append";
const COMMIT: &str = "/internal/takeover/committed";
const VOTE: &str = "/internal/takeover/vote";

/// No Debug/Serialize: a scoped access key never belongs in logs or URLs.
pub(crate) struct Target {
    origin: url::Url,
    access: Zeroizing<String>,
}
impl Target {
    pub(crate) fn https(origin: &str, access: String) -> Result<Self, Refusal> {
        let url = url::Url::parse(origin).map_err(|_| Refusal("replica origin is invalid"))?;
        if url.scheme() != "https"
            || url.host_str().is_none()
            || url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(Refusal("replica requires its admitted HTTPS origin"));
        }
        Self::checked(url, access)
    }
    /// The caller owns and authenticates the encrypted tunnel behind this forward.
    /// Plain LAN HTTP is deliberately not accepted.
    pub(crate) fn owned_forward(address: SocketAddr, access: String) -> Result<Self, Refusal> {
        if !address.ip().is_loopback() || address.port() == 0 {
            return Err(Refusal("replica forward must be owned loopback"));
        }
        Self::checked(
            url::Url::parse(&format!("http://{address}/"))
                .map_err(|_| Refusal("replica forward is invalid"))?,
            access,
        )
    }
    fn checked(origin: url::Url, access: String) -> Result<Self, Refusal> {
        if !valid_access(&access) {
            return Err(Refusal("replica access key is invalid"));
        }
        Ok(Self {
            origin,
            access: Zeroizing::new(access),
        })
    }
}
fn valid_access(key: &str) -> bool {
    key.len() == 64
        && key
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", content = "evidence", deny_unknown_fields)]
enum Reply {
    Ack(Ack),
    Nack(Nack),
}
impl From<AppendAnswer> for Reply {
    fn from(a: AppendAnswer) -> Self {
        match a {
            AppendAnswer::Ack(a) => Self::Ack(a),
            AppendAnswer::Nack(n) => Self::Nack(n),
        }
    }
}
impl From<Reply> for AppendAnswer {
    fn from(a: Reply) -> Self {
        match a {
            Reply::Ack(a) => Self::Ack(a),
            Reply::Nack(n) => Self::Nack(n),
        }
    }
}

/// An endpoint only for a selected offline replica. The caller binds it to the
/// private tunnel or TLS listener. Access is checked BEFORE reading the body;
/// signatures and the configured cohort are checked again by the replica.
#[derive(Clone)]
struct Service {
    replica: Arc<Mutex<Replica>>,
    access: Arc<Zeroizing<String>>,
    slots: Arc<tokio::sync::Semaphore>,
}
pub(crate) fn router(replica: Arc<Mutex<Replica>>, access: String) -> Result<Router, Refusal> {
    if !valid_access(&access) {
        return Err(Refusal("replica access key is invalid"));
    }
    let service = Service {
        replica,
        access: Arc::new(Zeroizing::new(access)),
        slots: Arc::new(tokio::sync::Semaphore::new(2)),
    };
    Ok(Router::new()
        .route(APPEND, post(append))
        .route(COMMIT, post(committed))
        .route(VOTE, post(vote))
        .with_state(service))
}
async fn admitted(
    service: &Service,
    request: Request,
) -> Result<(Vec<u8>, tokio::sync::OwnedSemaphorePermit), StatusCode> {
    let token = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or(StatusCode::UNAUTHORIZED)?;
    if token.len() != 64 || !bool::from(token.as_bytes().ct_eq(service.access.as_bytes())) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let permit = service
        .slots
        .clone()
        .try_acquire_owned()
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    let bytes = tokio::time::timeout(
        Duration::from_secs(5),
        to_bytes(request.into_body(), BODY_LIMIT),
    )
    .await
    .map_err(|_| StatusCode::REQUEST_TIMEOUT)?
    .map_err(|_| StatusCode::PAYLOAD_TOO_LARGE)?;
    Ok((bytes.to_vec(), permit))
}
async fn append(State(service): State<Service>, request: Request) -> Response {
    let (bytes, permit) = match admitted(&service, request).await {
        Ok(b) => b,
        Err(s) => return s.into_response(),
    };
    let entry: LogEntry = match serde_json::from_slice(&bytes) {
        Ok(e) => e,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };
    let result = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        service
            .replica
            .try_lock()
            .map_err(|_| ())?
            .append(&entry)
            .map(Reply::from)
            .map_err(|_| ())
    })
    .await;
    match result {
        Ok(Ok(reply)) => axum::Json(reply).into_response(),
        _ => StatusCode::CONFLICT.into_response(),
    }
}
async fn committed(State(service): State<Service>, request: Request) -> Response {
    let (bytes, permit) = match admitted(&service, request).await {
        Ok(b) => b,
        Err(s) => return s.into_response(),
    };
    let notice: CommitNotice = match serde_json::from_slice(&bytes) {
        Ok(e) => e,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };
    let result = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        service
            .replica
            .try_lock()
            .map_err(|_| ())?
            .committed(&notice)
            .map_err(|_| ())
    })
    .await;
    match result {
        Ok(Ok(())) => StatusCode::NO_CONTENT.into_response(),
        _ => StatusCode::CONFLICT.into_response(),
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", content = "evidence", deny_unknown_fields)]
enum Ballot {
    Granted(Vote),
    Refused(Nack),
}
async fn vote(State(service): State<Service>, request: Request) -> Response {
    let (bytes, permit) = match admitted(&service, request).await {
        Ok(b) => b,
        Err(s) => return s.into_response(),
    };
    if bytes.len() > MAX_CONTROL_JSON {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    }
    let ask: VoteRequest = match serde_json::from_slice(&bytes) {
        Ok(e) => e,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };
    let result = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let mut replica = service.replica.try_lock().map_err(|_| ())?;
        crate::takeover_failover::answer_vote(&mut replica, &ask, Instant::now())
            .map(|answer| match answer {
                VoteAnswer::Granted(v) => Ballot::Granted(v),
                VoteAnswer::Refused(n) => Ballot::Refused(n),
            })
            .map_err(|_| ())
    })
    .await;
    match result {
        Ok(Ok(ballot)) => axum::Json(ballot).into_response(),
        _ => StatusCode::CONFLICT.into_response(),
    }
}

/// Asks the other approved copies for a vote, each with its own scoped key.
/// Rare and bounded: one short-lived worker thread per attempt.
pub(crate) struct HttpVotePeers {
    targets: Vec<Target>,
}
impl HttpVotePeers {
    pub(crate) fn new(targets: Vec<Target>) -> Self {
        Self { targets }
    }
}
impl crate::takeover_failover::VotePeers for HttpVotePeers {
    fn ask(&mut self, request: &VoteRequest) -> Vec<Option<VoteAnswer>> {
        let Ok(body) = serde_json::to_vec(request) else {
            return vec![None; self.targets.len()];
        };
        let targets = &self.targets;
        std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .ok()?;
                    let client = reqwest::Client::builder()
                        .no_proxy()
                        .redirect(reqwest::redirect::Policy::none())
                        .connect_timeout(Duration::from_secs(3))
                        .timeout(Duration::from_secs(5))
                        .build()
                        .ok()?;
                    Some(runtime.block_on(async {
                        let mut answers = Vec::new();
                        for target in targets {
                            answers.push(ballot(&client, target, &body).await);
                        }
                        answers
                    }))
                })
                .join()
                .ok()
                .flatten()
        })
        .unwrap_or_else(|| vec![None; self.targets.len()])
    }
}
async fn ballot(client: &reqwest::Client, target: &Target, body: &[u8]) -> Option<VoteAnswer> {
    let url = target.origin.join(VOTE).ok()?;
    let mut auth =
        reqwest::header::HeaderValue::from_str(&format!("Bearer {}", target.access.as_str()))
            .ok()?;
    auth.set_sensitive(true);
    let response = client
        .post(url)
        .header(reqwest::header::AUTHORIZATION, auth)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body.to_vec())
        .send()
        .await
        .ok()?;
    if response.status() != reqwest::StatusCode::OK
        || response.content_length().is_some_and(|n| n > REPLY_LIMIT as u64)
    {
        return None;
    }
    let bytes = response.bytes().await.ok()?;
    if bytes.len() > REPLY_LIMIT {
        return None;
    }
    match serde_json::from_slice::<Ballot>(&bytes).ok()? {
        Ballot::Granted(v) => Some(VoteAnswer::Granted(v)),
        Ballot::Refused(n) => Some(VoteAnswer::Refused(n)),
    }
}

struct Job {
    path: &'static str,
    body: Arc<Vec<u8>>,
    deadline: Instant,
    reply: Option<mpsc::SyncSender<[Option<AppendAnswer>; 2]>>,
}
/// One owned worker, a bounded queue and two concurrent requests per append.
/// SQLite's synchronous commit callback never nests an async runtime. A slow
/// peer is bounded by the same absolute deadline, without sequential timeouts.
pub(crate) struct HttpPeers {
    jobs: mpsc::SyncSender<Job>,
}
impl HttpPeers {
    pub(crate) fn new(targets: [Target; 2]) -> Result<Self, Refusal> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| Refusal("replica transport could not start"))?;
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(20))
            .build()
            .map_err(|_| Refusal("replica transport could not start"))?;
        let (jobs, incoming) = mpsc::sync_channel::<Job>(2);
        std::thread::Builder::new()
            .name("relay-replica-transport".into())
            .spawn(move || {
                while let Ok(job) = incoming.recv() {
                    let replies = runtime.block_on(async {
                        let (a, b) = tokio::join!(
                            send(&client, &targets[0], &job),
                            send(&client, &targets[1], &job)
                        );
                        [a, b]
                    });
                    if let Some(reply) = job.reply {
                        let _ = reply.try_send(replies);
                    }
                }
                // Both scoped tokens are dropped/zeroized with this owned worker.
            })
            .map_err(|_| Refusal("replica transport worker could not start"))?;
        Ok(Self { jobs })
    }
}
impl Replicate for HttpPeers {
    fn append(&mut self, entry: &LogEntry, deadline: Instant) -> [Option<AppendAnswer>; 2] {
        let Ok(body) = serde_json::to_vec(entry) else {
            return [None, None];
        };
        if body.len() > BODY_LIMIT {
            return [None, None];
        }
        let (reply, result) = mpsc::sync_channel(1);
        if self
            .jobs
            .try_send(Job {
                path: APPEND,
                body: Arc::new(body),
                deadline,
                reply: Some(reply),
            })
            .is_err()
        {
            return [None, None];
        }
        result
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap_or([None, None])
    }
    fn committed(&mut self, notice: &CommitNotice, deadline: Instant) {
        if let Ok(body) = serde_json::to_vec(notice) {
            if body.len() <= BODY_LIMIT {
                // The durable source certificate permits catch-up if this bounded
                // best-effort delivery is interrupted. Never infer peer apply here.
                let _ = self.jobs.try_send(Job {
                    path: COMMIT,
                    body: Arc::new(body),
                    deadline,
                    reply: None,
                });
            }
        }
    }
}
async fn send(client: &reqwest::Client, target: &Target, job: &Job) -> Option<AppendAnswer> {
    let url = target.origin.join(job.path).ok()?;
    let mut auth =
        reqwest::header::HeaderValue::from_str(&format!("Bearer {}", target.access.as_str()))
            .ok()?;
    auth.set_sensitive(true);
    let limit = tokio::time::Instant::from_std(job.deadline);
    tokio::time::timeout_at(limit, async {
        let mut response = client
            .post(url)
            .header(reqwest::header::AUTHORIZATION, auth)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(job.body.as_ref().clone())
            .send()
            .await
            .ok()?;
        if job.path == COMMIT {
            return None;
        }
        if response.status() != reqwest::StatusCode::OK {
            return None;
        }
        if response
            .content_length()
            .is_some_and(|n| n > REPLY_LIMIT as u64)
        {
            return None;
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.ok()? {
            if bytes.len().saturating_add(chunk.len()) > REPLY_LIMIT {
                return None;
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice::<Reply>(&bytes)
            .ok()
            .map(AppendAnswer::from)
    })
    .await
    .ok()
    .flatten()
}
