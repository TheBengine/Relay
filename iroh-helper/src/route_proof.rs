//! A separate, bounded route-proof exchange.
//!
//! After a community moved, its members must check that the host at a new iroh
//! address is the one they pinned, before they send it anything. The proof is one
//! signed JSON object the new host returns for a fresh challenge. This module only
//! fetches it. It never judges it: the caller owns the pin, the ancestry, the
//! freshness of the challenge and every login decision.
//!
//! It must not disturb the route a person already uses, which may hold an older
//! admission token for the very same host key. So it shares nothing with `client`:
//! it never calls the route, probe or route-lookup functions, never touches the route
//! map, a gateway listener or a voice socket, and never looks up another route. Each
//! exchange has its own fresh endpoint, its own connection and one stream for one
//! fixed request, and closes only those. It carries no account credential, follows no
//! redirect, and has no WebSocket, media or datagram capability.
use crate::{
    client::RouteSpec,
    protocol::{self, ALPN},
    Network,
};
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::{
    header::{CONNECTION, CONTENT_LENGTH, CONTENT_TYPE, HOST},
    Method, Request, StatusCode,
};
use hyper_util::rt::TokioIo;
use iroh::{endpoint::Connection, Endpoint, EndpointAddr};
use std::{sync::Arc, time::Duration};
use tokio::{
    sync::Semaphore,
    task::JoinHandle,
    time::{timeout, timeout_at, Instant},
};

/// The only request this exchange ever makes.
pub const PATH: &str = "/api/hosting/handover/route";
/// The largest proof read. Checked as it streams, before it is held in memory.
pub const MAX_PROOF_BYTES: usize = 65_536;
/// Everything from creating the endpoint to the last byte of the answer.
pub const DEADLINE: Duration = Duration::from_secs(12);
/// Exchanges at once in one helper; more are refused, not queued.
pub const MAX_IN_FLIGHT: usize = 2;
/// The admission answer has its own, shorter wait inside the deadline.
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
/// How long closing the exchange's own endpoint may take after it ended.
const CLEANUP: Duration = Duration::from_secs(1);

/// Why an exchange gave no proof. Deliberately free of any value from the request
/// or the answer, so nothing a caller supplied can reach a log or an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProofError {
    /// The route or the challenge is not acceptable. Nothing was attempted.
    Invalid,
    /// Two exchanges are already running. Nothing was attempted.
    Busy,
    /// The host did not give a usable proof in time, for whatever reason.
    Failed,
}

/// Runs route-proof exchanges, at most `MAX_IN_FLIGHT` at a time.
pub struct Prover {
    slots: Arc<Semaphore>,
    deadline: Duration,
}

impl Default for Prover {
    fn default() -> Self {
        Self::new()
    }
}

impl Prover {
    pub fn new() -> Self {
        Self::with_deadline(DEADLINE)
    }

    /// A shorter deadline, for tests; the helper itself uses `DEADLINE`.
    pub fn with_deadline(deadline: Duration) -> Self {
        Self {
            slots: Arc::new(Semaphore::new(MAX_IN_FLIGHT)),
            deadline,
        }
    }

    /// Asks the host named by `spec` for its proof of `challenge`. Returns the
    /// host's JSON object exactly as sent, once it is known to be one, and nothing
    /// is concluded from it.
    pub async fn prove(
        &self,
        network: &Network,
        spec: RouteSpec,
        challenge: &str,
    ) -> Result<Bytes, ProofError> {
        if !valid_challenge(challenge) || !protocol::valid_token(&spec.token) {
            return Err(ProofError::Invalid);
        }
        let _slot = self
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| ProofError::Busy)?;
        let deadline = Instant::now() + self.deadline;
        // Owns the exchange's own endpoint, connection and task. If this future is
        // dropped (the caller went away), dropping the guard closes exactly those.
        let mut own = Own::default();
        let outcome = timeout_at(deadline, run(network, &spec, challenge, &mut own)).await;
        own.finish().await;
        match outcome {
            Ok(Ok(proof)) => Ok(proof),
            _ => Err(ProofError::Failed),
        }
    }
}

