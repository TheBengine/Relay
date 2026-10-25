//! A directory is an address book, never an identity provider. Invite-derived
//! capabilities stay in POST bodies; only a signed, decryptable hint may open
//! an isolated transport, and only a fresh host proof may change the route.
use crate::host_route::{REFUSED, UNAVAILABLE};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chacha20poly1305::{
    aead::{Aead, KeyInit, Payload},
    ChaCha20Poly1305, Nonce,
};
use relay_protocol::{
    handover::{self, RoutePin},
    host_locator as model,
};
use sha2::{Digest, Sha256};
use std::{
    io::Read,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

fn now_ms() -> Result<u64, String> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| REFUSED)?
        .as_millis()
        .min(u64::MAX as u128) as u64)
}
#[cfg(test)]
fn capability(pin: &RoutePin, code: &str, encryption: bool) -> [u8; 32] {
    let verifier: [u8; 32] = Sha256::digest(code.trim().as_bytes()).into();
    Sha256::digest(model::capability_material(pin, &verifier, encryption)).into()
}
#[cfg(test)]
fn lookup_id(pin: &RoutePin, code: &str) -> String {
    capability(pin, code, false)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
/// Derived locally from an independently pinned host and an existing secret.
/// No raw session token is retained by the lookup worker or sent to the directory.
#[derive(Clone)]
pub(crate) struct ReadCapability {
    pin: RoutePin,
    id: String,
    key: zeroize::Zeroizing<[u8; 32]>,
}
impl ReadCapability {
    fn derive(pin: &RoutePin, secret: &str, purpose: model::Purpose) -> Option<Self> {
        if !crate::host_route::valid_pin(pin) || secret.is_empty() || secret.len() > 128 {
            return None;
        }
        // Session authentication hashes exact bytes. Invitation parsing trims its code.
        let bytes = match purpose {
            model::Purpose::Invitation => secret.trim().as_bytes(),
            model::Purpose::Session => secret.as_bytes(),
        };
        let verifier: [u8; 32] = Sha256::digest(bytes).into();
        let material = match purpose {
            model::Purpose::Invitation => model::capability_material,
            model::Purpose::Session => model::session_material,
        };
        Some(Self {
            pin: pin.clone(),
            id: format!("{:x}", Sha256::digest(material(pin, &verifier, false))),
            key: zeroize::Zeroizing::new(Sha256::digest(material(pin, &verifier, true)).into()),
        })
    }
    pub(crate) fn session(pin: &RoutePin, token: &str) -> Option<Self> {
        Self::derive(pin, token, model::Purpose::Session)
    }
}
pub(crate) fn directory() -> Option<String> {
    // Only the explicitly configured app-wide provider chooses this service.
    // Neither an invitation nor a community response can choose a lookup URL.
    crate::central_account::Provider::configured()
        .ok()??
        .people_url()
        .ok()
}
fn client() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(1))
        .timeout(Duration::from_secs(4))
        .build()
        .map_err(|_| UNAVAILABLE.into())
}
fn bounded<T: serde::de::DeserializeOwned>(
    response: reqwest::blocking::Response,
) -> Result<T, String> {
    if !response.status().is_success() {
        return Err(UNAVAILABLE.into());
    }
    let mut bytes = Vec::new();
    response
        .take(model::MAX_RECORD_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| UNAVAILABLE)?;
    if bytes.len() > model::MAX_RECORD_BYTES {
        return Err(REFUSED.into());
    }
    serde_json::from_slice(&bytes).map_err(|_| REFUSED.into())
}

