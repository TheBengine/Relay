//! The server's side of the voice mesh between homes ("two households").
//!
//! Registered community gateways in one voice channel report their mesh port,
//! their iroh endpoint, their measured upload and their links to each other.
//! From that the server builds one route plan per channel and hands out:
//!
//! * to each gateway in the plan: an approval bound to community, channel and
//!   a fresh media session, the plan, one key per link to a tree neighbour
//!   (both ends get the same id and key), and the leases of its own members;
//! * to each person in voice whose home gateway is in the plan (the gateway's
//!   own seat, and whoever holds a live LAN lease from it): their stream id and
//!   their own mesh lease.
//!
//! Every key is fresh, short (`LEASE`), renewed only by the reports of a live,
//! still-valid registration or seat, and dropped at once with the role, the
//! registration, the lease or the seat.
//!
//! The voice relay skips sending a frame to a person only while the mesh
//! verifiably carries both ends: every gateway of the plan reported within
//! `FRESH`, applied the current epoch and sees all its tree links up, and both
//! people reported a working hop. Anything less, and the shared host delivers
//! as before; the coverage expires on its own after `READY`.
use super::*;
use relay_protocol::voice_mesh::{
    self as wire_mesh, Binding, Candidate, Gateway, HopQuality, NodeId, RoutePlan, Topology,
};
use std::collections::HashSet;

/// A gateway report older than this no longer counts.
const FRESH: StdDuration = StdDuration::from_millis(1500);
/// Coverage lapses on its own this long after the last good report.
const READY: StdDuration = StdDuration::from_millis(1200);

struct Key {
    id: [u8; 16],
    key: Zeroizing<[u8; 32]>,
    expires: Instant,
}
impl Key {
    /// Keeps a live key and restarts its validity; issues a fresh one otherwise.
    fn renew(slot: &mut Option<Key>, now: Instant) {
        match slot {
            Some(key) if key.expires > now => key.expires = now + LEASE,
            _ => {
                *slot = Some(Key {
                    id: fresh(),
                    key: Zeroizing::new(fresh()),
                    expires: now + LEASE,
                })
            }
        }
    }
    fn ttl_ms(&self, now: Instant) -> u32 {
        (self.expires.saturating_duration_since(now).as_millis() as u32).min(wire::LEASE_TTL_MS)
    }
    fn text(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.key.as_slice())
    }
}

struct Node {
    node: NodeId,
    instance: [u8; 16],
    session_id: u64,
    port: u16,
    endpoint_id: Option<String>,
    relay_url: Option<String>,
    upload_kbps: Option<u32>,
    host_rtt_ms: Option<u32>,
    applied: Option<u32>,
    links: HashMap<String, (HopQuality, bool)>,
    at: Instant,
}

struct Person {
    user: String,
    stream: u32,
    key: Option<Key>,
    healthy_until: Option<Instant>,
}

struct ChannelMesh {
    media_session: u64,
    epoch: u32,
    nodes: HashMap<String, Node>,
    people: HashMap<u64, Person>,
    next_stream: u32,
    links: HashMap<(String, String), Option<Key>>,
    plan: Option<RoutePlan>,
}

#[derive(Default)]
pub(super) struct Meshes {
    channels: HashMap<String, ChannelMesh>,
}

/// Sessions the mesh carries now. Read for every voice packet, so it is its
/// own small lock; whoever holds it never takes another one.
#[derive(Default)]
pub(crate) struct Coverage {
    by_session: HashMap<u64, (String, Instant)>,
}
impl Coverage {
    fn set(&mut self, channel: &str, sessions: HashSet<u64>, until: Instant) {
        self.by_session.retain(|_, (c, _)| c != channel);
        for session in sessions {
            self.by_session.insert(session, (channel.to_owned(), until));
        }
    }
    fn clear(&mut self, channel: &str) {
        self.by_session.retain(|_, (c, _)| c != channel);
    }
}

fn pair(a: &str, b: &str) -> (String, String) {
    if a < b {
        (a.to_owned(), b.to_owned())
    } else {
        (b.to_owned(), a.to_owned())
    }
}

fn worse(a: HopQuality, b: HopQuality) -> HopQuality {
    HopQuality {
        rtt_ms: a.rtt_ms.max(b.rtt_ms),
        loss_permille: a.loss_permille.max(b.loss_permille),
        jitter_ms: a.jitter_ms.max(b.jitter_ms),
    }
}