/// A challenge is exactly 32 random bytes in canonical unpadded base64url.
pub fn valid_challenge(challenge: &str) -> bool {
    challenge.len() == 43
        && URL_SAFE_NO_PAD
            .decode(challenge)
            .is_ok_and(|raw| raw.len() == 32 && URL_SAFE_NO_PAD.encode(&raw) == challenge)
}

fn addr(spec: &RouteSpec) -> EndpointAddr {
    let mut addr = EndpointAddr::new(spec.id);
    if let Some(relay) = &spec.relay {
        addr = addr.with_relay_url(relay.clone());
    }
    for direct in &spec.direct {
        addr = addr.with_ip_addr(*direct);
    }
    addr
}

/// The one request: a fixed method, path and headers, and a body that is only the
/// challenge. The host replaces the Host header for the community, as for any friend.
fn request(challenge: &str) -> Result<Request<Full<Bytes>>> {
    let body = format!("{{\"challenge\":\"{challenge}\"}}");
    Ok(Request::builder()
        .method(Method::POST)
        .uri(PATH)
        .header(HOST, "127.0.0.1")
        .header(CONTENT_TYPE, "application/json")
        .header(CONTENT_LENGTH, body.len())
        .header(CONNECTION, "close")
        .body(Full::new(Bytes::from(body)))?)
}

/// What one exchange owns. Nothing here is shared with any other route.
#[derive(Default)]
struct Own {
    endpoint: Option<Endpoint>,
    connection: Option<Connection>,
    driver: Option<JoinHandle<()>>,
}

impl Own {
    /// Ends the exchange's own task, connection and endpoint, within a short wait.
    async fn finish(&mut self) {
        if let Some(driver) = self.driver.take() {
            driver.abort();
        }
        if let Some(connection) = self.connection.take() {
            connection.close(0u32.into(), b"done");
        }
        if let Some(endpoint) = self.endpoint.take() {
            let _ = timeout(CLEANUP, endpoint.close()).await;
        }
    }
}

impl Drop for Own {
    fn drop(&mut self) {
        if let Some(driver) = self.driver.take() {
            driver.abort();
        }
        if let Some(connection) = self.connection.take() {
            connection.close(0u32.into(), b"cancelled");
        }
        // Only if the caller dropped the exchange before `finish`: close the endpoint
        // in a bounded task of its own, so nothing is left bound.
        if let Some(endpoint) = self.endpoint.take() {
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    let _ = timeout(CLEANUP, endpoint.close()).await;
                });
            }
        }
    }
}

