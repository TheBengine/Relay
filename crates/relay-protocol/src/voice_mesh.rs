//! Route plan for linked local voice gateways ("two households").
//!
//! People in one home lease the same approved local gateway. A gateway fans a
//! stream out to the people in its home, and sends it ONCE to each tree
//! neighbour that leads to a listener. A stream from home A to home B therefore
//! crosses the internet once and is copied locally at B; speech between two
//! people in the same home never leaves it. People without a gateway attach to
//! the shared voice host or to a gateway, picked by the whole path to everyone
//! else, not by one ping.
//!
//! Forwarders (the host and the approved gateways) form one tree per plan. A
//! stream is only sent along tree edges and never back the way it came, so it
//! cannot loop and each forwarder receives it at most once. The data plane also
//! checks every packet: the binding, the hop limit, the visited forwarders, the
//! reverse path to the stream's origin, its age and a per-stream replay window.
//!
//! Gateways only ever see this header and an opaque payload: the original
//! end-to-end protected voice frame, never decoded or re-encoded.
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// Wire version of [`MeshHeader`].
pub const VERSION: u8 = 1;
/// Fixed header in front of the original protected frame.
pub const HEADER_LEN: usize = 34;
/// Forwarders a packet may pass (host and gateways), sender and listener excluded.
pub const MAX_FORWARDERS: u8 = 4;
/// Accumulated link time after which a frame is useless for live speech.
pub const MAX_AGE_MS: u16 = 400;
/// A frame this much later than the stream's best observed delay is dropped.
pub const MAX_LATENESS_MS: u32 = 300;
/// Forwarders in one plan, the host included.
pub const MAX_NODES: usize = 16;
/// People in one plan.
pub const MAX_MEMBERS: usize = 64;
/// The shared voice host is always node 0.
pub const HOST: NodeId = NodeId(0);
/// Upload one copy of one stream needs on a link: a 200-byte protected frame,
/// 87 bytes of mesh envelope and 28 of IP/UDP, 50 times a second.
pub const MESH_STREAM_KBPS: u32 = 130;
/// The mesh may use at most this share of a gateway's measured upload, so a
/// home's own traffic and video keep the rest.
pub const UPLOAD_SHARE_PERCENT: u32 = 50;
/// People assumed to speak at once when sizing upload (as the host overview does).
pub const DEFAULT_SPEAKERS: u32 = 2;
/// A link between homes is used only if it is at most this much worse than
/// going through the shared host.
pub const MESH_MARGIN_MS: u32 = 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct NodeId(pub u8);

/// What a gateway and every lease are approved for. A packet, lease or plan for
/// another community, channel or media session is refused.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Binding {
    pub community_id: String,
    pub channel_id: String,
    /// The channel's current call instance. A new call is a new binding.
    pub media_session: u64,
}
impl Binding {
    pub fn valid(&self) -> bool {
        (1..=128).contains(&self.community_id.len())
            && (1..=128).contains(&self.channel_id.len())
            && self.media_session != 0
    }
    /// Short label carried in every packet. Not a secret and not the security
    /// boundary: the data plane mixes the full binding into every hop key.
    pub fn tag(&self) -> u64 {
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        let mut eat = |bytes: &[u8]| {
            for byte in bytes {
                hash ^= u64::from(*byte);
                hash = hash.wrapping_mul(0x0100_0000_01b3);
            }
        };
        eat(b"relay-voice-mesh-v1\0");
        for part in [self.community_id.as_bytes(), self.channel_id.as_bytes()] {
            eat(&(part.len() as u32).to_le_bytes());
            eat(part);
        }
        eat(&self.media_session.to_le_bytes());
        hash
    }
}

