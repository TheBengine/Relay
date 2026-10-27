mod account_identity;
mod activity;
mod assets;
mod authenticator;
mod chat_controls;
mod community;
mod conversations;
mod discord_bridge;
mod discord_channel_status;
mod discord_host;
mod e2ee;
mod gateway_admission;
mod group_invites;
mod group_presence;
mod handover_locator_page;
mod host_locator_directory;
mod lan_pairing;
mod handover;
mod handover_locator;
pub mod handover_readiness;
pub mod handover_target;
pub mod handover_transfer;
mod hosting;
pub mod lan_discovery;
// Internal data-copy core; no pull agent or handover is activated by this module.
mod ordering;
mod password_policy;
mod people;
mod private_calls;
mod profiles;
mod protection;
#[allow(dead_code)]
mod replica;
pub mod replica_agent;
mod request_host;
pub use request_host::validate_names as validate_allowed_hosts;
mod roles;
mod rtc;
mod rtc_media;
mod server_settings;
mod upload_policy;
mod social;
mod takeover_voter;
mod takeover_store;
mod takeover_signer;
mod takeover_transaction;
mod takeover_inflight;
mod takeover_commit;
mod takeover_runtime;
mod takeover_standby;
mod takeover_transport;
mod takeover_wal_vfs;
mod voice;
mod voice_chat;
mod voice_recovery;
mod ws_limits;
pub use account_identity::Provider as AccountProviderConfig;
use community::*;
pub use community::{backup_database, restore_database, ServerConfig, SCHEMA_VERSION};
pub use rtc::RtcConfig;
pub use voice::{run_udp, run_udp_tailscale};

use argon2::{
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use axum::{
    body::{Body, Bytes},
    extract::{
        ws::{Message as WsMessage, WebSocket},
        ConnectInfo, DefaultBodyLimit, Path, Query, State, WebSocketUpgrade,
    },
    http::{header, HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, post, put},
    Json, Router,
};
use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
    Engine,
};
use rand::{rngs::OsRng, RngCore};
use relay_protocol::*;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    net::SocketAddr,
    path::Path as FsPath,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use subtle::ConstantTimeEq;
use tokio::sync::broadcast;
use uuid::Uuid;

const TOKEN_LIFETIME: u64 = 7 * 24 * 3600;
const VOICE_LIFETIME: u64 = 4 * 3600;

#[derive(Clone)]
pub struct AppState(Arc<Inner>);

/// Set only by the dedicated authenticated network-helper listener, never by a
/// request header. Its loopback socket does not grant local-owner privileges.
#[derive(Clone, Copy)]
pub struct NetworkIngress {
    peer_ip: std::net::Ipv4Addr,
}

struct Inner {
    db: Mutex<Connection>,
    name: String,
    udp_port: u16,
    events: broadcast::Sender<Notice>,
    voice: Mutex<HashMap<u64, voice::VoiceSession>>,
    voice_permissions: Mutex<()>,
    /// 0 idle, 1 running, 2 refresh requested; coalesces notice bursts.
    voice_refresh: std::sync::atomic::AtomicU8,
    voice_socket: Mutex<Option<Arc<tokio::net::UdpSocket>>>,
    frames: Mutex<HashMap<(String, String), StoredFrame>>,
    auth_rates: Mutex<HashMap<String, (u64, u32)>>,
    route_rates: Mutex<HashMap<String, (u64, u32)>>,
    /// Per-ingress budgets of the host locator directory; bounded and pruned.
    locator_rates: Mutex<HashMap<String, (u64, u32)>>,
    /// Short-lived LAN pairing entries and per-peer counters. In memory only.
    lan_pairing: lan_pairing::Store,
    mutation_rates: Mutex<HashMap<String, (u64, u32)>>,
    password_jobs: Arc<tokio::sync::Semaphore>,
    transfer_jobs: Arc<tokio::sync::Semaphore>,
    avatar_jobs: Arc<tokio::sync::Semaphore>,
    request_jobs: Arc<tokio::sync::Semaphore>,
    maintenance: Arc<tokio::sync::RwLock<()>>,
    data: std::path::PathBuf,
    config: ServerConfig,
    instance_id: String,
    shutdown: tokio::sync::watch::Sender<bool>,
    discord_changes: tokio::sync::watch::Sender<u64>,
    discord_status: Mutex<&'static str>,
    discord_credential: Mutex<discord_host::Store>,
    discord_activity: tokio::sync::watch::Sender<u64>,
    discord_message_status: Mutex<&'static str>,
    #[cfg(test)]
    test_discord_proxy: Mutex<Option<String>>,
    #[cfg(test)]
    discord_attempts: std::sync::atomic::AtomicUsize,
    rtc_members: Mutex<HashMap<String, rtc::RtcMembership>>,
    rtc_removals: Mutex<HashMap<(String, String), ()>>,
    typing: Mutex<HashMap<chat_controls::TypingKey, chat_controls::TypingEntry>>,
    /// Opt-in game activity: in memory only, short-lived, never stored.
    activity: activity::Store,
    private_calls: Mutex<HashMap<String, PrivateCall>>,
    identity_jobs: Arc<tokio::sync::Semaphore>,
    /// Further own addresses for account sign-in proofs, told by the host app
    /// on this PC (its account-free iroh address). Kept in memory only.
    account_destinations: Mutex<std::collections::HashSet<String>>,
    event_connections: ws_limits::ConnectionBudget,
    rtc_connections: ws_limits::ConnectionBudget,
    rtc_media_connections: ws_limits::ConnectionBudget,
    voice_connections: ws_limits::ConnectionBudget,
    gateways: gateway_admission::Broker,
    replica: replica::service::Service,
    handover: handover::Authority,
    handover_exports: handover_transfer::Service,
    takeover: Mutex<Option<Arc<takeover_runtime::Source>>>,
    takeover_failed: std::sync::atomic::AtomicBool,
}

#[derive(Clone)]
struct Notice {
    event: Event,
    audience: Option<Vec<String>>,
}

#[derive(Clone)]
struct StoredFrame {
    metadata: MediaFrame,
    channel_id: String,
    bytes: Bytes,
    updated: u64,
}

#[derive(Clone)]
struct Auth {
    user: User,
    permissions: u32,
    token_hash: String,
    expires: u64,
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    message: &'static str,
    cooldown: Option<Cooldown>,
}
#[derive(Debug)]
struct Cooldown {
    code: &'static str,
    retry_after_ms: u64,
    channel_id: Option<String>,
}
type ApiResult<T> = Result<T, ApiError>;
impl ApiError {
    fn new(status: StatusCode, message: &'static str) -> Self {
        Self {
            status,
            message,
            cooldown: None,
        }
    }
    fn cooldown(
        code: &'static str,
        message: &'static str,
        retry_after_ms: u64,
        channel_id: Option<String>,
    ) -> Self {
        Self {
            status: StatusCode::TOO_MANY_REQUESTS,
            message,
            cooldown: Some(Cooldown {
                code,
                retry_after_ms: retry_after_ms.max(1),
                channel_id,
            }),
        }
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        // Any 429, including bounded worker exhaustion, has the same useful wire contract.
        let cooldown = self.cooldown.or_else(|| {
            (self.status == StatusCode::TOO_MANY_REQUESTS).then_some(Cooldown {
                code: "service_busy",
                retry_after_ms: 1000,
                channel_id: None,
            })
        });
        let mut failure = ApiFailure {
            error: self.message.into(),
            ..Default::default()
        };
        let retry_seconds = cooldown
            .as_ref()
            .map(|value| value.retry_after_ms.div_ceil(1000));
        if let Some(value) = cooldown {
            failure.code = Some(value.code.into());
            failure.retry_after_ms = Some(value.retry_after_ms);
            failure.channel_id = value.channel_id;
        }
        let mut response = (self.status, Json(failure)).into_response();
        if let Some(seconds) = retry_seconds {
            if let Ok(value) = axum::http::HeaderValue::from_str(&seconds.to_string()) {
                response.headers_mut().insert(header::RETRY_AFTER, value);
            }
        }
        response
    }
}
impl From<rusqlite::Error> for ApiError {
    fn from(error: rusqlite::Error) -> Self {
        if let rusqlite::Error::SqliteFailure(failure, _) = &error {
            if failure.code == rusqlite::ErrorCode::DiskFull {
                return Self::new(
                    StatusCode::INSUFFICIENT_STORAGE,
                    "Server storage is full; existing data was kept",
                );
            }
            if matches!(
                failure.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            ) {
                return Self::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Database is busy. Retry shortly",
                );
            }
        }
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Database operation failed",
        )
    }
}
impl From<serde_json::Error> for ApiError {
    fn from(_: serde_json::Error) -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Stored data could not be decoded",
        )
    }
}
fn bad(message: &'static str) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, message)
}
fn forbidden() -> ApiError {
    ApiError::new(StatusCode::FORBIDDEN, "Access denied")
}
fn missing() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "Not found")
}
fn unauthorized() -> ApiError {
    ApiError::new(StatusCode::UNAUTHORIZED, "Sign in again")
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as u64
}
fn id() -> String {
    Uuid::new_v4().to_string()
}
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}
fn token_hash(token: &str) -> String {
    format!("{:x}", Sha256::digest(token.as_bytes()))
}
fn random_token() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

