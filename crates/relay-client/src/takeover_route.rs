//! Following a community to the co-host a majority chose after its host vanished.
//!
//! The app remembers the approved co-hosts' addresses while the host is up.
//! When the pinned host stops answering, each address is asked for a fresh
//! signed route plus the majority mandate. The app switches only when the old
//! host it pinned signed the set, two of three approved copies signed votes for
//! exactly this co-host, and the co-host proves its key with the new challenge.
//! A directory, an address or a good ping alone is never enough.
use super::*;
use relay_protocol::takeover::Mandate;

/// Election terms only grow; more than this many in one outage is refused.
const MAX_TERMS: u64 = 1024;
const MAX_STANDBY: usize = 2;
pub(crate) const PATH: &str = "/api/hosting/takeover/route";

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Reply {
    pub(crate) proof: RouteProof,
    pub(crate) mandate: Mandate,
}

/// The new route, if this reply proves a legitimate takeover of `pin`.
pub(crate) fn verified(pin: &RoutePin, url: &str, challenge: &str, reply: &Reply) -> Option<VerifiedMove> {
    let next = reply.mandate.route(
        |bytes| format!("{:x}", Sha256::digest(bytes)),
        verify,
    )?;
    let state = &reply.proof.state;
    let ok = valid_pin(pin)
        && reply.mandate.set.body.pin == *pin
        && state.pin == next
        && challenge.len() == 43
        && state.challenge == challenge
        && state.writes_allowed
        && state.moved.is_none()
        && state.history.is_empty()
        && next.term.checked_sub(pin.term).is_some_and(|n| (1..=MAX_TERMS).contains(&n))
        && verify(&next.host, &reply.proof.signature, &model::route_message(state));
    ok.then(|| VerifiedMove {
        url: url.into(),
        pin: next,
    })
}

fn store_path(server_id: &str) -> Option<std::path::PathBuf> {
    if !uuid(server_id) {
        return None;
    }
    let base = std::env::var_os("RELAY_PROFILE_DIR")
        .or_else(|| std::env::var_os("RELAY_CONFIG_DIR"))
        .map(std::path::PathBuf::from)
        .or_else(|| {
            directories::ProjectDirs::from("org", "Relay", "Relay").map(|v| v.config_dir().to_owned())
        })?;
    Some(base.join("takeover-standby").join(format!("{server_id}.json")))
}

/// Remembers the approved co-hosts the host announced. Only safe addresses are kept.
pub(crate) fn remember(server_id: &str, urls: &[String]) -> Result<(), String> {
    let Some(path) = store_path(server_id) else {
        return Err(REFUSED.into());
    };
    let urls: Vec<String> = urls
        .iter()
        .filter_map(|u| safe_destination(u).ok())
        .take(MAX_STANDBY)
        .collect();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|_| UNAVAILABLE)?;
    }
    std::fs::write(&path, serde_json::to_vec(&urls).map_err(|_| REFUSED)?).map_err(|_| UNAVAILABLE.into())
}

pub(crate) fn remembered(server_id: &str) -> Vec<String> {
    store_path(server_id)
        .and_then(|p| std::fs::read(p).ok())
        .filter(|b| b.len() <= 8192)
        .and_then(|b| serde_json::from_slice::<Vec<String>>(&b).ok())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|u| safe_destination(&u).ok())
        .take(MAX_STANDBY)
        .collect()
}

/// Asks each remembered co-host. `None` when none of them proves a takeover.
pub(crate) fn discover(pin: &RoutePin, cancel: &std::sync::atomic::AtomicBool) -> Option<VerifiedMove> {
    let urls = remembered(&pin.server_id);
    if urls.is_empty() {
        return None;
    }
    let http = reqwest::blocking::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(1))
        .timeout(Duration::from_secs(3))
        .build()
        .ok()?;
    for url in urls {
        if cancel.load(std::sync::atomic::Ordering::Acquire) {
            return None;
        }
        let mut nonce = [0u8; 32];
        getrandom::getrandom(&mut nonce).ok()?;
        let challenge = URL_SAFE_NO_PAD.encode(nonce);
        if let Some(reply) = fetch(&http, &url, &challenge) {
            if let Some(found) = verified(pin, &url, &challenge, &reply) {
                return Some(found);
            }
        }
    }
    None
}

