//! Bounded, call-scoped control plane for approved LAN voice gateways.
//! Only existing authenticated community or accepted Friends-call request factories
//! enter this module. The server binds every grant to the caller's exact live call.
//! Discovery packets cannot create a grant, select a route or supply a URL.
use crate::local_voice_gateway::{Grant, Path, Worker};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use relay_protocol::gateway_broker as wire;
use reqwest::{blocking::RequestBuilder, Method};
use serde::{de::DeserializeOwned, Serialize};
use std::{
    collections::{HashMap, HashSet},
    io::Read,
    net::{IpAddr, SocketAddr, UdpSocket},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};
use zeroize::{Zeroize, Zeroizing};

const ROOT: &str = "/api/hosting/voice-gateways";
const PERIOD: Duration = Duration::from_millis(400);
const RETRY: Duration = Duration::from_secs(5);
const MAX_IDS: usize = 4096;

/// Not Debug: request factories carry an ordinary authenticated session.
pub(crate) struct Config {
    session: u64,
    private_route: Option<(crate::iroh_route::SavedIroh, String)>,
    list: RequestBuilder,
    register: RequestBuilder,
    grants: RequestBuilder,
    lease: RequestBuilder,
    release: RequestBuilder,
    deregister: RequestBuilder,
}
impl Config {
    pub fn new(
        session: u64,
        request: impl Fn(Method, &str) -> Result<RequestBuilder, String>,
    ) -> Option<Self> {
        Some(Self {
            session,
            private_route: None,
            list: request(Method::GET, ROOT).ok()?,
            register: request(Method::POST, &format!("{ROOT}/registration")).ok()?,
            grants: request(Method::GET, &format!("{ROOT}/grants")).ok()?,
            lease: request(Method::POST, &format!("{ROOT}/leases")).ok()?,
            release: request(Method::DELETE, &format!("{ROOT}/leases")).ok()?,
            deregister: request(Method::DELETE, &format!("{ROOT}/registration")).ok()?,
        })
    }
    /// Only the authenticated call worker supplies this already pinned route.
    /// It is never a voice port borrowed from the user's ordinary connection.
    pub fn with_private_route(mut self, saved: crate::iroh_route::SavedIroh, server_id: String) -> Self {
        self.private_route = Some((saved, server_id));
        self
    }
    #[cfg(test)]
    pub(crate) fn prepared_list_for_test(&self) -> reqwest::blocking::Request {
        self.list.try_clone().unwrap().build().unwrap()
    }
    fn copy(&self) -> Option<Self> {
        Some(Self {
            session: self.session,
            private_route: self.private_route.clone(),
            list: self.list.try_clone()?,
            register: self.register.try_clone()?,
            grants: self.grants.try_clone()?,
            lease: self.lease.try_clone()?,
            release: self.release.try_clone()?,
            deregister: self.deregister.try_clone()?,
        })
    }
}

enum Failure {
    Transient,
    Refused,
    Invalid,
}
fn response<T: DeserializeOwned>(request: RequestBuilder) -> Result<T, Failure> {
    let response = request
        .timeout(Duration::from_millis(600))
        .send()
        .map_err(|_| Failure::Transient)?;
    if !response.status().is_success() {
        return Err(
            if response.status().is_server_error() || response.status().as_u16() == 429 {
                Failure::Transient
            } else {
                Failure::Refused
            },
        );
    }
    let mut bytes = Zeroizing::new(Vec::new());
    response
        .take(16_385)
        .read_to_end(&mut bytes)
        .map_err(|_| Failure::Transient)?;
    if bytes.len() > 16_384 {
        return Err(Failure::Invalid);
    }
    serde_json::from_slice(&bytes).map_err(|_| Failure::Invalid)
}
fn reply<T: DeserializeOwned>(request: RequestBuilder) -> Option<T> {
    response(request).ok()
}
fn post<T: DeserializeOwned, B: Serialize>(
    request: &RequestBuilder,
    body: &B,
) -> Result<T, Failure> {
    response(request.try_clone().ok_or(Failure::Invalid)?.json(body))
}
fn discard(request: Option<RequestBuilder>) {
    if let Some(request) = request {
        let _ = request.timeout(Duration::from_millis(600)).send();
    }
}
fn grant(id: &str, session: u64, key: &mut String, ttl: u32) -> Option<Grant> {
    let decoded = URL_SAFE_NO_PAD.decode(key.as_bytes()).ok();
    key.zeroize();
    let mut decoded = Zeroizing::new(decoded?);
    let bytes = <[u8; 32]>::try_from(decoded.as_slice()).ok()?;
    decoded.zeroize();
    if ttl == 0 || ttl > wire::LEASE_TTL_MS {
        return None;
    }
    Some(Grant {
        id: wire::decode_id(id)?,
        session,
        key: Zeroizing::new(bytes),
        valid_for: Duration::from_millis(ttl as u64),
    })
}