impl Book {
    /// Rebuilds one channel's plan from fresh gateway reports, live
    /// registrations, live LAN leases and live seats. Returns the sessions
    /// the mesh may carry now.
    fn mesh_refresh(
        &mut self,
        channel: &str,
        community: &str,
        seats: &HashMap<u64, voice::VoiceSession>,
        now: Instant,
    ) -> HashSet<u64> {
        let at = crate::now();
        let registrations = &self.gateways;
        let Some(mesh) = self.meshes.channels.get_mut(channel) else {
            return HashSet::new();
        };
        mesh.nodes.retain(|user, node| {
            node.at + FRESH > now
                && registrations.get(user).is_some_and(|r| {
                    r.instance == node.instance
                        && r.session_id == node.session_id
                        && r.channel_id == channel
                        && r.call_id.is_none()
                        && r.expires > now
                })
        });
        if mesh.nodes.is_empty() {
            self.meshes.channels.remove(channel);
            return HashSet::new();
        }
        // Home members: each gateway's own seat and its live LAN leases.
        let mut homes: Vec<(u64, String, NodeId)> = Vec::new();
        for (user, node) in &mesh.nodes {
            homes.push((node.session_id, user.clone(), node.node));
        }
        for lease in self.leases.values() {
            if let Some(node) = mesh.nodes.get(&lease.gateway) {
                if lease.expires > now
                    && lease.gateway_instance == node.instance
                    && lease.channel_id == channel
                    && lease.call_id.is_none()
                {
                    homes.push((lease.session_id, lease.client_user.clone(), node.node));
                }
            }
        }
        homes.retain(|(session, user, _)| {
            seats.get(session).is_some_and(|s| {
                s.user.id == *user
                    && s.channel_id == channel
                    && s.private_call_id.is_none()
                    && s.alive_at(at)
            })
        });
        homes.sort();
        homes.dedup_by_key(|(session, _, _)| *session);
        mesh.people.retain(|session, person| {
            homes
                .iter()
                .any(|(s, u, _)| s == session && *u == person.user)
        });
        for (session, user, _) in &homes {
            if !mesh.people.contains_key(session) {
                mesh.next_stream = mesh.next_stream.wrapping_add(1).max(1);
                mesh.people.insert(
                    *session,
                    Person {
                        user: user.clone(),
                        stream: mesh.next_stream,
                        key: None,
                        healthy_until: None,
                    },
                );
            }
        }
        let binding = Binding {
            community_id: community.to_owned(),
            channel_id: channel.to_owned(),
            media_session: mesh.media_session,
        };
        let users: Vec<&String> = mesh.nodes.keys().collect();
        let mut trunks = Vec::new();
        for (i, a) in users.iter().enumerate() {
            for b in &users[i + 1..] {
                let (x, y) = (&mesh.nodes[*a], &mesh.nodes[*b]);
                if let (Some((qa, true)), Some((qb, true))) = (x.links.get(*b), y.links.get(*a)) {
                    trunks.push(wire_mesh::Trunk {
                        a: x.node,
                        b: y.node,
                        quality: worse(*qa, *qb),
                    });
                }
            }
        }
        let topology = Topology {
            binding: binding.clone(),
            epoch: mesh.epoch,
            gateways: mesh
                .nodes
                .iter()
                .map(|(user, node)| Gateway {
                    id: user.clone(),
                    node: node.node,
                    approval: Some(binding.clone()),
                    uplink: node.host_rtt_ms.map(|rtt_ms| HopQuality {
                        rtt_ms,
                        loss_permille: 0,
                        jitter_ms: 0,
                    }),
                    // Unmeasured upload: home-local speech only.
                    upload_kbps: Some(node.upload_kbps.unwrap_or(0)),
                })
                .collect(),
            trunks,
            members: homes
                .iter()
                .map(|(session, _, node)| {
                    let seat = &seats[session];
                    wire_mesh::Member {
                        id: session.to_string(),
                        stream: mesh.people[session].stream,
                        home: Some(*node),
                        candidates: vec![Candidate {
                            node: *node,
                            first_hop: HopQuality::LAN,
                        }],
                        muted: seat.state.muted || !seat.can_speak,
                        deafened: seat.state.deafened,
                    }
                })
                .collect(),
            host_relay: false,
            speakers: 0,
        };
        let plan = RoutePlan::build(&topology)
            .ok()
            .filter(|p| !p.members.is_empty());
        let changed = match (&plan, &mesh.plan) {
            (Some(new), Some(old)) => {
                let mut same = new.clone();
                same.epoch = old.epoch;
                same != *old
            }
            (None, None) => false,
            _ => true,
        };
        if changed {
            mesh.epoch = mesh.epoch.wrapping_add(1);
        }
        mesh.plan = plan.map(|mut p| {
            p.epoch = mesh.epoch;
            p
        });
        let Some(plan) = &mesh.plan else {
            mesh.links.clear();
            return HashSet::new();
        };
        let user_of = |node: NodeId| {
            mesh.nodes
                .iter()
                .find(|(_, n)| n.node == node)
                .map(|(u, _)| u.clone())
        };
        let edges: HashSet<(String, String)> = plan
            .edges
            .iter()
            .filter_map(|e| Some(pair(&user_of(e.a)?, &user_of(e.b)?)))
            .collect();
        mesh.links.retain(|k, _| edges.contains(k));
        // Coverage: the whole tree must be up, applied and fresh.
        let tree_ready = plan.nodes.iter().all(|node| {
            let Some(user) = user_of(*node) else {
                return false;
            };
            let state = &mesh.nodes[&user];
            state.applied == Some(mesh.epoch)
                && plan.neighbors(*node).into_iter().all(|peer| {
                    user_of(peer)
                        .is_some_and(|peer| state.links.get(&peer).is_some_and(|(_, up)| *up))
                })
        });
        if !tree_ready {
            return HashSet::new();
        }
        plan.members
            .iter()
            .filter_map(|m| {
                let session: u64 = m.id.parse().ok()?;
                let person = mesh.people.get(&session)?;
                (person.healthy_until.is_some_and(|until| until > now)
                    && person.key.as_ref().is_some_and(|k| k.expires > now))
                .then_some(session)
            })
            .collect()
    }