/// One measured hop. Costs add up along a path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HopQuality {
    pub rtt_ms: u32,
    pub loss_permille: u16,
    pub jitter_ms: u32,
}
impl HopQuality {
    pub const LAN: HopQuality = HopQuality {
        rtt_ms: 1,
        loss_permille: 0,
        jitter_ms: 0,
    };
    /// Effective one-way milliseconds: half the round trip, the jitter buffer it
    /// forces, and 2 ms per permille of loss (1 % loss costs like 20 ms delay).
    pub fn cost(&self) -> u32 {
        (self.rtt_ms / 2)
            .saturating_add(self.jitter_ms.saturating_mul(2))
            .saturating_add(u32::from(self.loss_permille.min(1000)) * 2)
    }
    pub fn one_way_ms(&self) -> u16 {
        (self.rtt_ms / 2).min(u32::from(u16::MAX)) as u16
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Gateway {
    pub id: String,
    pub node: NodeId,
    /// The binding the server approved this gateway for, if any.
    pub approval: Option<Binding>,
    /// Measured link to the shared voice host, if it has one.
    pub uplink: Option<HopQuality>,
    /// Measured upload capacity. `None` means not limited; a server without a
    /// measurement passes `Some(0)`, which allows home-local speech only.
    #[serde(default)]
    pub upload_kbps: Option<u32>,
}

/// A measured, server-granted link between two gateways.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Trunk {
    pub a: NodeId,
    pub b: NodeId,
    pub quality: HopQuality,
}

/// A place a person can attach to, with that first hop measured.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidate {
    pub node: NodeId,
    pub first_hop: HopQuality,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    pub id: String,
    /// The person's voice stream id in this media session.
    pub stream: u32,
    /// The gateway in their own home, when they hold its lease.
    pub home: Option<NodeId>,
    pub candidates: Vec<Candidate>,
    pub muted: bool,
    pub deafened: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Topology {
    pub binding: Binding,
    pub epoch: u32,
    pub gateways: Vec<Gateway>,
    pub trunks: Vec<Trunk>,
    pub members: Vec<Member>,
    /// Whether the shared host forwards inside the tree. When it does not, the
    /// host still serves everyone the plan leaves out, as an ordinary voice host,
    /// and uplinks only judge whether a link between homes is better.
    #[serde(default = "yes")]
    pub host_relay: bool,
    /// People sized for speaking at once; zero means [`DEFAULT_SPEAKERS`].
    #[serde(default)]
    pub speakers: u32,
}
fn yes() -> bool {
    true
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edge {
    pub a: NodeId,
    pub b: NodeId,
    pub one_way_ms: u16,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    pub id: String,
    pub stream: u32,
    pub node: NodeId,
    pub first_hop: HopQuality,
    pub muted: bool,
    pub deafened: bool,
    /// Attached at their own home's gateway over the LAN. Anyone else reaches
    /// the gateway over the internet, which costs that home upload.
    #[serde(default)]
    pub home: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanError {
    InvalidBinding,
    TooManyNodes,
    TooManyMembers,
    DuplicateNode,
    DuplicateMember,
    NotATree,
    UnknownNode,
    TooManyHops,
    UnknownStream,
}

/// One tree of forwarders and who is attached where.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutePlan {
    pub binding: Binding,
    pub epoch: u32,
    /// Every forwarder in the tree, sorted. One node and no edges is valid.
    pub nodes: Vec<NodeId>,
    pub edges: Vec<Edge>,
    pub members: Vec<Attachment>,
    /// People no candidate could reach within the hop limit.
    pub unroutable: Vec<String>,
}

type Costs = BTreeMap<(NodeId, NodeId), u32>;

fn key(a: NodeId, b: NodeId) -> (NodeId, NodeId) {
    if a < b {
        (a, b)
    } else {
        (b, a)
    }
}

/// Tree distances in cost and in forwarders (both ends counted) from `start`.
fn tree_walk(
    nodes: &BTreeSet<NodeId>,
    edges: &Costs,
    start: NodeId,
) -> BTreeMap<NodeId, (u32, usize)> {
    let mut out = BTreeMap::new();
    if !nodes.contains(&start) {
        return out;
    }
    out.insert(start, (0, 1));
    let mut queue = VecDeque::from([start]);
    while let Some(at) = queue.pop_front() {
        let (cost, count) = out[&at];
        for (&(a, b), &weight) in edges {
            let next = if a == at {
                b
            } else if b == at {
                a
            } else {
                continue;
            };
            if !out.contains_key(&next) {
                out.insert(next, (cost.saturating_add(weight), count + 1));
                queue.push_back(next);
            }
        }
    }
    out
}

/// Cheapest graph path from any tree node to `target` (Dijkstra, tiny graph).
fn connect(
    graph: &Costs,
    all: &BTreeSet<NodeId>,
    tree: &BTreeSet<NodeId>,
    target: NodeId,
) -> Option<(u32, Vec<NodeId>)> {
    if tree.contains(&target) {
        return Some((0, vec![target]));
    }
    if tree.is_empty() {
        return Some((0, vec![target]));
    }
    let mut best: BTreeMap<NodeId, (u32, Option<NodeId>)> =
        tree.iter().map(|n| (*n, (0, None))).collect();
    let mut done = BTreeSet::new();
    loop {
        let at = best
            .iter()
            .filter(|(n, _)| !done.contains(*n))
            .min_by_key(|(n, (cost, _))| (*cost, **n))
            .map(|(n, _)| *n)?;
        if at == target {
            break;
        }
        done.insert(at);
        let base = best[&at].0;
        for next in all {
            if let Some(weight) = graph.get(&key(at, *next)) {
                let cost = base.saturating_add(*weight);
                if best.get(next).is_none_or(|(old, _)| cost < *old) && !done.contains(next) {
                    best.insert(*next, (cost, Some(at)));
                }
            }
        }
    }
    let mut path = vec![target];
    let mut at = target;
    while let Some(Some(previous)) = best.get(&at).map(|(_, p)| *p) {
        path.push(previous);
        at = previous;
    }
    Some((best[&target].0, path))
}

fn add_path(tree: &mut BTreeSet<NodeId>, edges: &mut Costs, graph: &Costs, path: &[NodeId]) {
    for node in path {
        tree.insert(*node);
    }
    for pair in path.windows(2) {
        let k = key(pair[0], pair[1]);
        edges.insert(k, graph[&k]);
    }
}

fn budget(upload_kbps: u32) -> u32 {
    (u64::from(upload_kbps) * u64::from(UPLOAD_SHARE_PERCENT) / 100) as u32
}

impl RoutePlan {
    /// Builds a plan within every gateway's upload budget. A gateway over its
    /// budget first loses the lone people attached to it, then its place in the
    /// tree; its own home then falls back to the shared host.
    pub fn build(topology: &Topology) -> Result<RoutePlan, PlanError> {
        let speakers = if topology.speakers == 0 {
            DEFAULT_SPEAKERS
        } else {
            topology.speakers
        };
        let mut excluded = BTreeSet::new();
        let mut banned = BTreeSet::new();
        loop {
            let plan = Self::build_with(topology, &excluded, &banned)?;
            let over = topology
                .gateways
                .iter()
                .filter(|g| plan.nodes.contains(&g.node))
                .filter_map(|g| {
                    let budget = budget(g.upload_kbps?);
                    let need = plan.upload_kbps(g.node, speakers);
                    (need > budget).then(|| (need - budget, g.node))
                })
                .max();
            let Some((_, node)) = over else {
                return Ok(plan);
            };
            // Each round removes one option, so this ends.
            match plan
                .members
                .iter()
                .filter(|m| m.node == node && !m.home)
                .map(|m| m.id.clone())
                .max()
            {
                Some(id) => {
                    banned.insert((id, node));
                }
                None => {
                    excluded.insert(node);
                }
            }
        }
    }

    fn build_with(
        topology: &Topology,
        excluded: &BTreeSet<NodeId>,
        banned: &BTreeSet<(String, NodeId)>,
    ) -> Result<RoutePlan, PlanError> {
        let binding = &topology.binding;
        if !binding.valid() {
            return Err(PlanError::InvalidBinding);
        }
        if topology.gateways.len() + 1 > MAX_NODES {
            return Err(PlanError::TooManyNodes);
        }
        if topology.members.len() > MAX_MEMBERS {
            return Err(PlanError::TooManyMembers);
        }
        // Only gateways approved for exactly this binding take part.
        let mut all = BTreeSet::from([HOST]);
        let mut graph = Costs::new();
        for gateway in &topology.gateways {
            if gateway.node == HOST || !all.insert(gateway.node) {
                return Err(PlanError::DuplicateNode);
            }
        }
        all.retain(|node| {
            *node == HOST
                || (!excluded.contains(node)
                    && topology
                        .gateways
                        .iter()
                        .any(|g| g.node == *node && g.approval.as_ref() == Some(binding)))
        });
        let uplink = |node: NodeId| {
            topology
                .gateways
                .iter()
                .find(|g| g.node == node)
                .and_then(|g| g.uplink)
        };
        if topology.host_relay {
            for gateway in &topology.gateways {
                if let (true, Some(uplink)) = (all.contains(&gateway.node), gateway.uplink) {
                    graph.insert(key(HOST, gateway.node), uplink.cost().max(1));
                }
            }
        }
        for trunk in &topology.trunks {
            // A link between homes is used only when it is no worse than going
            // through the shared host by more than the margin.
            let better = match (uplink(trunk.a), uplink(trunk.b)) {
                (Some(a), Some(b)) => {
                    trunk.quality.cost()
                        <= a.cost()
                            .saturating_add(b.cost())
                            .saturating_add(MESH_MARGIN_MS)
                }
                _ => true,
            };
            if better
                && trunk.a != trunk.b
                && trunk.a != HOST
                && trunk.b != HOST
                && all.contains(&trunk.a)
                && all.contains(&trunk.b)
            {
                graph.insert(key(trunk.a, trunk.b), trunk.quality.cost().max(1));
            }
        }
        if !topology.host_relay {
            // The host is not in the tree; it serves whoever the plan leaves out.
            all.remove(&HOST);
        }
        let mut seen_ids = BTreeSet::new();
        let mut seen_streams = BTreeSet::new();
        for member in &topology.members {
            if !seen_ids.insert(member.id.as_str()) || !seen_streams.insert(member.stream) {
                return Err(PlanError::DuplicateMember);
            }
        }

        // Homes with an approved gateway are fixed. A Steiner-style tree joins
        // them: each step adds the home nearest to the tree by real link cost.
        let mut attached: Vec<Attachment> = Vec::new();
        let mut lone: Vec<&Member> = Vec::new();
        for member in &topology.members {
            match member.home {
                Some(home) if home != HOST && all.contains(&home) => attached.push(Attachment {
                    id: member.id.clone(),
                    stream: member.stream,
                    node: home,
                    first_hop: member
                        .candidates
                        .iter()
                        .find(|c| c.node == home)
                        .map_or(HopQuality::LAN, |c| c.first_hop),
                    muted: member.muted,
                    deafened: member.deafened,
                    home: true,
                }),
                _ => lone.push(member),
            }
        }
        let mut tree = BTreeSet::new();
        let mut edges = Costs::new();
        let mut homes: BTreeSet<NodeId> = attached.iter().map(|a| a.node).collect();
        let mut unroutable = Vec::new();
        while !homes.is_empty() {
            let mut options: Vec<(u32, NodeId, Vec<NodeId>)> = homes
                .iter()
                .filter_map(|home| connect(&graph, &all, &tree, *home).map(|(c, p)| (c, *home, p)))
                .collect();
            options.sort();
            let Some((_, home, path)) = options.into_iter().next() else {
                // A home that no link reaches: its people cannot be planned.
                for home in std::mem::take(&mut homes) {
                    attached.retain(|a| {
                        let keep = a.node != home;
                        if !keep {
                            unroutable.push(a.id.clone());
                        }
                        keep
                    });
                }
                break;
            };
            add_path(&mut tree, &mut edges, &graph, &path);
            homes.remove(&home);
        }

        // People without a home gateway: whole-path choice, deterministic order.
        lone.sort_by(|a, b| a.id.cmp(&b.id));
        for member in lone {
            let mut best: Option<((u32, u32, usize, NodeId), Candidate, Vec<NodeId>)> = None;
            for candidate in &member.candidates {
                if !all.contains(&candidate.node)
                    || banned.contains(&(member.id.clone(), candidate.node))
                {
                    continue;
                }
                let Some((_, path)) = connect(&graph, &all, &tree, candidate.node) else {
                    continue;
                };
                let mut trial_tree = tree.clone();
                let mut trial_edges = edges.clone();
                add_path(&mut trial_tree, &mut trial_edges, &graph, &path);
                let walk = tree_walk(&trial_tree, &trial_edges, candidate.node);
                let mut worst = 0u32;
                let mut total = 0u32;
                let mut within_limit = true;
                for other in &attached {
                    let Some((cost, forwarders)) = walk.get(&other.node) else {
                        within_limit = false;
                        break;
                    };
                    if *forwarders > usize::from(MAX_FORWARDERS) {
                        within_limit = false;
                        break;
                    }
                    let whole = candidate
                        .first_hop
                        .cost()
                        .saturating_add(*cost)
                        .saturating_add(other.first_hop.cost());
                    worst = worst.max(whole);
                    total = total.saturating_add(whole);
                }
                if attached.is_empty() {
                    worst = candidate.first_hop.cost();
                    total = worst;
                }
                if !within_limit {
                    continue;
                }
                let added = trial_edges.len() - edges.len();
                let score = (worst, total, added, candidate.node);
                if best.as_ref().is_none_or(|(old, _, _)| score < *old) {
                    best = Some((score, *candidate, path));
                }
            }
            match best {
                Some((_, candidate, path)) => {
                    add_path(&mut tree, &mut edges, &graph, &path);
                    attached.push(Attachment {
                        id: member.id.clone(),
                        stream: member.stream,
                        node: candidate.node,
                        first_hop: candidate.first_hop,
                        muted: member.muted,
                        deafened: member.deafened,
                        home: false,
                    });
                }
                None => unroutable.push(member.id.clone()),
            }
        }

        // Drop relay-only leaves nobody needs.
        loop {
            let leaf = tree.iter().copied().find(|node| {
                !attached.iter().any(|a| a.node == *node)
                    && edges.keys().filter(|(a, b)| a == node || b == node).count() <= 1
                    && tree.len() > 1
            });
            let Some(leaf) = leaf else { break };
            tree.remove(&leaf);
            edges.retain(|(a, b), _| *a != leaf && *b != leaf);
        }
        if tree.is_empty() {
            tree.insert(all.iter().next().copied().unwrap_or(HOST));
        }
        attached.sort_by_key(|a| a.stream);
        unroutable.sort();
        let quality = |a: NodeId, b: NodeId| -> u16 {
            if a == HOST || b == HOST {
                let gateway = if a == HOST { b } else { a };
                topology
                    .gateways
                    .iter()
                    .find(|g| g.node == gateway)
                    .and_then(|g| g.uplink)
                    .map_or(0, |q| q.one_way_ms())
            } else {
                topology
                    .trunks
                    .iter()
                    .find(|t| key(t.a, t.b) == key(a, b))
                    .map_or(0, |t| t.quality.one_way_ms())
            }
        };
        let plan = RoutePlan {
            binding: binding.clone(),
            epoch: topology.epoch,
            nodes: tree.into_iter().collect(),
            edges: edges
                .keys()
                .map(|(a, b)| Edge {
                    a: *a,
                    b: *b,
                    one_way_ms: quality(*a, *b),
                })
                .collect(),
            members: attached,
            unroutable,
        };
        plan.validate()?;
        Ok(plan)
    }

    /// Checks a plan received over the wire before any gateway uses it.
    pub fn validate(&self) -> Result<(), PlanError> {
        if !self.binding.valid() {
            return Err(PlanError::InvalidBinding);
        }
        if self.nodes.is_empty() || self.nodes.len() > MAX_NODES {
            return Err(PlanError::TooManyNodes);
        }
        if self.members.len() > MAX_MEMBERS {
            return Err(PlanError::TooManyMembers);
        }
        let nodes: BTreeSet<NodeId> = self.nodes.iter().copied().collect();
        if nodes.len() != self.nodes.len() {
            return Err(PlanError::DuplicateNode);
        }
        let mut edges = Costs::new();
        for edge in &self.edges {
            if edge.a == edge.b || !nodes.contains(&edge.a) || !nodes.contains(&edge.b) {
                return Err(PlanError::UnknownNode);
            }
            if edges.insert(key(edge.a, edge.b), 1).is_some() {
                return Err(PlanError::NotATree);
            }
        }
        // n - 1 edges and connected: a tree, so no forwarding loop exists.
        if edges.len() + 1 != nodes.len()
            || tree_walk(&nodes, &edges, self.nodes[0]).len() != nodes.len()
        {
            return Err(PlanError::NotATree);
        }
        for node in &nodes {
            let farthest = tree_walk(&nodes, &edges, *node)
                .values()
                .map(|(_, count)| *count)
                .max()
                .unwrap_or(1);
            if farthest > usize::from(MAX_FORWARDERS) {
                return Err(PlanError::TooManyHops);
            }
        }
        let mut streams = BTreeSet::new();
        let mut ids = BTreeSet::new();
        for member in &self.members {
            if !nodes.contains(&member.node) {
                return Err(PlanError::UnknownNode);
            }
            if !streams.insert(member.stream) || !ids.insert(member.id.as_str()) {
                return Err(PlanError::DuplicateMember);
            }
        }
        Ok(())
    }

    pub fn attachment(&self, stream: u32) -> Option<&Attachment> {
        self.members.iter().find(|m| m.stream == stream)
    }

    pub fn neighbors(&self, node: NodeId) -> Vec<NodeId> {
        self.edges
            .iter()
            .filter_map(|e| {
                if e.a == node {
                    Some(e.b)
                } else if e.b == node {
                    Some(e.a)
                } else {
                    None
                }
            })
            .collect()
    }

    pub fn edge(&self, a: NodeId, b: NodeId) -> Option<&Edge> {
        self.edges.iter().find(|e| key(e.a, e.b) == key(a, b))
    }

    /// The neighbour of `at` on the tree path to `target`; `None` when equal or absent.
    pub fn toward(&self, at: NodeId, target: NodeId) -> Option<NodeId> {
        if at == target {
            return None;
        }
        let mut parent: BTreeMap<NodeId, NodeId> = BTreeMap::new();
        let mut queue = VecDeque::from([target]);
        let mut seen = BTreeSet::from([target]);
        while let Some(node) = queue.pop_front() {
            for next in self.neighbors(node) {
                if seen.insert(next) {
                    parent.insert(next, node);
                    queue.push_back(next);
                }
            }
        }
        parent.get(&at).copied()
    }

    /// Forwarders on the tree path from `a` to `b`, both included.
    pub fn forwarders_between(&self, a: NodeId, b: NodeId) -> Option<usize> {
        let mut count = 1;
        let mut at = a;
        while at != b {
            at = self.toward(at, b)?;
            count += 1;
        }
        Some(count)
    }

    fn listens(&self, sender: u32, node: NodeId) -> bool {
        self.members
            .iter()
            .any(|m| m.node == node && m.stream != sender && !m.deafened)
    }

    /// Tree neighbours of `at` that need `stream`, having arrived from `from`
    /// (`None`: from a person attached at `at`). Never back toward `from`.
    pub fn forward_to(&self, at: NodeId, from: Option<NodeId>, stream: u32) -> Vec<NodeId> {
        let Some(sender) = self.attachment(stream) else {
            return Vec::new();
        };
        if sender.muted {
            return Vec::new();
        }
        // Reverse-path rule: a stream enters a forwarder only from its origin side.
        let expected = self.toward(at, sender.node);
        if from != expected {
            return Vec::new();
        }
        self.neighbors(at)
            .into_iter()
            .filter(|next| Some(*next) != from)
            .filter(|next| {
                self.nodes
                    .iter()
                    .any(|n| (self.toward(at, *n) == Some(*next)) && self.listens(stream, *n))
            })
            .collect()
    }

    /// People attached at `at` who should hear `stream`.
    pub fn local_receivers(&self, at: NodeId, stream: u32) -> Vec<u32> {
        match self.attachment(stream) {
            Some(sender) if !sender.muted => self
                .members
                .iter()
                .filter(|m| m.node == at && m.stream != stream && !m.deafened)
                .map(|m| m.stream)
                .collect(),
            _ => Vec::new(),
        }
    }

    /// Copies of `stream` that leave `node` over the internet: one per tree
    /// neighbour that needs it, and one per listener attached from outside.
    pub fn copies_out(&self, node: NodeId, stream: u32) -> u32 {
        let Some(sender) = self.attachment(stream) else {
            return 0;
        };
        let from = self.toward(node, sender.node);
        let forwarded = self.forward_to(node, from, stream).len();
        let remote = self
            .local_receivers(node, stream)
            .iter()
            .filter(|s| self.attachment(**s).is_some_and(|a| !a.home))
            .count();
        (forwarded + remote) as u32
    }

    /// Upload the mesh needs at `node` when the `speakers` busiest streams talk.
    pub fn upload_kbps(&self, node: NodeId, speakers: u32) -> u32 {
        let mut copies: Vec<u32> = self
            .members
            .iter()
            .map(|m| self.copies_out(node, m.stream))
            .collect();
        copies.sort_unstable_by(|a, b| b.cmp(a));
        copies
            .iter()
            .take(speakers as usize)
            .sum::<u32>()
            .saturating_mul(MESH_STREAM_KBPS)
    }

    /// A controlled move of one person to another forwarder of this tree. Their
    /// mute and deafen state carry over unchanged; the epoch advances.
    pub fn switch(
        &self,
        stream: u32,
        node: NodeId,
        first_hop: HopQuality,
    ) -> Result<RoutePlan, PlanError> {
        if !self.nodes.contains(&node) {
            return Err(PlanError::UnknownNode);
        }
        let mut next = self.clone();
        let member = next
            .members
            .iter_mut()
            .find(|m| m.stream == stream)
            .ok_or(PlanError::UnknownStream)?;
        member.node = node;
        member.first_hop = first_hop;
        next.epoch = self.epoch.wrapping_add(1);
        next.validate()?;
        Ok(next)
    }

    pub fn with_state(
        &self,
        stream: u32,
        muted: bool,
        deafened: bool,
    ) -> Result<RoutePlan, PlanError> {
        let mut next = self.clone();
        let member = next
            .members
            .iter_mut()
            .find(|m| m.stream == stream)
            .ok_or(PlanError::UnknownStream)?;
        member.muted = muted;
        member.deafened = deafened;
        next.epoch = self.epoch.wrapping_add(1);
        Ok(next)
    }

    /// Revocation: the person leaves the plan and is neither heard nor sent to.
    pub fn without(&self, stream: u32) -> RoutePlan {
        let mut next = self.clone();
        next.members.retain(|m| m.stream != stream);
        next.epoch = self.epoch.wrapping_add(1);
        next
    }

    /// Copies of one frame from every unmuted speaker that cross each tree edge,
    /// and that reach each forwarder's local listeners. For reports and tests.
    pub fn copies(&self) -> (BTreeMap<(NodeId, NodeId), usize>, BTreeMap<NodeId, usize>) {
        let mut links = BTreeMap::new();
        let mut local = BTreeMap::new();
        for sender in &self.members {
            let mut queue = VecDeque::from([(sender.node, None::<NodeId>)]);
            while let Some((at, from)) = queue.pop_front() {
                *local.entry(at).or_insert(0) += self.local_receivers(at, sender.stream).len();
                for next in self.forward_to(at, from, sender.stream) {
                    *links.entry((at, next)).or_insert(0) += 1;
                    queue.push_back((next, Some(at)));
                }
            }
        }
        (links, local)
    }
}

/// Mesh header in front of the original protected frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MeshHeader {
    pub hops_left: u8,
    /// Forwarders already passed, in order.
    pub via: Vec<NodeId>,
    pub binding: u64,
    pub stream: u32,
    /// Assigned by the sender; continuous across a path switch.
    pub seq: u64,
    /// Sender's media clock in milliseconds (wrapping).
    pub origin_ms: u32,
    /// Link time accumulated so far.
    pub age_ms: u16,
}
impl MeshHeader {
    pub fn first(binding: u64, stream: u32, seq: u64, origin_ms: u32) -> Self {
        Self {
            hops_left: MAX_FORWARDERS,
            via: Vec::new(),
            binding,
            stream,
            seq,
            origin_ms,
            age_ms: 0,
        }
    }
    pub fn encode(&self, payload: &[u8]) -> Option<Vec<u8>> {
        if payload.is_empty()
            || self.via.len() > usize::from(MAX_FORWARDERS)
            || self.hops_left > MAX_FORWARDERS
        {
            return None;
        }
        let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
        out.push(VERSION);
        out.push(self.hops_left);
        out.push(self.via.len() as u8);
        out.push(0);
        let mut via = [0u8; 4];
        for (slot, node) in via.iter_mut().zip(&self.via) {
            *slot = node.0;
        }
        out.extend(via);
        out.extend(self.binding.to_le_bytes());
        out.extend(self.stream.to_le_bytes());
        out.extend(self.seq.to_le_bytes());
        out.extend(self.origin_ms.to_le_bytes());
        out.extend(self.age_ms.to_le_bytes());
        out.extend(payload);
        Some(out)
    }
    pub fn decode(bytes: &[u8]) -> Option<(MeshHeader, &[u8])> {
        if bytes.len() <= HEADER_LEN || bytes[0] != VERSION || bytes[3] != 0 {
            return None;
        }
        let (hops_left, count) = (bytes[1], bytes[2]);
        if count > MAX_FORWARDERS || hops_left.checked_add(count)? > MAX_FORWARDERS {
            return None;
        }
        let via: Vec<NodeId> = bytes[4..4 + usize::from(count)]
            .iter()
            .map(|n| NodeId(*n))
            .collect();
        if via.iter().collect::<BTreeSet<_>>().len() != via.len()
            || bytes[4 + usize::from(count)..8].iter().any(|b| *b != 0)
        {
            return None;
        }
        Some((
            MeshHeader {
                hops_left,
                via,
                binding: u64::from_le_bytes(bytes[8..16].try_into().ok()?),
                stream: u32::from_le_bytes(bytes[16..20].try_into().ok()?),
                seq: u64::from_le_bytes(bytes[20..28].try_into().ok()?),
                origin_ms: u32::from_le_bytes(bytes[28..32].try_into().ok()?),
                age_ms: u16::from_le_bytes(bytes[32..34].try_into().ok()?),
            },
            &bytes[HEADER_LEN..],
        ))
    }
}

/// Server to gateway: approval to forward for exactly one binding as one node.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeshApproval {
    pub gateway_id: String,
    /// The registered gateway instance (32 lowercase hex digits).
    pub instance: String,
    pub binding: Binding,
    pub node: NodeId,
    pub epoch: u32,
    pub ttl_ms: u32,
}

