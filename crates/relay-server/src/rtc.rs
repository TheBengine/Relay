//! Server-only LiveKit grants and explicit participant revocation.
use super::*;
use hmac::{Hmac, Mac};
use serde_json::{json, Value};

#[derive(Clone)]
pub struct RtcConfig {
    pub url: String,
    pub api_url: String,
    pub api_key: String,
    pub api_secret: String,
    pub(super) media_port: Option<u16>,
    client: reqwest::Client,
}
impl std::fmt::Debug for RtcConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RtcConfig")
            .field("url", &self.url)
            .field("api_url", &self.api_url)
            .finish_non_exhaustive()
    }
}
impl RtcConfig {
    pub fn new(
        url: String,
        api_url: String,
        api_key: String,
        api_secret: String,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let public = reqwest::Url::parse(&url)?;
        let api = reqwest::Url::parse(&api_url)?;
        if !matches!(public.scheme(), "ws" | "wss")
            || !matches!(api.scheme(), "http" | "https")
            || api_key.is_empty()
            || api_secret.len() < 16
            || !public.username().is_empty()
            || public.password().is_some()
            || !api.username().is_empty()
            || api.password().is_some()
        {
            return Err("Invalid RTC URLs or credentials (secret minimum16bytes)".into());
        }
        Ok(Self {
            url,
            api_url: api_url.trim_end_matches('/').to_owned(),
            api_key,
            api_secret,
            media_port: None,
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(3))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
        })
    }
    pub fn from_env() -> Result<Option<Self>, Box<dyn std::error::Error>> {
        let vars = [
            "RELAY_RTC_URL",
            "RELAY_RTC_API_URL",
            "RELAY_RTC_API_KEY",
            "RELAY_RTC_API_SECRET",
        ]
        .map(|name| std::env::var(name).unwrap_or_default());
        if vars.iter().all(String::is_empty) {
            return Ok(None);
        }
        let [url, mut api, key, secret] = vars;
        if api.is_empty() {
            api = url
                .replacen("wss://", "https://", 1)
                .replacen("ws://", "http://", 1);
        }
        let mut config = Self::new(url, api, key, secret)?;
        if let Ok(port) = std::env::var("RELAY_RTC_MEDIA_UDP_PORT") {
            config = config.with_media_bridge(port.parse()?)?;
        }
        Ok(Some(config))
    }
    /// Explicitly opt in only for an app-owned SFU on numeric loopback. The
    /// remote peer can never choose another UDP address or port.
    pub fn with_media_bridge(mut self, port: u16) -> Result<Self, Box<dyn std::error::Error>> {
        let api = reqwest::Url::parse(&self.api_url)?;
        let public = reqwest::Url::parse(&self.url)?;
        if port == 0
            || !api
                .host_str()
                .is_some_and(|h| matches!(h, "127.0.0.1" | "[::1]" | "::1"))
            || public.path() != "/"
            || public.query().is_some()
            || public.fragment().is_some()
            || !(public.scheme() == "wss" || private_signaling(&public)
                || cfg!(test) && public.scheme() == "ws" && public.host_str() == Some("127.0.0.1"))
        {
            return Err("Secure media requires the local SFU and its exact signaling origin".into());
        }
        self.media_port = Some(port);
        Ok(self)
    }
    fn media_bridge(&self, claims: &Value) -> ApiResult<Option<relay_protocol::RtcMediaBridge>> {
        let Some(port) = self.media_port else {
            return Ok(None);
        };
        let mut url = reqwest::Url::parse(&self.url).map_err(|_| unavailable())?;
        url.set_path("/api/rtc/media");
        let mut media = claims.clone();
        // An ongoing call may open another ICE allocation after the one-minute
        // signaling admission expires. This capability opens only the fixed
        // media route and is checked against live membership every second.
        media["exp"] = json!(now() + VOICE_LIFETIME);
        media["purpose"] = json!("relay-https-media-v1");
        media["video"]["roomJoin"] = json!(false);
        media["video"]["canSubscribe"] = json!(false);
        Ok(Some(relay_protocol::RtcMediaBridge {
            url: url.to_string(),
            peer_ip: "100.127.255.254".into(),
            peer_port: port,
            token: self.sign(&media)?,
        }))
    }
    pub(super) fn sign(&self, claims: &Value) -> ApiResult<String> {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"HS256","typ":"JWT"}"#);
        let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims)?);
        let signing = format!("{header}.{payload}");
        let mut mac = Hmac::<Sha256>::new_from_slice(self.api_secret.as_bytes())
            .map_err(|_| unavailable())?;
        mac.update(signing.as_bytes());
        Ok(format!(
            "{signing}.{}",
            URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
        ))
    }
    pub(super) fn verify(&self, token: &str) -> ApiResult<Value> {
        if token.len() > 16 * 1024 {
            return Err(unauthorized());
        }
        let parts = token.split('.').collect::<Vec<_>>();
        if parts.len() != 3 {
            return Err(unauthorized());
        }
        let header: Value = serde_json::from_slice(
            &URL_SAFE_NO_PAD
                .decode(parts[0])
                .map_err(|_| unauthorized())?,
        )
        .map_err(|_| unauthorized())?;
        if header["alg"] != "HS256" {
            return Err(unauthorized());
        }
        let mut mac = Hmac::<Sha256>::new_from_slice(self.api_secret.as_bytes())
            .map_err(|_| unavailable())?;
        mac.update(format!("{}.{}", parts[0], parts[1]).as_bytes());
        mac.verify_slice(
            &URL_SAFE_NO_PAD
                .decode(parts[2])
                .map_err(|_| unauthorized())?,
        )
        .map_err(|_| unauthorized())?;
        let claims: Value = serde_json::from_slice(
            &URL_SAFE_NO_PAD
                .decode(parts[1])
                .map_err(|_| unauthorized())?,
        )
        .map_err(|_| unauthorized())?;
        if claims["iss"] != self.api_key
            || claims["exp"].as_u64().is_none_or(|v| v <= now())
            || claims["nbf"].as_u64().is_none_or(|v| v > now() + 5)
        {
            return Err(unauthorized());
        }
        Ok(claims)
    }
    async fn remove(&self, room: &str, identity: &str) -> ApiResult<()> {
        let token=self.sign(&json!({"iss":self.api_key,"nbf":now().saturating_sub(5),"exp":now()+30,"video":{"room":room,"roomAdmin":true}}))?;
        let result = self
            .client
            .post(format!(
                "{}/twirp/livekit.RoomService/RemoveParticipant",
                self.api_url
            ))
            .bearer_auth(token)
            .json(&json!({"room":room,"identity":identity}))
            .send()
            .await
            .map_err(|_| unavailable())?;
        if result.status().is_success() || result.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(());
        }
        Err(unavailable())
    }
}