    /// Forget everything a member had in any mesh: their gateway node and
    /// their own place. Returns the channels that changed.
    fn mesh_forget(&mut self, user: &str) -> Vec<String> {
        let mut changed = Vec::new();
        for (channel, mesh) in self.meshes.channels.iter_mut() {
            let before = (mesh.nodes.len(), mesh.people.len());
            mesh.nodes.remove(user);
            mesh.people.retain(|_, p| p.user != user);
            if before != (mesh.nodes.len(), mesh.people.len()) {
                changed.push(channel.clone());
            }
        }
        changed
    }

    fn assignment(&mut self, channel: &str, user: &str, now: Instant) -> wire_mesh::MeshAssignment {
        let empty = wire_mesh::MeshAssignment {
            approval: None,
            plan: None,
            links: Vec::new(),
            members: Vec::new(),
            peers: Vec::new(),
        };
        let Some(mesh) = self.meshes.channels.get_mut(channel) else {
            return empty;
        };
        let Some(me) = mesh.nodes.get(user) else {
            return empty;
        };
        let (node, instance) = (me.node, me.instance);
        let peers = mesh
            .nodes
            .iter()
            .filter(|(other, _)| other.as_str() != user)
            .filter_map(|(other, n)| {
                Some(wire_mesh::PeerOffer {
                    gateway_id: other.clone(),
                    node: n.node,
                    endpoint_id: n.endpoint_id.clone()?,
                    relay_url: n.relay_url.clone(),
                })
            })
            .collect::<Vec<_>>();
        let Some(plan) = mesh.plan.clone().filter(|p| p.nodes.contains(&node)) else {
            return wire_mesh::MeshAssignment { peers, ..empty };
        };
        let mut links = Vec::new();
        for peer in plan.neighbors(node) {
            let Some((peer_user, peer_state)) = mesh.nodes.iter().find(|(_, n)| n.node == peer)
            else {
                continue;
            };
            let Some(endpoint_id) = peer_state.endpoint_id.clone() else {
                continue;
            };
            let relay_url = peer_state.relay_url.clone();
            let slot = mesh.links.entry(pair(user, peer_user)).or_insert(None);
            Key::renew(slot, now);
            let key = slot.as_ref().expect("renewed");
            links.push(wire_mesh::TrunkGrant {
                id: wire::encode_id(&key.id),
                peer,
                peer_gateway: peer_user.clone(),
                endpoint_id,
                relay_url,
                key: key.text(),
                ttl_ms: key.ttl_ms(now),
            });
        }
        let members = plan
            .members
            .iter()
            .filter(|m| m.node == node)
            .filter_map(|m| {
                let person = mesh.people.get(&m.id.parse().ok()?)?;
                let key = person.key.as_ref().filter(|k| k.expires > now)?;
                Some(wire_mesh::MemberGrant {
                    id: wire::encode_id(&key.id),
                    stream: person.stream,
                    key: key.text(),
                    ttl_ms: key.ttl_ms(now),
                })
            })
            .collect();
        wire_mesh::MeshAssignment {
            approval: Some(wire_mesh::MeshApproval {
                gateway_id: user.to_owned(),
                instance: wire::encode_id(&instance),
                binding: plan.binding.clone(),
                node,
                epoch: plan.epoch,
                ttl_ms: wire::LEASE_TTL_MS,
            }),
            plan: Some(plan),
            links,
            members,
            peers,
        }
    }
}