/// Server to gateway: one authenticated link to another forwarder of the same
/// plan. Holds a secret key, so it has no `Debug`. Both ends get the same id and
/// key. The link runs over iroh to the peer's endpoint (direct, or through a
/// relay when NAT allows nothing else); a packet on this key from any other
/// endpoint is refused.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrunkGrant {
    pub id: String,
    pub peer: NodeId,
    /// The peer gateway's account id.
    pub peer_gateway: String,
    /// The peer's iroh endpoint id.
    pub endpoint_id: String,
    #[serde(default)]
    pub relay_url: Option<String>,
    /// 32-byte link key, URL-safe base64 without padding.
    pub key: String,
    pub ttl_ms: u32,
}

/// Longest iroh endpoint id or relay URL the server stores.
pub const MAX_ENDPOINT_TEXT: usize = 200;
/// Links one gateway may report.
pub const MAX_LINK_REPORTS: usize = 8;

/// One measured link from a gateway to another gateway of its channel.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkReport {
    pub gateway_id: String,
    pub quality: HopQuality,
    /// Health replies arrive on the link now.
    pub up: bool,
}

/// `POST /api/hosting/voice-gateways/mesh`: a registered gateway's report,
/// sent every few hundred milliseconds. It is also the gateway's request for
/// its current approval, plan, links and members.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeshReport {
    /// The registered instance; anything else is refused.
    pub instance: String,
    pub session_id: u64,
    /// The mesh worker's port on the registered LAN address.
    pub port: u16,
    #[serde(default)]
    pub endpoint_id: Option<String>,
    #[serde(default)]
    pub relay_url: Option<String>,
    /// Measured upload of this machine. Without it the gateway keeps speech
    /// inside its own home only.
    #[serde(default)]
    pub upload_kbps: Option<u32>,
    /// Round trip of this machine's own voice session to the host.
    #[serde(default)]
    pub host_rtt_ms: Option<u32>,
    /// Epoch of the plan the data plane applied.
    #[serde(default)]
    pub applied_epoch: Option<u32>,
    #[serde(default)]
    pub links: Vec<LinkReport>,
}
impl MeshReport {
    pub fn validate(&self) -> Result<(), &'static str> {
        let text = |t: &Option<String>| {
            t.as_ref().is_none_or(|t| {
                (1..=MAX_ENDPOINT_TEXT).contains(&t.len())
                    && t.bytes().all(|b| b.is_ascii_graphic())
            })
        };
        if self.port < 1024 {
            return Err("Use a mesh port of at least 1024");
        }
        if !text(&self.endpoint_id) || !text(&self.relay_url) {
            return Err("The link endpoint is invalid");
        }
        if self.links.len() > MAX_LINK_REPORTS
            || self.links.iter().any(|l| {
                l.gateway_id.is_empty()
                    || l.gateway_id.len() > 64
                    || l.quality.loss_permille > 1000
                    || l.quality.rtt_ms > 10_000
                    || l.quality.jitter_ms > 10_000
            })
        {
            return Err("A link measurement is invalid");
        }
        Ok(())
    }
}

