//! A destination holds its real listeners while proving readiness. No normal
//! community route or SQLite writer is exposed before the durable source grant.
use crate::{handover, handover_target::TargetKey, handover_transfer, RtcConfig};
use axum::{
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
    Json, Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use relay_protocol::handover::{self as model, ReadyProof};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs,
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;
use tokio::{
    net::{TcpListener, UdpSocket},
    sync::{oneshot, Semaphore},
};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

type Failure = Box<dyn std::error::Error>;
type Reply<T> = Result<Json<T>, (StatusCode, Json<Value>)>;
#[path = "handover_media.rs"]
mod media;
fn refuse(code: StatusCode, text: &'static str) -> (StatusCode, Json<Value>) {
    (code, Json(json!({"error":text})))
}
fn sha(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn lock<T>(value: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    value.lock().unwrap_or_else(|e| e.into_inner())
}

struct Standby {
    data: PathBuf,
    status: model::Status,
    receipt: model::Receipt,
    key: Arc<TargetKey>,
    rtc: Option<RtcConfig>,
    token: Zeroizing<String>,
    instance_id: String,
    http_port: u16,
    voice_port: u16,
    activating: AtomicBool,
    proof_busy: Semaphore,
    rate: Mutex<(Instant, u32)>,
    activated: Mutex<Option<oneshot::Sender<handover::activation::Activated>>>,
    probe: Mutex<Option<Arc<media::Probe>>>,
    probe_signal: Arc<Semaphore>,
    probe_media: Arc<Semaphore>,
}

pub struct Prepared {
    http: TcpListener,
    udp: UdpSocket,
    state: Arc<Standby>,
    activated: oneshot::Receiver<handover::activation::Activated>,
    reservation: fs::File,
    peer: Option<Peer>,
}
/// A machine-local ingress reservation carried intact into the active router.
/// The token is private runtime state, never part of a receipt or grant.
pub struct Peer {
    pub http: TcpListener,
    pub token: Zeroizing<String>,
}
impl Peer {
    pub async fn bind(port: u16, token: Zeroizing<String>) -> Result<Self, Failure> {
        if !(32..=256).contains(&token.len()) { return Err("A private network ingress token is required".into()); }
        Ok(Self { http: TcpListener::bind((std::net::Ipv4Addr::LOCALHOST,port)).await?, token })
    }
}
pub struct ReadyRuntime {
    pub http: TcpListener,
    pub udp: UdpSocket,
    pub activation: handover::activation::Activated,
    pub rtc: Option<RtcConfig>,
    pub reservation: fs::File,
    pub peer: Option<Peer>,
}

impl ReadyRuntime {
    /// Once the source grant exists, a restart must not need another source
    /// retirement or overwrite writes made by the already activated target.
    pub async fn resume(
        data: &Path,
        status: model::Status,
        bind: IpAddr,
        http_port: u16,
        voice_port: u16,
        rtc: Option<RtcConfig>,
    ) -> Result<Self, Failure> {
        if !bind.is_loopback() {
            return Err("Use a loopback destination listener".into());
        }
        handover::direct(data)?;
        crate::handover_target::private_directory(data)?;
        let path = data.join("readiness.lock");
        handover::direct(&path)?;
        let reservation = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        reservation
            .try_lock()
            .map_err(|_| "This destination already has a prepared runtime")?;
        let grant = status.retired.as_ref().ok_or("No durable source grant")?;
        let key = TargetKey::open_bound(
            data,
            status
                .target_identity
                .as_ref()
                .ok_or("No destination identity")?,
        )?;
        let preparation = status.preparation.as_ref().ok_or("No prepared handover")?;
        if !uuid::Uuid::parse_str(&preparation.id).is_ok_and(|id|id.to_string()==preparation.id) {
            return Err("Invalid handover preparation".into());
        }
        let manifest = handover_transfer::source_manifest_received(
            &data.join("handover-received").join(&preparation.id),
        )?;
        if manifest.context.runtime.rtc_setup_required && rtc.is_none() {
            return Err(
                "Configure this destination's own voice/video service before resuming".into(),
            );
        }
        let http = TcpListener::bind(SocketAddr::new(bind, http_port)).await?;
        let udp = UdpSocket::bind(SocketAddr::new(bind, voice_port)).await?;
        if let Some(config) = &rtc {
            config.handover_ready().await.map_err(|e| e.message)?;
        }
        // A conflicting listener or missing media service must fail before
        // activation touches the received database, including on a first resume.
        let activation = handover::activation::activate(data, &status, grant, &key)?;
        Ok(Self {
            http,
            udp,
            activation,
            rtc,
            reservation,
            peer: None,
        })
    }
}

impl Prepared {
    /// A prepared public HTTPS proxy may forward to these numeric-loopback
    /// listeners. This mode never silently opens a LAN/public bind or firewall.
    pub async fn bind(
        data: &Path,
        status: model::Status,
        bind: IpAddr,
        http_port: u16,
        voice_port: u16,
        rtc: Option<RtcConfig>,
        token: Zeroizing<String>,
    ) -> Result<Self, Failure> {
        if !bind.is_loopback() || token.len() < 32 || token.len() > 256 {
            return Err("Use a loopback standby listener and a private control token".into());
        }
        handover::direct(data)?;
        crate::handover_target::private_directory(data)?;
        let path = data.join("readiness.lock");
        handover::direct(&path)?;
        let reservation = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        reservation
            .try_lock()
            .map_err(|_| "This destination already has a prepared runtime")?;
        let key = Arc::new(TargetKey::open_bound(
            data,
            status
                .target_identity
                .as_ref()
                .ok_or("No destination identity")?,
        )?);
        let receipt = handover::activation::attest(data, &status, &key)?;
        if status.received.as_ref() != Some(&receipt) || status.retired.is_some() {
            return Err("The source must record this exact standby copy before preparation".into());
        }
        let p = status.preparation.as_ref().ok_or("No prepared handover")?;
        let manifest = handover_transfer::source_manifest_received(
            &data.join("handover-received").join(&p.id),
        )?;
        if manifest.context.runtime.rtc_setup_required && rtc.is_none() {
            return Err(
                "Configure this destination's own voice/video service before the move".into(),
            );
        }
        // Hold BOTH ports before publishing any positive result. They are kept
        // through activation, including the normal server's initialization.
        let http = TcpListener::bind(SocketAddr::new(bind, http_port)).await?;
        let udp = UdpSocket::bind(SocketAddr::new(bind, voice_port)).await?;
        if let Some(config) = &rtc {
            config.handover_ready().await.map_err(|e| e.message)?;
        }
        let (send, activated) = oneshot::channel();
        let state = Arc::new(Standby {
            data: data.to_owned(),
            status,
            receipt,
            key,
            rtc,
            token,
            instance_id: uuid::Uuid::new_v4().to_string(),
            http_port: http.local_addr()?.port(),
            voice_port: udp.local_addr()?.port(),
            activating: AtomicBool::new(false),
            proof_busy: Semaphore::new(1),
            rate: Mutex::new((Instant::now(), 0)),
            activated: Mutex::new(Some(send)),
            probe: Mutex::new(None),
            probe_signal: Arc::new(Semaphore::new(2)),
            probe_media: Arc::new(Semaphore::new(4)),
        });
        Ok(Self {
            http,
            udp,
            state,
            activated,
            reservation,
            peer: None,
        })
    }
    pub fn report(&self) -> Value {
        json!({"mode":"handover-standby","server_id":self.state.status.server_id,"target_host_id":self.state.key.identity().host_id,"instance_id":self.state.instance_id,"http_port":self.state.http_port,"voice_port":self.state.voice_port,"network_peer_port":self.peer.as_ref().map(|p|p.http.local_addr().map(|a|a.port())).transpose().ok().flatten()})
    }
    pub fn with_peer(mut self, peer: Option<Peer>) -> Self { self.peer=peer; self }
    pub(crate) fn router(&self) -> Router {
        self.public_router().route("/api/local/handover/activate",post(activate).with_state(self.state.clone()))
            .layer(DefaultBodyLimit::max(16*1024))
    }
    pub(crate) fn peer_router(&self)->Option<Router>{
        self.peer.as_ref().map(|peer|crate::network_ingress_gate(self.public_router(),peer.token.to_string()))
    }
    fn public_router(&self) -> Router {
        Router::new()
            .route("/health", get(health))
            .route("/api/hosting/handover/target", post(prove))
            .route("/api/hosting/handover/rtc-ticket", post(media::ticket))
            .route("/api/rtc/media", get(media::media))
            .route("/rtc", get(media::signal))
            .route("/rtc/v1", get(media::signal))
            .route("/rtc/validate", get(media::validate))
            .route("/rtc/v1/validate", get(media::validate))
            .route("/api/hosting/handover/grant", post(accept_grant))
            .layer(DefaultBodyLimit::max(model::CONTROL_BYTES as usize))
            .with_state(self.state.clone())
    }
    /// Duplicating the listening socket retains the exact reservation while
    /// the standby router drains; the active router then uses that same socket.
    pub async fn wait(self) -> Result<ReadyRuntime, Failure> {
        let router = self.router();
        let peer_router = self.peer_router();
        let raw = self.http.into_std()?;
        let serving = TcpListener::from_std(raw.try_clone()?)?;
        let retained = TcpListener::from_std(raw)?;
        let stop = CancellationToken::new();
        let server_stop = stop.clone();
        struct Serving(tokio::task::JoinHandle<std::io::Result<()>>);
        impl Drop for Serving {
            fn drop(&mut self) {
                self.0.abort();
            }
        }
        let mut task = Serving(tokio::spawn(async move {
            axum::serve(
                serving,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async move { server_stop.cancelled().await })
            .await
        }));
        let mut peer_task=None;
        let mut retained_peer=None;
        if let Some(peer)=self.peer {
            let raw=peer.http.into_std()?;
            let serving=TcpListener::from_std(raw.try_clone()?)?;
            retained_peer=Some(Peer{http:TcpListener::from_std(raw)?,token:peer.token});
            let router=peer_router.ok_or("Missing private destination router")?;
            let stop=stop.clone();
            peer_task=Some(Serving(tokio::spawn(async move {
                axum::serve(serving,router.into_make_service_with_connect_info::<SocketAddr>())
                    .with_graceful_shutdown(async move{stop.cancelled().await}).await
            })));
        }
        let activation = tokio::select! {
            result=self.activated=>result.map_err(|_|"Destination activation ended without a grant")?,
            _=&mut task.0=>return Err("The prepared destination listener stopped".into()),
            _=async {match &mut peer_task{Some(task)=>{let _=(&mut task.0).await;},None=>std::future::pending::<()>().await}}=>return Err("The prepared network ingress stopped".into()),
        };
        stop.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(3), &mut task.0).await;
        if let Some(task)=&mut peer_task {let _=tokio::time::timeout(Duration::from_secs(3),&mut task.0).await;}
        Ok(ReadyRuntime {
            http: retained,
            udp: self.udp,
            activation,
            rtc: self.state.rtc.clone(),
            reservation: self.reservation,
            peer: retained_peer,
        })
    }
}

async fn health() -> Json<Value> {
    Json(json!({"mode":"handover-standby","writable":false}))
}
fn challenge_matches(s: &Standby, c: &model::ReadinessChallenge) -> bool {
    c.source.server_id == s.status.server_id
        && Some(&c.source.host) == s.status.source_identity.as_ref()
        && c.source.term == s.status.term
        && c.source_host_id == s.status.source_host_id
        && c.preparation_id == s.receipt.body.preparation_id
        && c.receipt_sha256 == sha(&serde_json::to_vec(&s.receipt).expect("bounded receipt"))
        && c.challenge.len() == 43
        && URL_SAFE_NO_PAD
            .decode(&c.challenge)
            .is_ok_and(|v| v.len() == 32)
        && c.expires_at > crate::now()
        && c.expires_at <= crate::now().saturating_add(70)
        && crate::handover_target::verify(
            &model::HostProof {
                identity: c.source.host.clone(),
                signature: c.signature.clone(),
            },
            &model::readiness_challenge_message(c),
        )
}
async fn prove(
    State(s): State<Arc<Standby>>,
    Json(request): Json<model::ReadinessChallenge>,
) -> Reply<ReadyProof> {
    if !challenge_matches(&s, &request) || s.activating.load(Ordering::Acquire) {
        return Err(refuse(
            StatusCode::CONFLICT,
            "This check does not match the waiting destination",
        ));
    }
    {
        let mut rate = lock(&s.rate);
        if rate.0.elapsed() >= Duration::from_secs(60) {
            *rate = (Instant::now(), 0);
        }
        if rate.1 >= 30 {
            return Err(refuse(
                StatusCode::TOO_MANY_REQUESTS,
                "Wait before checking the destination again",
            ));
        }
        rate.1 += 1;
    }
    let _permit = s.proof_busy.try_acquire().map_err(|_| {
        refuse(
            StatusCode::TOO_MANY_REQUESTS,
            "A destination check is already running",
        )
    })?;
    if let Some(config) = &s.rtc {
        config.handover_ready().await.map_err(|_| {
            refuse(
                StatusCode::SERVICE_UNAVAILABLE,
                "The destination voice/video service did not answer",
            )
        })?;
    }
    if s.activating.load(Ordering::Acquire) {
        return Err(refuse(
            StatusCode::CONFLICT,
            "Destination activation is already in progress",
        ));
    }
    let rtc_media_ready = media::completed(&s, &request).then_some(true);
    let body = model::ReadyBody {
        request,
        target: s.key.identity(),
        target_url: s.receipt.body.target_url.clone(),
        instance_id: s.instance_id.clone(),
        http_port: s.http_port,
        voice_port: s.voice_port,
        rtc_ready: s.rtc.is_some(),
        rtc_media_ready,
    };
    Ok(Json(ReadyProof {
        signature: s.key.sign(&model::ready_message(&body)),
        body,
    }))
}
async fn activate(
    State(s): State<Arc<Standby>>,
    headers: HeaderMap,
    Json(grant): Json<model::ActivationGrant>,
) -> Reply<Value> {
    let given = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or_default();
    if given.len() != s.token.len() || !bool::from(given.as_bytes().ct_eq(s.token.as_bytes())) {
        return Err(refuse(
            StatusCode::UNAUTHORIZED,
            "Local destination control required",
        ));
    }
    apply_grant(s, grant).await
}
// The receiver explicitly started this standby and accepted the complete data.
// A correctly scoped source grant is the authorization; no source account
// bearer or receiver-local control secret crosses this public endpoint.
async fn accept_grant(
    State(s): State<Arc<Standby>>,
    Json(grant): Json<model::ActivationGrant>,
) -> Reply<Value> {
    apply_grant(s, grant).await
}
async fn apply_grant(s: Arc<Standby>, grant: model::ActivationGrant) -> Reply<Value> {
    if grant.receipt != s.receipt
        || Some(&grant.source_identity) != s.status.source_identity.as_ref()
        || s.status.term.checked_add(1) != Some(grant.next_term)
        || !crate::handover_target::verify(
            &model::HostProof {
                identity: grant.source_identity.clone(),
                signature: grant.signature.clone(),
            },
            &model::grant_message(&grant),
        )
    {
        return Err(refuse(
            StatusCode::CONFLICT,
            "The grant belongs to another standby copy",
        ));
    }
    if s.activating
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Err(refuse(
            StatusCode::CONFLICT,
            "Destination activation is already in progress",
        ));
    }
    let result = async {
        if let Some(probe) = lock(&s.probe).as_ref() {
            probe.stop();
        }
        if let Some(config) = &s.rtc {
            config.handover_ready().await.map_err(|_| {
                refuse(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "The destination voice/video service did not answer",
                )
            })?;
        }
        let mut status = s.status.clone();
        status.activation_available = true;
        status.retired = Some(grant.clone());
        let data = s.data.clone();
        let key = s.key.clone();
        let activated = tokio::task::spawn_blocking(move || {
            handover::activation::activate(&data, &status, &grant, &key).map_err(|_| ())
        })
        .await
        .map_err(|_| {
            refuse(
                StatusCode::SERVICE_UNAVAILABLE,
                "Destination activation could not finish",
            )
        })?
        .map_err(|_| {
            refuse(
                StatusCode::CONFLICT,
                "The signed grant did not activate this exact standby copy",
            )
        })?;
        lock(&s.activated)
            .take()
            .ok_or_else(|| {
                refuse(
                    StatusCode::CONFLICT,
                    "Destination activation already completed",
                )
            })?
            .send(activated)
            .map_err(|_| {
                refuse(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Destination runtime stopped; its saved activation is retained",
                )
            })?;
        Ok(Json(json!({"activating":true})))
    }
    .await;
    if result.is_err() {
        s.activating.store(false, Ordering::Release);
    }
    result
}