impl Broker {
    /// Whether the mesh carries the frame from `sender` to `recipient` now, so
    /// the voice relay need not send it.
    pub(crate) fn mesh_carries(&self, sender: u64, recipient: u64) -> bool {
        let coverage = lock(&self.coverage);
        let now = Instant::now();
        match (
            coverage.by_session.get(&sender),
            coverage.by_session.get(&recipient),
        ) {
            (Some((a, until_a)), Some((b, until_b))) => a == b && *until_a > now && *until_b > now,
            _ => false,
        }
    }

    pub(super) fn mesh_forget(&self, user: &str) {
        let changed = lock(&self.book).mesh_forget(user);
        let mut coverage = lock(&self.coverage);
        for channel in changed {
            coverage.clear(&channel);
        }
    }

    fn publish(&self, channel: &str, sessions: HashSet<u64>) {
        lock(&self.coverage).set(channel, sessions, Instant::now() + READY);
    }

    #[cfg(test)]
    pub(crate) fn age_mesh_reports(&self) {
        for mesh in lock(&self.book).meshes.channels.values_mut() {
            for node in mesh.nodes.values_mut() {
                node.at = Instant::now() - FRESH;
            }
        }
    }
}

fn community_id(state: &AppState) -> ApiResult<String> {
    community::server_id(&lock(&state.0.db)).map_err(|_| missing())
}

/// `POST /api/hosting/voice-gateways/mesh`: a registered community gateway's
/// report and its current approval, plan, links and member leases.
pub(crate) async fn report(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<wire_mesh::MeshReport>,
) -> ApiResult<Json<wire_mesh::MeshAssignment>> {
    let auth = state.auth(&headers)?;
    state.0.gateways.rate(&auth.user.id)?;
    input.validate().map_err(bad)?;
    let instance = instance_of(&input.instance)?;
    let registration = {
        let mut book = lock(&state.0.gateways.book);
        book.sweep();
        book.gateways.get(&auth.user.id).cloned()
    };
    let Some(registration) = registration.filter(|r| r.call_id.is_none()) else {
        return Err(forbidden());
    };
    if registration.instance != instance || registration.session_id != input.session_id {
        return Err(ApiError::new(StatusCode::CONFLICT, "stale_instance"));
    }
    let community = community_id(&state)?;
    let gateway_ok = {
        let db = lock(&state.0.db);
        gateway_valid(&state, &db, &auth.user.id, &registration)
    };
    let now = Instant::now();
    let (assignment, covered, channel) = {
        let mut book = lock(&state.0.gateways.book);
        if !gateway_ok {
            if book.registration_is(&auth.user.id, &registration.seen()) {
                book.drop_gateway(&auth.user.id);
            }
            drop(book);
            state.0.gateways.mesh_forget(&auth.user.id);
            return Err(forbidden());
        }
        if !book.registration_is(&auth.user.id, &registration.seen()) {
            return Err(ApiError::new(StatusCode::CONFLICT, "stale_instance"));
        }
        let channel = registration.channel_id.clone();
        let mesh = book
            .meshes
            .channels
            .entry(channel.clone())
            .or_insert_with(|| ChannelMesh {
                media_session: u64::from_le_bytes(fresh()),
                epoch: 0,
                nodes: HashMap::new(),
                people: HashMap::new(),
                next_stream: 0,
                links: HashMap::new(),
                plan: None,
            });
        let taken: HashSet<NodeId> = mesh
            .nodes
            .iter()
            .filter(|(u, _)| u.as_str() != auth.user.id)
            .map(|(_, n)| n.node)
            .collect();
        let node = match mesh.nodes.get(&auth.user.id) {
            Some(n) if n.instance == instance => n.node,
            _ => match (1..=wire_mesh::MAX_NODES as u8 - 1)
                .map(NodeId)
                .find(|n| !taken.contains(n))
            {
                Some(node) => node,
                None => return Err(ApiError::new(StatusCode::TOO_MANY_REQUESTS, "mesh_full")),
            },
        };
        mesh.nodes.insert(
            auth.user.id.clone(),
            Node {
                node,
                instance,
                session_id: input.session_id,
                port: input.port,
                endpoint_id: input.endpoint_id.clone(),
                relay_url: input.relay_url.clone(),
                upload_kbps: input.upload_kbps,
                host_rtt_ms: input.host_rtt_ms,
                applied: input.applied_epoch,
                links: input
                    .links
                    .iter()
                    .map(|l| (l.gateway_id.clone(), (l.quality, l.up)))
                    .collect(),
                at: now,
            },
        );
        let covered = {
            let seats = lock(&state.0.voice);
            book.mesh_refresh(&channel, &community, &seats, now)
        };
        (
            book.assignment(&channel, &auth.user.id, now),
            covered,
            channel,
        )
    };
    state.0.gateways.publish(&channel, covered);
    Ok(Json(assignment))
}