/// Another gateway of the channel, to link with and measure.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerOffer {
    pub gateway_id: String,
    pub node: NodeId,
    pub endpoint_id: String,
    #[serde(default)]
    pub relay_url: Option<String>,
}

/// One person's mesh lease, as their gateway receives it. Holds a key: no `Debug`.
#[derive(Clone, Serialize, Deserialize)]
pub struct MemberGrant {
    pub id: String,
    pub stream: u32,
    pub key: String,
    pub ttl_ms: u32,
}

/// Answer to a gateway's report. Holds keys: no `Debug`.
#[derive(Clone, Serialize, Deserialize)]
pub struct MeshAssignment {
    /// `None`: this gateway is not part of the current plan (for example its
    /// upload is too small); its home then uses the shared host.
    pub approval: Option<MeshApproval>,
    pub plan: Option<RoutePlan>,
    pub links: Vec<TrunkGrant>,
    pub members: Vec<MemberGrant>,
    pub peers: Vec<PeerOffer>,
}

/// `POST /api/hosting/voice-gateways/mesh/member`: a person in voice asks for
/// their place in the mesh and says whether their hop works.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemberReport {
    pub session_id: u64,
    /// The person's hop to their gateway answers health probes now.
    pub healthy: bool,
}

/// Answer to a person: where to send. Holds a key: no `Debug`. `null` when the
/// mesh does not carry them; then the shared host does, as always.
#[derive(Clone, Serialize, Deserialize)]
pub struct MemberAssignment {
    pub binding: Binding,
    pub epoch: u32,
    pub stream: u32,
    pub node: NodeId,
    pub gateway_id: String,
    pub ip: String,
    pub port: u16,
    pub lease_id: String,
    pub key: String,
    pub ttl_ms: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: NodeId = NodeId(1);
    const B: NodeId = NodeId(2);
    fn q(rtt_ms: u32) -> HopQuality {
        HopQuality {
            rtt_ms,
            loss_permille: 0,
            jitter_ms: 0,
        }
    }
    fn binding() -> Binding {
        Binding {
            community_id: "community".into(),
            channel_id: "voice-1".into(),
            media_session: 77,
        }
    }
    fn home(id: &str, stream: u32, node: NodeId) -> Member {
        Member {
            id: id.into(),
            stream,
            home: Some(node),
            candidates: vec![Candidate {
                node,
                first_hop: HopQuality::LAN,
            }],
            muted: false,
            deafened: false,
        }
    }
    fn lone(id: &str, stream: u32, candidates: &[(NodeId, u32)]) -> Member {
        Member {
            id: id.into(),
            stream,
            home: None,
            candidates: candidates
                .iter()
                .map(|(node, rtt)| Candidate {
                    node: *node,
                    first_hop: q(*rtt),
                })
                .collect(),
            muted: false,
            deafened: false,
        }
    }
    /// 3 + 3 + 1 + 1: homes A and B, lone C and D.
    pub(crate) fn households() -> Topology {
        let gateway = |node, id: &str| Gateway {
            id: id.into(),
            node,
            approval: Some(binding()),
            uplink: Some(q(40)),
            upload_kbps: None,
        };
        Topology {
            binding: binding(),
            epoch: 1,
            gateways: vec![gateway(A, "gw-a"), gateway(B, "gw-b")],
            trunks: vec![Trunk {
                a: A,
                b: B,
                quality: q(30),
            }],
            members: vec![
                home("a1", 1, A),
                home("a2", 2, A),
                home("a3", 3, A),
                home("b1", 4, B),
                home("b2", 5, B),
                home("b3", 6, B),
                // C pings the host fastest, but the host is 40 ms from both homes.
                lone("c", 7, &[(HOST, 20), (A, 30)]),
                lone("d", 8, &[(HOST, 25), (B, 60)]),
            ],
            host_relay: true,
            speakers: 0,
        }
    }