async fn run(network: &Network, spec: &RouteSpec, challenge: &str, own: &mut Own) -> Result<Bytes> {
    let request = request(challenge)?;
    // A fresh key and no stored state: the host sees an unknown, one-off visitor.
    let endpoint = crate::bind(network, None, false).await?;
    own.endpoint = Some(endpoint.clone());
    let connection = endpoint
        .connect(addr(spec), ALPN)
        .await
        .context("the host did not answer")?;
    own.connection = Some(connection.clone());

    // Admission, exactly as any friend: a hello with the token, on the first stream.
    let (mut send, mut recv) = connection.open_bi().await?;
    let hello = protocol::encode_hello(&spec.token)
        .map_err(|_| anyhow::anyhow!("the route is not valid"))?;
    send.write_all(&hello).await?;
    send.finish()?;
    let accepted = timeout(HELLO_TIMEOUT, protocol::read_reply(&mut recv))
        .await
        .context("the host did not answer the admission")?
        .map_err(|_| anyhow::anyhow!("the host did not accept the admission"))?;
    if !accepted {
        bail!("the host did not accept the admission");
    }

    // The exchange itself: a new stream for this one request.
    let (send, recv) = connection.open_bi().await?;
    let (mut sender, driver) =
        hyper::client::conn::http1::handshake(TokioIo::new(tokio::io::join(recv, send))).await?;
    own.driver = Some(tokio::spawn(async move {
        let _ = driver.await;
    }));
    let response = sender.send_request(request).await?;
    if response.status() != StatusCode::OK {
        bail!("the host did not return a proof");
    }
    // A declared size over the limit is refused before reading; an undeclared or
    // dishonest one is cut off by the limit as it streams.
    if response
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok())
        .is_some_and(|length| length > MAX_PROOF_BYTES)
    {
        bail!("the proof is too large");
    }
    let proof = Limited::new(response.into_body(), MAX_PROOF_BYTES)
        .collect()
        .await
        .map_err(|_| anyhow::anyhow!("the proof could not be read"))?
        .to_bytes();
    // Only that it is a JSON object; what it says is the caller's to judge.
    if !serde_json::from_slice::<serde_json::Value>(&proof).is_ok_and(|value| value.is_object()) {
        bail!("the host did not return a proof object");
    }
    Ok(proof)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_challenge_is_exactly_32_bytes_of_canonical_base64url() {
        let good = URL_SAFE_NO_PAD.encode([7u8; 32]);
        assert!(valid_challenge(&good));
        for bad in [
            URL_SAFE_NO_PAD.encode([7u8; 31]),
            URL_SAFE_NO_PAD.encode([7u8; 33]),
            format!("{good}="),
            format!("{}+{}", &good[..20], &good[21..]),
            String::new(),
            "x".repeat(43),
            format!("{} ", &good[..42]),
        ] {
            assert!(!valid_challenge(&bad), "{bad:?}");
        }
    }

    #[test]
    fn the_request_is_one_fixed_post_with_no_credentials() {
        let challenge = URL_SAFE_NO_PAD.encode([9u8; 32]);
        let request = request(&challenge).unwrap();
        assert_eq!(request.method(), Method::POST);
        assert_eq!(request.uri().path(), "/api/hosting/handover/route");
        assert!(request.uri().query().is_none() && request.uri().authority().is_none());
        let names: Vec<_> = request.headers().keys().map(|k| k.as_str()).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(
            sorted,
            vec!["connection", "content-length", "content-type", "host"]
        );
        assert_eq!(request.headers()[CONNECTION], "close");
        assert_eq!(request.headers()[HOST], "127.0.0.1");
        for forbidden in [
            "authorization",
            "cookie",
            "upgrade",
            "origin",
            "x-relay-network-peer",
        ] {
            assert!(!request.headers().contains_key(forbidden));
        }
    }

    #[tokio::test]
    async fn invalid_input_and_a_full_helper_are_refused_before_anything_is_opened() {
        let prover = Prover::with_deadline(Duration::from_millis(50));
        let spec = |token: &str| RouteSpec {
            id: iroh::SecretKey::from_bytes(&[3u8; 32]).public(),
            relay: None,
            token: token.to_owned(),
            direct: Vec::new(),
        };
        let challenge = URL_SAFE_NO_PAD.encode([1u8; 32]);
        let token = "abcdefghijklmnopqrstuvwxyzABCDEF0123456789_-";
        assert_eq!(
            prover.prove(&Network::Loopback, spec(token), "short").await,
            Err(ProofError::Invalid)
        );
        assert_eq!(
            prover
                .prove(&Network::Loopback, spec("short"), &challenge)
                .await,
            Err(ProofError::Invalid)
        );
        // Both slots taken: a third is refused at once rather than queued.
        let _a = prover.slots.clone().try_acquire_owned().unwrap();
        let _b = prover.slots.clone().try_acquire_owned().unwrap();
        assert_eq!(
            prover
                .prove(&Network::Loopback, spec(token), &challenge)
                .await,
            Err(ProofError::Busy)
        );
    }
}