#[cfg(test)]
fn open(
    record: &model::Record,
    pin: &RoutePin,
    code: &str,
    now: u64,
) -> Result<model::Destination, String> {
    let capability =
        ReadCapability::derive(pin, code, model::Purpose::Invitation).ok_or(REFUSED)?;
    open_capability(record, &capability, now)
}
fn open_capability(
    record: &model::Record,
    capability: &ReadCapability,
    now: u64,
) -> Result<model::Destination, String> {
    let pin = &capability.pin;
    if record.id != capability.id
        || record.anchor != *pin
        || record.issued_ms == 0
        || record.issued_ms > now.saturating_add(30_000)
        || record.expires_ms <= now
        || record.expires_ms < record.issued_ms
        || record.expires_ms - record.issued_ms > model::MAX_LIFETIME_MS
        || record.nonce.len() != 16
        || record.ciphertext.len() > model::MAX_CIPHERTEXT_BYTES * 4 / 3 + 4
        || !crate::host_route::history_connects(pin, &record.current, &record.history)
        || !crate::host_route::verify(
            &record.current.host,
            &record.signature,
            &model::record_message(record),
        )
    {
        return Err(REFUSED.into());
    }
    let nonce: [u8; 12] = URL_SAFE_NO_PAD
        .decode(&record.nonce)
        .map_err(|_| REFUSED)?
        .try_into()
        .map_err(|_| REFUSED)?;
    let ciphertext = URL_SAFE_NO_PAD
        .decode(&record.ciphertext)
        .map_err(|_| REFUSED)?;
    if !(16..=model::MAX_CIPHERTEXT_BYTES).contains(&ciphertext.len()) {
        return Err(REFUSED.into());
    }
    let plaintext = zeroize::Zeroizing::new(
        ChaCha20Poly1305::new_from_slice(capability.key.as_ref())
            .map_err(|_| REFUSED)?
            .decrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: &ciphertext,
                    aad: &model::associated_data(record),
                },
            )
            .map_err(|_| REFUSED)?,
    );
    let destination: model::Destination =
        serde_json::from_slice(&plaintext).map_err(|_| REFUSED)?;
    let url = crate::host_route::safe_destination(&destination.url)?;
    if url != destination.url
        || record.history.last().is_some_and(|g| {
            crate::host_route::safe_destination(&g.receipt.body.target_url)
                .ok()
                .as_deref()
                != Some(url.as_str())
        })
    {
        return Err(REFUSED.into());
    }
    if let Some(route) = &destination.iroh {
        let saved = saved_route(route)?;
        if route.scope.as_deref() != Some(record.current.host.host_id.as_str())
            || saved.address() != url
        {
            return Err(REFUSED.into());
        }
    } else if crate::iroh_route::is_iroh(&url) {
        return Err(REFUSED.into());
    }
    Ok(destination)
}
fn saved_route(route: &handover::IrohDestination) -> Result<crate::iroh_route::SavedIroh, String> {
    let invite = crate::invitation_routes::IrohRoute::new(
        &route.endpoint_id,
        route.relay.as_deref(),
        &route.token,
    )
    .map_err(|_| REFUSED)?;
    Ok(crate::iroh_route::SavedIroh::from_invitation(&invite))
}

