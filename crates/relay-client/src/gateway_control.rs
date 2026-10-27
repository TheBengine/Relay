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
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Condvar, Mutex,
    },
    thread,
    time::{Duration, Instant},
};
use zeroize::{Zeroize, Zeroizing};

const ROOT: &str = "/api/hosting/voice-gateways";

/// The voice mesh between homes: gateway reports and each person's place.
mod mesh;
const PERIOD: Duration = Duration::from_millis(400);
const RETRY: Duration = Duration::from_secs(5);
const MAX_IDS: usize = 4096;

#[cfg(test)]
mod toggle_tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn rapid_reenable_retires_old_path_and_preserves_new_call() {
        let enabled = Arc::new(Toggle::new(true));
        let path = || {
            let now = Instant::now();
            Path::new("127.0.0.1:49899".parse().unwrap(), Grant {
                id: [13; 16], session: 7, key: Zeroizing::new([53; 32]),
                valid_for: Duration::from_secs(2),
            }, now, now).unwrap()
        };
        let old = Session::fixture(path(), enabled.clone());
        let admitted = enabled.revision();
        assert!(old.with_path(|p| p.id()).is_some());
        old.state.lock().unwrap().active = true;
        enabled.store(false, Ordering::Release);
        enabled.store(true, Ordering::Release);
        // Both worker loops could still be awaiting a broker response. The
        // audio path must reject their old admission without awaiting cleanup.
        assert!(!enabled.current(admitted));
        assert!(old.with_path(|p| p.id()).is_none());
        assert!(old.wrap_selected(&[0], Instant::now()).is_err());
        assert_eq!(old.select(Instant::now(), 0, true), (false, true));
        assert_eq!(old.state.lock().unwrap().seen.len(), 1);
        let new_call = Session::fixture(path(), enabled.clone());
        drop(old);
        assert!(enabled.load(Ordering::Acquire));
        assert!(new_call.with_path(|p| p.id()).is_some());
    }

    #[test]
    fn dormant_workers_wake_on_live_opt_in_and_call_stop() {
        for enable in [true, false] {
            let toggle = Arc::new(Toggle::new(false));
            let alive = Arc::new(AtomicBool::new(true));
            let (ready_tx, ready_rx) = mpsc::channel();
            let (done_tx, done_rx) = mpsc::channel();
            let mut workers = Vec::new();
            for _ in 0..2 {
                let toggle = toggle.clone();
                let alive = alive.clone();
                let ready = ready_tx.clone();
                let done = done_tx.clone();
                workers.push(thread::spawn(move || {
                    ready.send(()).unwrap();
                    toggle.wait_enabled(&|| alive.load(Ordering::Acquire));
                    done.send(()).unwrap();
                }));
            }
            for _ in 0..2 { ready_rx.recv_timeout(Duration::from_secs(2)).unwrap(); }
            let dormant = done_rx.recv_timeout(Duration::from_millis(40)).is_err();
            if enable {
                toggle.store(true, Ordering::Release);
            } else {
                // Session::drop sets the stop flag, then wakes both worker loops.
                alive.store(false, Ordering::Release);
                toggle.wake();
            }
            let first = done_rx.recv_timeout(Duration::from_secs(2));
            let second = done_rx.recv_timeout(Duration::from_secs(2));
            // Unpark before assertions so a failed regression leaves no worker.
            alive.store(false, Ordering::Release);
            toggle.wake();
            for worker in workers { worker.join().unwrap(); }
            assert!(dormant, "disabled workers must stay dormant");
            assert!(first.is_ok() && second.is_ok(), "both workers must wake");
        }
    }
}