fn fetch(http: &reqwest::blocking::Client, url: &str, challenge: &str) -> Option<Reply> {
    crate::embedded_network::ensure_route(url).ok()?;
    let response = http
        .post(crate::embedded_network::endpoint(&format!("{url}{PATH}")))
        .json(&model::RouteChallenge {
            challenge: challenge.into(),
        })
        .send()
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let limit = 4 * model::CONTROL_BYTES;
    let mut bytes = Vec::new();
    response.take(limit + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > limit {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use relay_protocol::takeover::{
        self as wire, AcceptBody, AuthoritySet, CandidateAccept, SetBody, Tip, Vote, VoteBody,
        VoteRequest, VoteRequestBody,
    };

    fn sign(key: &SigningKey, message: &[u8]) -> String {
        URL_SAFE_NO_PAD.encode(key.sign(message).to_bytes())
    }
    fn hex(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    struct World {
        pin: RoutePin,
        keys: [SigningKey; 3],
        mandate: Mandate,
    }

    /// Anna (index 0) hosts at `pin`; Bo (1) wins with Clara's (2) vote.
    fn world() -> World {
        let keys = [61u8, 62, 63].map(|s| SigningKey::from_bytes(&[s; 32]));
        let id = |i: usize| identity(&keys[i].verifying_key());
        let pin = RoutePin {
            server_id: uuid::Uuid::new_v4().to_string(),
            host: id(0),
            term: 2,
        };
        let mut candidates = [id(1), id(2)];
        candidates.sort_by(|a, b| a.host_id.cmp(&b.host_id));
        let body = SetBody {
            schema: wire::SCHEMA,
            pin: pin.clone(),
            epoch: 1,
            candidates,
        };
        let set = AuthoritySet {
            signature: sign(&keys[0], &body.message()),
            body,
        };
        let set_sha = hex(&set.body.message());
        let accept_body = AcceptBody {
            schema: wire::SCHEMA,
            server_id: pin.server_id.clone(),
            set_sha256: set_sha.clone(),
            epoch: 1,
            candidate: id(1),
            recipient_key: "A".repeat(43),
            full_data: true,
            takeover: true,
        };
        let accept = CandidateAccept {
            signature: sign(&keys[1], &accept_body.message()),
            body: accept_body,
        };
        let mut consent = accept.body.message();
        consent.extend(accept.signature.as_bytes());
        let request_body = VoteRequestBody {
            schema: wire::SCHEMA,
            server_id: pin.server_id.clone(),
            set_sha256: set_sha.clone(),
            epoch: 1,
            term: 3,
            candidate: id(1),
            tip: Tip {
                term: 2,
                index: 0,
                digest: "c".repeat(64),
            },
            accept,
        };
        let request = VoteRequest {
            signature: sign(&keys[1], &request_body.message()),
            body: request_body,
        };
        let vote = |i: usize| {
            let body = VoteBody {
                schema: wire::SCHEMA,
                server_id: pin.server_id.clone(),
                set_sha256: set_sha.clone(),
                epoch: 1,
                term: 3,
                candidate: id(1),
                candidate_tip: request.body.tip.clone(),
                accept_sha256: hex(&consent),
                voter: id(i),
            };
            Vote {
                signature: sign(&keys[i], &body.message()),
                body,
            }
        };
        let votes = vec![vote(1), vote(2)];
        World {
            pin,
            mandate: Mandate { set, request, votes },
            keys,
        }
    }

    fn reply(w: &World, challenge: &str) -> Reply {
        let state = model::RouteState {
            pin: RoutePin {
                server_id: w.pin.server_id.clone(),
                host: identity(&w.keys[1].verifying_key()),
                term: 3,
            },
            challenge: challenge.into(),
            writes_allowed: true,
            moved: None,
            history: Vec::new(),
        };
        Reply {
            proof: super::super::tests::sign(&w.keys[1], state),
            mandate: w.mandate.clone(),
        }
    }

    #[test]
    fn a_majority_mandate_and_a_fresh_proof_move_the_app_to_the_chosen_co_host() {
        let w = world();
        let challenge = URL_SAFE_NO_PAD.encode([9u8; 32]);
        let url = "https://bo.example";
        let moved = verified(&w.pin, url, &challenge, &reply(&w, &challenge)).unwrap();
        assert_eq!(moved.url, url);
        assert_eq!(moved.pin.host, identity(&w.keys[1].verifying_key()));
        assert_eq!(moved.pin.term, 3);
    }

    #[test]
    fn anything_less_than_a_proven_majority_for_the_pinned_host_is_refused() {
        let w = world();
        let challenge = URL_SAFE_NO_PAD.encode([9u8; 32]);
        let url = "https://bo.example";
        // A stale or replayed proof.
        assert!(verified(&w.pin, url, &URL_SAFE_NO_PAD.encode([8u8; 32]), &reply(&w, &challenge)).is_none());
        // One vote is not a majority, and a repeated vote counts once.
        let mut short = reply(&w, &challenge);
        short.mandate.votes.truncate(1);
        assert!(verified(&w.pin, url, &challenge, &short).is_none());
        let mut doubled = reply(&w, &challenge);
        doubled.mandate.votes[1] = doubled.mandate.votes[0].clone();
        assert!(verified(&w.pin, url, &challenge, &doubled).is_none());
        // A set for another host or term than the one this app pinned.
        let mut other = w.pin.clone();
        other.term = 5;
        assert!(verified(&other, url, &challenge, &reply(&w, &challenge)).is_none());
        // Clara cannot present Bo's mandate as her own route.
        let mut clara = reply(&w, &challenge);
        clara.proof = super::super::tests::sign(
            &w.keys[2],
            model::RouteState {
                pin: RoutePin {
                    host: identity(&w.keys[2].verifying_key()),
                    ..clara.proof.state.pin.clone()
                },
                ..clara.proof.state.clone()
            },
        );
        assert!(verified(&w.pin, url, &challenge, &clara).is_none());
        // A route that does not accept changes is no takeover.
        let mut paused = reply(&w, &challenge);
        paused.proof = super::super::tests::sign(
            &w.keys[1],
            model::RouteState {
                writes_allowed: false,
                ..paused.proof.state.clone()
            },
        );
        assert!(verified(&w.pin, url, &challenge, &paused).is_none());
    }
}