impl AppState {
    pub fn open(
        data: &FsPath,
        name: &str,
        udp_port: u16,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        Self::open_with_config(data, name, udp_port, ServerConfig::default())
    }
    pub fn open_with_config(
        data: &FsPath,
        name: &str,
        udp_port: u16,
        mut config: ServerConfig,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        takeover_runtime::require_ordinary_boot(data)?;
        if config.people && config.account_provider.is_none() {
            return Err("Friends require a configured Relay account provider".into());
        }
        if let Some(provider) = &config.account_provider {
            if let Some(name) = url::Url::parse(&provider.public().community_url)?.domain() {
                if !config
                    .allowed_hosts
                    .iter()
                    .any(|v| v.eq_ignore_ascii_case(name))
                {
                    config.allowed_hosts.push(name.to_owned());
                }
            }
        }
        request_host::validate_names(&config.allowed_hosts)?;
        std::fs::create_dir_all(data)?;
        let existing = data.join("relay.sqlite3").exists();
        let mut connection = Connection::open(data.join("relay.sqlite3"))?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.execute_batch("PRAGMA foreign_keys=ON;")?;
        let previous: u32 = connection.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if previous > SCHEMA_VERSION {
            return Err("Database belongs to a newer Relay release; retained unchanged".into());
        }
        let existing_authority = if previous > 0 {
            Some(handover::Authority::open_database(data, &connection)?)
        } else {
            None
        };
        if existing_authority
            .as_ref()
            .is_some_and(|authority| !authority.writes_allowed())
        {
            if previous != SCHEMA_VERSION {
                return Err("This host is paused for handover; cancel preparation before upgrading its database".into());
            }
            connection.pragma_update(None, "query_only", true)?;
        } else {
            connection.pragma_update(None, "journal_mode", "WAL")?;
        }
        let populated = connection.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='users'",
            [],
            |r| r.get::<_, u32>(0),
        )? > 0
            && connection.query_row("SELECT COUNT(*) FROM users", [], |r| r.get::<_, u32>(0))? > 0;
        if previous < SCHEMA_VERSION && populated {
            std::fs::create_dir_all(data.join("backups"))?;
            backup_database(
                &connection,
                &data
                    .join("backups")
                    .join(format!("pre-migration-{}.sqlite3", id())),
            )?;
        }
        migrate_database(&mut connection)?;
        let authority = match existing_authority {
            Some(authority) => authority,
            None => handover::Authority::open_database(data, &connection)?,
        };
        let handover_exports = handover_transfer::Service::default();
        handover_exports.recover(data, &authority.status());
        replica::service::startup(data);
        people::initialize(&connection, &config, existing)?;
        if authority.writes_allowed() {
            // Rebase only implausible future timestamps after a clock adjustment.
            // A restart never grants a free send; rebasing retains one full cooldown.
            connection.execute(
                "UPDATE channel_send_cooldowns SET last_sent_ms=?1 WHERE last_sent_ms>?1",
                [now_ms()],
            )?;
            connection.execute(
                "UPDATE message_send_log SET sent_at_ms=?1 WHERE sent_at_ms>?1",
                [now_ms()],
            )?;
            connection.execute(
                "DELETE FROM message_send_log WHERE sent_at_ms<=?1",
                [now_ms().saturating_sub(chat_controls::MESSAGE_BURST_WINDOW_MS)],
            )?;
            if !config.people
                && connection
                    .query_row("SELECT COUNT(*) FROM channels", [], |r| r.get::<_, i64>(0))?
                    == 0
            {
                for (name, kind, topic) in [
                    ("general", "text", "The place for everything"),
                    ("clips", "text", "Share what you are playing"),
                    ("Lounge", "voice", "Join the conversation"),
                ] {
                    connection.execute(
                        "INSERT INTO channels(id,name,kind,topic) VALUES(?1,?2,?3,?4)",
                        params![id(), name, kind, topic],
                    )?;
                }
            }
            connection.execute("DELETE FROM sessions WHERE expires <= ?1", [now()])?;
            connection.execute(
                "DELETE FROM authenticator_setup WHERE expires <= ?1",
                [now()],
            )?;
            voice_chat::ensure_rooms(&connection)?;
            connection.execute(
                "DELETE FROM uploads WHERE message_id IS NULL AND created < ?1",
                [now().saturating_sub(86400)],
            )?;
        } else {
            connection.pragma_update(None, "query_only", true)?;
        }
        let discord_credential = discord_host::Store::new(data, server_id(&connection)?)?;
        let (events, _) = broadcast::channel(256);
        Ok(Self(Arc::new(Inner {
            db: Mutex::new(connection),
            name: name.chars().take(100).collect(),
            udp_port,
            events,
            voice: Mutex::new(HashMap::new()),
            voice_permissions: Mutex::new(()),
            voice_refresh: std::sync::atomic::AtomicU8::new(0),
            voice_socket: Mutex::new(None),
            frames: Mutex::new(HashMap::new()),
            auth_rates: Mutex::new(HashMap::new()),
            route_rates: Mutex::new(HashMap::new()),
            locator_rates: Mutex::new(HashMap::new()),
            lan_pairing: lan_pairing::Store::default(),
            mutation_rates: Mutex::new(HashMap::new()),
            password_jobs: Arc::new(tokio::sync::Semaphore::new(4)),
            transfer_jobs: Arc::new(tokio::sync::Semaphore::new(8)),
            avatar_jobs: Arc::new(tokio::sync::Semaphore::new(4)),
            request_jobs: Arc::new(tokio::sync::Semaphore::new(32)),
            maintenance: Arc::new(tokio::sync::RwLock::new(())),
            data: data.to_owned(),
            config,
            instance_id: id(),
            shutdown: tokio::sync::watch::channel(false).0,
            discord_changes: tokio::sync::watch::channel(0).0,
            discord_status: Mutex::new("off"),
            discord_credential: Mutex::new(discord_credential),
            discord_activity: tokio::sync::watch::channel(0).0,
            discord_message_status: Mutex::new("off"),
            #[cfg(test)]
            test_discord_proxy: Mutex::new(None),
            #[cfg(test)]
            discord_attempts: std::sync::atomic::AtomicUsize::new(0),
            rtc_members: Mutex::new(HashMap::new()),
            rtc_removals: Mutex::new(HashMap::new()),
            typing: Mutex::new(HashMap::new()),
            activity: Default::default(),
            private_calls: Mutex::new(HashMap::new()),
            identity_jobs: Arc::new(tokio::sync::Semaphore::new(2)),
            account_destinations: Mutex::new(std::collections::HashSet::new()),
            event_connections: ws_limits::ConnectionBudget::new(256, 4),
            rtc_connections: ws_limits::ConnectionBudget::new(128, 2),
            rtc_media_connections: ws_limits::ConnectionBudget::new(256, 8),
            voice_connections: ws_limits::ConnectionBudget::new(128, 2),
            gateways: Default::default(),
            replica: Default::default(),
            handover: authority,
            handover_exports,
            takeover: Mutex::new(None),
            takeover_failed: std::sync::atomic::AtomicBool::new(false),
        })))
    }

    fn authenticate_token(&self, token: &str) -> ApiResult<Auth> {
        if token.len() > 128 {
            return Err(unauthorized());
        }
        let hash = token_hash(token);
        let db = lock(&self.0.db);
        let mut auth = db.query_row("SELECT u.id,u.username,u.role,p.display_name,p.bio,p.version,p.avatar IS NOT NULL,p.appearance,s.expires FROM sessions s JOIN users u ON u.id=s.user_id LEFT JOIN user_profiles p ON p.user_id=u.id JOIN membership m ON m.user_id=u.id WHERE m.status='active' AND s.token_hash=?1 AND s.expires>?2", params![hash,now()], |r| {
            Ok(Auth { user: row_user(r)?, permissions: 0, token_hash: hash.clone(), expires:r.get(8)? })
        }).optional()?.ok_or_else(unauthorized)?;
        auth.permissions = roles::permissions(&db, &auth.user.id)?;
        // Handover status/cancellation still authenticate while the source DB
        // is read-only. Presence must not turn that read into a hidden write.
        if self.writes_allowed() {
            social::heartbeat(&db, &auth.user.id)?;
        }
        Ok(auth)
    }

    fn auth(&self, headers: &HeaderMap) -> ApiResult<Auth> {
        let token = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .ok_or_else(unauthorized)?;
        self.authenticate_token(token)
    }

    fn notify(
        &self,
        kind: &str,
        channel_id: Option<String>,
        user_id: Option<String>,
        audience: Option<Vec<String>>,
    ) {
        if self.takeover_blocked() { return; }
        if self.0.discord_activity.receiver_count() > 0
            && matches!(
                kind,
                "voice" | "channels" | "refresh" | "users" | "roles" | "members" | "permissions"
            )
        {
            self.0
                .discord_activity
                .send_modify(|v| *v = v.wrapping_add(1));
        }
        let _ = self.0.events.send(Notice {
            event: Event {
                kind: kind.to_string(),
                channel_id,
                user_id,
                recipient_id: None,
                typing: None,
                expires_at_ms: None,
            },
            audience,
        });
    }

    fn remove_voice(&self, db: &Connection, user_id: &str, token: Option<&str>) -> ApiResult<()> {
        private_calls::leave(self, db, user_id, token)?;
        self.remove_voice_runtime(user_id, token);
        Ok(())
    }

    fn remove_voice_runtime(&self, user_id: &str, token: Option<&str>) {
        self.revoke_rtc(user_id, token);
        let removed = {
            let mut sessions = lock(&self.0.voice);
            let old = sessions.len();
            sessions.retain(|_, value| {
                value.user.id != user_id || token.is_some_and(|token| token != value.token_hash)
            });
            old != sessions.len()
        };
        if removed {
            lock(&self.0.frames).retain(|(owner, _), _| owner != user_id);
            self.notify("voice", None, Some(user_id.to_string()), None);
        }
    }
    fn enforce_voice_permissions(&self) -> ApiResult<()> {
        // Snapshot voice first without retaining that lock; durable private
        // reconciliation then follows DB -> private_calls -> voice ordering.
        let _refresh = lock(&self.0.voice_permissions);
        let snapshot: Vec<_> = lock(&self.0.voice)
            .iter()
            .map(|(&id, v)| {
                (
                    id,
                    v.user.id.clone(),
                    v.channel_id.clone(),
                    v.token_hash.clone(),
                )
            })
            .collect();
        let permissions: HashMap<_, _> = {
            let db = lock(&self.0.db);
            private_calls::reconcile(self, &db)?;
            voice_recovery::enforce_leases(&db)?;
            snapshot
                .into_iter()
                .map(|(id, user, channel, token)| {
                    let valid = channel_access(&db, &channel, &user, JOIN).is_ok()
                        && db
                            .query_row(
                                "SELECT COUNT(*) FROM sessions WHERE token_hash=?1 AND expires>?2",
                                params![token, now()],
                                |r| r.get::<_, u32>(0),
                            )
                            .unwrap_or(0)
                            > 0;
                    let access = valid
                        .then(|| voice::permissions(&db, &channel, &user).ok())
                        .flatten();
                    (id, access)
                })
                .collect()
        };
        let streaming: std::collections::HashSet<_> = {
            let mut voice = lock(&self.0.voice);
            voice.retain(|id, session| match permissions.get(id) {
                Some(Some(access)) => {
                    session.can_speak = access.can_speak;
                    session.can_stream = access.can_stream;
                    session
                        .whisper_channels
                        .clone_from(&access.whisper_channels);
                    if !access.can_speak {
                        session.state.muted = true;
                    }
                    if !access.can_stream {
                        session.state.sharing = false;
                        session.state.camera = false;
                    }
                    true
                }
                Some(None) => false,
                // A new admission already computed its cache in its transaction.
                None => true,
            });
            // Include freshly admitted tickets too: they may have arrived after
            // our snapshot and already published a valid first video frame.
            voice
                .values()
                .filter(|v| v.can_stream)
                .map(|v| (v.user.id.clone(), v.channel_id.clone()))
                .collect()
        };
        lock(&self.0.frames).retain(|(user, _), frame| {
            streaming.contains(&(user.clone(), frame.channel_id.clone()))
        });
        Ok(())
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .merge(handover::routes())
        .route("/health", get(health))
        .route("/api/health", get(health))
        .route("/api/register", post(register))
        .route("/api/login", post(login))
        .route(
            "/api/account-login",
            get(account_identity::login::metadata).post(account_identity::login::finish),
        )
        .route(
            "/api/account-login/challenge",
            post(account_identity::login::begin),
        )
        .route(
            "/api/local/account-destination",
            post(account_identity::login::register_destination),
        )
        .route(
            "/api/authenticator",
            get(authenticator::status).post(authenticator::change),
        )
        .route("/api/logout", post(logout))
        .route("/api/invites", post(create_invite).get(list_invites))
        .route("/api/invites/{id}", delete(revoke_invite))
        .route("/api/bootstrap", get(bootstrap))
        .route(
            "/api/account-link",
            get(account_identity::status).post(account_identity::change),
        )
        .route("/api/assets", get(assets::get_assets))
        .route(
            "/api/assets/{kind}/{name}",
            get(assets::download)
                .put(assets::upload)
                .delete(assets::remove)
                .layer(DefaultBodyLimit::max(3 * 1024 * 1024)),
        )
        .route("/api/social", get(social::get_social))
        .route("/api/people/requests", post(people::request))
        .route("/api/conversations", post(conversations::create))
        .route(
            "/api/conversations/{id}",
            axum::routing::patch(conversations::edit).delete(conversations::leave),
        )
        .route(
            "/api/stages/{id}",
            get(conversations::stage_state).put(conversations::stage_change),
        )
        .route("/api/scheduled-events", post(conversations::save_event))
        .route(
            "/api/scheduled-events/{id}",
            delete(conversations::delete_event),
        )
        .route(
            "/api/scheduled-events/{id}/interest",
            post(conversations::interest),
        )
        .route(
            "/api/friends/{id}",
            post(social::friend_request).delete(social::remove_friend),
        )
        .route("/api/private-calls", post(private_calls::start))
        .route("/api/private-calls/{id}", post(private_calls::answer))
        .route(
            "/api/private-calls/{id}/join",
            post(private_calls::answer_and_join),
        )
        .route("/api/voice/leave-exact", post(voice_recovery::leave_exact))
        .route(
            "/api/blocks/{id}",
            post(social::block_user).delete(social::unblock_user),
        )
        .route("/api/presence", put(social::set_presence))
        .route(
            "/api/activity",
            put(activity::share).delete(activity::withdraw),
        )
        .route(
            "/api/conversations/{id}/invitations",
            post(group_invites::invite),
        )
        .route(
            "/api/conversations/{id}/invitations/{user}",
            delete(group_invites::revoke),
        )
        .route(
            "/api/group-invitations/{id}/accept",
            post(group_invites::accept),
        )
        .route(
            "/api/group-invitations/{id}/decline",
            post(group_invites::decline),
        )
        // The independent host locator: exact POST routes, signed by hosts, no bearer.
        .route(
            relay_protocol::host_locator::LOOKUP_PATH,
            post(host_locator_directory::lookup).layer(DefaultBodyLimit::max(
                host_locator_directory::LOOKUP_BODY_BYTES,
            )),
        )
        .route(
            relay_protocol::host_locator::PUBLISH_PATH,
            post(host_locator_directory::publish).layer(DefaultBodyLimit::max(
                relay_protocol::host_locator::MAX_RECORD_BYTES,
            )),
        )
        // LAN pairing: administrators create and revoke; a LAN peer redeems with a MAC.
        .route(
            relay_protocol::lan_pairing::CREATE_PATH,
            post(lan_pairing::create).layer(DefaultBodyLimit::max(
                relay_protocol::lan_pairing::MAX_CREATE_BYTES,
            )),
        )
        .route(
            relay_protocol::lan_pairing::REDEEM_PATH,
            post(lan_pairing::redeem).layer(DefaultBodyLimit::max(
                relay_protocol::lan_pairing::MAX_REDEEM_BYTES,
            )),
        )
        .route(
            relay_protocol::lan_pairing::REVOKE_PATH,
            post(lan_pairing::revoke).layer(DefaultBodyLimit::max(lan_pairing::REVOKE_BODY_BYTES)),
        )
        // Local operator control only: authenticated by the private local-control secret
        // in its own extractor, never by a community session.
        .route(
            relay_protocol::host_locator::LOCAL_PAGE_PATH,
            post(handover_locator_page::page).layer(DefaultBodyLimit::max(
                handover_locator_page::BODY_BYTES,
            )),
        )
        .route(
            relay_protocol::host_locator::LOCAL_SESSION_PAGE_PATH,
            post(handover_locator_page::session_page).layer(DefaultBodyLimit::max(
                handover_locator_page::BODY_BYTES,
            )),
        )
        .route("/api/reports", post(social::report).get(social::reports))
        .route("/api/reports/{id}", put(social::resolve_report))
        .route("/api/roles", get(roles::get_roles).post(roles::create_role))
        .route(
            "/api/roles/{id}",
            axum::routing::put(roles::edit_role).delete(roles::delete_role),
        )
        .route(
            "/api/members/{id}/roles",
            axum::routing::put(roles::set_member_roles),
        )
        .route("/api/audit-log", get(roles::audit_log))
        .route(
            "/api/protection",
            axum::routing::put(protection::put_protection).layer(DefaultBodyLimit::max(1024)),
        )
        .route("/api/protection/call", post(protection::open_call))
        .route(
            "/api/server-settings",
            get(server_settings::get_settings).patch(server_settings::save_settings),
        )
        .route("/api/system-events", get(server_settings::system_events))
        .route("/api/settings/uploads", get(upload_policy::get_policy).put(upload_policy::put_policy).layer(DefaultBodyLimit::max(32 * 1024)))
        .route("/api/hosting/voice-gateways", get(gateway_admission::list))
        .route(
            "/api/hosting/voice-gateways/registration",
            post(gateway_admission::register)
                .delete(gateway_admission::deregister)
                .layer(DefaultBodyLimit::max(1024)),
        )
        .route(
            "/api/hosting/voice-gateways/grants",
            get(gateway_admission::grants),
        )
        .route(
            "/api/hosting/voice-gateways/leases",
            post(gateway_admission::lease)
                .delete(gateway_admission::release)
                .layer(DefaultBodyLimit::max(1024)),
        )
        .route(
            "/api/replica/token",
            post(replica::service::mint_token).delete(replica::service::revoke_token),
        )
        .route("/api/replica/pull/info", get(replica::service::info))
        .route(
            "/api/replica/pull/exports",
            post(replica::service::create_export).layer(DefaultBodyLimit::max(1024 * 1024)),
        )
        .route(
            "/api/replica/pull/exports/{id}",
            get(replica::service::get_export).delete(replica::service::cancel_export),
        )
        .route(
            "/api/replica/pull/exports/{id}/parts/{index}",
            get(replica::service::get_part),
        )
        .route(
            "/api/replica/pull/exports/{id}/files/{table}/{file_id}",
            get(replica::service::get_file),
        )
        .route(
            "/api/replica/pull/exports/{id}/done",
            post(replica::service::finish_export).layer(DefaultBodyLimit::max(1024)),
        )
        .route(
            "/api/replica/pull/status",
            post(replica::service::post_status).layer(DefaultBodyLimit::max(1024)),
        )
        .route("/api/hosting", get(hosting::get_hosting))
        .route(
            "/api/hosting/offers",
            post(hosting::offer_roles).layer(DefaultBodyLimit::max(1024)),
        )
        .route(
            "/api/hosting/answer",
            post(hosting::answer_offer).layer(DefaultBodyLimit::max(1024)),
        )
        .route(
            "/api/hosting/candidates/{user}",
            axum::routing::delete(hosting::revoke_roles),
        )
        .route(
            "/api/discord",
            get(discord_bridge::get_settings).patch(discord_bridge::save_settings),
        )
        .route(
            "/api/e2ee/device",
            get(e2ee::own_devices).put(e2ee::register_device),
        )
        .route(
            "/api/e2ee/device/attestation",
            put(e2ee::put_invite_attestation).layer(DefaultBodyLimit::max(1024)),
        )
        .route("/api/e2ee/device/{id}", delete(e2ee::revoke_device))
        .route(
            "/api/e2ee/key-packages",
            get(e2ee::package_status)
                .post(e2ee::publish_packages)
                .layer(DefaultBodyLimit::max(512 * 1024)),
        )
        .route(
            "/api/e2ee/key-packages/consume",
            post(e2ee::consume_package),
        )
        .route("/api/e2ee/context", get(e2ee::context))
        .route("/api/e2ee/directory", get(e2ee::directory))
        .route("/api/e2ee/call-inbox", get(e2ee::call_inbox))
        .route(
            "/api/e2ee/call-requests",
            get(e2ee::call_requests)
                .post(e2ee::send_call_request)
                .layer(DefaultBodyLimit::max(16 * 1024)),
        )
        .route(
            "/api/e2ee/call-requests/ack",
            post(e2ee::ack_call_request).layer(DefaultBodyLimit::max(2048)),
        )
        .route(
            "/api/e2ee/events",
            post(e2ee::append_events).layer(DefaultBodyLimit::max(e2ee::MAX_BATCH_JSON_BYTES)),
        )
        .route(
            "/api/profile",
            get(profiles::own_profile).put(profiles::update_profile),
        )
        .route("/api/profiles/{id}", get(profiles::member_profile))
        .route(
            "/api/profile/avatar",
            axum::routing::put(profiles::upload_avatar)
                .delete(profiles::remove_avatar)
                .layer(DefaultBodyLimit::max(profiles::MAX_AVATAR_UPLOAD_BYTES)),
        )
        .route("/api/profiles/{id}/avatar", get(profiles::download_avatar))
        .route("/api/channels", post(create_channel))
        .route("/api/channels/{id}/move", put(ordering::channel))
        .route("/api/categories/{id}/move", put(ordering::category))
        .route("/api/channels/{id}/pins", get(chat_controls::pins))
        .route(
            "/api/channels/{id}/pins/{message_id}",
            axum::routing::put(chat_controls::pin_message).delete(chat_controls::unpin_message),
        )
        .route(
            "/api/channels/{id}",
            get(channel_details)
                .patch(edit_channel)
                .delete(delete_channel),
        )
        .route(
            "/api/channels/{id}/permissions",
            get(permissions).put(set_permissions),
        )
        .route("/api/categories", get(categories).post(create_category))
        .route(
            "/api/categories/{id}",
            delete(delete_category).patch(edit_category),
        )
        .route("/api/members", get(list_members))
        .route("/api/members/{id}", axum::routing::patch(edit_member))
        .route("/api/history", get(history))
        .route("/api/changes", get(changes))
        .route("/api/read-state", get(read_states).patch(mark_read))
        .route("/api/sessions", get(sessions))
        .route("/api/sessions/{id}", delete(revoke_session))
        .route("/api/password", axum::routing::patch(change_password))
        .route("/api/storage", get(storage))
        .route("/api/admin/metadata", get(admin_metadata))
        .route("/api/admin/shutdown", post(admin_shutdown))
        .route("/api/local/shutdown", post(local_shutdown))
        .route("/api/admin/backup", get(snapshot_backup))
        .route("/api/rtc/token", post(rtc::rtc_token))
        .route("/api/rtc/media", get(rtc_media::upgrade))
        .route("/rtc", get(rtc::rtc_proxy))
        .route("/rtc/v1", get(rtc::rtc_proxy))
        .route("/rtc/validate", get(rtc::rtc_validate))
        .route("/rtc/v1/validate", get(rtc::rtc_validate))
        .route("/api/rtc/authorize", get(rtc::rtc_authorize))
        .route("/api/admin/backups", get(backups).post(create_backup))
        .route("/api/admin/backups/{id}", get(download_backup))
        .route("/api/admin/restore", post(restore_backup))
        .route(
            "/api/messages",
            get(messages)
                .post(send_message)
                .layer(DefaultBodyLimit::max(128 * 1024)),
        )
        .route(
            "/api/typing",
            get(chat_controls::typing_state).post(chat_controls::set_typing),
        )
        .route(
            "/api/messages/{message_id}",
            delete(delete_message)
                .patch(edit_message)
                .layer(DefaultBodyLimit::max(128 * 1024)),
        )
        .route("/api/messages/{message_id}/reactions", post(reaction))
        .route(
            "/api/uploads",
            post(upload).layer(DefaultBodyLimit::max(MAX_SUPPORTED_UPLOAD_BYTES)),
        )
        .route("/api/uploads/{upload_id}", get(download))
        .route("/api/events", get(events))
        .route("/api/voice/join", post(voice_join))
        .route("/api/voice/resume", post(voice_recovery::resume))
        .route("/api/voice/tunnel", get(voice::tunnel))
        .route("/api/voice/state", post(voice_state))
        .route("/api/voice/leave", post(voice_leave))
        .route("/api/media/frames", get(media_frames))
        .route(
            "/api/media/frame",
            post(media_frame)
                .delete(stop_media)
                .layer(DefaultBodyLimit::max(MAX_FRAME_BYTES)),
        )
        .route("/api/media/frames/{frame_id}", get(download_frame))
        .route("/api/media/live/{user_id}/{kind}", get(download_live_frame))
        .merge(discord_host::routes())
        .merge(discord_channel_status::routes())
        .layer(DefaultBodyLimit::max(64 * 1024))
        .layer(middleware::from_fn_with_state(state.clone(), rate_limit))
        .layer(middleware::from_fn_with_state(state.clone(), dispatch))
        .layer(middleware::from_fn_with_state(state.clone(), people::guard))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            request_host::check,
        ))
        .with_state(state)
}

