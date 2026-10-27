//! The voice mesh between homes, for one call: a gateway's report to the
//! server and the person's own place in the mesh.
//!
//! Nothing here decides who may forward or who is carried: the server's plan
//! does. A gateway applies exactly the approval, plan, links and member leases
//! the server returns for its own registered instance; a person uses exactly
//! the lease the server returns for their own seat. While anything is missing
//! or failing, frames still go to the shared host as before, and the host only
//! stops sending a pair once the server has seen the whole mesh carry it.
use super::*;
use crate::local_voice_gateway::mesh::{Approval, MeshClient, MeshWorker, Reach};
use relay_protocol::voice_mesh as plan;

/// A person's place in the mesh, shared by the control thread and the media worker.
#[derive(Default)]
pub(crate) struct Member {
    client: Option<MeshClient>,
    /// Every lease id this call used: a dropped id is never used again with
    /// fresh counters.
    used: HashSet<[u8; 16]>,
}

impl Member {
    /// Applies the server's answer. `None`: the mesh does not carry this person.
    fn assign(&mut self, assignment: Option<plan::MemberAssignment>, requested: Instant) {
        let Some(mut a) = assignment else {
            self.client = None;
            return;
        };
        let address = wire::parse_private_addr(&a.ip, a.port)
            .ok()
            .map(SocketAddr::V4)
            // Tests run every machine on loopback.
            .or_else(|| {
                cfg!(test)
                    .then(|| format!("{}:{}", a.ip, a.port).parse::<SocketAddr>().ok())
                    .flatten()
                    .filter(|a| a.ip().is_loopback())
            });
        let lease = grant(&a.lease_id, a.binding.media_session, &mut a.key, a.ttl_ms);
        let (Some(address), Some(lease)) = (address, lease) else {
            self.client = None;
            return;
        };
        let now = Instant::now();
        if let Some(client) = self.client.as_mut() {
            let (active, next) = client.lease_ids();
            let same_person = client.binding() == &a.binding && client.stream() == a.stream;
            if same_person && (active == lease.id || next == Some(lease.id)) {
                if !client.renew(lease, requested, now) {
                    self.client = None;
                }
                return;
            }
            if same_person && client.node() != a.node && self.used.insert(lease.id) {
                // A controlled switch: the old hop keeps carrying until the new one is proven.
                if !client.begin_switch(address, a.node, lease, requested, now) {
                    self.client = None;
                }
                return;
            }
        }
        if self.used.len() >= MAX_IDS || !self.used.insert(lease.id) {
            self.client = None;
            return;
        }
        self.client =
            MeshClient::new(&a.binding, a.stream, address, a.node, lease, requested, now);
    }

    fn healthy(&mut self, now: Instant) -> bool {
        let Some(client) = self.client.as_mut() else {
            return false;
        };
        if client.switching() {
            client.commit_switch(now);
        }
        client.healthy(now)
    }

    /// The original protected frame for the gateway, when the hop works.
    pub fn frame(&mut self, protected: &[u8], now: Instant) -> Vec<(SocketAddr, Vec<u8>)> {
        match self.client.as_mut() {
            Some(client) if client.healthy(now) => client.frame(protected, now),
            _ => Vec::new(),
        }
    }

    pub fn probes(&mut self, now: Instant) -> Vec<(SocketAddr, Vec<u8>)> {
        self.client
            .as_mut()
            .map(|client| client.probes(now))
            .unwrap_or_default()
    }

    /// `None`: not from a mesh hop. `Some(None)`: consumed (a health reply or
    /// a refused packet). `Some(Some(frame))`: the original protected frame.
    pub fn receive(
        &mut self,
        from: SocketAddr,
        bytes: &[u8],
        deafened: bool,
        now: Instant,
    ) -> Option<Option<Vec<u8>>> {
        let client = self.client.as_mut()?;
        if !client.serves(from) {
            return None;
        }
        client.set_deafened(deafened);
        Some(client.receive(from, bytes, now).map(|(_, frame)| frame))
    }
}