    #[test]
    fn two_households_send_each_stream_across_once_and_keep_home_speech_local() {
        let plan = RoutePlan::build(&households()).unwrap();
        assert!(plan.unroutable.is_empty());
        let (links, local) = plan.copies();
        // Three speakers in A reach the three listeners in B as three copies, not nine.
        let a_streams_into_b = [1, 2, 3]
            .iter()
            .filter(|s| plan.forward_to(A, None, **s).contains(&B))
            .count();
        assert_eq!(a_streams_into_b, 3);
        assert_eq!(plan.local_receivers(B, 1), vec![4, 5, 6]);
        // Home speech: a1 to a2 and a3 never leaves A (C, attached at A, too).
        assert_eq!(plan.local_receivers(A, 1), vec![2, 3, 7]);
        // Tree HOST - A - B; C attaches at A, D at the host. One frame from all eight:
        assert_eq!(plan.edges.len(), 2);
        assert_eq!(links[&(A, B)], 5, "a1 a2 a3, c and d each cross once");
        assert_eq!(links[&(B, A)], 3);
        assert_eq!(links[&(A, HOST)], 7, "only d listens behind the host");
        assert_eq!(links[&(HOST, A)], 1);
        // Every listener hears every other speaker exactly once: 8 * 7 deliveries.
        assert_eq!(local.values().sum::<usize>(), 8 * 7);
    }

