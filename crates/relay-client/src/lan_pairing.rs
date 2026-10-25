//! A host-shared random code authenticates a secure invitation over an untrusted LAN.
//! Discovery and plain HTTP never receive a password, session, or the code itself.
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chacha20poly1305::{
    aead::{Aead, KeyInit, Payload},
    ChaCha20Poly1305, Nonce,
};
use hmac::{Hmac, Mac};
use relay_protocol::lan_pairing as model;
use sha2::{Digest, Sha256};
use std::{
    io::Read,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc, Arc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use zeroize::Zeroizing;

pub(crate) const FAILED:&str="The local code could not be used. Check the code and ask the host for a new one if it expired.";
const UNSUPPORTED:&str="Check the code with the host. If it is correct, update the host or ask for a secure invitation.";
static JOINING: AtomicUsize = AtomicUsize::new(0);

pub(crate) fn now_ms() -> Result<u64, String> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| FAILED)?
        .as_millis()
        .min(u64::MAX as u128) as u64)
}
/// No Debug, serialization or persistence: this is a random bearer, not a password.
pub(crate) struct Code(Zeroizing<[u8; model::CODE_BYTES]>);
impl Code {
    pub(crate) fn generate() -> Result<Self, String> {
        let mut raw = Zeroizing::new([0u8; model::CODE_BYTES]);
        getrandom::getrandom(raw.as_mut()).map_err(|_| FAILED)?;
        Ok(Self(raw))
    }
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        let text = text
            .trim()
            .strip_prefix(model::PREFIX)
            .ok_or("Paste the full LAN- code from the host.")?;
        let raw = Zeroizing::new(URL_SAFE_NO_PAD.decode(text).map_err(|_| FAILED)?);
        if raw.len() != model::CODE_BYTES || URL_SAFE_NO_PAD.encode(raw.as_slice()) != text {
            return Err(FAILED.into());
        }
        Ok(Self(Zeroizing::new(
            raw.as_slice().try_into().map_err(|_| FAILED)?,
        )))
    }
    pub(crate) fn wire(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.0.as_slice())
    }
    pub(crate) fn display(&self) -> Zeroizing<String> {
        Zeroizing::new(format!("{}{}", model::PREFIX, self.wire()))
    }
    fn key(&self, server_id: &str, purpose: model::Purpose) -> Zeroizing<[u8; 32]> {
        Zeroizing::new(Sha256::digest(model::material(server_id, &self.0, purpose)).into())
    }
    pub(crate) fn id(&self, server_id: &str) -> String {
        self.key(server_id, model::Purpose::Lookup)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }
    fn redeem(&self, server_id: &str) -> Result<model::Redeem, String> {
        let mut nonce = [0u8; model::CHALLENGE_BYTES];
        getrandom::getrandom(&mut nonce).map_err(|_| FAILED)?;
        let mut request = model::Redeem {
            server_id: server_id.into(),
            id: self.id(server_id),
            challenge: URL_SAFE_NO_PAD.encode(nonce),
            tag: String::new(),
        };
        let key = self.key(server_id, model::Purpose::Auth);
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key.as_ref()).map_err(|_| FAILED)?;
        mac.update(&model::redeem_message(&request));
        request.tag = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
        Ok(request)
    }
    fn open(
        &self,
        server_id: &str,
        envelope: &model::Envelope,
        now: u64,
    ) -> Result<crate::invitation::Invitation, String> {
        if envelope.server_id != server_id
            || envelope.id != self.id(server_id)
            || envelope.expires_ms <= now
            || envelope.expires_ms > now.saturating_add(model::LIFETIME_MS + 30_000)
            || envelope.nonce.len() != 16
            || envelope.ciphertext.len() > model::MAX_CIPHERTEXT_BYTES * 4 / 3 + 4
        {
            return Err(FAILED.into());
        }
        let nonce: [u8; 12] = URL_SAFE_NO_PAD
            .decode(&envelope.nonce)
            .map_err(|_| FAILED)?
            .try_into()
            .map_err(|_| FAILED)?;
        let ciphertext = URL_SAFE_NO_PAD
            .decode(&envelope.ciphertext)
            .map_err(|_| FAILED)?;
        if !(16..=model::MAX_CIPHERTEXT_BYTES).contains(&ciphertext.len()) {
            return Err(FAILED.into());
        }
        let key = self.key(server_id, model::Purpose::Seal);
        let plain = Zeroizing::new(
            ChaCha20Poly1305::new_from_slice(key.as_ref())
                .map_err(|_| FAILED)?
                .decrypt(
                    Nonce::from_slice(&nonce),
                    Payload {
                        msg: &ciphertext,
                        aad: &model::associated_data(envelope),
                    },
                )
                .map_err(|_| FAILED)?,
        );
        let payload: model::Invitation = serde_json::from_slice(&plain).map_err(|_| FAILED)?;
        if payload.server_id != server_id
            || payload.authority.server_id != server_id
            || !crate::host_route::valid_pin(&payload.authority)
        {
            return Err(FAILED.into());
        }
        let url =
            crate::host_route::safe_destination(&payload.destination.url).map_err(|_| FAILED)?;
        if url != payload.destination.url {
            return Err(FAILED.into());
        }
        let mut invitation = crate::invitation::Invitation::new(&url, server_id, &payload.code)?;
        if let Some(route) = payload.destination.iroh {
            if route.scope.as_deref() != Some(payload.authority.host.host_id.as_str()) {
                return Err(FAILED.into());
            }
            let route = crate::invitation_routes::IrohRoute::new(
                &route.endpoint_id,
                route.relay.as_deref(),
                &route.token,
            )?;
            let saved = crate::iroh_route::SavedIroh::from_invitation(&route);
            if saved.address() != url {
                return Err(FAILED.into());
            }
            invitation = invitation.with_iroh(route);
        } else if crate::iroh_route::is_iroh(&url) {
            return Err(FAILED.into());
        }
        invitation.with_authority(Some(payload.authority))
    }
}