/// The router of the explicit pairing-only LAN listener of a desktop host which keeps its
/// ordinary HTTP and voice on loopback: the ordinary router behind a filter which admits
/// exactly `POST` of the LAN pairing redemption path and nothing else.
pub fn lan_pairing_router(state: AppState) -> Router {
    lan_pairing::narrow(router(state))
}

pub fn network_ingress_router(state: AppState, token: String) -> Router {
    network_ingress_gate(router(state), token)
}
pub(crate) fn network_ingress_gate(routes: Router, token: String) -> Router {
    let valid_token = (32..=256).contains(&token.len());
    let token_digest = Sha256::digest(token.as_bytes());
    routes.layer(middleware::from_fn(
        move |mut request: axum::extract::Request, next: Next| {
            let expected_digest = token_digest;
            async move {
                if !request
                    .extensions()
                    .get::<ConnectInfo<SocketAddr>>()
                    .is_some_and(|peer| peer.0.ip().is_loopback())
                {
                    return forbidden().into_response();
                }
                let proof = request
                    .headers()
                    .get("x-relay-network-ingress")
                    .and_then(|value| value.to_str().ok());
                if !valid_token
                    || !proof.is_some_and(|proof| {
                        proof.len() <= 256
                            && bool::from(
                                Sha256::digest(proof.as_bytes())[..].ct_eq(&expected_digest[..]),
                            )
                    })
                {
                    return unauthorized().into_response();
                }
                let peer_ip = request
                    .headers()
                    .get("x-relay-network-peer")
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| {
                        value.parse::<std::net::Ipv4Addr>().ok().filter(|ip| {
                            let bytes = ip.octets();
                            bytes[0] == 100
                                && (64..=127).contains(&bytes[1])
                                && ip.to_string() == value
                        })
                    });
                let Some(peer_ip) = peer_ip else {
                    return unauthorized().into_response();
                };
                // Never carry helper capabilities or claimed local-owner/proxy
                // identity into handlers, logging, upgrades, or credentials APIs.
                let remove: Vec<_> = request
                    .headers()
                    .keys()
                    .filter(|name| {
                        matches!(
                            name.as_str(),
                            "origin"
                                | "forwarded"
                                | "x-relay-network-ingress"
                                | "x-relay-network-peer"
                                | "x-relay-local-control"
                                | "x-relay-instance-id"
                                | "x-relay-server-id"
                        ) || name.as_str().starts_with("x-forwarded-")
                    })
                    .cloned()
                    .collect();
                for name in remove {
                    request.headers_mut().remove(name);
                }
                request.extensions_mut().insert(NetworkIngress { peer_ip });
                // Only the authenticated SDK listener replaces socket-loopback
                // identity. Owner HTTP never trusts network-peer headers.
                request.extensions_mut().insert(ConnectInfo(SocketAddr::new(
                    std::net::IpAddr::V4(peer_ip),
                    0,
                )));
                let path = request.uri().path();
                if path == "/api/local" || path.starts_with("/api/local/") {
                    return forbidden().into_response();
                }
                next.run(request).await
            }
        },
    ))
}