    #[test]
    fn lone_people_are_routed_by_the_whole_path_not_the_first_ping() {
        let plan = RoutePlan::build(&households()).unwrap();
        // C: host is 10 + 20 + 15 = 45 ms to B, gateway A is 15 + 15 = 30 ms.
        assert_eq!(plan.attachment(7).unwrap().node, A);
        // D: the host is cheaper in total than a 60 ms first hop into B.
        assert_eq!(plan.attachment(8).unwrap().node, HOST);
        let mut only_ping = households();
        only_ping.trunks.clear();
        only_ping
            .members
            .retain(|m| m.id == "c" || m.home == Some(A));
        let plan = RoutePlan::build(&only_ping).unwrap();
        assert_eq!(plan.attachment(7).unwrap().node, A);
    }

    #[test]
    fn unapproved_or_wrongly_bound_gateways_are_not_used() {
        let mut topology = households();
        topology.gateways[1].approval = Some(Binding {
            media_session: 78,
            ..binding()
        });
        let plan = RoutePlan::build(&topology).unwrap();
        assert!(!plan.nodes.contains(&B));
        // B's people have no other candidate, so they are reported, not dropped silently.
        assert_eq!(plan.unroutable, vec!["b1", "b2", "b3"]);
        topology.gateways[1].approval = None;
        assert!(!RoutePlan::build(&topology).unwrap().nodes.contains(&B));
    }

