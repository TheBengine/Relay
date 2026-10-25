//! Standby-only synthetic media. No copied database is opened by these routes.
use super::*;
use crate::ApiError;
use axum::http::{header, Uri};
use axum::{
    extract::{OriginalUri, WebSocketUpgrade},
    response::Response,
};
use std::sync::atomic::AtomicU8;

pub(super) struct Probe {
    body: model::RtcProbeBody,
    until: Instant,
    cancel: CancellationToken,
    signaling: AtomicU8,
    to_sfu: AtomicU8,
    from_sfu: AtomicU8,
}
impl Probe {
    pub(super) fn stop(&self) {
        self.cancel.cancel();
    }
    fn active(&self, s: &Standby) -> bool {
        !self.cancel.is_cancelled()
            && Instant::now() < self.until
            && !s.activating.load(Ordering::Acquire)
    }
}
pub(super) fn completed(s: &Standby, request: &model::ReadinessChallenge) -> bool {
    lock(&s.probe).as_ref().is_some_and(|p| {
        p.active(s)
            && &p.body.request == request
            && p.signaling.load(Ordering::Acquire) == 3
            && p.to_sfu.load(Ordering::Acquire) == 3
            && p.from_sfu.load(Ordering::Acquire) == 3
    })
}
fn unavailable() -> (StatusCode, Json<Value>) {
    refuse(
        StatusCode::SERVICE_UNAVAILABLE,
        "The destination's secure media route is not configured",
    )
}
pub(super) async fn ticket(
    State(s): State<Arc<Standby>>,
    Json(request): Json<model::ReadinessChallenge>,
) -> Reply<model::RtcProbeTicket> {
    if !challenge_matches(&s, &request) || s.activating.load(Ordering::Acquire) {
        return Err(refuse(
            StatusCode::CONFLICT,
            "Request a fresh signed destination check",
        ));
    }
    let config = s.rtc.as_ref().ok_or_else(unavailable)?;
    let port = config.media_port.ok_or_else(unavailable)?;
    let mut expected =
        reqwest::Url::parse(&s.receipt.body.target_url).map_err(|_| unavailable())?;
    let scheme=if expected.scheme()=="http"{"ws"}else{"wss"};
    expected.set_scheme(scheme).map_err(|_| unavailable())?;
    let public = reqwest::Url::parse(&config.url).map_err(|_| unavailable())?;
    if public != expected {
        return Err(unavailable());
    }
    let mut current = lock(&s.probe);
    if let Some(p) = current
        .as_ref()
        .filter(|p| p.active(&s) && p.body.request == request)
    {
        return Ok(Json(model::RtcProbeTicket {
            body: p.body.clone(),
            signature: s.key.sign(&model::rtc_probe_ticket_message(&p.body)),
        }));
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
    let expires = crate::now().saturating_add(30).min(request.expires_at);
    let seconds = expires.saturating_sub(crate::now());
    if seconds < 5 {
        return Err(refuse(
            StatusCode::CONFLICT,
            "Request a fresh destination check",
        ));
    }
    let room = format!(
        "relay-preflight-{}-{}",
        s.instance_id,
        sha(request.challenge.as_bytes())
    );
    let mut media_url = public.clone();
    media_url.set_path("/api/rtc/media");
    let make = || -> Result<relay_protocol::RtcTicket, (StatusCode, Json<Value>)> {
        let user = format!("relay-preflight-{}", uuid::Uuid::new_v4());
    let signal=config.sign(&json!({"iss":config.api_key,"sub":user,"nbf":crate::now().saturating_sub(5),"exp":expires,
        "purpose":"relay-handover-probe-signaling-v1","video":{"room":room,"roomJoin":true,"canPublish":true,
            "canSubscribe":true,"canPublishData":false,"canUpdateOwnMetadata":false,"canPublishSources":["camera","screen_share","screen_share_audio"]}})).map_err(|_|unavailable())?;
    let media=config.sign(&json!({"iss":config.api_key,"sub":user,"nbf":crate::now().saturating_sub(5),"exp":expires,
        "purpose":"relay-handover-probe-media-v1","nonce":request.challenge,"video":{"room":room,"roomJoin":false,"canPublish":false,"canSubscribe":false}})).map_err(|_|unavailable())?;
        Ok(relay_protocol::RtcTicket {
            url: config.url.clone(),
            token: signal,
            room: room.clone(),
            media_bridge: Some(relay_protocol::RtcMediaBridge {
                url: media_url.to_string(),
                peer_ip: "100.127.255.254".into(),
                peer_port: port,
                token: media,
            }),
        })
    };
    let rtc = [make()?, make()?];
    let body = model::RtcProbeBody {
        request,
        target: s.key.identity(),
        target_url: s.receipt.body.target_url.clone(),
        instance_id: s.instance_id.clone(),
        expires_at: expires,
        rtc,
    };
    if let Some(old) = current.take() {
        old.stop();
    }
    let signature = s.key.sign(&model::rtc_probe_ticket_message(&body));
    *current = Some(Arc::new(Probe {
        body: body.clone(),
        until: Instant::now() + Duration::from_secs(seconds),
        cancel: CancellationToken::new(),
        signaling: AtomicU8::new(0),
        to_sfu: AtomicU8::new(0),
        from_sfu: AtomicU8::new(0),
    }));
    Ok(Json(model::RtcProbeTicket { body, signature }))
}
fn denied() -> ApiError {
    ApiError::new(
        StatusCode::FORBIDDEN,
        "This standby probe is no longer authorized",
    )
}
fn admit(
    s: &Standby,
    headers: &HeaderMap,
    uri: &Uri,
    media: bool,
) -> Result<(Arc<Probe>, usize), ApiError> {
    if uri.to_string().len() > 16 * 1024
        || headers.contains_key(header::ORIGIN)
        || headers.contains_key("sec-fetch-site")
        || uri.query().is_some_and(|query| {
            media || url::form_urlencoded::parse(query.as_bytes()).any(|(k, _)| k == "access_token")
        })
    {
        return Err(denied());
    }
    let given = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(denied)?;
    let held = lock(&s.probe);
    let p = held.as_ref().filter(|p| p.active(s)).ok_or_else(denied)?;
    let index = p
        .body
        .rtc
        .iter()
        .position(|rtc| {
    let wanted = if media {
                rtc.media_bridge
            .as_ref()
                    .map(|v| v.token.as_str())
                    .unwrap_or("")
    } else {
                rtc.token.as_str()
    };
            given.len() == wanted.len() && bool::from(given.as_bytes().ct_eq(wanted.as_bytes()))
        })
        .ok_or_else(denied)?;
    Ok((p.clone(), index))
}
fn active(s: &Standby, p: &Arc<Probe>) -> bool {
    p.active(s)
        && lock(&s.probe)
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, p))
}
fn stopped(p: Arc<Probe>) -> impl std::future::Future<Output = ()> + Send {
    async move {
        tokio::select! {_=p.cancel.cancelled()=>{},_=tokio::time::sleep_until(p.until.into())=>{}}
    }
}
pub(super) async fn validate(
    State(s): State<Arc<Standby>>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> crate::ApiResult<Response> {
    let (p, index) = admit(&s, &headers, &uri, false)?;
    s.rtc
        .as_ref()
        .ok_or_else(denied)?
        .validate_signal(&uri, &p.body.rtc[index].token)
        .await
}
pub(super) async fn signal(
    State(s): State<Arc<Standby>>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> crate::ApiResult<Response> {
    let (p, index) = admit(&s, &headers, &uri, false)?;
    let permit = s.probe_signal.clone().try_acquire_owned().map_err(|_| {
        ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "Close another standby signaling probe",
        )
    })?;
    let upstream = s
        .rtc
        .as_ref()
        .ok_or_else(denied)?
        .connect_signal(&uri, &p.body.rtc[index].token)
        .await?;
    if !active(&s, &p) {
        return Err(denied());
    }
    p.signaling.fetch_or(1 << index, Ordering::AcqRel);
    Ok(ws
        .max_message_size(1024 * 1024)
        .max_frame_size(1024 * 1024)
        .on_upgrade(move |socket| async move {
            let _permit = permit;
            let checking = s.clone();
            let check_probe = p.clone();
            let valid = s.clone();
            let valid_probe = p.clone();
            crate::rtc::forward_signal(
                socket,
                upstream,
                move || {
                    let result = active(&checking, &check_probe);
                    async move { result }
                },
                move || active(&valid, &valid_probe),
                stopped(p),
            )
            .await;
        }))
}
pub(super) async fn media(
    State(s): State<Arc<Standby>>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> crate::ApiResult<Response> {
    let (p, index) = admit(&s, &headers, &uri, true)?;
    let permit = s.probe_media.clone().try_acquire_owned().map_err(|_| {
        ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "Close another standby media probe",
        )
    })?;
    let port = s
        .rtc
        .as_ref()
        .and_then(|c| c.media_port)
        .ok_or_else(denied)?;
    let udp = UdpSocket::bind("127.0.0.1:0").await.map_err(|_| denied())?;
    udp.connect((std::net::Ipv4Addr::LOCALHOST, port))
        .await
        .map_err(|_| denied())?;
    if !active(&s, &p) {
        return Err(denied());
    }
    Ok(ws
        .max_message_size(1602)
        .max_frame_size(1602)
        .on_upgrade(move |socket| async move {
            let _permit = permit;
            let checking = s.clone();
            let check_probe = p.clone();
            let valid = s.clone();
            let valid_probe = p.clone();
            let traffic = p.clone();
            crate::rtc_media::forward_packets(
                socket,
                udp,
                move || {
                    let result = active(&checking, &check_probe);
                    async move { result }
                },
                move || active(&valid, &valid_probe),
                move |outbound| {
                    if outbound {
                        traffic.to_sfu.fetch_or(1 << index, Ordering::AcqRel);
                    } else {
                        traffic.from_sfu.fetch_or(1 << index, Ordering::AcqRel);
                    }
                },
                stopped(p),
            )
            .await;
        }))
}