/// `POST /api/hosting/voice-gateways/mesh/member`: the caller's own place in
/// the mesh, if their home gateway is in the plan. `null` otherwise.
pub(crate) async fn member(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<wire_mesh::MemberReport>,
) -> ApiResult<Json<Option<wire_mesh::MemberAssignment>>> {
    let auth = state.auth(&headers)?;
    state.0.gateways.rate(&auth.user.id)?;
    let seat = own_seat(&state, &auth).ok_or_else(forbidden)?;
    if input.session_id != seat.session_id {
        return Err(stale_session());
    }
    if seat.call.is_some() {
        return Err(forbidden());
    }
    let community = community_id(&state)?;
    let caller_ok = {
        let db = lock(&state.0.db);
        seat_valid(
            &state,
            &db,
            seat.session_id,
            &auth.user.id,
            &seat.channel,
            &auth.token_hash,
            None,
        )
    };
    if !caller_ok {
        return Err(forbidden());
    }
    let now = Instant::now();
    let (answer, covered) = {
        let mut book = lock(&state.0.gateways.book);
        let Some(mesh) = book.meshes.channels.get_mut(&seat.channel) else {
            return Ok(Json(None));
        };
        let in_plan = mesh.people.get(&seat.session_id).is_some_and(|p| {
            p.user == auth.user.id
                && mesh
                    .plan
                    .as_ref()
                    .is_some_and(|plan| plan.attachment(p.stream).is_some())
        });
        if let Some(person) = mesh.people.get_mut(&seat.session_id).filter(|_| in_plan) {
            Key::renew(&mut person.key, now);
            person.healthy_until = input.healthy.then_some(now + READY);
        }
        let covered = {
            let seats = lock(&state.0.voice);
            book.mesh_refresh(&seat.channel, &community, &seats, now)
        };
        let answer = book.meshes.channels.get(&seat.channel).and_then(|mesh| {
            let plan = mesh.plan.as_ref()?;
            let person = mesh.people.get(&seat.session_id)?;
            let place = plan.attachment(person.stream)?;
            let (gateway, node) = mesh.nodes.iter().find(|(_, n)| n.node == place.node)?;
            let registration = book.gateways.get(gateway)?;
            let key = person.key.as_ref().filter(|k| k.expires > now)?;
            Some(wire_mesh::MemberAssignment {
                binding: plan.binding.clone(),
                epoch: plan.epoch,
                stream: person.stream,
                node: place.node,
                gateway_id: gateway.clone(),
                ip: registration.address.ip().to_string(),
                port: node.port,
                lease_id: wire::encode_id(&key.id),
                key: key.text(),
                ttl_ms: key.ttl_ms(now),
            })
        });
        (answer, covered)
    };
    state.0.gateways.publish(&seat.channel, covered);
    Ok(Json(answer))
}