    #[test]
    fn forwarding_never_returns_the_way_it_came_and_plans_with_loops_are_refused() {
        let plan = RoutePlan::build(&households()).unwrap();
        for member in &plan.members {
            for node in &plan.nodes {
                for from in plan.neighbors(*node) {
                    assert!(!plan
                        .forward_to(*node, Some(from), member.stream)
                        .contains(&from));
                }
            }
        }
        // A stream from A arriving at B from anywhere but A's side is refused.
        assert!(plan.forward_to(A, Some(B), 1).is_empty());
        let mut looped = plan.clone();
        looped.nodes = vec![HOST, A, B];
        looped.edges = vec![
            Edge {
                a: HOST,
                b: A,
                one_way_ms: 1,
            },
            Edge {
                a: A,
                b: B,
                one_way_ms: 1,
            },
            Edge {
                a: HOST,
                b: B,
                one_way_ms: 1,
            },
        ];
        assert_eq!(looped.validate(), Err(PlanError::NotATree));
        let mut chain = plan.clone();
        chain.nodes = (0..6).map(NodeId).collect();
        chain.edges = (0..5)
            .map(|n| Edge {
                a: NodeId(n),
                b: NodeId(n + 1),
                one_way_ms: 1,
            })
            .collect();
        chain.members.clear();
        assert_eq!(chain.validate(), Err(PlanError::TooManyHops));
    }

