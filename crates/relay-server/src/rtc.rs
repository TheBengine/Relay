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
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(3))
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
        Ok(Some(Self::new(url, api, key, secret)?))
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
    let room = format!("relay-{}-{}", server_id(&db)?, input.channel_id);
    let can_publish = channel_access(&db, &input.channel_id, &auth.user.id, STREAM).is_ok();
    let token=config.sign(&json!({"iss":config.api_key,"sub":auth.user.id,"nbf":now().saturating_sub(5),"exp":now()+60,"attributes":{"relay_session":auth.token_hash,"relay_channel":input.channel_id,"relay_server":server_id(&db)?},"video":{"room":room,"roomJoin":true,"canSubscribe":true,"canPublish":can_publish,"canPublishData":false,"canUpdateOwnMetadata":false,"canPublishSources":["camera","screen_share","screen_share_audio"]}}))?;
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
fn validate_claims(state: &AppState, claims: &Value) -> ApiResult<()> {
    let user = claims["sub"].as_str().ok_or_else(unauthorized)?;
    let session = claims["attributes"]["relay_session"]
        .as_str()
        .ok_or_else(unauthorized)?;
    let channel = claims["attributes"]["relay_channel"]
        .as_str()
        .ok_or_else(unauthorized)?;
    let db = lock(&state.0.db);
    if claims["attributes"]["relay_server"] != server_id(&db)?
        || claims["video"]["room"] != format!("relay-{}-{channel}", server_id(&db)?)
        || claims["video"]["roomJoin"] != true
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
    let result = config
        .client
        .get(format!("{}{}", config.api_url, uri))
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
pub async fn rtc_proxy(
    State(state): State<AppState>,
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> ApiResult<Response> {
    use tokio_tungstenite::tungstenite::{client::IntoClientRequest, protocol::WebSocketConfig};
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
    let upstream = format!(
        "{}{}",
        config
            .api_url
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
    Ok(ws
        .max_message_size(1024 * 1024)
        .max_frame_size(1024 * 1024)
        .on_upgrade(move |socket| async move {
            let _permit = permit;
            bridge(state, claims, socket, upstream).await
        }))
}
async fn bridge(
    state: AppState,
    claims: Value,
    mut socket: WebSocket,
    mut upstream: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message as Upstream;
    let mut check = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            _=state.shutdown_requested()=>break,
            client=socket.recv()=>{let Some(Ok(client))=client else{break;};let message=match client{WsMessage::Text(text)=>Upstream::Text(text.to_string().into()),WsMessage::Binary(bytes)=>Upstream::Binary(bytes),WsMessage::Ping(bytes)=>Upstream::Ping(bytes),WsMessage::Pong(bytes)=>Upstream::Pong(bytes),WsMessage::Close(_)=>break};
                if !matches!(tokio::time::timeout(Duration::from_secs(3),upstream.send(message)).await,Ok(Ok(()))){break;}}
            server=upstream.next()=>{let Some(Ok(server))=server else{break;};let message=match server{Upstream::Text(text)=>WsMessage::Text(text.to_string().into()),Upstream::Binary(bytes)=>WsMessage::Binary(bytes),Upstream::Ping(bytes)=>WsMessage::Ping(bytes),Upstream::Pong(bytes)=>WsMessage::Pong(bytes),Upstream::Close(_)=>break,Upstream::Frame(_)=>continue};
                if !matches!(tokio::time::timeout(Duration::from_secs(3),socket.send(message)).await,Ok(Ok(()))){break;}}
            _=check.tick()=>{
                let check_state=state.clone();let claims=claims.clone();
                let validation=tokio::task::spawn_blocking(move||validate_claims(&check_state,&claims));
                if !wait_for_validation(&state,validation).await {break;}
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