pub(crate) struct Ready {
    pub(crate) code: Zeroizing<String>,
    pub(crate) id: String,
    pub(crate) expires_ms: u64,
}

fn fetch(
    found: &crate::lan_discovery::Found,
    code: &Code,
    cancel: &AtomicBool,
) -> Result<crate::invitation::Invitation, String> {
    fetch_at(found, code, cancel, &found.url())
}
fn fetch_at(
    found: &crate::lan_discovery::Found,
    code: &Code,
    cancel: &AtomicBool,
    base: &str,
) -> Result<crate::invitation::Invitation, String> {
    if !relay_protocol::lan_discovery::is_lan_ipv4(found.ip)
        || found.port < relay_protocol::lan_discovery::MIN_PORT
        || cancel.load(Ordering::Acquire)
    {
        return Err(FAILED.into());
    }
    let server_id = uuid::Uuid::parse_str(&found.server_id)
        .map_err(|_| FAILED)?
        .to_string();
    let request = code.redeem(&server_id)?;
    let http = reqwest::blocking::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(1))
        .timeout(Duration::from_secs(4))
        .build()
        .map_err(|_| FAILED)?;
    for _ in 0..2 {
        if cancel.load(Ordering::Acquire) {
            return Err(FAILED.into());
        }
        let Ok(response) = http
            .post(format!("{}{}", base, model::REDEEM_PATH))
            .json(&request)
            .send()
        else {
            continue;
        };
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(UNSUPPORTED.into());
        }
        if !response.status().is_success() {
            return Err(FAILED.into());
        }
        let mut bytes = Vec::new();
        // A lost reply, including an interrupted body, retries the SAME challenge.
        if response
            .take(model::MAX_ENVELOPE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .is_err()
        {
            continue;
        }
        if bytes.len() > model::MAX_ENVELOPE_BYTES || cancel.load(Ordering::Acquire) {
            return Err(FAILED.into());
        }
        let envelope: model::Envelope = serde_json::from_slice(&bytes).map_err(|_| FAILED)?;
        return code.open(&server_id, &envelope, now_ms()?);
    }
    Err(FAILED.into())
}

pub(crate) struct JoinJob {
    receiver: mpsc::Receiver<Result<crate::invitation::Invitation, String>>,
    cancel: Arc<AtomicBool>,
}
impl JoinJob {
    pub(crate) fn start(
        found: crate::lan_discovery::Found,
        text: &str,
        known_urls: Vec<String>,
        mut pins: Vec<relay_protocol::handover::RoutePin>,
        wake: impl FnOnce() + Send + 'static,
    ) -> Result<Self, String> {
        let code = Code::parse(text)?;
        JOINING
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < 2).then_some(n + 1)
            })
            .map_err(|_| "A local connection is already being checked. Try again shortly.")?;
        struct Permit;
        impl Drop for Permit {
            fn drop(&mut self) {
                JOINING.fetch_sub(1, Ordering::AcqRel);
            }
        }
        let permit = Permit;
        let (send, receiver) = mpsc::sync_channel(1);
        let cancel = Arc::new(AtomicBool::new(false));
        let stopped = cancel.clone();
        std::thread::Builder::new().name("relay-lan-pairing".into()).spawn(move||{
            let _permit=permit;
            let result=(||{
                if known_urls.len()>32 || pins.len()>32{return Err(FAILED.into());}
                for url in known_urls {
                    let saved=crate::credential_vault::load(&url).map_err(|_|"Relay could not check this community's saved identity. Use its saved connection.")?;
                    if let Some(saved)=saved {if let Some(authority)=saved.authority {pins.push(authority.pin);}}
                }
                let invitation=fetch(&found,&code,&stopped)?;
                let pin=invitation.authority.as_ref().ok_or(FAILED)?;
                if pins.iter().any(|old|old!=pin){return Err("This community has a different saved host identity. Use its saved secure connection to follow a host move.".into());}
                if stopped.load(Ordering::Acquire){return Err(FAILED.into());}
                if let Some(route)=invitation.iroh.as_ref(){
                    let saved=crate::iroh_route::SavedIroh::from_invitation(route);
                    let mut random=[0u8;32];getrandom::getrandom(&mut random).map_err(|_|FAILED)?;
                    let challenge=URL_SAFE_NO_PAD.encode(random);
                    let proof=crate::iroh_route::route_proof(&saved,&challenge).map_err(|_|FAILED)?;
                    let checked=crate::host_route::confirm_reply(&invitation.server,pin,&challenge,&proof)?;
                    if checked!=*pin || stopped.load(Ordering::Acquire){return Err(FAILED.into());}
                    crate::iroh_route::register_proved(&saved,&invitation.server_id)?;
                }
                Ok(invitation)
            })();
            if !stopped.load(Ordering::Acquire){let _=send.try_send(result);}wake();
        }).map_err(|_|FAILED)?;
        Ok(Self { receiver, cancel })
    }
    pub(crate) fn poll(&self) -> Option<Result<crate::invitation::Invitation, String>> {
        match self.receiver.try_recv() {
            Ok(value) => Some(value),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(_) => Some(Err(FAILED.into())),
        }
    }
}
impl Drop for JoinJob {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
    }
}