async fn health(State(state): State<AppState>) -> Json<serde_json::Value> {
    let db = lock(&state.0.db);
    // A database is created before the owner finishes setup. This optional
    // form hint never grants registration; register() still enforces first-owner rules.
    let setup_required = db
        .query_row("SELECT NOT EXISTS(SELECT 1 FROM users)", [], |r| {
            r.get::<_, bool>(0)
        })
        .ok();
    let setup_required = if state.0.config.people {
        Some(false)
    } else {
        setup_required
    };
    Json(
        serde_json::json!({"ok":true,"setup_required":setup_required,"server_name":server_settings::presentation(&db, &state.0.name).map(|p| p.name).unwrap_or_else(|_| state.0.name.clone()),"version":env!("CARGO_PKG_VERSION"),"server_id":server_id(&db).unwrap_or_default(),"protocol_version":API_PROTOCOL_VERSION,"schema_version":SCHEMA_VERSION,"managed_instance":true,"instance_id":state.0.instance_id,"pid":std::process::id(),"rtc_enabled":state.0.config.rtc.is_some(),"people_service":state.0.config.people}),
    )
}

async fn rate_limit(
    State(state): State<AppState>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    if request.extensions().get::<NetworkIngress>().is_some()
        && (request.uri().path() == "/api/local" || request.uri().path().starts_with("/api/local/"))
    {
        return forbidden().into_response();
    }
    // The replica pull routes use their own token, checked by the handler on every
    // request; a login token does not open them and their token opens nothing else.
    if request.uri().path().starts_with("/api/replica/pull/") {
        return next.run(request).await;
    }
    // The operator pages are authenticated by the local-control secret in their own extractor,
    // before any body is read. Only these two exact POSTs, and only after the network-helper
    // check above has already refused every ingress request on /api/local.
    if !state.0.config.people
        && handover_locator_page::is_route(request.uri().path(), request.method())
    {
        return next.run(request).await;
    }
    // A LAN redemption has no session: its own extractor admits only a real LAN socket and
    // the handler checks a MAC made with the shared code. Exact POST, communities only.
    if !state.0.config.people
        && lan_pairing::is_redeem_route(request.uri().path(), request.method())
    {
        return next.run(request).await;
    }
    // The host locator takes no bearer: a host's signature authenticates it and the
    // handler bounds each ingress before reading the body. Exact POST routes, Friends only.
    if state.0.config.people
        && host_locator_directory::is_route(request.uri().path(), request.method())
    {
        return next.run(request).await;
    }
    // Authenticate protected requests before body extraction can allocate upload buffers.
    if !matches!(
        request.uri().path(),
        "/health"
            | "/api/health"
            | "/api/register"
            | "/api/login"
            | "/api/account-login"
            | "/api/account-login/challenge"
            | "/api/hosting/handover/route"
            | "/api/events"
            | "/api/voice/tunnel"
            | "/api/rtc/authorize"
            | "/api/rtc/media"
            | "/api/local/shutdown"
            | "/api/local/account-destination"
            | "/rtc"
            | "/rtc/v1"
            | "/rtc/validate"
            | "/rtc/v1/validate"
    ) {
        if let Err(error) = state.auth(request.headers()) {
            return error.into_response();
        }
    }
    let _transfer_permit = if (request.method() == axum::http::Method::POST
        && matches!(request.uri().path(), "/api/uploads" | "/api/media/frame"))
        || (request.method() == axum::http::Method::PUT
            && request.uri().path() == "/api/profile/avatar")
    {
        match state.0.transfer_jobs.clone().try_acquire_owned() {
            Ok(permit) => Some(permit),
            Err(_) => {
                return ApiError::new(
                    StatusCode::TOO_MANY_REQUESTS,
                    "Upload service is busy. Try again shortly",
                )
                .into_response()
            }
        }
    } else {
        None
    };
    let path = request.uri().path();
    let signin = matches!(
        path,
        "/api/register" | "/api/login" | "/api/account-login" | "/api/account-login/challenge"
    );
    let mutation = !matches!(
        *request.method(),
        axum::http::Method::GET | axum::http::Method::HEAD
    ) && !matches!(
        path,
        "/api/media/frame"
            | "/api/voice/state"
            // Authenticated, recipient-encrypted file reads use POST for their
            // bounded proof body; they must not inherit the mutation/minute cap.
            | "/api/hosting/handover/part"
            | "/api/local/shutdown"
            | "/api/local/account-destination"
    );
    let route_probe = path == "/api/hosting/handover/route";
    if signin || route_probe || mutation {
        let direct = request
            .extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .map(|v| v.0.ip());
        let network_ingress = request.extensions().get::<NetworkIngress>();
        let mut remote = if let Some(network) = network_ingress {
            format!("network-ingress:{}", network.peer_ip)
        } else {
            direct
                .map(|v| v.to_string())
                .unwrap_or_else(|| "local".into())
        };
        if network_ingress.is_none()
            && direct.is_some_and(|ip| state.0.config.trusted_proxy_ips.contains(&ip))
        {
            if let Some(header) = request
                .headers()
                .get("x-forwarded-for")
                .and_then(|v| v.to_str().ok())
                .filter(|v| v.len() < 2048)
            {
                if let Some(client) = header
                    .split(',')
                    .rev()
                    .filter_map(|s| s.trim().parse::<std::net::IpAddr>().ok())
                    .find(|ip| !state.0.config.trusted_proxy_ips.contains(ip))
                {
                    remote = client.to_string();
                }
            }
        }
        let key = if route_probe {
            format!("route:{remote}")
        } else if signin {
            format!("auth:{remote}")
        } else {
            match state.auth(request.headers()) {
                Ok(auth) => format!("mutation:{}", auth.user.id),
                Err(error) => return error.into_response(),
            }
        };
        let maximum = if route_probe {
            60
        } else if signin {
            15
        } else {
            300
        };
        let retry_after_ms = {
            // Unauthenticated source churn cannot consume signed-in users' slots.
            // Keep both budgets bounded and fail closed instead of evicting a
            // source that could then get a fresh brute-force allowance.
            let mut rates = lock(if route_probe {
                &state.0.route_rates
            } else if signin {
                &state.0.auth_rates
            } else {
                &state.0.mutation_rates
            });
            let timestamp = now_ms();
            rates.retain(|_, (start, _)| timestamp.saturating_sub(*start) < 60_000);
            if rates.len() >= 10000 && !rates.contains_key(&key) {
                Some(1000)
            } else {
                let record = rates.entry(key).or_insert((timestamp, 0));
                if record.1 >= maximum {
                    Some(
                        record
                            .0
                            .saturating_add(60_000)
                            .saturating_sub(timestamp)
                            .clamp(1, 60_000),
                    )
                } else {
                    record.1 += 1;
                    None
                }
            }
        };
        if let Some(delay) = retry_after_ms {
            return ApiError::cooldown(
                "rate_limited",
                "Too many requests. Retry after the cooldown",
                delay,
                None,
            )
            .into_response();
        }
    }
    next.run(request).await
}

/// Letters a to z, digits, dots, dashes and underscores only: nothing that can
/// look like another person's name in a different alphabet.
fn plain_username(name: &str) -> bool {
    name.trim()
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

fn validate_auth(input: &AuthRequest) -> ApiResult<()> {
    let name = input.username.trim();
    if name.len() < 2
        || name.len() > 32
        || !name
            .chars()
            .all(|v| v.is_alphanumeric() || matches!(v, '_' | '-' | '.'))
    {
        return Err(bad(
            "Username must be 2–32 letters, numbers, dots, dashes or underscores",
        ));
    }
    if input.password.len() < 10 || input.password.len() > 256 {
        return Err(bad("Password must be 10–256 characters"));
    }
    if input.invite.len() > 128 {
        return Err(bad("Invalid invitation"));
    }
    Ok(())
}

fn new_session(db: &Connection, user: User) -> ApiResult<Session> {
    let token = random_token();
    db.execute(
        "INSERT INTO sessions(token_hash,user_id,expires,created_at) VALUES(?1,?2,?3,?4)",
        params![token_hash(&token), user.id, now() + TOKEN_LIFETIME, now()],
    )?;
    Ok(Session { token, user })
}

async fn register(
    State(state): State<AppState>,
    ingress: Option<axum::Extension<NetworkIngress>>,
    Json(input): Json<AuthRequest>,
) -> ApiResult<Json<Session>> {
    validate_auth(&input)?;
    password_policy::validate(&input.username, &input.password)?;
    // New names are plain ASCII, so "Аlex" with a Cyrillic А cannot pass for
    // "Alex". Existing accounts keep their names and can still sign in.
    if !plain_username(&input.username) {
        return Err(bad(
            "New usernames use a–z, 0–9, dots, dashes or underscores. Your display name can use any letters.",
        ));
    }
    let permit = state
        .0
        .password_jobs
        .clone()
        .try_acquire_owned()
        .map_err(|_| {
            ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "Sign-in service is busy. Try again shortly",
            )
        })?;
    let password = input.password.clone();
    let password_hash = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        Argon2::default()
            .hash_password(password.as_bytes(), &SaltString::generate(&mut OsRng))
            .map(|hash| hash.to_string())
    })
    .await
    .map_err(|_| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "Password hashing failed"))?
    .map_err(|_| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "Password hashing failed"))?;
    let session = {
        let mut db = lock(&state.0.db);
        let tx = db.transaction()?;
        let first = tx.query_row("SELECT COUNT(*) FROM users", [], |r| r.get::<_, i64>(0))? == 0;
        if first && ingress.is_some() {
            return Err(forbidden());
        }
        if !first {
            if input.initial_protection.is_some() {
                return Err(bad(
                    "Initial encryption can only be chosen for a new server",
                ));
            }
            let affected = tx.execute(
                "UPDATE invites SET remaining_uses=remaining_uses-1 WHERE code_hash=?1 AND expires>?2 AND remaining_uses>0",
                params![token_hash(input.invite.trim()), now()],
            )?;
            if affected == 0 {
                return Err(forbidden());
            }
        }
        if tx
            .query_row(
                "SELECT id FROM users WHERE username=?1 COLLATE NOCASE",
                [input.username.trim()],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .is_some()
        {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "Username is already taken",
            ));
        }
        let user = User {
            id: id(),
            username: input.username.trim().to_string(),
            role: if first { "admin" } else { "member" }.into(),
            profile: Default::default(),
        };
        tx.execute(
            "INSERT INTO users VALUES(?1,?2,?3,?4)",
            params![user.id, user.username, password_hash, user.role],
        )?;
        tx.execute("INSERT INTO membership(user_id) VALUES(?1)", [&user.id])?;
        tx.execute("INSERT INTO user_profiles(user_id) VALUES(?1)", [&user.id])?;
        if first && input.initial_protection == Some(ProtectionLevel::Open) {
            let protection = CommunityProtection {
                level: ProtectionLevel::Open,
                revision: 1,
                changed_by: Some(user.id.clone()),
            };
            tx.execute(
                "INSERT INTO metadata(key,value) VALUES('protection',?1)",
                [serde_json::to_string(&protection)
                    .map_err(|_| bad("Invalid protection setting"))?],
            )?;
            roles::audit(&tx, &user.id, "server.protection", "open")?;
        }
        if !first {
            // Only the hash of the code: lets this member prove, once, which
            // invitation they joined through (device attestation).
            tx.execute(
                "INSERT INTO member_invitations(user_id,invite_id) VALUES(?1,?2)",
                params![user.id, token_hash(input.invite.trim())],
            )?;
        }
        server_settings::record_system_event(&tx, &user.id, "joined")?;
        let result = new_session(&tx, user)?;
        tx.commit()?;
        result
    };
    state.notify("members", None, Some(session.user.id.clone()), None);
    Ok(Json(session))
}