pub(super) fn private_signaling(url:&reqwest::Url)->bool{
    url.scheme()=="ws"&&url.port()==Some(4580)&&url.username().is_empty()
        &&url.password().is_none()&&url.path()=="/"&&url.query().is_none()&&url.fragment().is_none()
        &&url.host_str().and_then(|h|h.strip_suffix(".iroh")).is_some_and(|key|
            key.len()==64&&key.bytes().all(|b|b.is_ascii_digit()||(b'a'..=b'f').contains(&b)))
}
fn unavailable() -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "Video server is unavailable; retry shortly",
    )
}
#[derive(Clone)]
pub(super) struct RtcMembership {
    pub channel_id: String,
    pub room: String,
    pub token_hash: String,
    pub can_publish: bool,
}
impl AppState {
    pub(super) fn revoke_rtc(&self, user: &str, token: Option<&str>) {
        let mut members = lock(&self.0.rtc_members);
        if members
            .get(user)
            .is_some_and(|m| token.is_none_or(|t| t == m.token_hash))
        {
            if let Some(m) = members.remove(user) {
                lock(&self.0.rtc_removals).insert((m.room, user.to_owned()), ());
            }
        }
    }
    pub(super) fn revoke_all_rtc(&self) {
        let members = std::mem::take(&mut *lock(&self.0.rtc_members));
        let mut removals = lock(&self.0.rtc_removals);
        for (user, m) in members {
            removals.insert((m.room, user), ());
        }
    }
    pub(super) fn enforce_rtc_permissions(&self) {
        let revoked = {
            let db = lock(&self.0.db);
            let members = lock(&self.0.rtc_members);
            let voice = lock(&self.0.voice);
            members
                .iter()
                .filter(|(user, m)| {
                    channel_access(&db, &m.channel_id, user, JOIN).is_err()
                        || db
                            .query_row(
                                "SELECT COUNT(*) FROM sessions WHERE token_hash=?1 AND expires>?2",
                                params![m.token_hash, now()],
                                |r| r.get::<_, u32>(0),
                            )
                            .unwrap_or(0)
                            == 0
                        || !voice.values().any(|v| {
                            v.user.id == **user
                                && v.token_hash == m.token_hash
                                && v.channel_id == m.channel_id
                                && v.alive_at(now())
                        })
                        || (m.can_publish
                            && channel_access(&db, &m.channel_id, user, STREAM).is_err())
                })
                .map(|(u, _)| u.clone())
                .collect::<Vec<_>>()
        };
        for user in revoked {
            self.revoke_rtc(&user, None);
        }
    }
    pub(super) async fn flush_rtc_removals(&self) -> ApiResult<()> {
        let Some(config) = &self.0.config.rtc else {
            return Ok(());
        };
        let removals = lock(&self.0.rtc_removals)
            .keys()
            .take(8)
            .cloned()
            .collect::<Vec<_>>();
        let mut jobs = tokio::task::JoinSet::new();
        for (room, user) in removals {
            let config = config.clone();
            jobs.spawn(async move {
                let success = config.remove(&room, &user).await.is_ok();
                (room, user, success)
            });
        }
        while let Some(result) = jobs.join_next().await {
            if let Ok((room, user, true)) = result {
                lock(&self.0.rtc_removals).remove(&(room, user));
            }
        }
        if lock(&self.0.rtc_removals).is_empty() {
            Ok(())
        } else {
            Err(unavailable())
        }
    }
}
pub async fn rtc_token(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<VoiceJoin>,
) -> ApiResult<Json<RtcTicket>> {
    let auth = state.auth(&headers)?;
    let config = state.0.config.rtc.as_ref().ok_or_else(unavailable)?;
    state.flush_rtc_removals().await?;
    let db = lock(&state.0.db);
    channel_access(&db, &input.channel_id, &auth.user.id, JOIN)?;
    if !lock(&state.0.voice).values().any(|v| {
        v.user.id == auth.user.id
            && v.token_hash == auth.token_hash
            && v.channel_id == input.channel_id
            && v.alive_at(now())
    }) {
        return Err(forbidden());
    }
    let room = state.0.handover.media_room(&input.channel_id);
    let can_publish = channel_access(&db, &input.channel_id, &auth.user.id, STREAM).is_ok();
    let claims = json!({"iss":config.api_key,"sub":auth.user.id,"nbf":now().saturating_sub(5),"exp":now()+60,"attributes":{"relay_session":auth.token_hash,"relay_channel":input.channel_id,"relay_server":server_id(&db)?},"video":{"room":room,"roomJoin":true,"canSubscribe":true,"canPublish":can_publish,"canPublishData":false,"canUpdateOwnMetadata":false,"canPublishSources":["camera","screen_share","screen_share_audio"]}});
    let token = config.sign(&claims)?;
    let media_bridge = config.media_bridge(&claims)?;
    lock(&state.0.rtc_members).insert(
        auth.user.id,
        RtcMembership {
            channel_id: input.channel_id,
            room: room.clone(),
            token_hash: auth.token_hash,
            can_publish,
        },
    );
    Ok(Json(RtcTicket {
        url: config.url.clone(),
        token,
        room,
        media_bridge,
    }))
}
#[derive(Deserialize)]
pub struct RtcAuthorizeQuery {
    access_token: Option<String>,
}
/// For a trusted reverse proxy's forward-auth subrequest. This validates the SFU JWT,
/// not a Relay bearer token; cached and refreshed JWTs must still have a live session.
pub async fn rtc_authorize(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<RtcAuthorizeQuery>,
) -> ApiResult<StatusCode> {
    let config = state.0.config.rtc.as_ref().ok_or_else(unavailable)?;
    let forwarded = headers
        .get("x-forwarded-uri")
        .and_then(|h| h.to_str().ok())
        .and_then(|uri| reqwest::Url::parse(&format!("http://localhost{uri}")).ok())
        .and_then(|url| {
            url.query_pairs()
                .find(|(key, _)| key == "access_token")
                .map(|(_, v)| v.into_owned())
        });
    let token = query
        .access_token
        .or(forwarded)
        .or_else(|| {
            headers
                .get(header::AUTHORIZATION)
                .and_then(|h| h.to_str().ok())
                .and_then(|s| s.strip_prefix("Bearer "))
                .map(str::to_owned)
        })
        .ok_or_else(unauthorized)?;
    let claims = config.verify(&token)?;
    validate_claims(&state, &claims)?;
    Ok(StatusCode::OK)
}
pub(super) fn validate_claims(state: &AppState, claims: &Value) -> ApiResult<()> {
    if claims["video"]["roomJoin"] != true || !claims["purpose"].is_null() {
        return Err(forbidden());
    }
    validate_membership(state, claims)
}
pub(super) fn validate_media_claims(state: &AppState, claims: &Value) -> ApiResult<()> {
    if claims["purpose"] != "relay-https-media-v1" || claims["video"]["roomJoin"] != false {
        return Err(forbidden());
    }
    validate_membership(state, claims)
}
fn validate_membership(state: &AppState, claims: &Value) -> ApiResult<()> {
    if !state.0.handover.writes_allowed() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "This host is paused for handover",
        ));
    }
    let user = claims["sub"].as_str().ok_or_else(unauthorized)?;
    let session = claims["attributes"]["relay_session"]
        .as_str()
        .ok_or_else(unauthorized)?;
    let channel = claims["attributes"]["relay_channel"]
        .as_str()
        .ok_or_else(unauthorized)?;
    let db = lock(&state.0.db);
    if claims["attributes"]["relay_server"] != server_id(&db)?
        || claims["video"]["room"] != state.0.handover.media_room(channel)
    {
        return Err(forbidden());
    }
    if db.query_row(
        "SELECT COUNT(*) FROM sessions WHERE token_hash=?1 AND user_id=?2 AND expires>?3",
        params![session, user, now()],
        |r| r.get::<_, u32>(0),
    )? != 1
    {
        return Err(unauthorized());
    }
    channel_access(&db, channel, user, JOIN)?;
    if claims["video"]["canPublish"] == true {
        channel_access(&db, channel, user, STREAM)?;
    }
    if !lock(&state.0.voice).values().any(|v| {
        v.user.id == user && v.token_hash == session && v.channel_id == channel && v.alive_at(now())
    }) {
        return Err(forbidden());
    }
    Ok(())
}