/// The person's side: asks for their place every period and says whether
/// their hop works. Runs on its own thread; never on the audio path.
pub(super) fn member_loop(
    config: Config,
    member: Arc<Mutex<Member>>,
    valid: Arc<dyn Fn() -> bool + Send + Sync>,
    alive: Arc<dyn Fn() -> bool + Send + Sync>,
    enabled: Arc<Toggle>,
) {
    let mut revision = enabled.revision();
    while alive() {
        if !valid() || !enabled.current(revision) {
            if let Ok(mut m) = member.lock() {
                m.client = None;
            }
            revision = enabled.revision();
            enabled.wait_enabled(alive.as_ref());
            continue;
        }
        let healthy = member.lock().is_ok_and(|mut m| m.healthy(Instant::now()));
        let requested = Instant::now();
        let body = plan::MemberReport {
            session_id: config.session,
            healthy,
        };
        let answer: Result<Option<plan::MemberAssignment>, Failure> = post(&config.mesh_member, &body);
        match answer {
            Ok(assignment) if valid() && enabled.current(revision) => {
                if let Ok(mut m) = member.lock() {
                    m.assign(assignment, requested);
                }
            }
            // A missed reply keeps a live lease; its own expiry still fails closed.
            Err(Failure::Transient) => {}
            _ => {
                if let Ok(mut m) = member.lock() {
                    m.client = None;
                }
            }
        }
        enabled.wait(alive.as_ref(), PERIOD, revision);
    }
}

/// A gateway's mesh during one serving attempt.
pub(super) struct Gateway {
    worker: MeshWorker,
    endpoint: Option<iroh_route_link::Endpoint>,
    /// Peer endpoint id -> loopback port of its link in the helper.
    ports: HashMap<String, u16>,
    /// Peer gateway id -> what its link measures now.
    measured: HashMap<String, (plan::HopQuality, bool)>,
    approved: Option<(plan::Binding, plan::NodeId)>,
    linked: HashMap<plan::NodeId, [u8; 16]>,
    admitted: HashMap<u32, [u8; 16]>,
}

/// The helper calls, kept apart so tests can run without a helper.
mod iroh_route_link {
    pub struct Endpoint {
        pub id: String,
        pub relay: Option<String>,
    }
}

impl Gateway {
    pub fn start(lan: IpAddr) -> Option<Self> {
        let worker = MeshWorker::start(SocketAddr::new(lan, 0)).ok()?;
        // Links between homes need the iroh helper. Without it the gateway
        // still keeps speech inside its own home local.
        let endpoint = crate::iroh_route::mesh_link_start(worker.link_address.port())
            .ok()
            .map(|info| iroh_route_link::Endpoint {
                id: info.endpoint_id,
                relay: info.relay_url,
            });
        Some(Self {
            worker,
            endpoint,
            ports: HashMap::new(),
            measured: HashMap::new(),
            approved: None,
            linked: HashMap::new(),
            admitted: HashMap::new(),
        })
    }

    /// One report and its answer. `false`: the server refused this gateway.
    pub fn tick(&mut self, config: &Config, instance: &str, host_rtt_ms: Option<u32>) -> bool {
        let status = self.worker.status();
        let report = plan::MeshReport {
            instance: instance.to_owned(),
            session_id: config.session,
            port: self.worker.address.port(),
            endpoint_id: self.endpoint.as_ref().map(|e| e.id.clone()),
            relay_url: self.endpoint.as_ref().and_then(|e| e.relay.clone()),
            upload_kbps: crate::host_measurements::upload_kbps(),
            host_rtt_ms,
            applied_epoch: status.applied_epoch,
            links: self
                .measured
                .iter()
                .take(plan::MAX_LINK_REPORTS)
                .map(|(gateway_id, (quality, up))| plan::LinkReport {
                    gateway_id: gateway_id.clone(),
                    quality: *quality,
                    up: *up,
                })
                .collect(),
        };
        let requested = Instant::now();
        let answer: Result<plan::MeshAssignment, Failure> = post(&config.mesh, &report);
        match answer {
            Ok(assignment) => {
                self.apply(assignment, instance, requested);
                true
            }
            Err(Failure::Transient) => true,
            Err(_) => {
                self.worker.withdraw();
                false
            }
        }
    }