#[derive(Default)]
struct State {
    path: Option<Path>,
    active: bool,
    seen: HashSet<[u8; 16]>,
}
pub(crate) struct Session {
    state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
    enabled: Arc<AtomicBool>,
}
impl Session {
    #[cfg(test)]
    pub fn fixture(path: Path, enabled: Arc<AtomicBool>) -> Self {
        let mut state = State::default();
        state.seen.insert(path.id());
        state.path = Some(path);
        Self {
            state: Arc::new(Mutex::new(state)),
            stop: Arc::new(AtomicBool::new(false)),
            enabled,
        }
    }
    pub fn start(
        config: Config,
        upstream: SocketAddr,
        enabled: Arc<AtomicBool>,
        call_stop: Arc<AtomicBool>,
    ) -> Option<Self> {
        // An ordinary per-person loopback route must never be shared. The iroh
        // mode instead opens a dedicated independent endpoint in serve_loop.
        if !enabled.load(Ordering::Acquire) || !upstream.is_ipv4()
            || (upstream.ip().is_loopback() && config.private_route.is_none()) {
            return None;
        }
        let serving = config.copy()?;
        let state = Arc::new(Mutex::new(State::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let valid = {
            let stop = stop.clone();
            let enabled = enabled.clone();
            let call_stop = call_stop.clone();
            Arc::new(move || {
                !stop.load(Ordering::Acquire)
                    && !call_stop.load(Ordering::Acquire)
                    && enabled.load(Ordering::Acquire)
            })
        };
        let alive = {
            let stop = stop.clone();
            Arc::new(move || !stop.load(Ordering::Acquire) && !call_stop.load(Ordering::Acquire))
        };
        let use_state = state.clone();
        let use_valid = valid.clone();
        let use_alive = alive.clone();
        thread::Builder::new()
            .name("voice-gateway-admission".into())
            .spawn(move || {
                lease_loop(config, use_state, use_valid, use_alive);
            })
            .ok()?;
        if thread::Builder::new()
            .name("voice-gateway-grants".into())
            .spawn(move || {
                serve_loop(serving, upstream, valid, alive);
            })
            .is_err()
        {
            stop.store(true, Ordering::Release);
            return None;
        }
        Some(Self {
            state,
            stop,
            enabled,
        })
    }
    pub fn with_path<T>(&self, f: impl FnOnce(&mut Path) -> T) -> Option<T> {
        if !self.enabled.load(Ordering::Acquire) || self.stop.load(Ordering::Acquire) {
            return None;
        }
        let mut state = self.state.lock().ok()?;
        Some(f(state.path.as_mut()?))
    }
    /// A change is committed by an authenticated keepalive before any audio.
    pub fn select(&self, now: Instant, previous_rtt: u64, failed: bool) -> (bool, bool) {
        let Ok(mut state) = self.state.lock() else {
            return (false, false);
        };
        let active = self.enabled.load(Ordering::Acquire)
            && !self.stop.load(Ordering::Acquire)
            && state
                .path
                .as_ref()
                .is_some_and(|path| path.usable(now, state.active, previous_rtt, failed));
        let changed = active != state.active;
        state.active = active;
        (active, changed)
    }
    pub fn active(&self) -> bool {
        self.state.lock().is_ok_and(|state| state.active)
    }
    /// Err means the selected path expired mid-batch. Drop that packet; the next
    /// loop commits fallback first, rather than sending audio to an unbound path.
    pub fn wrap_selected(
        &self,
        bytes: &[u8],
        now: Instant,
    ) -> Result<Option<(SocketAddr, Vec<u8>)>, ()> {
        let mut state = self.state.lock().map_err(|_| ())?;
        if !state.active {
            return Ok(None);
        }
        if !self.enabled.load(Ordering::Acquire) {
            return Err(());
        }
        let path = state.path.as_mut().ok_or(())?;
        if !path.usable(now, true, 0, false) {
            return Err(());
        }
        Ok(Some((path.address, path.wrap(bytes, now).ok_or(())?)))
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Ok(mut state) = self.state.lock() {
            state.path = None;
            state.active = false;
        }
        // Bounded HTTP requests finish off the audio/UI thread. Cleanup names
        // this exact session/instance/id, never a newer call with the same login.
    }
}

fn wait(alive: &dyn Fn() -> bool, duration: Duration) {
    let end = Instant::now() + duration;
    while alive() && Instant::now() < end {
        thread::sleep(Duration::from_millis(50));
    }
}
fn release(config: &Config, id: &str) {
    discard(config.release.try_clone().map(|r| {
        r.query(&[
            ("session_id", config.session.to_string()),
            ("lease_id", id.to_owned()),
        ])
    }));
}

fn lease_loop(
    config: Config,
    state: Arc<Mutex<State>>,
    valid: Arc<dyn Fn() -> bool + Send + Sync>,
    alive: Arc<dyn Fn() -> bool + Send + Sync>,
) {
    let mut offer: Option<wire::GatewayOffer> = None;
    let mut lease_id: Option<String> = None;
    let mut began = Instant::now();
    let mut next_list = Instant::now();
    let mut skip: Option<(String, Instant)> = None;
    while alive() {
        if !valid() {
            if let Ok(mut shared) = state.lock() {
                shared.path = None;
            }
            if let Some(id) = lease_id.take() {
                release(&config, &id);
            }
            offer = None;
            wait(alive.as_ref(), PERIOD);
            continue;
        }
        if offer.is_none() && Instant::now() >= next_list {
            next_list = Instant::now() + RETRY;
            let subnets = crate::invitation_probe::local_subnets();
            let offers: Option<wire::GatewayList> = config.list.try_clone().and_then(reply);
            if let Some(list) = offers.filter(|v| v.gateways.len() <= 16) {
                offer = list.gateways.into_iter().find(|v| {
                    wire::valid_instance(&v.instance)
                        && skip
                            .as_ref()
                            .is_none_or(|(id, until)| id != &v.instance || Instant::now() >= *until)
                        && wire::parse_private_addr(&v.ip, v.port).is_ok_and(|a| {
                            crate::invitation_probe::allowed_ip(IpAddr::V4(*a.ip()), &subnets)
                        })
                });
                began = Instant::now();
            }
        }
        let Some(candidate) = offer.as_ref() else {
            wait(alive.as_ref(), PERIOD);
            continue;
        };
        let requested = Instant::now();
        // Include the exact call seat. A delayed request cannot lease a new call.
        let body = serde_json::json!({"gateway_id":candidate.gateway_id,"instance":candidate.instance,"session_id":config.session});
        let response: Result<wire::LeaseGrant, Failure> = post(&config.lease, &body);
        let transient = matches!(&response, Err(Failure::Transient));
        let mut accepted = false;
        if let Ok(mut response) = response {
            let address = wire::parse_private_addr(&response.ip, response.port)
                .ok()
                .map(SocketAddr::V4);
            let expected = wire::parse_private_addr(&candidate.ip, candidate.port)
                .ok()
                .map(SocketAddr::V4);
            let matching = response.session_id == config.session
                && response.gateway_id == candidate.gateway_id
                && response.instance == candidate.instance
                && address == expected;
            if matching && valid() {
                if let Some(grant) = grant(
                    &response.lease_id,
                    response.session_id,
                    &mut response.key,
                    response.ttl_ms,
                ) {
                    if let Ok(mut shared) = state.lock() {
                        if let Some(path) = shared.path.as_mut().filter(|p| p.id() == grant.id) {
                            accepted = path.renew(&grant, requested, Instant::now());
                        } else if shared.seen.len() < MAX_IDS && shared.seen.insert(grant.id) {
                            shared.path = address
                                .and_then(|addr| Path::new(addr, grant, requested, Instant::now()));
                            accepted = shared.path.is_some();
                            if accepted {
                                began = Instant::now();
                            }
                        }
                    }
                    lease_id = Some(response.lease_id.clone());
                }
            }
            response.key.zeroize();
        }
        // A missing renewal response does not revoke a still-valid admission.
        // Leave room for a retry; local expiry still fails closed in the data path.
        if transient && valid() {
            accepted = state.lock().is_ok_and(|s| {
                s.path
                    .as_ref()
                    .is_some_and(|p| p.remaining(Instant::now()) > Duration::from_millis(300))
            });
        }
        // A local hop alone never qualifies. After a bounded attempt, rotate or
        // fall back. Keep the same Path on live renewals to retain nonce state.
        let tried = Instant::now().saturating_duration_since(began) > Duration::from_secs(3);
        let proven = state.lock().is_ok_and(|s| {
            s.path
                .as_ref()
                .is_some_and(|p| p.usable(Instant::now(), true, 0, false))
        });
        if !accepted || (tried && !proven) {
            if let Ok(mut shared) = state.lock() {
                shared.path = None;
            }
            if let Some(id) = lease_id.take() {
                release(&config, &id);
            }
            skip = Some((
                candidate.instance.clone(),
                Instant::now() + Duration::from_secs(15),
            ));
            offer = None;
            next_list = Instant::now() + PERIOD;
        }
        wait(alive.as_ref(), PERIOD);
    }
    if let Some(id) = lease_id {
        release(&config, &id);
    }
}

fn private_interfaces(subnets: &[crate::invitation_probe::Subnet]) -> Vec<IpAddr> {
    let mut out = Vec::new();
    for subnet in subnets {
        let crate::invitation_probe::Subnet(IpAddr::V4(ip), _) = *subnet else { continue; };
        if ip.is_private() && !out.contains(&IpAddr::V4(ip)) && out.len() < 8 { out.push(IpAddr::V4(ip)); }
    }
    out
}
fn serve_loop(
    config: Config,
    upstream: SocketAddr,
    valid: Arc<dyn Fn() -> bool + Send + Sync>,
    alive: Arc<dyn Fn() -> bool + Send + Sync>,
) {
    let mut backoff = RETRY;
    let mut nic_cursor = 0usize;
    while alive() {
        if !valid() {
            wait(alive.as_ref(), PERIOD);
            continue;
        }
        // Iroh gets its own helper client/voice socket; normal voice keeps its
        // existing endpoint, application address, sequence and admission.
        let private = match &config.private_route {
            Some((saved, server_id)) => match crate::iroh_route::PreparedRoute::open_voice_gateway(saved, server_id) {
                Ok(route) if alive() && valid() => Some(route),
                _ => { wait(alive.as_ref(), RETRY); continue; }
            },
            None => None,
        };
        let fixed = match &private {
            Some(route) => match route.voice_gateway_address() {
                Ok(address) => address,
                Err(_) => { wait(alive.as_ref(), RETRY); continue; }
            },
            None => upstream,
        };
        let binds = if private.is_some() {
            private_interfaces(&crate::invitation_probe::local_subnets())
        } else {
            // The direct mode still uses the NIC the OS routes to the real server.
            UdpSocket::bind("0.0.0.0:0").and_then(|s| {s.connect(upstream)?; s.local_addr()}).ok()
                .filter(|a|matches!(a.ip(),IpAddr::V4(ip) if ip.is_private())).map(|a|vec![a.ip()]).unwrap_or_default()
        };
        if binds.is_empty() { wait(alive.as_ref(), RETRY); continue; }
        let local = binds[nic_cursor % binds.len()];
        nic_cursor = nic_cursor.wrapping_add(1);
        // The owned lease is declared before Worker, so packets stop before its
        // helper endpoint is closed on every exit from this serving attempt.
        let Ok(worker) = Worker::start(SocketAddr::new(local, 0), fixed) else {
            wait(alive.as_ref(), RETRY);
            continue;
        };
        let mut private_renewal = Instant::now() + Duration::from_secs(30);
        let mut idle_until = Instant::now() + Duration::from_secs(12);
        let mut random = [0; 16];
        if getrandom::getrandom(&mut random).is_err() || random == [0; 16] {
            return;
        }
        let instance = wire::encode_id(&random);
        let mut renewal = Instant::now();
        let mut registered_until = Instant::now();
        let mut missed_polls = 0u8;
        let mut installed: HashMap<u64, ([u8; 16], Instant)> = HashMap::new();
        while alive() && valid() {
            if Instant::now() >= renewal {
                let body = serde_json::json!({"instance":instance,"session_id":config.session,"ip":worker.address.ip().to_string(),"port":worker.address.port()});
                let requested = Instant::now();
                let result: Result<wire::Registered, Failure> = post(&config.register, &body);
                match result {
                    Ok(v)
                        if v.instance == instance
                            && v.session_id == config.session
                            && (30..=wire::REGISTRATION_TTL_SECS).contains(&v.expires_in_secs)
                            && v.max_grants <= 32 =>
                    {
                        registered_until = requested + Duration::from_secs(v.expires_in_secs);
                        renewal = Instant::now() + Duration::from_secs(30);
                        backoff = RETRY;
                    }
                    Err(Failure::Transient) if registered_until > Instant::now() + PERIOD => {
                        renewal = Instant::now() + PERIOD;
                    }
                    Err(Failure::Refused) => {
                        // Most members may USE gateways without being allowed to
                        // SERVE one. Do not churn sockets/requests for that case.
                        backoff = Duration::from_secs(120);
                        break;
                    }
                    _ => break,
                }
            }
            let requested = Instant::now();
            let grants: Result<wire::GatewayGrants, Failure> = config
                .grants
                .try_clone()
                .ok_or(Failure::Invalid)
                .and_then(|r| response(r.query(&[("instance", &instance)])));
            let mut grants = match grants {
                Ok(g) if g.grants.len() <= 32 && valid() => {
                    missed_polls = 0;
                    g
                }
                Err(Failure::Transient)
                    if missed_polls < 2
                        && valid()
                        && installed
                            .values()
                            .all(|(_, end)| *end > Instant::now() + Duration::from_millis(300)) =>
                {
                    missed_polls += 1;
                    wait(alive.as_ref(), PERIOD);
                    continue;
                }
                _ => break,
            };
            let mut present = HashSet::new();
            let mut okay = true;
            for item in &mut grants.grants {
                let Some(grant) = grant(&item.id, item.session, &mut item.key, item.ttl_ms) else {
                    okay = false;
                    continue;
                };
                if requested + grant.valid_for <= Instant::now()
                    || installed
                        .get(&grant.session)
                        .is_some_and(|(id, end)| *id == grant.id && Instant::now() >= *end)
                {
                    // Conservative local expiry requires a NEW worker instance,
                    // never reinstalling a used id with reset counters.
                    okay = false;
                    continue;
                }
                present.insert(grant.session);
                installed.insert(grant.session, (grant.id, requested + grant.valid_for));
                okay &= worker.grant(grant, requested);
            }
            let missing: Vec<_> = installed
                .keys()
                .filter(|s| !present.contains(s))
                .copied()
                .collect();
            for session in missing {
                okay &= worker.revoke(session);
                installed.remove(&session);
            }
            if !okay { break; }
            // A gateway is renewed only after this exact account/call's grant
            // poll succeeded. Failed auth, ended calls and disabled opt-in leave
            // the loop; no keeper thread can keep a stale private route alive.
            if let Some(route) = &private {
                if Instant::now() >= private_renewal {
                    if !valid() || route.renew_voice_gateway().is_err() { break; }
                    private_renewal = Instant::now() + Duration::from_secs(30);
                }
                if worker.take_peer_activity() { idle_until = Instant::now() + Duration::from_secs(12); }
                // On several NICs, try another *own* address only while nobody
                // uses this instance. Every path is measured before selection.
                if binds.len() > 1 && Instant::now() >= idle_until { break; }
            }
            wait(alive.as_ref(), PERIOD);
        }
        drop(worker);
        drop(private);
        discard(config.deregister.try_clone().map(|r| {
            r.query(&[
                ("instance", instance),
                ("session_id", config.session.to_string()),
            ])
        }));
        wait(alive.as_ref(), backoff);
    }
}