async fn login(
    State(state): State<AppState>,
    Json(input): Json<AuthRequest>,
) -> ApiResult<Json<Session>> {
    validate_auth(&input)?;
    let permit = state
        .0
        .password_jobs
        .clone()
        .try_acquire_owned()
        .map_err(|_| {
            ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "Sign-in service is busy. Try again shortly",
            )
        })?;
    let found = {
        let db = lock(&state.0.db);
        db.query_row(
            "SELECT u.id,u.username,u.role,p.display_name,p.bio,p.version,p.avatar IS NOT NULL,p.appearance,u.password_hash FROM users u LEFT JOIN user_profiles p ON p.user_id=u.id WHERE u.username=?1 COLLATE NOCASE",
            [input.username.trim()],
            |r| {
                Ok((
                    row_user(r)?,
                    r.get::<_, String>(8)?,
                ))
            },
        )
        .optional()?
    };
    // Run a real password hash even for an unknown account, to avoid a cheap username timing oracle.
    let mfa_password = zeroize::Zeroizing::new(input.password.clone());
    let password = zeroize::Zeroizing::new(input.password);
    let hash = found.as_ref().map(|(_, hash)| hash.clone());
    let valid = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        match hash {
            Some(hash) => PasswordHash::new(&hash).is_ok_and(|hash| {
                Argon2::default()
                    .verify_password(password.as_bytes(), &hash)
                    .is_ok()
            }),
            None => {
                let _ = Argon2::default()
                    .hash_password(password.as_bytes(), &SaltString::generate(&mut OsRng));
                false
            }
        }
    })
    .await
    .map_err(|_| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Password verification failed",
        )
    })?;
    if !valid {
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "Incorrect username or password",
        ));
    }
    let (user, verified_hash) = found.ok_or_else(unauthorized)?;
    let factor = authenticator::prepare(
        &state,
        &user.id,
        mfa_password,
        input.second_factor.as_deref().unwrap_or_default(),
    )
    .await?;
    let mut db = lock(&state.0.db);
    let tx = db.transaction()?;
    let same_password: bool = tx.query_row(
        "SELECT password_hash=?2 FROM users WHERE id=?1",
        params![user.id, verified_hash],
        |row| row.get(0),
    )?;
    if !same_password {
        return Err(unauthorized());
    }
    authenticator::consume(&tx, &user.id, factor.as_ref())?;
    let status: String = tx.query_row(
        "SELECT status FROM membership WHERE user_id=?1",
        [&user.id],
        |r| r.get(0),
    )?;
    if status == "removed" {
        if tx.execute("UPDATE invites SET remaining_uses=remaining_uses-1 WHERE code_hash=?1 AND expires>?2 AND remaining_uses>0",params![token_hash(&input.invite),now()])?!=1{return Err(ApiError::new(StatusCode::FORBIDDEN,"A new valid invite is required to rejoin this community"));}
        tx.execute(
            "UPDATE membership SET status='active' WHERE user_id=?1",
            [&user.id],
        )?;
        tx.execute(
            "INSERT OR REPLACE INTO member_invitations(user_id,invite_id) VALUES(?1,?2)",
            params![user.id, token_hash(&input.invite)],
        )?;
        server_settings::record_system_event(&tx, &user.id, "joined")?;
    } else if status != "active" {
        return Err(forbidden());
    }
    let result = new_session(&tx, user)?;
    tx.commit()?;
    Ok(Json(result))
}

