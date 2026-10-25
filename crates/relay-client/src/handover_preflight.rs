//! A fresh destination check never forwards the source account bearer.
use super::*;

fn clock() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
pub(super) struct Destination {
    base: String,
    http: reqwest::blocking::Client,
    private: Option<crate::iroh_route::PreparedRoute>,
}
impl Destination {
    pub(super) fn new(status: &model::Status) -> Result<Self> {
        let p = status.preparation.as_ref().ok_or(Fault::Changed)?;
        let base =
            crate::host_route::safe_destination(&p.target_url).map_err(|_| Fault::Binding)?;
        let private=if crate::iroh_route::is_iroh(&base){
            validate_destination_status(status)?;
            let route=status.destination.as_ref().and_then(|c|c.body.private_route.as_ref()).ok_or(Fault::Binding)?;
            let saved=crate::iroh_route::SavedIroh{endpoint_id:route.endpoint_id.clone(),relay:route.relay.clone(),token:route.token.clone()};
            Some(crate::iroh_route::PreparedRoute::open(&saved,&status.server_id).map_err(|_|Fault::Route)?)
        }else if url::Url::parse(&base).map_err(|_|Fault::Binding)?.scheme()=="https"{None}
        else{return Err(Fault::Route)};
        Ok(Self {
            base,
            private,
            http: reqwest::blocking::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(3))
                .timeout(Duration::from_secs(5))
                .build()
                .map_err(|_| Fault::Connection)?,
        })
    }
    fn post<T: DeserializeOwned>(
        &self,
        path: &str,
        request: &impl Serialize,
        cancel: &AtomicBool,
    ) -> Result<T> {
        stopped(cancel)?;
        let mut reply = self
            .http
            .post(format!("{}{path}", self.private.as_ref().map(|r|r.gateway().map_err(|_|Fault::Route)).transpose()?.as_deref().unwrap_or(&self.base).trim_end_matches('/')))
            .json(request)
            .send()
            .map_err(|_| Fault::Connection)?;
        if !reply.status().is_success() {
            return Err(Fault::Media);
        }
        let mut bytes = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            stopped(cancel)?;
            let n = reply.read(&mut chunk).map_err(|_| Fault::Connection)?;
            if n == 0 {
                break;
            }
            if bytes.len() + n > 64 * 1024 {
                return Err(Fault::Integrity);
            }
            bytes.extend_from_slice(&chunk[..n]);
        }
        serde_json::from_slice(&bytes).map_err(|_| Fault::Integrity)
    }
}
fn request_matches(
    s: &model::Status,
    pin: &model::RoutePin,
    r: &model::ReadinessChallenge,
) -> bool {
    let Some(p) = &s.preparation else {
        return false;
    };
    let Some(receipt) = &s.received else {
        return false;
    };
    r.source == *pin
        && r.source_host_id == s.source_host_id
        && r.preparation_id == p.id
        && r.receipt_sha256 == sha(&serde_json::to_vec(receipt).unwrap_or_default())
        && r.expires_at > clock()
        && r.expires_at <= clock().saturating_add(70)
        && r.challenge.len() == 43
        && URL_SAFE_NO_PAD
            .decode(&r.challenge)
            .is_ok_and(|v| v.len() == 32)
        && crate::host_route::verify(
            &pin.host,
            &r.signature,
            &model::readiness_challenge_message(r),
        )
}
fn ready_matches(
    s: &model::Status,
    r: &model::ReadinessChallenge,
    proof: &model::ReadyProof,
) -> bool {
    let Some(p) = &s.preparation else {
        return false;
    };
    let b = &proof.body;
    b.request == *r
        && s.target_identity.as_ref() == Some(&b.target)
        && b.target_url == p.target_url
        && uuid(&b.instance_id)
        && b.http_port != 0
        && b.voice_port != 0
        && crate::host_route::verify(&b.target, &proof.signature, &model::ready_message(b))
}
fn ticket_matches(
    s: &model::Status,
    r: &model::ReadinessChallenge,
    initial: &model::ReadyProof,
    ticket: &model::RtcProbeTicket,
) -> bool {
    let b = &ticket.body;
    let Some(p) = &s.preparation else {
        return false;
    };
    if b.request != *r
        || Some(&b.target) != s.target_identity.as_ref()
        || b.target_url != p.target_url
        || b.instance_id != initial.body.instance_id
        || b.expires_at <= clock() + 5
        || b.expires_at > clock() + 30
        || b.expires_at > r.expires_at
        || !crate::host_route::verify(
            &b.target,
            &ticket.signature,
            &model::rtc_probe_ticket_message(b),
        )
    {
        return false;
    }
    let Ok(mut expected) = url::Url::parse(&p.target_url) else {
        return false;
    };
    let scheme=if crate::iroh_route::is_iroh(&p.target_url){"ws"}else{"wss"};
    if expected.set_scheme(scheme).is_err() {
        return false;
    }
    let room = format!(
        "relay-preflight-{}-{}",
        b.instance_id,
        sha(r.challenge.as_bytes())
    );
    b.rtc.iter().all(|rtc| {
        let Ok(signal) = url::Url::parse(&rtc.url) else {
            return false;
        };
        let Some(media) = &rtc.media_bridge else {
            return false;
        };
        let Ok(url) = url::Url::parse(&media.url) else {
            return false;
        };
        signal == expected
            && rtc.room == room
            && !rtc.token.is_empty()
            && rtc.token.len() <= 16_384
            && url.origin() == expected.origin()
            && url.path() == "/api/rtc/media"
            && url.query().is_none()
            && url.fragment().is_none()
            && url.username().is_empty()
            && url.password().is_none()
            && media.peer_ip == "100.127.255.254"
            && media.peer_port != 0
            && !media.token.is_empty()
            && media.token.len() <= 16_384
    }) && b.rtc[0].token != b.rtc[1].token
}
pub(super) fn check(
    remote: &mut impl Remote,
    s: &model::Status,
    cancel: &AtomicBool,
) -> Result<model::ReadyProof> {
    let destination = Destination::new(s)?;
    let request: model::ReadinessChallenge =
        remote.json(true, &format!("{ROOT}/readiness"), &change(s)?, 16 * 1024)?;
    if !request_matches(s, remote.pin(), &request) {
        return Err(Fault::Binding);
    }
    let initial: model::ReadyProof =
        destination.post(&format!("{ROOT}/target"), &request, cancel)?;
    if !ready_matches(s, &request, &initial) {
        return Err(Fault::Binding);
    }
    if !initial.body.rtc_ready {
        return Ok(initial);
    }
    let ticket: model::RtcProbeTicket =
        destination.post(&format!("{ROOT}/rtc-ticket"), &request, cancel)?;
    if !ticket_matches(s, &request, &initial, &ticket) {
        return Err(Fault::Binding);
    }
    stopped(cancel)?;
    #[cfg(feature = "native-rtc")]
    crate::rtc::handover_probe::check(ticket.body.rtc, cancel, destination.private.as_ref()).map_err(|_| {
        if cancel.load(Ordering::Acquire) {
            Fault::Cancelled
        } else {
            Fault::Media
        }
    })?;
    #[cfg(not(feature = "native-rtc"))]
    return Err(Fault::Media);
    stopped(cancel)?;
    current(remote, s)?;
    let proof: model::ReadyProof = destination.post(&format!("{ROOT}/target"), &request, cancel)?;
    if !ready_matches(s, &request, &proof)
        || proof.body.instance_id != initial.body.instance_id
        || proof.body.rtc_media_ready != Some(true)
        || request.expires_at <= clock()
    {
        return Err(Fault::Media);
    }
    Ok(proof)
}
pub(super) fn finish(
    remote: &mut impl Remote,
    s: &model::Status,
    proof: model::ReadyProof,
) -> Result<model::Status> {
    let grant: model::ActivationGrant = remote
        .json(
            true,
            &format!("{ROOT}/retire"),
            &model::Retire {
                change: change(s)?,
                confirm_stop_source: true,
                readiness: Some(proof),
            },
            32 * 1024,
        )
        .map_err(|_| Fault::GrantUncertain)?;
    if s.received.as_ref() != Some(&grant.receipt)
        || !crate::host_route::valid_grant(remote.pin(), &grant)
        || !crate::host_route::committed_history(&grant,&s.history)
    {
        return Err(Fault::GrantUncertain);
    }
    let mut done = s.clone();
    done.retired = Some(grant);
    done.activation_available = true;
    done.writes_allowed = false;
    Ok(done)
}
pub(super) fn deliver(s: &model::Status) -> Result<()> {
    let grant = s.retired.as_ref().ok_or(Fault::Binding)?;
    let destination = Destination::new(s)?;
    let continue_owned_work = AtomicBool::new(false);
    if destination
        .post::<serde_json::Value>(&format!("{ROOT}/grant"), grant, &continue_owned_work)
        .is_ok_and(|reply| reply["activating"] == true)
    {
        return Ok(());
    }
    // A successful activation can switch routers before its acknowledgement
    // reaches us. Prove the exact new authority rather than replaying a write.
    let mut random=[0u8;32];getrandom::getrandom(&mut random).map_err(|_|Fault::DestinationPending)?;
    let nonce=URL_SAFE_NO_PAD.encode(random);
    let proof: model::RouteProof = destination
        .post(
            &format!("{ROOT}/route"),
            &model::RouteChallenge {
                challenge: nonce.clone(),
            },
            &continue_owned_work,
        )
        .map_err(|_| Fault::DestinationPending)?;
    let expected = model::RoutePin {
        server_id: s.server_id.clone(),
        host: grant.receipt.body.target_identity.clone(),
        term: grant.next_term,
    };
    if proof.state.pin != expected
        || proof.state.challenge != nonce
        || !proof.state.writes_allowed
        || proof.state.moved.is_some()
        || !crate::host_route::verify(
            &expected.host,
            &proof.signature,
            &model::route_message(&proof.state),
        )
    {
        return Err(Fault::DestinationPending);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    fn sign(key: &SigningKey, bytes: Vec<u8>) -> String {
        URL_SAFE_NO_PAD.encode(key.sign(&bytes).to_bytes())
    }
    #[test]
    fn a_signed_probe_cannot_change_the_origin_image_instance_or_deadline() {
        let (source, target, pin, grant) = crate::host_route::tests::fixture();
        let mut s = crate::handover_runtime::tests::recipe().status;
        let b = &grant.receipt.body;
        s.server_id = pin.server_id.clone();
        s.source_host_id = b.source_host_id.clone();
        s.term = pin.term;
        s.source_identity = Some(pin.host.clone());
        s.target_identity = Some(b.target_identity.clone());
        s.received = Some(grant.receipt.clone());
        let p = s.preparation.as_mut().unwrap();
        p.id = b.preparation_id.clone();
        p.target_url = b.target_url.clone();
        p.target_host_id = b.target_identity.host_id.clone();
        let mut request = model::ReadinessChallenge {
            source: pin.clone(),
            source_host_id: s.source_host_id.clone(),
            preparation_id: b.preparation_id.clone(),
            receipt_sha256: sha(&serde_json::to_vec(&grant.receipt).unwrap()),
            challenge: URL_SAFE_NO_PAD.encode([35u8; 32]),
            expires_at: clock() + 40,
            signature: String::new(),
        };
        request.signature = sign(&source, model::readiness_challenge_message(&request));
        assert!(request_matches(&s, &pin, &request));
        let ready = model::ReadyBody {
            request: request.clone(),
            target: b.target_identity.clone(),
            target_url: b.target_url.clone(),
            instance_id: uuid::Uuid::new_v4().to_string(),
            http_port: 4580,
            voice_port: 4581,
            rtc_ready: true,
            rtc_media_ready: None,
        };
        let proof = model::ReadyProof {
            signature: sign(&target, model::ready_message(&ready)),
            body: ready,
        };
        let mut public = url::Url::parse(&b.target_url).unwrap();
        public.set_scheme("wss").unwrap();
        let mut media = public.clone();
        media.set_path("/api/rtc/media");
        let room = format!(
            "relay-preflight-{}-{}",
            proof.body.instance_id,
            sha(request.challenge.as_bytes())
        );
        let rtc = [0, 1].map(|i| relay_protocol::RtcTicket {
            url: public.to_string(),
            room: room.clone(),
            token: format!("synthetic.signal.{i}"),
            media_bridge: Some(relay_protocol::RtcMediaBridge {
                url: media.to_string(),
                peer_ip: "100.127.255.254".into(),
                peer_port: 7882,
                token: format!("synthetic.media.{i}"),
            }),
        });
        let body = model::RtcProbeBody {
            request: request.clone(),
            target: b.target_identity.clone(),
            target_url: b.target_url.clone(),
            instance_id: proof.body.instance_id.clone(),
            expires_at: clock() + 29,
            rtc,
        };
        let ticket = model::RtcProbeTicket {
            signature: sign(&target, model::rtc_probe_ticket_message(&body)),
            body,
        };
        assert!(ticket_matches(&s, &request, &proof, &ticket));
        for change in 0..9 {
            let mut bad = ticket.clone();
            match change {
                0 => {
                    bad.body.rtc[1].media_bridge.as_mut().unwrap().url =
                        "wss://other.example/api/rtc/media".into()
                }
                1 => bad.body.rtc[0].media_bridge.as_mut().unwrap().peer_ip = "127.0.0.1".into(),
                2 => bad.body.instance_id = uuid::Uuid::new_v4().to_string(),
                3 => bad.body.request.receipt_sha256 = "a".repeat(64),
                4 => bad.body.expires_at = clock() - 1,
                5 => bad.body.expires_at = clock() + 3600,
                6 => bad.body.rtc[1].room = "another-room".into(),
                7 => bad.body.rtc[1].token = bad.body.rtc[0].token.clone(),
                _ => {}
            }
            if change != 8 {
                bad.signature = sign(&target, model::rtc_probe_ticket_message(&bad.body));
            } else {
                bad.signature.clear();
            }
            assert!(
                !ticket_matches(&s, &request, &proof, &bad),
                "mutation {change}"
            );
        }
    }
    struct ReplyLost {
        origin: CommandOrigin,
        pin: model::RoutePin,
        grant: model::ActivationGrant,
        calls: usize,
        lose: bool,
    }
    impl Remote for ReplyLost {
        fn origin(&self) -> &CommandOrigin {
            &self.origin
        }
        fn pin(&self) -> &model::RoutePin {
            &self.pin
        }
        fn json<T: DeserializeOwned>(
            &mut self,
            post: bool,
            path: &str,
            _: &impl Serialize,
            _: u64,
        ) -> Result<T> {
            assert!(post && path.ends_with("/retire"));
            self.calls += 1;
            if self.lose {
                return Err(Fault::Connection);
            }
            serde_json::from_value(serde_json::json!(self.grant)).map_err(|_| Fault::Integrity)
        }
        fn part(&mut self, _: &model::PartRequest) -> Result<Vec<u8>> {
            unreachable!()
        }
    }
    #[test]
    fn a_lost_retirement_reply_is_uncertain_and_is_never_automatically_retried() {
        let recipe = crate::handover_runtime::tests::recipe();
        let mut s = recipe.status.clone();
        let grant = s.retired.take().unwrap();
        let pin = model::RoutePin {
            server_id: s.server_id.clone(),
            host: grant.source_identity.clone(),
            term: s.term,
        };
        let request = model::ReadinessChallenge {
            source: pin.clone(),
            source_host_id: s.source_host_id.clone(),
            preparation_id: s.preparation.as_ref().unwrap().id.clone(),
            receipt_sha256: String::new(),
            challenge: String::new(),
            expires_at: 0,
            signature: String::new(),
        };
        let proof = model::ReadyProof {
            body: model::ReadyBody {
                request,
                target: grant.receipt.body.target_identity.clone(),
                target_url: grant.receipt.body.target_url.clone(),
                instance_id: uuid::Uuid::new_v4().to_string(),
                http_port: 4580,
                voice_port: 4581,
                rtc_ready: true,
                rtc_media_ready: Some(true),
            },
            signature: String::new(),
        };
        let mut remote = ReplyLost {
            origin: recipe.origin,
            pin,
            grant,
            calls: 0,
            lose: true,
        };
        assert!(matches!(
            finish(&mut remote, &s, proof.clone()),
            Err(Fault::GrantUncertain)
        ));
        assert_eq!(remote.calls, 1);
        remote.lose = false;
        let done = finish(&mut remote, &s, proof).unwrap();
        assert!(done.retired.is_some() && !done.writes_allowed && done.activation_available);
    }
}