pub(crate) fn discover(
    pin: &RoutePin,
    code: &str,
    cancel: &AtomicBool,
) -> Result<crate::host_route::VerifiedMove, String> {
    let capability =
        ReadCapability::derive(pin, code, model::Purpose::Invitation).ok_or(UNAVAILABLE)?;
    discover_capability(&capability, cancel)
}
pub(crate) fn discover_capability(
    capability: &ReadCapability,
    cancel: &AtomicBool,
) -> Result<crate::host_route::VerifiedMove, String> {
    let base = directory().ok_or(UNAVAILABLE)?;
    discover_at(&client()?, &base, capability, cancel)
}
fn discover_at(
    http: &reqwest::blocking::Client,
    base: &str,
    capability: &ReadCapability,
    cancel: &AtomicBool,
) -> Result<crate::host_route::VerifiedMove, String> {
    if cancel.load(Ordering::Acquire) {
        return Err(UNAVAILABLE.into());
    }
    let record: model::Record = bounded(
        http.post(format!(
            "{}{path}",
            base.trim_end_matches('/'),
            path = model::LOOKUP_PATH
        ))
        .json(&model::Lookup {
            id: capability.id.clone(),
        })
        .send()
        .map_err(|_| UNAVAILABLE)?,
    )?;
    let destination = resolve_capability(
        &record,
        capability,
        now_ms()?,
        cancel,
        |destination, challenge| {
            let saved = destination.iroh.as_ref().map(saved_route).transpose()?;
            if let Some(saved) = saved.as_ref() {
                return crate::iroh_route::route_proof(saved, challenge)
                    .map_err(|_| UNAVAILABLE.into());
            }
            let endpoint = format!("{}{}", destination.url, handover_route_path());
            bounded(
                http.post(endpoint)
                    .json(&handover::RouteChallenge {
                        challenge: challenge.into(),
                    })
                    .send()
                    .map_err(|_| UNAVAILABLE)?,
            )
        },
    )?;
    if cancel.load(Ordering::Acquire) {
        return Err(UNAVAILABLE.into());
    }
    if let Some(saved) = destination.iroh.as_ref().map(saved_route).transpose()? {
        crate::iroh_route::register_proved(&saved, &record.current.server_id)
            .map_err(|_| UNAVAILABLE)?;
    }
    Ok(crate::host_route::VerifiedMove {
        url: destination.url,
        pin: record.current,
    })
}
#[cfg(test)]
fn resolve_record(
    record: &model::Record,
    pin: &RoutePin,
    code: &str,
    now: u64,
    cancel: &AtomicBool,
    prove: impl FnMut(&model::Destination, &str) -> Result<handover::RouteProof, String>,
) -> Result<model::Destination, String> {
    let capability =
        ReadCapability::derive(pin, code, model::Purpose::Invitation).ok_or(REFUSED)?;
    resolve_capability(record, &capability, now, cancel, prove)
}
fn resolve_capability(
    record: &model::Record,
    capability: &ReadCapability,
    now: u64,
    cancel: &AtomicBool,
    mut prove: impl FnMut(&model::Destination, &str) -> Result<handover::RouteProof, String>,
) -> Result<model::Destination, String> {
    let destination = open_capability(record, capability, now)?;
    if cancel.load(Ordering::Acquire) {
        return Err(UNAVAILABLE.into());
    }
    let mut nonce = [0u8; 32];
    getrandom::getrandom(&mut nonce).map_err(|_| REFUSED)?;
    let challenge = URL_SAFE_NO_PAD.encode(nonce);
    let proof = prove(&destination, &challenge)?;
    crate::host_route::confirm_reply(&destination.url, &record.current, &challenge, &proof)?;
    if proof.state.pin != record.current || cancel.load(Ordering::Acquire) {
        return Err(UNAVAILABLE.into());
    }
    Ok(destination)
}

fn handover_route_path() -> &'static str {
    "/api/hosting/handover/route"
}