fn request_token(uri: &axum::http::Uri, headers: &HeaderMap) -> ApiResult<String> {
    if uri.to_string().len() > 64 * 1024 {
        return Err(bad("Signaling request is too large"));
    }
    let query = reqwest::Url::parse(&format!("http://localhost{uri}"))
        .map_err(|_| bad("Invalid signaling request"))?;
    query
        .query_pairs()
        .find(|(k, _)| k == "access_token")
        .map(|(_, v)| v.into_owned())
        .or_else(|| {
            headers
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
                .map(str::to_owned)
        })
        .ok_or_else(unauthorized)
}
pub async fn rtc_validate(
    State(state): State<AppState>,
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let config = state.0.config.rtc.as_ref().ok_or_else(unavailable)?;
    let token = request_token(&uri, &headers)?;
    validate_claims(&state, &config.verify(&token)?)?;
    config.validate_signal(&uri, &token).await
}
pub async fn rtc_proxy(
    State(state): State<AppState>,
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> ApiResult<Response> {
    let authority = state.0.handover.watch_transports();
    let config = state.0.config.rtc.as_ref().ok_or_else(unavailable)?;
    let token = request_token(&uri, &headers)?;
    let claims = config.verify(&token)?;
    validate_claims(&state, &claims)?;
    let user = claims["sub"].as_str().ok_or_else(unauthorized)?;
    let permit = state.0.rtc_connections.try_acquire(user).ok_or_else(|| {
        ApiError::cooldown(
            "service_busy",
            "Too many video signaling connections; close another connection and retry",
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
    let upstream = config.connect_signal(&uri, &token).await?;
    Ok(ws
        .max_message_size(1024 * 1024)
        .max_frame_size(1024 * 1024)
        .on_upgrade(move |socket| async move {
            let _permit = permit;
            bridge(state, claims, socket, upstream, authority).await
        }))
}
pub(super) type SignalSocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;
impl RtcConfig {
    pub(super) async fn validate_signal(
        &self,
        uri: &axum::http::Uri,
        token: &str,
    ) -> ApiResult<Response> {
        let result = self
            .client
            .get(format!("{}{}", self.api_url, uri))
            .bearer_auth(token)
            .send()
            .await
            .map_err(|_| unavailable())?;
        let status = StatusCode::from_u16(result.status().as_u16()).map_err(|_| unavailable())?;
        if !status.is_success() {
            return Err(ApiError::new(status, "Video signaling validation failed"));
        }
        Ok("success".into_response())
    }
    pub(super) async fn connect_signal(
        &self,
        uri: &axum::http::Uri,
        token: &str,
    ) -> ApiResult<SignalSocket> {
        use tokio_tungstenite::tungstenite::{
            client::IntoClientRequest, protocol::WebSocketConfig,
        };
    let upstream = format!(
        "{}{}",
            self.api_url
            .replacen("https://", "wss://", 1)
            .replacen("http://", "ws://", 1),
        uri
    );
    let mut request = upstream
        .into_client_request()
        .map_err(|_| bad("Invalid video server URL"))?;
    request.headers_mut().insert(
        header::AUTHORIZATION,
        format!("Bearer {token}")
            .parse()
            .map_err(|_| unauthorized())?,
    );
    let websocket_config = WebSocketConfig::default()
        .max_message_size(Some(1024 * 1024))
        .max_frame_size(Some(1024 * 1024));
    let (upstream, _) = tokio::time::timeout(
        Duration::from_secs(5),
        tokio_tungstenite::connect_async_with_config(request, Some(websocket_config), false),
    )
    .await
    .map_err(|_| unavailable())?
    .map_err(|_| unavailable())?;
        Ok(upstream)
    }
}
async fn bridge(
    state: AppState,
    claims: Value,
    socket: WebSocket,
    upstream: SignalSocket,
    mut authority: tokio::sync::watch::Receiver<()>,
) {
    let stopped = state.clone();
    let active = state.clone();
    let checking = state.clone();
    forward_signal(
        socket,
        upstream,
        move || {
            let state = checking.clone();
            let claims = claims.clone();
            async move {
                let checked = state.clone();
                let job = tokio::task::spawn_blocking(move || validate_claims(&checked, &claims));
                wait_for_validation(&state, job).await
            }
        },
        move || active.0.handover.writes_allowed() && !*active.0.shutdown.borrow(),
        async move {
            tokio::select! {_=authority.changed()=>{},_=stopped.shutdown_requested()=>{}}
        },
    )
    .await;
}
pub(super) async fn forward_signal<C, CF, A, S>(
    mut socket: WebSocket,
    mut upstream: SignalSocket,
    validate: C,
    active: A,
    stopped: S,
) where
    C: Fn() -> CF + Send + Sync,
    CF: std::future::Future<Output = bool> + Send,
    A: Fn() -> bool + Send + Sync,
    S: std::future::Future<Output = ()> + Send,
{
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message as Upstream;
    tokio::pin!(stopped);
    let mut check = tokio::time::interval(Duration::from_secs(1));
    loop {
        if !active() {
            break;
        }
        tokio::select! {
            biased;
            _=&mut stopped=>break,
            client=socket.recv()=>{let Some(Ok(client))=client else{break;};let message=match client{WsMessage::Text(text)=>Upstream::Text(text.to_string().into()),WsMessage::Binary(bytes)=>Upstream::Binary(bytes),WsMessage::Ping(bytes)=>Upstream::Ping(bytes),WsMessage::Pong(bytes)=>Upstream::Pong(bytes),WsMessage::Close(_)=>break};
                if !matches!(tokio::time::timeout(Duration::from_secs(3),upstream.send(message)).await,Ok(Ok(()))){break;}}
            server=upstream.next()=>{let Some(Ok(server))=server else{break;};let message=match server{Upstream::Text(text)=>WsMessage::Text(text.to_string().into()),Upstream::Binary(bytes)=>WsMessage::Binary(bytes),Upstream::Ping(bytes)=>WsMessage::Ping(bytes),Upstream::Pong(bytes)=>WsMessage::Pong(bytes),Upstream::Close(_)=>break,Upstream::Frame(_)=>continue};
                if !matches!(tokio::time::timeout(Duration::from_secs(3),socket.send(message)).await,Ok(Ok(()))){break;}}
            _=check.tick()=>{
                let valid=tokio::select!{biased;_=&mut stopped=>false,
                    result=tokio::time::timeout(Duration::from_secs(3),validate())=>matches!(result,Ok(true))};
                if !valid{break;}
            }
        }
    }
    let _ = tokio::time::timeout(Duration::from_secs(1), socket.send(WsMessage::Close(None))).await;
    let _ = tokio::time::timeout(Duration::from_secs(1), upstream.close(None)).await;
}

/// A blocking DB authorization check cannot be forcibly cancelled, but it must
/// never keep signaling or its admission permit alive after shutdown/deadline.
/// Its eventual result is discarded on either path; neither assumes access.
pub(super) async fn wait_for_validation(
    state: &AppState,
    validation: tokio::task::JoinHandle<ApiResult<()>>,
) -> bool {
    tokio::select! {
        biased;
        _=state.shutdown_requested()=>false,
        result=tokio::time::timeout(Duration::from_secs(3),validation)=>matches!(result,Ok(Ok(Ok(())))),
    }
}

impl RtcConfig {
    pub(crate) fn handover_binding(&self) -> String {
        let mut hash = Sha256::new();
        hash.update(b"relay-handover-sfu-binding-v1\0");
        for value in [&self.api_url, &self.api_key, &self.api_secret] {
            hash.update((value.len() as u64).to_be_bytes());
            hash.update(value.as_bytes());
        }
        hash.finalize().iter().map(|v| format!("{v:02x}")).collect()
    }

    /// Checks the destination's own administrative connection without changing
    /// rooms, producing a media token, or touching capture/playback devices.
    pub(crate) async fn handover_ready(&self) -> ApiResult<()> {
        let prefix = format!("relay-preflight-{}-", Uuid::new_v4());
        self.rooms_in_prefix(&prefix, false).await.map(|_| ())
    }

    async fn rooms_in_prefix(&self, prefix: &str, legacy: bool) -> ApiResult<Vec<String>> {
        let token = self.sign(&json!({"iss":self.api_key,"nbf":now().saturating_sub(5),"exp":now()+30,"video":{"roomList":true}}))?;
        let mut response = self
            .client
            .post(format!(
                "{}/twirp/livekit.RoomService/ListRooms",
                self.api_url
            ))
            .bearer_auth(token)
            .json(&json!({}))
            .send()
            .await
            .map_err(|_| unavailable())?;
        if !response.status().is_success() {
            return Err(unavailable());
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| unavailable())? {
            if body.len() + chunk.len() > 1024 * 1024 {
                return Err(unavailable());
            }
            body.extend_from_slice(&chunk);
        }
        #[derive(Deserialize)]
        struct Room {
            name: String,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Rooms {
            #[serde(default)]
            rooms: Vec<Room>,
        }
        let list: Rooms = serde_json::from_slice(&body).map_err(|_| unavailable())?;
        if list.rooms.len() > 4096 {
            return Err(unavailable());
        }
        let mut names = Vec::new();
        for room in list.rooms {
            let Some(channel) = room.name.strip_prefix(prefix) else {
                continue;
            };
            if Uuid::parse_str(channel).is_ok_and(|id| id.to_string() == channel) {
                names.push(room.name);
            } else if !legacy {
                return Err(unavailable());
            }
        }
        Ok(names)
    }
    pub(crate) async fn drain_prefix(&self, prefix: &str, legacy: bool) -> ApiResult<()> {
        let mut jobs = tokio::task::JoinSet::new();
        for room in self
            .rooms_in_prefix(prefix, legacy)
            .await?
            .into_iter()
            .take(8)
        {
            let config = self.clone();
            jobs.spawn(async move {
                let token = config.sign(&json!({"iss":config.api_key,"nbf":now().saturating_sub(5),"exp":now()+30,"video":{"roomCreate":true}}))?;
                let response = config.client.post(format!("{}/twirp/livekit.RoomService/DeleteRoom", config.api_url))
                    .bearer_auth(token).json(&json!({"room":room})).send().await.map_err(|_| unavailable())?;
                if response.status().is_success() || response.status() == reqwest::StatusCode::NOT_FOUND { Ok(()) } else { Err(unavailable()) }
            });
        }
        while let Some(result) = jobs.join_next().await {
            result.map_err(|_| unavailable())??;
        }
        if self.rooms_in_prefix(prefix, legacy).await?.is_empty() {
            Ok(())
        } else {
            Err(unavailable())
        }
    }
}