    #[test]
    fn mute_deafen_revocation_and_switch_change_only_what_they_should() {
        let plan = RoutePlan::build(&households()).unwrap();
        let muted = plan.with_state(4, true, false).unwrap();
        assert!(muted.forward_to(B, None, 4).is_empty());
        assert!(muted.local_receivers(B, 4).is_empty());
        let deaf = plan.with_state(5, false, true).unwrap();
        assert_eq!(deaf.local_receivers(B, 1), vec![4, 6]);
        let revoked = plan.without(6);
        assert!(revoked.attachment(6).is_none());
        assert_eq!(revoked.local_receivers(B, 1), vec![4, 5]);
        assert!(revoked.forward_to(B, None, 6).is_empty());
        let state = plan.with_state(7, true, true).unwrap();
        let moved = state
            .switch(7, *state.nodes.last().unwrap(), q(50))
            .unwrap();
        let c = moved.attachment(7).unwrap();
        assert!(c.muted && c.deafened, "a path switch keeps mute and deafen");
        assert_eq!(moved.epoch, state.epoch + 1);
        assert_eq!(
            state.switch(7, NodeId(9), q(5)),
            Err(PlanError::UnknownNode)
        );
    }

    #[test]
    fn header_round_trips_and_refuses_inconsistent_hop_counts() {
        let mut header = MeshHeader::first(binding().tag(), 7, 9, 1234);
        header.hops_left -= 1;
        header.via.push(A);
        let bytes = header.encode(b"opaque").unwrap();
        assert_eq!(bytes.len(), HEADER_LEN + 6);
        let (back, payload) = MeshHeader::decode(&bytes).unwrap();
        assert_eq!((back, payload), (header, &b"opaque"[..]));
        let mut bad = bytes.clone();
        bad[1] = MAX_FORWARDERS; // via 1 + hops 4 > limit
        assert!(MeshHeader::decode(&bad).is_none());
        let mut twice = bytes.clone();
        twice[2] = 2;
        twice[1] = 1;
        twice[5] = A.0;
        assert!(
            MeshHeader::decode(&twice).is_none(),
            "a forwarder listed twice is a loop"
        );
        assert!(MeshHeader::first(1, 1, 1, 1).encode(&[]).is_none());
        assert_ne!(
            binding().tag(),
            Binding {
                channel_id: "voice-2".into(),
                ..binding()
            }
            .tag()
        );
    }

    fn with_upload(a: Option<u32>, b: Option<u32>) -> Topology {
        let mut topology = households();
        topology.gateways[0].upload_kbps = a;
        topology.gateways[1].upload_kbps = b;
        topology
    }

    #[test]
    fn a_home_gateway_never_needs_more_than_half_its_measured_upload() {
        // Unlimited: C sits at A, so A carries a1-a3 to B and the host, and every stream to C.
        let free = RoutePlan::build(&households()).unwrap();
        assert_eq!(free.attachment(7).unwrap().node, A);
        assert_eq!(free.upload_kbps(A, 2), 6 * MESH_STREAM_KBPS);
        // 1.2 Mbit/s: C is moved off A to the host, and A stays within 600 kbit/s.
        let tight = RoutePlan::build(&with_upload(Some(1200), None)).unwrap();
        assert_eq!(tight.attachment(7).unwrap().node, HOST);
        assert!(tight.upload_kbps(A, 2) <= 600);
        assert!(
            tight.nodes.contains(&A),
            "A's own home still uses its gateway"
        );
        // 400 kbit/s cannot carry even A's own streams out: A's home falls back to the host.
        let weak = RoutePlan::build(&with_upload(Some(400), None)).unwrap();
        assert!(!weak.nodes.contains(&A));
        assert_eq!(weak.unroutable, vec!["a1", "a2", "a3"]);
        assert!(weak.nodes.contains(&B));
        for plan in [&free, &tight, &weak] {
            plan.validate().unwrap();
        }
        // Everyone at once is sized when asked for.
        let mut all = with_upload(Some(1200), None);
        all.speakers = 8;
        let plan = RoutePlan::build(&all).unwrap();
        assert!(plan.upload_kbps(A, 8) <= 600 || !plan.nodes.contains(&A));
    }

    #[test]
    fn a_link_between_homes_is_used_only_when_it_is_not_worse_than_the_host() {
        let mut slow = households();
        slow.trunks[0].quality = q(200); // 100 ms against 20 + 20 + 20 through the host
        let plan = RoutePlan::build(&slow).unwrap();
        assert!(plan.edge(A, B).is_none());
        assert_eq!(
            plan.toward(A, B),
            Some(HOST),
            "A and B meet through the host instead"
        );
        let mut no_host = slow.clone();
        no_host.host_relay = false;
        let plan = RoutePlan::build(&no_host).unwrap();
        assert!(!plan.nodes.contains(&HOST));
        assert_eq!(
            plan.nodes.len(),
            1,
            "one home keeps local speech; the other uses the host"
        );
        let mut good = households();
        good.host_relay = false;
        let plan = RoutePlan::build(&good).unwrap();
        assert_eq!(plan.nodes, vec![A, B]);
        assert!(plan.edges.iter().all(|e| e.a != HOST && e.b != HOST));
        assert!(plan.attachment(8).is_none_or(|d| d.node != HOST));
    }

    #[test]
    fn plans_from_older_peers_still_parse() {
        let plan = RoutePlan::build(&households()).unwrap();
        let mut json: serde_json::Value = serde_json::to_value(&plan).unwrap();
        for member in json["members"].as_array_mut().unwrap() {
            member.as_object_mut().unwrap().remove("home");
        }
        let back: RoutePlan = serde_json::from_value(json).unwrap();
        assert!(back.members.iter().all(|m| !m.home));
    }
}