async fn logout(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<StatusCode> {
    let auth = state.auth(&headers)?;
    lock(&state.0.db).execute(
        "DELETE FROM sessions WHERE token_hash=?1",
        [&auth.token_hash],
    )?;
    state.remove_voice(&lock(&state.0.db), &auth.user.id, Some(&auth.token_hash))?;
    Ok(StatusCode::NO_CONTENT)
}

fn row_user(row: &rusqlite::Row<'_>) -> rusqlite::Result<User> {
    Ok(User {
        id: row.get(0)?,
        username: row.get(1)?,
        role: row.get(2)?,
        profile: profiles::read_profile(row, 0, 3)?,
    })
}
fn get_user(db: &Connection, user_id: &str) -> ApiResult<User> {
    db.query_row(
        "SELECT u.id,u.username,u.role,p.display_name,p.bio,p.version,p.avatar IS NOT NULL,p.appearance FROM users u LEFT JOIN user_profiles p ON p.user_id=u.id WHERE u.id=?1",
        [user_id],
        row_user,
    )
    .optional()?
    .ok_or_else(missing)
}
fn voice_members(state: &AppState) -> Vec<VoiceMember> {
    lock(&state.0.voice)
        .iter()
        .filter(|(_, v)| v.alive_at(now()))
        .map(|(session_id, v)| VoiceMember {
            user: v.user.clone(),
            channel_id: v.channel_id.clone(),
            muted: v.state.muted,
            deafened: v.state.deafened,
            sharing: v.state.sharing,
            camera: v.state.camera,
            session_id: *session_id,
            device_id: v.device_id.clone(),
        })
        .collect()
}

async fn bootstrap(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Bootstrap>> {
    let auth = state.auth(&headers)?;
    let db = lock(&state.0.db);
    let host_paused = state.0.handover.paused_view();
    if !host_paused {
        private_calls::reconcile(&state, &db)?;
        voice_recovery::renew_lease(&state, &db, &auth)?;
    }
    let channels = visible_channels(&db, &auth.user.id)?;
    let mut statement=db.prepare("SELECT u.id,u.username,u.role,p.display_name,p.bio,p.version,p.avatar IS NOT NULL,p.appearance FROM users u LEFT JOIN user_profiles p ON p.user_id=u.id JOIN membership m ON m.user_id=u.id WHERE m.status='active' ORDER BY username COLLATE NOCASE")?;
    let members = statement
        .query_map([], row_user)?
        .collect::<Result<Vec<_>, _>>()?;
    let visible = channels.iter().map(|c| c.id.as_str()).collect::<Vec<_>>();
    let voice_members = voice_members(&state)
        .into_iter()
        .filter(|v| visible.contains(&v.channel_id.as_str()))
        .collect();
    let mut result = Bootstrap {
        upload_limits: upload_policy::for_user(&db, &auth.user.id)?,
        host_paused,
        host_authority: (!state.0.config.people).then(|| state.0.handover.route_pin()),
        account_login_only: db.query_row(
            "SELECT password_hash=?2 FROM users WHERE id=?1",
            params![auth.user.id, account_identity::login::ACCOUNT_ONLY],
            |r| r.get(0),
        )?,
        account_provider: state.0.config.account_provider.as_ref().map(|p| p.public()),
        linked_identities: account_identity::visible(
            &db,
            state.0.config.account_provider.as_ref(),
        )?,
        protection: protection::current(&db)?,
        server_presentation: server_settings::for_user(&db, &state.0.name, &auth.user.id)?,
        assets: assets::list(&db)?,
        conversations: conversations::list(&db, &auth.user.id)?,
        group_invitations: if state.0.config.people {
            group_invites::incoming(&db, &auth.user.id)?
        } else {
            vec![]
        },
        group_invitations_sent: if state.0.config.people {
            group_invites::sent(&db, &auth.user.id)?
        } else {
            vec![]
        },
        // Filled for the Friends service by people::snapshot.
        group_presence: vec![],
        scheduled_events: conversations::events(&db, &auth.user.id)?,
        stage_members: conversations::stage_members(&db, &auth.user.id)?,
        social: if host_paused {
            social::relationships(&db, &auth.user.id)?
        } else {
            private_calls::social(&state, &db, &auth.user.id)?
        },
        presence: social::presence(&db, &auth.user.id)?,
        roles: roles::list(&db)?,
        member_roles: roles::assignments(&db)?,
        permissions: roles::permissions(&db, &auth.user.id)?,
        server_name: server_settings::presentation(&db, &state.0.name)?.name,
        user: auth.user.clone(),
        channels,
        members,
        voice_members,
        server_id: server_id(&db)?,
        instance_id: state.0.instance_id.clone(),
        protocol_version: API_PROTOCOL_VERSION,
        capabilities: {
            let mut capabilities = vec![
                "history".into(),
                "search".into(),
                "read-state".into(),
                "changes".into(),
                "idempotent-send".into(),
                "permissions".into(),
                "backups".into(),
                "pins-v1".into(),
                "typing-v1".into(),
                "slowmode-v1".into(),
                "message-cooldowns-v1".into(),
                "profiles-v1".into(),
                "profiles-v2".into(),
                "server-settings-v1".into(),
                "upload-policy-v1".into(),
                "hosting-v1".into(),
                "handover-preparation-v1".into(),
                "handover-private-setup-v1".into(),
                "gateway-broker-v1".into(),
                "replica-pull-v1".into(),
                "discord-command-v1".into(),
                "discord-host-credentials-v1".into(),
                "discord-channel-status-v1".into(),
                "welcome-v1".into(),
                "profiles-v3".into(),
                "custom-roles-v1".into(),
                "voice-text-v1".into(),
                "social-v1".into(),
                "private-calls-v1".into(),
                "private-call-switch-v1".into(),
                "authenticator-v1".into(),
                "account-link-v1".into(),
                "conversations-v1".into(),
                "assets-v1".into(),
                "e2ee_mls_v1".into(),
                "e2ee-voice-v3".into(),
                "e2ee-call-freshness-v1".into(),
                "protection-modes-v1".into(),
                "protection-calls-v1".into(),
                "voice-restart-resume-v1".into(),
            ];
            if state.0.config.rtc.is_some() {
                capabilities.push("rtc-livekit".into());
                capabilities.push("native-rtc-v1".into());
            }
            capabilities
        },
        read_states: all_read_states(&db, &auth.user.id)?,
        latest_change_sequence: latest_change(&db)?,
    };
    if !state.0.config.people { result.capabilities.push("host-locator-issuer-v1".into()); }
    if !state.0.config.people {
        result
            .capabilities
            .push(relay_protocol::lan_pairing::CAPABILITY.into());
    }
    if state.0.config.people {
        people::snapshot(&db, &auth.user.id, &mut result);
        activity::overlay(
            &state,
            &auth.user.id,
            &result.social.friends,
            &mut result.presence,
        );
    }
    Ok(Json(result))
}
async fn create_channel(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<CreateChannel>,
) -> ApiResult<Json<Channel>> {
    let auth = state.auth(&headers)?;
    if auth.permissions & (PERMISSION_CHANNELS | PERMISSION_ADMIN) == 0 {
        return Err(forbidden());
    }
    let name = input.name.trim();
    if name.is_empty()
        || name.len() > 64
        || input.topic.len() > 500
        || !matches!(input.kind.as_str(), "text" | "voice")
    {
        return Err(bad(
            "Choose a name up to 64 characters and a text or voice channel",
        ));
    }
    let channel = Channel {
        voice_parent_id: None,
        id: id(),
        name: name.to_string(),
        kind: input.kind.clone(),
        topic: input.topic,
        visibility: if input.visibility.is_empty() {
            "public".into()
        } else {
            input.visibility
        },
        category_id: input.category_id,
        position: input.position,
        can_send: true,
        can_join: true,
        can_stream: true,
        slowmode_seconds: input.slowmode_seconds,
        slowmode_bypass: true,
        can_manage_pins: input.kind == "text",
    };
    if !matches!(channel.visibility.as_str(), "public" | "private") {
        return Err(bad("Invalid channel visibility"));
    }
    if channel.slowmode_seconds > MAX_SLOWMODE_SECONDS
        || (channel.kind != "text" && channel.slowmode_seconds > 0)
    {
        return Err(bad("Slowmode must be 0–21600 seconds on a text channel"));
    }
    let mut connection = lock(&state.0.db);
    let db = connection.transaction()?;
    roles::require(&db, &auth.user.id, PERMISSION_CHANNELS)?;
    if let Some(category) = &channel.category_id {
        if db.query_row(
            "SELECT COUNT(*) FROM categories WHERE id=?1",
            [category],
            |r| r.get::<_, u32>(0),
        )? == 0
        {
            return Err(missing());
        }
    }
    db.execute(
        "INSERT INTO channels(id,name,kind,topic,visibility,category_id,position,slowmode_seconds) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
        params![channel.id, channel.name, channel.kind, channel.topic,channel.visibility,channel.category_id,channel.position,channel.slowmode_seconds],
    )?;
    if channel.kind == "voice" {
        voice_chat::ensure_rooms(&db)?;
    }
    roles::audit(&db, &auth.user.id, "channel.created", &channel.id)?;
    db.commit()?;
    drop(connection);
    state.notify(
        "channels",
        Some(channel.id.clone()),
        Some(auth.user.id),
        None,
    );
    Ok(Json(channel))
}

fn message_access(
    db: &Connection,
    message_id: &str,
    user_id: &str,
) -> ApiResult<(Option<String>, Option<String>, String)> {
    let data = db
        .query_row(
            "SELECT channel_id,recipient_id,author_id FROM messages WHERE id=?1",
            [message_id],
            |r| {
                Ok((
                    r.get::<_, Option<String>>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, String>(2)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(missing)?;
    active_user(db, user_id)?;
    if let Some(channel) = &data.0 {
        channel_access(db, channel, user_id, READ)?;
    }
    if data.0.is_none() && data.1.as_deref() != Some(user_id) && data.2 != user_id {
        return Err(forbidden());
    }
    if people::enabled(db) && data.0.is_none() {
        let peer = if data.2 == user_id {
            data.1.as_deref().ok_or_else(forbidden)?
        } else {
            &data.2
        };
        social::direct_access(db, user_id, peer)?;
    }
    Ok(data)
}

fn get_message(db: &Connection, message_id: &str) -> ApiResult<Message> {
    let mut message=db.query_row("SELECT m.id,m.channel_id,m.recipient_id,m.content,m.created_at,u.id,u.username,u.role,m.edited_at,o.sequence,m.reply_to_id,m.client_id,(SELECT pinned_at FROM channel_pins p WHERE p.message_id=m.id),up.display_name,up.bio,up.version,up.avatar IS NOT NULL,up.appearance FROM messages m JOIN users u ON u.id=m.author_id LEFT JOIN user_profiles up ON up.user_id=u.id JOIN message_order o ON o.message_id=m.id WHERE m.id=?1",[message_id],|r|Ok(Message{id:r.get(0)?,channel_id:r.get(1)?,recipient_id:r.get(2)?,content:r.get(3)?,created_at:r.get(4)?,author:User{id:r.get(5)?,username:r.get(6)?,role:r.get(7)?,profile:profiles::read_profile(r,5,13)?},edited_at:r.get(8)?,sequence:r.get(9)?,reply_to_id:r.get(10)?,client_id:r.get(11)?,pinned_at:r.get(12)?,mentions:vec![],attachments:vec![],reactions:vec![]})).optional()?.ok_or_else(missing)?;
    let mut mention_query =
        db.prepare("SELECT user_id FROM mentions WHERE message_id=?1 ORDER BY user_id")?;
    message.mentions = mention_query
        .query_map([message_id], |r| r.get(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let mut uploads =
        db.prepare("SELECT id,name,mime,size FROM uploads WHERE message_id=?1 ORDER BY rowid")?;
    message.attachments = uploads
        .query_map([message_id], |r| {
            let id: String = r.get(0)?;
            Ok(Attachment {
                url: format!("/api/uploads/{id}"),
                id,
                name: r.get(1)?,
                mime: r.get(2)?,
                size: r.get(3)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut reactions = db.prepare(
        "SELECT emoji,user_id FROM reactions WHERE message_id=?1 ORDER BY emoji,user_id",
    )?;
    for item in reactions.query_map([message_id], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })? {
        let (emoji, user_id) = item?;
        if let Some(last) = message.reactions.last_mut().filter(|v| v.emoji == emoji) {
            last.user_ids.push(user_id);
        } else {
            message.reactions.push(Reaction {
                emoji,
                user_ids: vec![user_id],
            });
        }
    }
    Ok(message)
}

async fn messages(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(mut q): Query<HistoryQuery>,
) -> ApiResult<Json<Vec<Message>>> {
    let auth = state.auth(&headers)?;
    if q.limit.is_none() {
        q.limit = Some(200);
    }
    Ok(Json(
        history_page(&lock(&state.0.db), &auth.user.id, &q)?.messages,
    ))
}
fn message_notice(state: &AppState, kind: &str, message: &Message) {
    let audience = message
        .recipient_id
        .as_ref()
        .map(|recipient| vec![message.author.id.clone(), recipient.clone()]);
    state.notify(
        kind,
        message.channel_id.clone(),
        Some(message.author.id.clone()),
        audience,
    );
}

async fn send_message(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<SendMessage>,
) -> ApiResult<Json<Message>> {
    let auth = state.auth(&headers)?;
    if (state.0.config.people && !input.content.starts_with("relay-e2ee:v1:"))
        || !e2ee::valid_message_content(&input.content)
        || input.attachment_ids.len() > 8
        || input.mentions.len() > 64
        || (input.content.trim().is_empty() && input.attachment_ids.is_empty())
        || input
            .client_id
            .as_ref()
            .is_some_and(|id| id.is_empty() || id.len() > 128)
    {
        return Err(bad(
            "Invalid message content, attachments, mentions or client request ID",
        ));
    }
    let hash = token_hash(&serde_json::to_string(&input)?);
    let result = {
        let mut db = lock(&state.0.db);
        let tx = db.transaction()?;
        match (&input.channel_id, &input.recipient_id) {
            (Some(channel), None) => {
                channel_access(&tx, channel, &auth.user.id, SEND)?;
                if get_channel(&tx, channel)?.kind != "text" {
                    return Err(missing());
                }
            }
            (None, Some(recipient)) => {
                social::direct_access(&tx, &auth.user.id, recipient)?;
                active_user(&tx, recipient)?;
                get_user(&tx, recipient)?;
            }
            _ => return Err(bad("Choose one text channel or recipient")),
        }
        if let Some(client_id) = &input.client_id {
            if let Some((existing,request_hash))=tx.query_row("SELECT message_id,request_hash FROM send_receipts WHERE author_id=?1 AND client_id=?2",params![auth.user.id,client_id],|r|Ok((r.get::<_,Option<String>>(0)?,r.get::<_,String>(1)?))).optional()?{if request_hash!=hash{return Err(ApiError::new(StatusCode::CONFLICT,"This send ID was already used for different content"));}let existing=existing.ok_or(ApiError::new(StatusCode::GONE,"This message was already sent and subsequently deleted"))?;let message=get_message(&tx,&existing)?;chat_controls::clear_sent_typing(&state,&auth.user.id,&input);return Ok(Json(message));}
        }
        if let Some(reply) = &input.reply_to_id {
            let (parent_channel, parent_recipient, parent_author) =
                message_access(&tx, reply, &auth.user.id)?;
            if parent_channel != input.channel_id
                || (parent_channel.is_none()
                    && !((parent_author == auth.user.id && parent_recipient == input.recipient_id)
                        || (Some(parent_author.clone()) == input.recipient_id
                            && parent_recipient.as_deref() == Some(auth.user.id.as_str()))))
            {
                return Err(bad("Reply must belong to this conversation"));
            }
        }
        for user in &input.mentions {
            active_user(&tx, user)?;
            match &input.channel_id {
                Some(channel) => channel_access(&tx, channel, user, READ)?,
                None => {
                    if user != &auth.user.id && Some(user) != input.recipient_id.as_ref() {
                        return Err(forbidden());
                    }
                }
            }
        }
        chat_controls::record_message_quota(&tx, &input, &auth.user.id, now_ms())?;
        let message_id = id();
        let created = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        tx.execute("INSERT INTO messages(id,channel_id,recipient_id,author_id,content,created_at,created,reply_to_id,client_id,request_hash) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",params![message_id,input.channel_id,input.recipient_id,auth.user.id,input.content,created,now(),input.reply_to_id,input.client_id,hash])?;
        tx.execute(
            "INSERT INTO message_order(message_id) VALUES(?1)",
            [&message_id],
        )?;
        if let Some(client_id) = &input.client_id {
            tx.execute(
                "INSERT INTO send_receipts VALUES(?1,?2,?3,?4)",
                params![auth.user.id, client_id, hash, message_id],
            )?;
        }
        for user in &input.mentions {
            tx.execute(
                "INSERT OR IGNORE INTO mentions VALUES(?1,?2)",
                params![message_id, user],
            )?;
        }
        for upload in &input.attachment_ids {
            if tx.execute("UPDATE uploads SET message_id=?1 WHERE id=?2 AND owner_id=?3 AND message_id IS NULL",params![message_id,upload,auth.user.id])?!=1{return Err(forbidden());}
        }
        let message = get_message(&tx, &message_id)?;
        record_change(&tx, &message, "created")?;
        tx.commit()?;
        message
    };
    chat_controls::clear_sent_typing(&state, &auth.user.id, &input);
    message_notice(&state, "messages", &result);
    Ok(Json(result))
}
async fn edit_message(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(message_id): Path<String>,
    Json(input): Json<EditMessage>,
) -> ApiResult<Json<Message>> {
    let auth = state.auth(&headers)?;
    if (state.0.config.people && !input.content.starts_with("relay-e2ee:v1:"))
        || !e2ee::valid_message_content(&input.content)
    {
        return Err(bad("Message text may contain at most 10000 characters"));
    }
    let (message, changed) = {
        let mut connection = lock(&state.0.db);
        let db = connection.transaction()?;
        message_access(&db, &message_id, &auth.user.id)?;
        let message = get_message(&db, &message_id)?;
        if let Some(channel) = &message.channel_id {
            channel_access(&db, channel, &auth.user.id, SEND)?;
        }
        // Administration permits moderation, never rewriting another person's words.
        if message.author.id != auth.user.id {
            return Err(forbidden());
        }
        if input.content.trim().is_empty() && message.attachments.is_empty() {
            return Err(bad("Message must contain text or files"));
        }
        if input.content == message.content {
            db.commit()?;
            (message, false)
        } else {
            let edited = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
            db.execute(
                "UPDATE messages SET content=?1,edited_at=?2 WHERE id=?3 AND author_id=?4",
                params![input.content, edited, message_id, auth.user.id],
            )?;
            let changed = get_message(&db, &message_id)?;
            record_change(&db, &changed, "updated")?;
            db.commit()?;
            (changed, true)
        }
    };
    if changed {
        message_notice(&state, "messages", &message);
    }
    Ok(Json(message))
}

#[derive(Deserialize)]
struct ReactionInput {
    emoji: String,
    #[serde(default)]
    active: Option<bool>,
}
async fn reaction(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(message_id): Path<String>,
    Json(input): Json<ReactionInput>,
) -> ApiResult<Json<Message>> {
    let auth = state.auth(&headers)?;
    if input.emoji.trim().is_empty()
        || input.emoji.len() > 32
        || input.emoji.chars().any(char::is_control)
    {
        return Err(bad("Choose an emoji up to 32 bytes"));
    }
    let message = {
        let mut connection = lock(&state.0.db);
        let db = connection.transaction()?;
        let (channel, _, _) = message_access(&db, &message_id, &auth.user.id)?;
        if let Some(channel) = channel {
            channel_access(&db, &channel, &auth.user.id, SEND)?;
        }
        let changed = if input.active == Some(true) {
            0
        } else {
            db.execute(
                "DELETE FROM reactions WHERE message_id=?1 AND emoji=?2 AND user_id=?3",
                params![message_id, input.emoji, auth.user.id],
            )?
        };
        if changed == 0 && input.active != Some(false) {
            db.execute(
                "INSERT OR IGNORE INTO reactions VALUES(?1,?2,?3)",
                params![message_id, input.emoji, auth.user.id],
            )?;
        }
        let result = get_message(&db, &message_id)?;
        record_change(&db, &result, "updated")?;
        db.commit()?;
        result
    };
    message_notice(&state, "messages", &message);
    Ok(Json(message))
}

async fn delete_message(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(message_id): Path<String>,
) -> ApiResult<StatusCode> {
    let auth = state.auth(&headers)?;
    let message = {
        let mut connection = lock(&state.0.db);
        let db = connection.transaction()?;
        message_access(&db, &message_id, &auth.user.id)?;
        let message = get_message(&db, &message_id)?;
        if message.author.id != auth.user.id && !roles::has(&db, &auth.user.id, PERMISSION_MESSAGES)
        {
            return Err(forbidden());
        }
        record_change(&db, &message, "deleted")?;
        if message.author.id != auth.user.id {
            roles::audit(&db, &auth.user.id, "message.deleted", &message.id)?;
        }
        db.execute("DELETE FROM messages WHERE id=?1", [message_id])?;
        db.commit()?;
        message
    };
    message_notice(&state, "messages", &message);
    if message.pinned_at.is_some() {
        state.notify("pins", message.channel_id.clone(), Some(auth.user.id), None);
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct UploadQuery {
    name: String,
    #[serde(default = "default_mime")]
    mime: String,
}
fn default_mime() -> String {
    "application/octet-stream".into()
}
async fn upload(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(input): Query<UploadQuery>,
    request: axum::extract::Request,
) -> ApiResult<Json<Attachment>> {
    let auth = state.auth(&headers)?;
    if !upload_policy::input_valid(&input.name, &input.mime) {
        return Err(bad("Invalid filename or MIME type"));
    }
    // Authenticate and calculate the account's allowance before buffering a
    // body. A role change during the read is checked again by commit().
    let limit = upload_policy::for_user(&lock(&state.0.db), &auth.user.id)?.effective();
    if limit == 0 {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "File uploads are disabled for your roles"));
    }
    let bytes = axum::body::to_bytes(request.into_body(), limit as usize)
        .await.map_err(|_| upload_policy::too_large(limit))?;
    Ok(Json(upload_policy::commit(&state, &auth.user.id, &input.name, &input.mime, &bytes)?))
}
async fn download(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(upload_id): Path<String>,
) -> ApiResult<Response> {
    let auth = state.auth(&headers)?;
    let db = lock(&state.0.db);
    let (owner, message_id, mime) = db
        .query_row(
            "SELECT owner_id,message_id,mime FROM uploads WHERE id=?1",
            [&upload_id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, String>(2)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(missing)?;
    match message_id {
        Some(message_id) => {
            message_access(&db, &message_id, &auth.user.id)?;
        }
        None if owner != auth.user.id => return Err(forbidden()),
        None => {}
    }
    let bytes: Vec<u8> =
        db.query_row("SELECT data FROM uploads WHERE id=?1", [upload_id], |r| {
            r.get(0)
        })?;
    let content_type = axum::http::HeaderValue::from_str(&mime)
        .unwrap_or_else(|_| axum::http::HeaderValue::from_static("application/octet-stream"));
    Ok((
        [
            (header::CONTENT_TYPE, content_type),
            (
                header::CONTENT_DISPOSITION,
                axum::http::HeaderValue::from_static("attachment"),
            ),
            (
                header::X_CONTENT_TYPE_OPTIONS,
                axum::http::HeaderValue::from_static("nosniff"),
            ),
        ],
        bytes,
    )
        .into_response())
}

async fn events(
    State(state): State<AppState>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> ApiResult<Response> {
    let auth = state.auth(&headers)?;
    let authority = state.0.handover.watch_transports();
    let permit = state
        .0
        .event_connections
        .try_acquire(&auth.user.id)
        .ok_or_else(|| {
            ApiError::cooldown(
                "service_busy",
                "Too many event connections; close another connection and retry",
                1000,
                None,
            )
        })?;
    if *state.0.shutdown.borrow() {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "Server is stopping",
        ));
    }
    Ok(upgrade
        .max_message_size(16 * 1024)
        .max_frame_size(16 * 1024)
        .on_upgrade(move |socket| async move {
            let _permit = permit;
            events_socket(socket, state, auth, authority).await
        })
        .into_response())
}
async fn events_socket(
    mut socket: WebSocket,
    state: AppState,
    auth: Auth,
    mut authority: tokio::sync::watch::Receiver<()>,
) {
    if !state.writes_allowed() || authority.has_changed().unwrap_or(true) {
        let _ = send_event(&mut socket, WsMessage::Close(None)).await;
        return;
    }
    let mut events = state.0.events.subscribe();
    let mut heartbeat = tokio::time::interval(Duration::from_secs(15));
    let mut auth_check = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            biased;
            _=authority.changed()=>{let _=send_event(&mut socket,WsMessage::Close(None)).await;break;},
            _=state.shutdown_requested()=>{let _=send_event(&mut socket,WsMessage::Close(None)).await;break;},
            notice=events.recv()=>match notice {
                Ok(notice)=>{
                    let active=lock(&state.0.db).query_row("SELECT COUNT(*) FROM sessions WHERE token_hash=?1 AND expires>?2",params![auth.token_hash,now()],|r|r.get::<_,i64>(0)).unwrap_or(0)>0;
                    if !active {let _=send_event(&mut socket,WsMessage::Close(None)).await;break;}
                    if state.0.config.people && !people::event_visible(&lock(&state.0.db), &auth.user.id, &notice.event) { continue; }
                    if notice.audience.as_ref().is_some_and(|users|!users.contains(&auth.user.id)){continue;}
                    if notice.event.channel_id.as_ref().is_some_and(|channel|channel_access(&lock(&state.0.db),channel,&auth.user.id,READ).is_err()){continue;}
                    if notice.event.kind=="typing" && !chat_controls::typing_notice_valid(&state,&notice.event){continue;}
                    if let Ok(text)=serde_json::to_string(&notice.event){if !send_event(&mut socket,WsMessage::Text(text.into())).await{break;}}
                },
                Err(broadcast::error::RecvError::Lagged(_))=>{
                    if !send_event(&mut socket,WsMessage::Text("{\"kind\":\"refresh\"}".into())).await{break;}
                },
                Err(_)=>break,
            },
            input=socket.recv()=>match input {Some(Ok(WsMessage::Ping(bytes)))=>{if !send_event(&mut socket,WsMessage::Pong(bytes)).await{break;}},Some(Ok(WsMessage::Close(_)))|None|Some(Err(_))=>break,_=>{}},
            _=heartbeat.tick()=>{
                let active=lock(&state.0.db).query_row("SELECT COUNT(*) FROM sessions WHERE token_hash=?1 AND expires>?2",params![auth.token_hash,now()],|r|r.get::<_,i64>(0)).unwrap_or(0)>0;
                if !active {let _=send_event(&mut socket,WsMessage::Close(None)).await;break;}
                if !send_event(&mut socket,WsMessage::Ping(Bytes::new())).await{break;}
            },
            _=auth_check.tick()=>{
                let active=lock(&state.0.db).query_row("SELECT COUNT(*) FROM sessions WHERE token_hash=?1 AND expires>?2",params![auth.token_hash,now()],|r|r.get::<_,i64>(0)).unwrap_or(0)>0;
                if !active {let _=send_event(&mut socket,WsMessage::Close(None)).await;break;}
            },
        }
    }
}
async fn send_event(socket: &mut WebSocket, message: WsMessage) -> bool {
    matches!(
        tokio::time::timeout(Duration::from_secs(5), socket.send(message)).await,
        Ok(Ok(()))
    )
}

async fn voice_join(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<VoiceJoin>,
) -> ApiResult<Json<VoiceTicket>> {
    voice_recovery::join(&state, state.auth(&headers)?, input)
}

async fn voice_state(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(mut input): Json<VoiceState>,
) -> ApiResult<StatusCode> {
    let auth = state.auth(&headers)?;
    let db = lock(&state.0.db);
    let channel_id = {
        let mut sessions = lock(&state.0.voice);
        let session = sessions
            .values_mut()
            .find(|v| {
                v.user.id == auth.user.id && v.token_hash == auth.token_hash && v.alive_at(now())
            })
            .ok_or_else(forbidden)?;
        channel_access(&db, &session.channel_id, &auth.user.id, JOIN)?;
        if !conversations::stage_speaker(&db, &session.channel_id, &auth.user.id)? {
            input.muted = true;
        }
        if channel_access(&db, &session.channel_id, &auth.user.id, STREAM).is_err() {
            input.sharing = false;
            input.camera = false;
        }
        voice_recovery::save_state(&db, &auth, &input)?;
        session.state = input.clone();
        session.last_seen = now();
        session.channel_id.clone()
    };
    drop(db);
    lock(&state.0.frames).retain(|(owner, kind), _| {
        owner != &auth.user.id
            || (kind == "screen" && input.sharing)
            || (kind == "camera" && input.camera)
    });
    state.notify("voice", Some(channel_id), Some(auth.user.id), None);
    Ok(StatusCode::NO_CONTENT)
}
async fn voice_leave(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<StatusCode> {
    let auth = state.auth(&headers)?;
    state.remove_voice(&lock(&state.0.db), &auth.user.id, Some(&auth.token_hash))?;
    Ok(StatusCode::NO_CONTENT)
}

fn voice_channel(state: &AppState, auth: &Auth) -> ApiResult<String> {
    let channel = lock(&state.0.voice)
        .values()
        .find(|v| v.user.id == auth.user.id && v.token_hash == auth.token_hash && v.alive_at(now()))
        .map(|v| v.channel_id.clone())
        .ok_or_else(forbidden)?;
    channel_access(&lock(&state.0.db), &channel, &auth.user.id, JOIN)?;
    Ok(channel)
}
#[derive(Deserialize)]
struct FramesQuery {
    channel_id: String,
}
async fn media_frames(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<FramesQuery>,
) -> ApiResult<Json<Vec<MediaFrame>>> {
    let auth = state.auth(&headers)?;
    if voice_channel(&state, &auth)? != query.channel_id {
        return Err(forbidden());
    }
    let mut frames = lock(&state.0.frames);
    frames.retain(|_, v| now().saturating_sub(v.updated) < 15);
    Ok(Json(
        frames
            .values()
            .filter(|v| v.channel_id == query.channel_id)
            .map(|v| v.metadata.clone())
            .collect(),
    ))
}
#[derive(Deserialize)]
struct FrameQuery {
    kind: String,
    width: Option<u32>,
    height: Option<u32>,
}
async fn media_frame(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<FrameQuery>,
    bytes: Bytes,
) -> ApiResult<Json<MediaFrame>> {
    let auth = state.auth(&headers)?;
    let channel_id = voice_channel(&state, &auth)?;
    channel_access(&lock(&state.0.db), &channel_id, &auth.user.id, STREAM)?;
    let width = query.width.ok_or_else(|| bad("Frame width required"))?;
    let height = query.height.ok_or_else(|| bad("Frame height required"))?;
    if !matches!(query.kind.as_str(), "screen" | "camera")
        || !(1..=3840).contains(&width)
        || !(1..=2160).contains(&height)
    {
        return Err(bad("Invalid frame kind or dimensions"));
    }
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Frame exceeds 512 KiB",
        ));
    }
    if bytes.len() < 4 || !bytes.starts_with(&[0xff, 0xd8]) || !bytes.ends_with(&[0xff, 0xd9]) {
        return Err(bad("Expected a JPEG frame"));
    }
    let dimensions = image::ImageReader::with_format(
        std::io::Cursor::new(bytes.as_ref()),
        image::ImageFormat::Jpeg,
    )
    .into_dimensions()
    .map_err(|_| bad("Invalid JPEG header"))?;
    if dimensions != (width, height) {
        return Err(bad(
            "JPEG dimensions must match the bounded frame dimensions",
        ));
    }
    let frame_id = id();
    let metadata = MediaFrame {
        user: auth.user.clone(),
        kind: query.kind.clone(),
        url: format!("/api/media/frames/{frame_id}"),
        frame_id,
        width,
        height,
    };
    {
        // Membership and frame insertion share one lock order with join/leave.
        // A leave cannot finish and then have an in-flight publication reappear.
        let mut sessions = lock(&state.0.voice);
        let session = sessions
            .values_mut()
            .find(|v| {
                v.user.id == auth.user.id
                    && v.token_hash == auth.token_hash
                    && v.channel_id == channel_id
                    && v.alive_at(now())
            })
            .ok_or_else(forbidden)?;
        let mut frames = lock(&state.0.frames);
        frames.retain(|_, v| now().saturating_sub(v.updated) < 15);
        let frame_key = (auth.user.id.clone(), query.kind.clone());
        if frames.len() >= 128 && !frames.contains_key(&frame_key) {
            return Err(ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "Live media capacity reached",
            ));
        }
        frames.insert(
            frame_key,
            StoredFrame {
                metadata: metadata.clone(),
                channel_id: channel_id.clone(),
                bytes,
                updated: now(),
            },
        );
        // Publishing a frame starts the corresponding live indicator.
        if query.kind == "screen" {
            session.state.sharing = true;
        } else {
            session.state.camera = true;
        }
        session.last_seen = now();
    }
    Ok(Json(metadata))
}
async fn stop_media(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<FrameQuery>,
) -> ApiResult<StatusCode> {
    let auth = state.auth(&headers)?;
    let channel_id = voice_channel(&state, &auth)?;
    if !matches!(query.kind.as_str(), "screen" | "camera") {
        return Err(bad("Invalid frame kind"));
    }
    lock(&state.0.frames).remove(&(auth.user.id.clone(), query.kind.clone()));
    if let Some(session) = lock(&state.0.voice)
        .values_mut()
        .find(|v| v.user.id == auth.user.id && v.token_hash == auth.token_hash)
    {
        if query.kind == "screen" {
            session.state.sharing = false;
        } else {
            session.state.camera = false;
        }
    }
    state.notify("voice", Some(channel_id), Some(auth.user.id), None);
    Ok(StatusCode::NO_CONTENT)
}
async fn download_frame(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(frame_id): Path<String>,
) -> ApiResult<Response> {
    let auth = state.auth(&headers)?;
    let channel_id = voice_channel(&state, &auth)?;
    let frames = lock(&state.0.frames);
    let frame = frames
        .values()
        .find(|v| v.metadata.frame_id == frame_id && now().saturating_sub(v.updated) < 15)
        .ok_or_else(missing)?;
    if frame.channel_id != channel_id {
        return Err(forbidden());
    }
    let mut response = Body::from(frame.bytes.clone()).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("image/jpeg"),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    Ok(response)
}

// A stable address avoids the metadata/download race when capture replaces frames at 30 fps.
async fn download_live_frame(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((user_id, kind)): Path<(String, String)>,
) -> ApiResult<Response> {
    let auth = state.auth(&headers)?;
    if !matches!(kind.as_str(), "screen" | "camera") {
        return Err(bad("Invalid frame kind"));
    }
    let channel_id = voice_channel(&state, &auth)?;
    let frames = lock(&state.0.frames);
    let frame = frames
        .get(&(user_id, kind))
        .filter(|v| now().saturating_sub(v.updated) < 15)
        .ok_or_else(missing)?;
    if frame.channel_id != channel_id {
        return Err(forbidden());
    }
    let mut response = Body::from(frame.bytes.clone()).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("image/jpeg"),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    if let Ok(id) = axum::http::HeaderValue::from_str(&frame.metadata.frame_id) {
        response.headers_mut().insert("x-relay-frame-id", id);
    }
    Ok(response)
}

/// Run only on the community host. The token is never part of an API response.
pub async fn run_discord_bridge(state: AppState, token: Option<String>) {
    discord_bridge::run(state, token).await;
}

/// The replica retention pass runs once every this many 10-second cleanup ticks.
const REPLICA_MAINTENANCE_TICKS: u32 = 60;

pub async fn cleanup_loop(state: AppState) {
    let mut interval = tokio::time::interval(Duration::from_secs(10));
    let mut ticks = 0u32;
    loop {
        interval.tick().await;
        ticks = ticks.wrapping_add(1);
        let _ = state.flush_handover_media().await;
        if !state.writes_allowed() {
            // External media removals must still drain while database writes are
            // barred. Otherwise a freeze would leave LiveKit seats connected.
            let _ = state.flush_rtc_removals().await;
            continue;
        }
        // Persist an expired/private call's end and invalidate its exact leases
        // before the ordinary sweep removes runtime seats. Failed storage must
        // not silently discard signalling while its durable state is accepted.
        if private_calls::reconcile(&state, &lock(&state.0.db)).is_err() {
            continue;
        }
        let removed = {
            let mut sessions = lock(&state.0.voice);
            let old = sessions.len();
            sessions.retain(|_, v| v.alive_at(now()));
            old != sessions.len()
        };
        let users = lock(&state.0.voice)
            .values()
            .map(|v| v.user.id.clone())
            .collect::<Vec<_>>();
        lock(&state.0.frames)
            .retain(|(user, _), v| users.contains(user) && now().saturating_sub(v.updated) < 15);
        if removed {
            state.notify("voice", None, None, None);
        }
        state.enforce_rtc_permissions();
        let _ = state.flush_rtc_removals().await;
        if ticks % REPLICA_MAINTENANCE_TICKS == 0 {
            replica::service::maintain(&state);
        }
        let db = lock(&state.0.db);
        let _ = db.execute("DELETE FROM voice_resume_leases WHERE expires<=?1", [now()]);
        let _ = db.execute("DELETE FROM sessions WHERE expires<=?1", [now()]);
        let _ = db.execute("DELETE FROM invites WHERE expires<=?1", [now()]);
        let _ = db.execute(
            "DELETE FROM uploads WHERE message_id IS NULL AND created<?1",
            [now().saturating_sub(86400)],
        );
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod chat_control_tests;

#[cfg(test)]
mod network_ingress_tests {
    use super::*;
    use http_body_util::BodyExt;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tower::ServiceExt;

    const INGRESS: &str = "fixture-network-ingress-capability-distinct-from-control";
    const CONTROL: &str = "fixture-private-owner-control-proof";
    const PEER: &str = "100.64.0.7";

    fn state(data: &tempfile::TempDir) -> AppState {
        AppState::open_with_config(
            data.path(),
            "network ingress fixture",
            0,
            ServerConfig {
                local_control_token: Some(CONTROL.into()),
                trusted_proxy_ips: vec!["127.0.0.1".parse().unwrap()],
                ..Default::default()
            },
        )
        .unwrap()
    }

    fn request(method: &str, path: &str) -> axum::http::request::Builder {
        axum::http::Request::builder()
            .method(method)
            .uri(path)
            .header("host", "127.0.0.1:4580")
            .header("content-type", "application/json")
            .extension(ConnectInfo(
                "127.0.0.1:54321".parse::<SocketAddr>().unwrap(),
            ))
    }

    fn proof(request: axum::http::request::Builder) -> axum::http::request::Builder {
        request
            .header("x-relay-network-ingress", INGRESS)
            .header("x-relay-network-peer", PEER)
    }

    fn unpolled_body(polled: Arc<AtomicBool>) -> Body {
        Body::from_stream(futures_util::stream::poll_fn(move |_| {
            polled.store(true, Ordering::SeqCst);
            std::task::Poll::Ready(None::<Result<Bytes, std::io::Error>>)
        }))
    }

    #[tokio::test]
    async fn ingress_requires_its_own_capability_and_canonical_nonlocal_peer_before_body() {
        let data = tempfile::tempdir().unwrap();
        let state = state(&data);
        let ingress = network_ingress_router(state.clone(), INGRESS.into());
        for path in ["/api/health", "/api/events", "/api/voice/tunnel", "/rtc"] {
            let polled = Arc::new(AtomicBool::new(false));
            let response = ingress
                .clone()
                .oneshot(
                    request("POST", path)
                        .header("x-relay-network-peer", PEER)
                        .body(unpolled_body(polled.clone()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            assert!(!polled.load(Ordering::SeqCst));
        }
        for (token, peer) in [
            ("wrong-capability", PEER),
            (CONTROL, PEER),
            (INGRESS, "127.0.0.1"),
            (INGRESS, "100.128.0.1"),
            (INGRESS, "100.064.0.7"),
            (INGRESS, "100.64.0.7:4580"),
        ] {
            let response = ingress
                .clone()
                .oneshot(
                    request("GET", "/api/health")
                        .header("x-relay-network-ingress", token)
                        .header("x-relay-network-peer", peer)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }
        let response = ingress
            .clone()
            .oneshot(
                request("GET", "/api/health")
                    .header("x-relay-network-ingress", INGRESS)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let response = ingress
            .clone()
            .oneshot(
                proof(request("GET", "/api/health"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        assert!(!String::from_utf8_lossy(&bytes).contains(INGRESS));
        assert!(!String::from_utf8_lossy(&bytes).contains(CONTROL));
        let remote_request = proof(request("GET", "/api/health"))
            .extension(ConnectInfo(
                "100.64.0.9:54321".parse::<SocketAddr>().unwrap(),
            ))
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            ingress.oneshot(remote_request).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
        // These headers cannot create an ingress marker on the private owner router.
        assert_eq!(
            router(state)
                .oneshot(
                    request("GET", "/api/health")
                        .header("x-relay-network-ingress", "wrong")
                        .header("x-relay-network-peer", "127.0.0.1")
                        .body(Body::empty())
                        .unwrap()
                )
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn ingress_denies_all_private_paths_and_methods_before_reading_any_body() {
        let data = tempfile::tempdir().unwrap();
        let state = state(&data);
        let app = network_ingress_router(state.clone(), INGRESS.into());
        for path in [
            "/api/local",
            "/api/local/shutdown",
            "/api/local/discord-credentials",
            "/api/local/future-private-api",
            "/api/local/handover/locator-page",
            "/api/local/handover/session-locator-page",
        ] {
            for method in ["GET", "HEAD", "POST", "PUT", "DELETE", "PATCH", "OPTIONS"] {
                let polled = Arc::new(AtomicBool::new(false));
                let response = app
                    .clone()
                    .oneshot(
                        proof(request(method, path))
                            .header("authorization", format!("Bearer {CONTROL}"))
                            .header("x-relay-local-control", CONTROL)
                            .header("x-relay-instance-id", state.instance_id())
                            .header("x-relay-server-id", state.server_id().unwrap())
                            .body(unpolled_body(polled.clone()))
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::FORBIDDEN, "{method} {path}");
                assert!(!polled.load(Ordering::SeqCst));
            }
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(20), state.shutdown_requested())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn ingress_cannot_claim_first_owner_but_valid_invites_and_authentication_still_work() {
        let data = tempfile::tempdir().unwrap();
        let state = state(&data);
        let app = network_ingress_router(state.clone(), INGRESS.into());
        let body = serde_json::json!({"username":"NetworkFirstOwner","password":"fixture-network-password-123","invite":""});
        let response = app
            .clone()
            .oneshot(
                proof(request("POST", "/api/register"))
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(!state.has_users().unwrap());
        assert_eq!(
            lock(&state.0.db)
                .query_row("SELECT COUNT(*) FROM sessions", [], |row| row
                    .get::<_, u32>(0))
                .unwrap(),
            0
        );
        let owner = crate::tests::signup(&router(state.clone()), "FixtureOwner", "").await;
        let invite = crate::tests::new_invite(&router(state.clone()), &owner).await;
        let body = serde_json::json!({"username":"NetworkMember","password":"fixture-network-password-123","invite":invite});
        let response = app
            .clone()
            .oneshot(
                proof(request("POST", "/api/register"))
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let session: Session =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(session.user.role, "member");
        assert_eq!(
            app.clone()
                .oneshot(
                    proof(request("GET", "/api/bootstrap"))
                        .body(Body::empty())
                        .unwrap()
                )
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        let response = app
            .clone()
            .oneshot(
                proof(request("GET", "/api/bootstrap"))
                    .header("authorization", format!("Bearer {}", session.token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        assert!(!String::from_utf8_lossy(&bytes).contains(INGRESS));
        assert!(!String::from_utf8_lossy(&bytes).contains(CONTROL));
        let polled = Arc::new(AtomicBool::new(false));
        let response = app
            .oneshot(
                proof(request("POST", "/api/uploads"))
                    .body(unpolled_body(polled.clone()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(!polled.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn network_signin_rates_use_authenticated_peer_identity_and_do_not_lock_out_owner() {
        let data = tempfile::tempdir().unwrap();
        let state = state(&data);
        let app = network_ingress_router(state.clone(), INGRESS.into());
        for index in 0..16 {
            let response = app
                .clone()
                .oneshot(
                    proof(request("POST", "/api/login"))
                        .header("x-forwarded-for", format!("100.65.0.{}", index + 1))
                        .header("forwarded", "for=127.0.0.1")
                        .body(Body::from("{\"username\":\"\",\"password\":\"\"}"))
                        .unwrap(),
                )
                .await
                .unwrap();
            let expected = if index < 15 {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::TOO_MANY_REQUESTS
            };
            assert_eq!(response.status(), expected);
        }
        assert_eq!(
            app.oneshot(
                request("POST", "/api/login")
                    .header("x-relay-network-ingress", INGRESS)
                    .header("x-relay-network-peer", "100.64.0.8")
                    .body(Body::from("{\"username\":\"\",\"password\":\"\"}"))
                    .unwrap()
            )
            .await
            .unwrap()
            .status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            router(state.clone())
                .oneshot(
                    proof(request("POST", "/api/login"))
                        .body(Body::from("{\"username\":\"\",\"password\":\"\"}"))
                        .unwrap()
                )
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
        let rates = lock(&state.0.auth_rates);
        assert!(rates.contains_key("auth:network-ingress:100.64.0.7"));
        assert!(rates.contains_key("auth:network-ingress:100.64.0.8"));
        assert!(rates.contains_key("auth:127.0.0.1"));
        assert_eq!(rates.len(), 3);
    }
}