/// The user's live opt-in. Dormant call workers sleep until it changes, without
/// polling the broker or opening a LAN/private gateway socket before consent.
pub(crate) struct Toggle {
    // Bit zero is the opt-in; the remaining bits change on every transition.
    // An off/on pair must retire old grants even during a pending HTTP request.
    revision: AtomicU64,
    wait_lock: Mutex<()>,
    changed: Condvar,
}
impl Toggle {
    pub fn new(enabled: bool) -> Self {
        Self { revision: AtomicU64::new(u64::from(enabled)), wait_lock: Mutex::new(()), changed: Condvar::new() }
    }
    pub fn load(&self, order: Ordering) -> bool { self.revision.load(order) & 1 != 0 }
    fn revision(&self) -> u64 { self.revision.load(Ordering::Acquire) }
    fn current(&self, revision: u64) -> bool { revision & 1 != 0 && self.revision() == revision }
    pub fn store(&self, enabled: bool, order: Ordering) {
        let _guard = self.wait_lock.lock().ok();
        let previous = self.revision();
        if (previous & 1 != 0) != enabled {
            self.revision.swap(previous.wrapping_add(2) ^ 1, order);
            self.changed.notify_all();
        }
    }
    fn wake(&self) {
        let _guard = self.wait_lock.lock().ok();
        self.changed.notify_all();
    }
    fn wait_enabled(&self, alive: &dyn Fn() -> bool) {
        let Ok(mut guard) = self.wait_lock.lock() else { return; };
        while alive() && !self.load(Ordering::Acquire) {
            let Ok(next) = self.changed.wait(guard) else { return; };
            guard = next;
        }
    }
    fn wait(&self, alive: &dyn Fn() -> bool, duration: Duration, revision: u64) {
        let Ok(guard) = self.wait_lock.lock() else { return; };
        if alive() && self.current(revision) {
            drop(self.changed.wait_timeout(guard, duration));
        }
    }
}

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
    mesh: RequestBuilder,
    mesh_member: RequestBuilder,
    hosting: RequestBuilder,
    measurements: RequestBuilder,
    upload_probe: RequestBuilder,
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
            mesh: request(Method::POST, &format!("{ROOT}/mesh")).ok()?,
            mesh_member: request(Method::POST, &format!("{ROOT}/mesh/member")).ok()?,
            hosting: request(Method::GET, "/api/hosting").ok()?,
            measurements: request(Method::POST, "/api/hosting/measurements").ok()?,
            upload_probe: request(Method::POST, "/api/hosting/measurements/upload").ok()?,
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
            mesh: self.mesh.try_clone()?,
            mesh_member: self.mesh_member.try_clone()?,
            hosting: self.hosting.try_clone()?,
            measurements: self.measurements.try_clone()?,
            upload_probe: self.upload_probe.try_clone()?,
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
    revision: u64,
    active: bool,
    seen: HashSet<[u8; 16]>,
}
pub(crate) struct Session {
    state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
    enabled: Arc<Toggle>,
    /// This person's place in the voice mesh between homes, if any.
    mesh: Arc<Mutex<mesh::Member>>,
    /// This machine's own round trip to the host, for a gateway's report.
    host_rtt_us: Arc<AtomicU64>,
    /// Incoming voice quality per speaker, for the host overview.
    heard: Arc<Mutex<Vec<(String, crate::voice_quality::Quality)>>>,
}
impl Session {
    #[cfg(test)]
    pub fn fixture(path: Path, enabled: Arc<Toggle>) -> Self {
        let mut state = State::default();
        state.seen.insert(path.id());
        state.path = Some(path);
        state.revision = enabled.revision();
        Self {
            state: Arc::new(Mutex::new(state)),
            stop: Arc::new(AtomicBool::new(false)),
            enabled,
            mesh: Arc::default(),
            host_rtt_us: Arc::default(),
            heard: Arc::default(),
        }
    }
    pub fn start(
        config: Config,
        upstream: SocketAddr,
        enabled: Arc<Toggle>,
        call_stop: Arc<AtomicBool>,
    ) -> Option<Self> {
        // An ordinary per-person loopback route must never be shared. The iroh
        // mode instead opens a dedicated independent endpoint in serve_loop.
        if !upstream.is_ipv4()
            || (upstream.ip().is_loopback() && config.private_route.is_none()) {
            return None;
        }
        let serving = config.copy()?;
        let meshing = config.copy()?;
        let measuring = config.copy()?;
        let heard: Arc<Mutex<Vec<(String, crate::voice_quality::Quality)>>> = Arc::default();
        let member = Arc::new(Mutex::new(mesh::Member::default()));
        let host_rtt_us = Arc::new(AtomicU64::new(0));
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
        let use_enabled = enabled.clone();
        thread::Builder::new()
            .name("voice-gateway-admission".into())
            .spawn(move || {
                lease_loop(config, use_state, use_valid, use_alive, use_enabled);
            })
            .ok()?;
        let serve_enabled = enabled.clone();
        let serve_rtt = host_rtt_us.clone();
        let (mesh_valid, mesh_alive, mesh_enabled, mesh_member) =
            (valid.clone(), alive.clone(), enabled.clone(), member.clone());
        let measure_alive = alive.clone();
        if thread::Builder::new()
            .name("voice-gateway-grants".into())
            .spawn(move || {
                serve_loop(serving, upstream, valid, alive, serve_enabled, serve_rtt);
            })
            .is_err()
            || thread::Builder::new()
                .name("voice-mesh-member".into())
                .spawn(move || {
                    mesh::member_loop(meshing, mesh_member, mesh_valid, mesh_alive, mesh_enabled);
                })
                .is_err()
            || {
                let (heard, rtt, alive) = (heard.clone(), host_rtt_us.clone(), measure_alive);
                thread::Builder::new()
                    .name("host-measurements".into())
                    .spawn(move || measurement_loop(measuring, heard, rtt, alive))
                    .is_err()
            }
        {
            stop.store(true, Ordering::Release);
            enabled.wake();
            return None;
        }
        Some(Self {
            state,
            stop,
            enabled,
            mesh: member,
            host_rtt_us,
            heard,
        })
    }
    pub fn with_path<T>(&self, f: impl FnOnce(&mut Path) -> T) -> Option<T> {
        if !self.enabled.load(Ordering::Acquire) || self.stop.load(Ordering::Acquire) {
            return None;
        }
        let mut state = self.state.lock().ok()?;
        if !self.enabled.current(state.revision) { return None; }
        Some(f(state.path.as_mut()?))
    }
    /// A change is committed by an authenticated keepalive before any audio.
    pub fn select(&self, now: Instant, previous_rtt: u64, failed: bool) -> (bool, bool) {
        let Ok(mut state) = self.state.lock() else {
            return (false, false);
        };
        let active = self.enabled.current(state.revision)
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
    fn mesh_allowed(&self) -> bool {
        self.enabled.load(Ordering::Acquire) && !self.stop.load(Ordering::Acquire)
    }
    /// The original protected frame for this person's mesh gateway, in
    /// addition to the ordinary send to the host. Empty when the mesh does not
    /// carry them now.
    pub fn mesh_frame(&self, protected: &[u8], now: Instant) -> Vec<(SocketAddr, Vec<u8>)> {
        if !self.mesh_allowed() || protected.first() != Some(&relay_protocol::AUDIO_PACKET_V3) {
            return Vec::new();
        }
        self.mesh.lock().map(|mut m| m.frame(protected, now)).unwrap_or_default()
    }
    pub fn mesh_probes(&self, now: Instant) -> Vec<(SocketAddr, Vec<u8>)> {
        if !self.mesh_allowed() {
            return Vec::new();
        }
        self.mesh.lock().map(|mut m| m.probes(now)).unwrap_or_default()
    }
    /// A datagram from a mesh hop: see mesh::Member::receive.
    pub fn mesh_receive(
        &self,
        from: SocketAddr,
        bytes: &[u8],
        deafened: bool,
        now: Instant,
    ) -> Option<Option<Vec<u8>>> {
        if !self.mesh_allowed() {
            return None;
        }
        self.mesh.lock().ok()?.receive(from, bytes, deafened, now)
    }
    pub fn set_host_rtt_us(&self, rtt_us: u64) {
        self.host_rtt_us.store(rtt_us, Ordering::Relaxed);
    }
    /// What this member hears from each speaker now, for the host overview.
    pub fn set_heard(&self, heard: Vec<(String, crate::voice_quality::Quality)>) {
        if let Ok(mut slot) = self.heard.lock() {
            *slot = heard;
        }
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
        if !self.enabled.current(state.revision) || self.stop.load(Ordering::Acquire) {
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
        self.enabled.wake();
        if let Ok(mut state) = self.state.lock() {
            state.path = None;
            state.active = false;
        }
        // Bounded HTTP requests finish off the audio/UI thread. Cleanup names
        // this exact session/instance/id, never a newer call with the same login.
    }
}

/// The host overview's real numbers (#19): this machine's capacity when its
/// member is a co-host, and voice quality to each co-host in the call. Runs
/// for the whole call, apart from the gateway opt-in; never on the audio path.
fn measurement_loop(
    config: Config,
    heard: Arc<Mutex<Vec<(String, crate::voice_quality::Quality)>>>,
    host_rtt_us: Arc<AtomicU64>,
    alive: Arc<dyn Fn() -> bool + Send + Sync>,
) {
    use crate::host_measurements as measure;
    const EVERY: Duration = Duration::from_secs(30);
    let mut cpu = measure::CpuSampler::default();
    let _ = cpu.sample();
    let mut next = Instant::now() + Duration::from_secs(10);
    let mut next_upload = Instant::now() + Duration::from_secs(20);
    while alive() {
        if Instant::now() < next {
            thread::sleep(Duration::from_millis(250));
            continue;
        }
        next = Instant::now() + EVERY;
        let Some(view): Option<relay_protocol::hosting::HostingView> =
            config.hosting.try_clone().and_then(reply)
        else {
            continue;
        };
        let cohost = view
            .candidates
            .iter()
            .any(|c| c.candidate.user_id == view.you && !c.candidate.effective().is_empty());
        if cohost && Instant::now() >= next_upload {
            next_upload = Instant::now() + Duration::from_secs(15 * 60);
            if let Some(request) = config.upload_probe.try_clone() {
                let started = Instant::now();
                let sent = request
                    .timeout(Duration::from_secs(20))
                    .body(vec![0u8; measure::UPLOAD_PROBE_BYTES])
                    .send()
                    .is_ok_and(|r| r.status().is_success());
                if let Some(kbps) =
                    sent.then(|| measure::kbps(measure::UPLOAD_PROBE_BYTES, started.elapsed())).flatten()
                {
                    measure::set_upload_kbps(kbps);
                }
            }
        }
        let capacity = cohost.then(|| relay_protocol::hosting::CapacityFacts {
            upload_kbps: measure::upload_kbps(),
            cpu_free_pct: cpu.sample(),
            free_disk_mb: crate::local_host::data_directory()
                .ok()
                .and_then(|dir| measure::free_disk_mb(&dir)),
            participants: 0,
        });
        let heard = heard.lock().map(|h| h.clone()).unwrap_or_default();
        let rtt_us = host_rtt_us.load(Ordering::Relaxed);
        let rtt_ms = (rtt_us > 0).then(|| rtt_us as f32 / 1000.0);
        if let Some(report) = measure::report(&view, &heard, rtt_ms, capacity) {
            discard(config.measurements.try_clone().map(|r| r.json(&report)));
        }
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
    enabled: Arc<Toggle>,
) {
    let mut offer: Option<wire::GatewayOffer> = None;
    let mut lease_id: Option<String> = None;
    let mut began = Instant::now();
    let mut next_list = Instant::now();
    let mut skip: Option<(String, Instant)> = None;
    let mut revision = enabled.revision();
    while alive() {
        if !valid() || !enabled.current(revision) {
            if let Ok(mut shared) = state.lock() {
                shared.path = None;
            }
            if let Some(id) = lease_id.take() {
                release(&config, &id);
            }
            offer = None;
            // A consent change is not evidence that this gateway failed.
            skip = None;
            next_list = Instant::now();
            revision = enabled.revision();
            enabled.wait_enabled(alive.as_ref());
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
            enabled.wait(alive.as_ref(), PERIOD, revision);
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
            if matching && valid() && enabled.current(revision) {
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
                        shared.revision = revision;
                    }
                    lease_id = Some(response.lease_id.clone());
                }
            } else if matching && wire::decode_id(&response.lease_id).is_some() {
                // A reply arriving after Stop belongs to this retired attempt,
                // even if the user has already enabled gateways again.
                release(&config, &response.lease_id);
            }
            response.key.zeroize();
        }
        // A missing renewal response does not revoke a still-valid admission.
        // Leave room for a retry; local expiry still fails closed in the data path.
        if transient && valid() && enabled.current(revision) {
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
        enabled.wait(alive.as_ref(), PERIOD, revision);
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
    enabled: Arc<Toggle>,
    host_rtt_us: Arc<AtomicU64>,
) {
    let mut backoff = RETRY;
    let mut nic_cursor = 0usize;
    while alive() {
        if !valid() {
            enabled.wait_enabled(alive.as_ref());
            continue;
        }
        let revision = enabled.revision();
        let valid = || valid() && enabled.current(revision);
        // Iroh gets its own helper client/voice socket; normal voice keeps its
        // existing endpoint, application address, sequence and admission.
        let private = match &config.private_route {
            Some((saved, server_id)) => match crate::iroh_route::PreparedRoute::open_voice_gateway(saved, server_id) {
                Ok(route) if alive() && valid() => Some(route),
                _ => { enabled.wait(alive.as_ref(), RETRY, revision); continue; }
            },
            None => None,
        };
        let fixed = match &private {
            Some(route) => match route.voice_gateway_address() {
                Ok(address) => address,
                Err(_) => { enabled.wait(alive.as_ref(), RETRY, revision); continue; }
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
        if binds.is_empty() { enabled.wait(alive.as_ref(), RETRY, revision); continue; }
        let local = binds[nic_cursor % binds.len()];
        nic_cursor = nic_cursor.wrapping_add(1);
        // The owned lease is declared before Worker, so packets stop before its
        // helper endpoint is closed on every exit from this serving attempt.
        let Ok(worker) = Worker::start(SocketAddr::new(local, 0), fixed) else {
            enabled.wait(alive.as_ref(), RETRY, revision);
            continue;
        };
        // The mesh between homes runs beside the LAN hop on the same NIC.
        let mut mesh = mesh::Gateway::start(local);
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
                            && v.max_grants <= 32 && valid() =>
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
                    enabled.wait(alive.as_ref(), PERIOD, revision);
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
            if let Some(gateway) = mesh.as_mut() {
                let rtt_us = host_rtt_us.load(Ordering::Relaxed);
                let rtt_ms = (rtt_us > 0).then(|| (rtt_us / 1000).clamp(1, 10_000) as u32);
                if !gateway.tick(&config, &instance, rtt_ms) {
                    mesh = None;
                }
            }
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
            enabled.wait(alive.as_ref(), PERIOD, revision);
        }
        drop(mesh);
        drop(worker);
        drop(private);
        discard(config.deregister.try_clone().map(|r| {
            r.query(&[
                ("instance", instance),
                ("session_id", config.session.to_string()),
            ])
        }));
        enabled.wait(alive.as_ref(), backoff, revision);
    }
}