pub(crate) fn publish_at(base: &str, record: &model::Record) -> Result<(), String> {
    let http = client()?;
    let response = http
        .post(format!(
            "{}{}",
            base.trim_end_matches('/'),
            model::PUBLISH_PATH
        ))
        .json(record)
        .send()
        .map_err(|_| UNAVAILABLE)?;
    if response.status().is_success() {
        return Ok(());
    }
    if !matches!(
        response.status(),
        reqwest::StatusCode::BAD_REQUEST | reqwest::StatusCode::CONFLICT
    ) {
        return Err(UNAVAILABLE.into());
    }
    // An old PC may still have a queued pre-move envelope. A rejection alone is
    // never an acknowledgement: ask the SAME directory, without credentials,
    // and discard the exact queued version only for verified newer authority.
    let stored: model::Record = bounded(
        http.post(format!(
            "{}{}",
            base.trim_end_matches('/'),
            model::LOOKUP_PATH
        ))
        .json(&model::Lookup {
            id: record.id.clone(),
        })
        .send()
        .map_err(|_| UNAVAILABLE)?,
    )?;
    if crate::host_locator_ack::supersedes(record, &stored, now_ms()?) {
        Ok(())
    } else {
        Err(UNAVAILABLE.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    fn fixture() -> (SigningKey, RoutePin, model::Record, String) {
        let (a, b, pin, mut grant) = crate::host_route::tests::fixture();
        let endpoint = "a3".repeat(32);
        grant.receipt.body.target_url = format!("http://{endpoint}.iroh:4580");
        grant.receipt.signature = URL_SAFE_NO_PAD.encode(
            b.sign(&handover::receipt_message(&grant.receipt.body))
                .to_bytes(),
        );
        grant.signature =
            URL_SAFE_NO_PAD.encode(a.sign(&handover::grant_message(&grant)).to_bytes());
        let current = RoutePin {
            server_id: pin.server_id.clone(),
            host: grant.receipt.body.target_identity.clone(),
            term: grant.next_term,
        };
        let code = "synthetic-invite-only";
        let token = URL_SAFE_NO_PAD.encode([93; 32]);
        let destination = model::Destination {
            url: grant.receipt.body.target_url.clone(),
            iroh: Some(handover::IrohDestination {
                scope: Some(current.host.host_id.clone()),
                endpoint_id: endpoint,
                relay: None,
                token: token.clone(),
            }),
        };
        let mut record = model::Record {
            id: lookup_id(&pin, code),
            anchor: pin.clone(),
            current,
            history: vec![grant],
            issued_ms: 100_000,
            expires_ms: 200_000,
            nonce: URL_SAFE_NO_PAD.encode([3; 12]),
            ciphertext: String::new(),
            signature: String::new(),
        };
        record.ciphertext = URL_SAFE_NO_PAD.encode(
            ChaCha20Poly1305::new_from_slice(&capability(&pin, code, true))
                .unwrap()
                .encrypt(
                    Nonce::from_slice(&[3; 12]),
                    Payload {
                        msg: &serde_json::to_vec(&destination).unwrap(),
                        aad: &model::associated_data(&record),
                    },
                )
                .unwrap(),
        );
        record.signature =
            URL_SAFE_NO_PAD.encode(b.sign(&model::record_message(&record)).to_bytes());
        (b, pin, record, token)
    }
    #[test]
    fn offline_source_hint_recovers_only_successor_admission_after_fresh_proof() {
        let (key, pin, record, token) = fixture();
        let wire = serde_json::to_string(&record).unwrap();
        assert!(!wire.contains(&token));
        assert!(!wire.contains("synthetic-invite-only"));
        let cancel = AtomicBool::new(false);
        let mut calls = 0;
        let destination = resolve_record(
            &record,
            &pin,
            "synthetic-invite-only",
            150_000,
            &cancel,
            |destination, challenge| {
                calls += 1;
                assert_eq!(destination.iroh.as_ref().unwrap().token, token);
                assert_eq!(
                    destination.url,
                    record.history.last().unwrap().receipt.body.target_url
                );
                assert_eq!(challenge.len(), 43);
                Ok(crate::host_route::tests::sign(
                    &key,
                    handover::RouteState {
                        pin: record.current.clone(),
                        history: record.history.clone(),
                        challenge: challenge.into(),
                        writes_allowed: true,
                        moved: None,
                    },
                ))
            },
        )
        .unwrap();
        assert_eq!(calls, 1); // No request to the old host and no credential sent.
        assert_eq!(destination.iroh.unwrap().token, token);
    }
    #[test]
    fn forged_stale_wrong_invite_or_replayed_proof_never_changes_the_route() {
        let (key, pin, record, _) = fixture();
        assert!(open(&record, &pin, "wrong-invite", 150_000).is_err());
        assert!(open(&record, &pin, "synthetic-invite-only", 200_000).is_err());
        for field in 0..5 {
            let mut bad = record.clone();
            match field {
                0 => bad.history[0].receipt.body.target_url = "https://attacker.example".into(),
                1 => bad.current.term += 1,
                2 => bad.ciphertext.replace_range(..1, "!"),
                3 => bad.anchor.host.public_key = "A".repeat(43),
                _ => bad.expires_ms += 1,
            }
            assert!(open(&bad, &pin, "synthetic-invite-only", 150_000).is_err());
        }
        for fault in 0..3 {
            let cancel = AtomicBool::new(false);
            assert!(resolve_record(
                &record,
                &pin,
                "synthetic-invite-only",
                150_000,
                &cancel,
                |_, challenge| {
                    if fault == 2 {
                        cancel.store(true, Ordering::Release);
                    }
                    Ok(crate::host_route::tests::sign(
                        &key,
                        handover::RouteState {
                            pin: record.current.clone(),
                            history: record.history.clone(),
                            challenge: if fault == 0 {
                                "old-challenge".into()
                            } else {
                                challenge.into()
                            },
                            writes_allowed: fault != 1,
                            moved: None,
                        },
                    ))
                }
            )
            .is_err());
        }
    }
    #[test]
    fn remembered_session_uses_exact_bytes_and_cannot_open_an_invitation_hint() {
        let (key, pin, mut record, _) = fixture();
        let invite =
            ReadCapability::derive(&pin, "synthetic-invite-only", model::Purpose::Invitation)
                .unwrap();
        let destination = open_capability(&record, &invite, 150_000).unwrap();
        let raw = " synthetic-session-only ";
        let session = ReadCapability::session(&pin, raw).unwrap();
        let trimmed = ReadCapability::session(&pin, raw.trim()).unwrap();
        let verifier: [u8; 32] = Sha256::digest(raw.as_bytes()).into();
        assert_eq!(
            session.id,
            format!(
                "{:x}",
                Sha256::digest(model::session_material(&pin, &verifier, false))
            )
        );
        assert_ne!(session.id, trimmed.id);
        assert!(open_capability(&record, &session, 150_000).is_err());
        record.id = session.id.clone();
        record.ciphertext = URL_SAFE_NO_PAD.encode(
            ChaCha20Poly1305::new_from_slice(session.key.as_ref())
                .unwrap()
                .encrypt(
                    Nonce::from_slice(&[3; 12]),
                    Payload {
                        msg: &serde_json::to_vec(&destination).unwrap(),
                        aad: &model::associated_data(&record),
                    },
                )
                .unwrap(),
        );
        record.signature =
            URL_SAFE_NO_PAD.encode(key.sign(&model::record_message(&record)).to_bytes());
        assert!(open_capability(&record, &trimmed, 150_000).is_err());
        assert!(open_capability(
            &record,
            &ReadCapability::derive(&pin, raw, model::Purpose::Invitation).unwrap(),
            150_000
        )
        .is_err());
        let cancel = AtomicBool::new(false);
        let recovered = resolve_capability(&record, &session, 150_000, &cancel, |_, challenge| {
            Ok(crate::host_route::tests::sign(
                &key,
                handover::RouteState {
                    pin: record.current.clone(),
                    history: record.history.clone(),
                    challenge: challenge.into(),
                    writes_allowed: true,
                    moved: None,
                },
            ))
        })
        .unwrap();
        assert_eq!(recovered.url, destination.url);
        assert!(!serde_json::to_string(&model::Lookup {
            id: session.id.clone()
        })
        .unwrap()
        .contains(raw.trim()));
        // A signed hint alone is insufficient: the target must still be the writable host.
        assert!(
            resolve_capability(&record, &session, 150_000, &cancel, |_, challenge| {
                Ok(crate::host_route::tests::sign(
                    &key,
                    handover::RouteState {
                        pin: record.current.clone(),
                        history: record.history.clone(),
                        challenge: challenge.into(),
                        writes_allowed: false,
                        moved: None,
                    },
                ))
            })
            .is_err()
        );
    }
}