#[derive(Default)]
pub(crate) struct JoinState {
    pub(crate) found: Option<crate::lan_discovery::Found>,
    pub(crate) code: Zeroizing<String>,
    pub(crate) job: Option<JoinJob>,
    pub(crate) error: Option<String>,
}

#[cfg(test)]
pub(crate) fn fetch_for_test(
    found: &crate::lan_discovery::Found,
    text: &str,
    base: &str,
) -> Result<crate::invitation::Invitation, String> {
    fetch_at(found, &Code::parse(text)?, &AtomicBool::new(false), base)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sealed(code: &Code) -> (String, model::Envelope) {
        let (_, _, pin, _) = crate::host_route::tests::fixture();
        let server = pin.server_id.clone();
        let invitation = model::Invitation {
            server_id: server.clone(),
            code: "synthetic-invite".into(),
            authority: pin,
            destination: relay_protocol::host_locator::Destination {
                url: "https://host.example".into(),
                iroh: None,
            },
        };
        let mut envelope = model::Envelope {
            server_id: server.clone(),
            id: code.id(&server),
            expires_ms: 150_000,
            nonce: URL_SAFE_NO_PAD.encode([7u8; 12]),
            ciphertext: String::new(),
        };
        let key = code.key(&server, model::Purpose::Seal);
        envelope.ciphertext = URL_SAFE_NO_PAD.encode(
            ChaCha20Poly1305::new_from_slice(key.as_ref())
                .unwrap()
                .encrypt(
                    Nonce::from_slice(&[7u8; 12]),
                    Payload {
                        msg: &serde_json::to_vec(&invitation).unwrap(),
                        aad: &model::associated_data(&envelope),
                    },
                )
                .unwrap(),
        );
        (server, envelope)
    }
    #[test]
    fn only_the_host_code_opens_a_bound_unexpired_secure_invitation() {
        let code = Code::generate().unwrap();
        let shown = code.display();
        let parsed = Code::parse(&shown).unwrap();
        let (server, envelope) = sealed(&code);
        let invitation = parsed.open(&server, &envelope, 100_000).unwrap();
        assert_eq!(invitation.server, "https://host.example");
        assert_eq!(invitation.code, "synthetic-invite");
        assert!(invitation.authority.is_some());
        assert!(Code::generate()
            .unwrap()
            .open(&server, &envelope, 100_000)
            .is_err());
        assert!(code
            .open(&uuid::Uuid::new_v4().to_string(), &envelope, 100_000)
            .is_err());
        assert!(code.open(&server, &envelope, 150_000).is_err());
        let mut changed = envelope.clone();
        changed.expires_ms += 1;
        assert!(code.open(&server, &changed, 100_000).is_err());
        changed = envelope.clone();
        changed.ciphertext = "x".repeat(model::MAX_CIPHERTEXT_BYTES * 2);
        assert!(code.open(&server, &changed, 100_000).is_err());
        for malformed in [
            "LAN-short".to_owned(),
            format!("{}=", shown.as_str()),
            "123456".into(),
            "LAN-AAAAAAAAAAAAAAAAAAAAAAA".into(),
        ] {
            assert!(Code::parse(&malformed).is_err());
        }
    }
    #[test]
    fn the_redemption_contains_only_domain_separated_id_challenge_and_mac() {
        let code = Code::generate().unwrap();
        let server = uuid::Uuid::new_v4().to_string();
        let request = code.redeem(&server).unwrap();
        let key = code.key(&server, model::Purpose::Auth);
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key.as_ref()).unwrap();
        mac.update(&model::redeem_message(&request));
        mac.verify_slice(&URL_SAFE_NO_PAD.decode(&request.tag).unwrap())
            .unwrap();
        let wire = serde_json::to_string(&request).unwrap();
        assert!(!wire.contains(&code.wire()));
        assert_ne!(
            code.key(&server, model::Purpose::Auth).as_slice(),
            code.key(&server, model::Purpose::Seal).as_slice()
        );
        assert_ne!(request.id, code.id(&uuid::Uuid::new_v4().to_string()));
    }
}