    fn apply(&mut self, mut a: plan::MeshAssignment, instance: &str, requested: Instant) {
        // Links to other homes: allow exactly the offered peers and measure them.
        if self.endpoint.is_some() {
            let mut keep = Vec::new();
            self.measured.clear();
            for peer in a.peers.iter().take(plan::MAX_LINK_REPORTS) {
                let Ok(info) =
                    crate::iroh_route::mesh_link_peer(&peer.endpoint_id, peer.relay_url.as_deref())
                else {
                    continue;
                };
                keep.push(peer.endpoint_id.clone());
                self.ports.insert(peer.endpoint_id.clone(), info.port);
                let rtt_ms = info.rtt_ms.unwrap_or(0.0).clamp(0.0, 10_000.0).round() as u32;
                self.measured.insert(
                    peer.gateway_id.clone(),
                    (
                        plan::HopQuality {
                            rtt_ms,
                            loss_permille: 0,
                            jitter_ms: 0,
                        },
                        info.connected,
                    ),
                );
            }
            self.ports.retain(|id, _| keep.contains(id));
            let _ = crate::iroh_route::mesh_link_retain(&keep);
        }
        let assigned = match (&a.approval, &a.plan) {
            (Some(approval), Some(plan))
                if approval.instance == instance
                    && approval.binding == plan.binding
                    && plan.validate().is_ok()
                    && plan.nodes.contains(&approval.node)
                    && (1..=wire::LEASE_TTL_MS).contains(&approval.ttl_ms) =>
            {
                Some((approval.clone(), plan.clone()))
            }
            _ => None,
        };
        let Some((approval, plan)) = assigned else {
            if self.approved.take().is_some() {
                self.worker.withdraw();
            }
            self.linked.clear();
            self.admitted.clear();
            return;
        };
        let session = approval.binding.media_session;
        if self.approved.as_ref() != Some(&(approval.binding.clone(), approval.node)) {
            // A new binding or node: nothing installed before carries over.
            self.linked.clear();
            self.admitted.clear();
        }
        self.approved = Some((approval.binding.clone(), approval.node));
        self.worker.assign(
            Approval {
                node: approval.node,
                binding: approval.binding,
                valid_until: requested + Duration::from_millis(u64::from(approval.ttl_ms)),
            },
            plan,
        );
        let mut linked = HashSet::new();
        for link in &mut a.links {
            let Some(port) = self.ports.get(&link.endpoint_id).copied() else {
                link.key.zeroize();
                continue;
            };
            let Some(lease) = grant(&link.id, session, &mut link.key, link.ttl_ms) else {
                continue;
            };
            linked.insert(link.peer);
            self.linked.insert(link.peer, lease.id);
            self.worker.link(
                link.peer,
                SocketAddr::from(([127, 0, 0, 1], port)),
                lease,
                requested,
            );
        }
        let gone: Vec<plan::NodeId> = self
            .linked
            .keys()
            .filter(|peer| !linked.contains(peer))
            .copied()
            .collect();
        for peer in gone {
            self.worker.unlink(peer);
            self.linked.remove(&peer);
        }
        let mut admitted = HashSet::new();
        for member in &mut a.members {
            let Some(lease) = grant(&member.id, session, &mut member.key, member.ttl_ms) else {
                continue;
            };
            admitted.insert(member.stream);
            self.admitted.insert(member.stream, lease.id);
            self.worker.admit(member.stream, Reach::Lan, lease, requested);
        }
        let gone: Vec<u32> = self
            .admitted
            .keys()
            .filter(|stream| !admitted.contains(stream))
            .copied()
            .collect();
        for stream in gone {
            self.worker.revoke(stream);
            self.admitted.remove(&stream);
        }
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        if self.endpoint.is_some() {
            crate::iroh_route::mesh_link_stop();
        }
    }
}

#[cfg(test)]
mod tests;
