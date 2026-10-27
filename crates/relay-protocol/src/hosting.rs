//! Host roles, candidate approval and an honest assessment of each candidate.
//!
//! Pure rules with no I/O, shared by the server (which stores and enforces them)
//! and the client (which explains them). Nothing here lets a machine write to a
//! community: a role is a permission to hold, never authority to decide. Choosing
//! who may accept new changes after an outage is a separate protocol.
use serde::{Deserialize, Serialize};

pub const SCHEMA: u32 = 1;
pub const MAX_CANDIDATES: usize = 16;
/// A copy report older than this is not trusted to describe the copy now.
pub const REPORT_FRESH_MS: u64 = 120_000;
/// Changes a copy may be missing and still count as catching up.
pub const CATCH_UP_LIMIT: u64 = 200;
/// Opus bitrate of the audio itself, per voice stream.
pub const VOICE_KBPS: u32 = 96;
/// Voice packets per second per stream (10 ms packets).
pub const PACKETS_PER_SECOND: u32 = 100;
/// ASSUMPTION: bytes added to every voice packet (UDP and IP headers plus
/// Relay framing and encryption). The real figure depends on the transport.
pub const PACKET_OVERHEAD_BYTES: u32 = 60;
/// Estimated line rate of one voice stream: audio plus the assumed headers.
/// An estimate only. It leaves out repair data, signalling and other traffic.
pub const STREAM_KBPS: u32 = VOICE_KBPS + PACKET_OVERHEAD_BYTES * 8 * PACKETS_PER_SECOND / 1000;
/// Group size assumed for capacity when no call has been measured.
pub const DEFAULT_PARTICIPANTS: u32 = 8;
/// ASSUMPTION: streams that talk at the same time in the capacity estimate.
pub const SIMULTANEOUS_SPEAKERS: u32 = 2;

/// What a candidate may hold. The active host is whoever runs the community now
/// and is not a role that can be offered.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostRole {
    /// Agrees to keep an approved copy of the community data once copying
    /// exists, and to take part in a handover. It never writes on its own.
    Replica,
    /// Agrees to relay voice for the people it serves once gateways exist.
    VoiceGateway,
}

impl HostRole {
    pub const ALL: [HostRole; 2] = [HostRole::Replica, HostRole::VoiceGateway];

    pub fn label(self) -> &'static str {
        match self {
            HostRole::Replica => "Data copy",
            HostRole::VoiceGateway => "Voice gateway",
        }
    }

    pub fn explanation(self) -> &'static str {
        match self {
            HostRole::Replica => {
                "Allows this member to keep a read-only copy of community data their account may receive. Copying starts only when they choose it. It does not make this PC a host or enable handover."
            }
            HostRole::VoiceGateway => {
                "Allows this member to relay voice for nearby participants. It starts only when they enable local voice gateways and join a call. It does not store messages or change community permissions."
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateState {
    /// Offered by an administrator; the member has not answered.
    Pending,
    /// The member accepted at least one offered role.
    Approved,
    Declined,
    /// Withdrawn by an administrator, or the member stepped down.
    Revoked,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidate {
    pub user_id: String,
    pub state: CandidateState,
    /// Roles an administrator offered.
    pub offered: Vec<HostRole>,
    /// Roles the member accepted. Only the member can add to this list.
    pub accepted: Vec<HostRole>,
    pub offered_by: String,
    pub updated_ms: u64,
}

impl Candidate {
    /// Roles that are both offered and accepted. Empty unless approved.
    pub fn effective(&self) -> Vec<HostRole> {
        if self.state != CandidateState::Approved {
            return Vec::new();
        }
        self.offered
            .iter()
            .copied()
            .filter(|role| self.accepted.contains(role))
            .collect()
    }

    pub fn holds(&self, role: HostRole) -> bool {
        self.effective().contains(&role)
    }

    /// Offered roles the member has not accepted yet.
    pub fn waiting_for_acceptance(&self) -> Vec<HostRole> {
        if !matches!(
            self.state,
            CandidateState::Pending | CandidateState::Approved
        ) {
            return Vec::new();
        }
        self.offered
            .iter()
            .copied()
            .filter(|role| !self.accepted.contains(role))
            .collect()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    TooManyCandidates,
    NoRoles,
    UnknownCandidate,
    /// The member tried to accept a role nobody offered.
    NotOffered,
    WrongState(&'static str),
}

impl Error {
    pub fn message(&self) -> &'static str {
        match self {
            Error::TooManyCandidates => "A community can have up to 16 host candidates.",
            Error::NoRoles => "Choose at least one role to offer.",
            Error::UnknownCandidate => "This member is not a host candidate.",
            Error::NotOffered => "You can only accept roles that were offered to you.",
            Error::WrongState(message) => message,
        }
    }
}

fn normalized(roles: &[HostRole]) -> Vec<HostRole> {
    let mut roles = roles.to_vec();
    roles.sort();
    roles.dedup();
    roles
}

/// The approved candidates of one community. Every change raises `revision`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Roster {
    pub schema: u32,
    pub revision: u64,
    pub candidates: Vec<Candidate>,
    /// How the host may change. Older rosters have none: keep the current host.
    #[serde(default)]
    pub switching: crate::host_switch::SwitchPolicy,
}

impl Default for Roster {
    fn default() -> Self {
        Self {
            schema: SCHEMA,
            revision: 0,
            candidates: Vec::new(),
            switching: Default::default(),
        }
    }
}

impl Roster {
    pub fn candidate(&self, user: &str) -> Option<&Candidate> {
        self.candidates.iter().find(|c| c.user_id == user)
    }

    fn candidate_mut(&mut self, user: &str) -> Result<&mut Candidate, Error> {
        self.candidates
            .iter_mut()
            .find(|c| c.user_id == user)
            .ok_or(Error::UnknownCandidate)
    }

    /// Members that currently hold a role.
    pub fn holders(&self, role: HostRole) -> Vec<&Candidate> {
        self.candidates.iter().filter(|c| c.holds(role)).collect()
    }

    /// An administrator offers roles. Roles that are taken away apply at once.
    /// Roles that are added wait until the member accepts them.
    pub fn offer(
        &mut self,
        user: &str,
        roles: &[HostRole],
        by: &str,
        now_ms: u64,
    ) -> Result<(), Error> {
        let roles = normalized(roles);
        if roles.is_empty() {
            return Err(Error::NoRoles);
        }
        if self.candidate(user).is_none() {
            if self.candidates.len() >= MAX_CANDIDATES {
                return Err(Error::TooManyCandidates);
            }
            self.candidates.push(Candidate {
                user_id: user.into(),
                state: CandidateState::Pending,
                offered: Vec::new(),
                accepted: Vec::new(),
                offered_by: by.into(),
                updated_ms: now_ms,
            });
        }
        let candidate = self.candidate_mut(user)?;
        match candidate.state {
            CandidateState::Declined | CandidateState::Revoked => candidate.accepted.clear(),
            CandidateState::Pending | CandidateState::Approved => {
                candidate.accepted.retain(|role| roles.contains(role));
            }
        }
        candidate.offered = roles;
        candidate.state = if candidate.accepted.is_empty() {
            CandidateState::Pending
        } else {
            CandidateState::Approved
        };
        candidate.offered_by = by.into();
        candidate.updated_ms = now_ms;
        self.revision += 1;
        Ok(())
    }

    /// The member answers with the roles they accept. Accepting nothing declines
    /// a pending offer, or steps down from an approved one.
    pub fn respond(&mut self, user: &str, accepted: &[HostRole], now_ms: u64) -> Result<(), Error> {
        let accepted = normalized(accepted);
        let candidate = self.candidate_mut(user)?;
        if !matches!(
            candidate.state,
            CandidateState::Pending | CandidateState::Approved
        ) {
            return Err(Error::WrongState("There is no open offer to answer."));
        }
        if accepted
            .iter()
            .any(|role| !candidate.offered.contains(role))
        {
            return Err(Error::NotOffered);
        }
        candidate.state = match (accepted.is_empty(), candidate.state) {
            (false, _) => CandidateState::Approved,
            (true, CandidateState::Approved) => CandidateState::Revoked,
            (true, _) => CandidateState::Declined,
        };
        candidate.accepted = accepted;
        candidate.updated_ms = now_ms;
        self.revision += 1;
        Ok(())
    }

    /// An administrator withdraws every role at once.
    pub fn revoke(&mut self, user: &str, now_ms: u64) -> Result<(), Error> {
        let candidate = self.candidate_mut(user)?;
        if !matches!(
            candidate.state,
            CandidateState::Pending | CandidateState::Approved
        ) {
            return Err(Error::WrongState("This member holds no roles to withdraw."));
        }
        candidate.state = CandidateState::Revoked;
        candidate.accepted.clear();
        candidate.updated_ms = now_ms;
        self.revision += 1;
        Ok(())
    }

    /// A member left the community, so nothing they held survives.
    pub fn forget(&mut self, user: &str) -> bool {
        let before = self.candidates.len();
        self.candidates.retain(|c| c.user_id != user);
        let changed = self.candidates.len() != before;
        if changed {
            self.revision += 1;
        }
        changed
    }
}

// ---------------------------------------------------------------------------
// Assessment

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Green,
    Yellow,
    Red,
    /// Not enough measurements to say.
    Unknown,
}

impl Verdict {
    /// Words, so the meaning never depends on colour.
    pub fn word(self) -> &'static str {
        match self {
            Verdict::Green => "Ready",
            Verdict::Yellow => "Limited",
            Verdict::Red => "Blocked",
            Verdict::Unknown => "Not measured",
        }
    }

    /// A failing requirement outranks missing information, which outranks a
    /// limitation.
    fn rank(self) -> u8 {
        match self {
            Verdict::Green => 0,
            Verdict::Yellow => 1,
            Verdict::Unknown => 2,
            Verdict::Red => 3,
        }
    }

    pub fn worst(self, other: Verdict) -> Verdict {
        if other.rank() > self.rank() {
            other
        } else {
            self
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Aspect {
    Permission,
    DataCopy,
    Takeover,
    VoiceQuality,
    Capacity,
}

impl Aspect {
    pub const ALL: [Aspect; 5] = [
        Aspect::Permission,
        Aspect::DataCopy,
        Aspect::Takeover,
        Aspect::VoiceQuality,
        Aspect::Capacity,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Aspect::Permission => "Permission",
            Aspect::DataCopy => "Data copy",
            Aspect::Takeover => "Takeover",
            Aspect::VoiceQuality => "Voice quality",
            Aspect::Capacity => "Capacity",
        }
    }
}

/// What a candidate is being judged for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Task {
    VoiceGateway,
    DataStandby,
    PlannedHandover,
    UnexpectedTakeover,
}

impl Task {
    pub const ALL: [Task; 4] = [
        Task::VoiceGateway,
        Task::DataStandby,
        Task::PlannedHandover,
        Task::UnexpectedTakeover,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Task::VoiceGateway => "Relay voice",
            Task::DataStandby => "Keep a data copy",
            Task::PlannedHandover => "Planned handover",
            Task::UnexpectedTakeover => "Take over after an outage",
        }
    }

    pub fn role(self) -> HostRole {
        match self {
            Task::VoiceGateway => HostRole::VoiceGateway,
            _ => HostRole::Replica,
        }
    }

    pub fn needs(self, aspect: Aspect) -> bool {
        use Aspect::*;
        match self {
            Task::VoiceGateway => matches!(aspect, Permission | VoiceQuality | Capacity),
            Task::DataStandby => matches!(aspect, Permission | DataCopy | Capacity),
            Task::PlannedHandover | Task::UnexpectedTakeover => {
                matches!(aspect, Permission | DataCopy | Takeover | Capacity)
            }
        }
    }
}

/// What a copy reported about itself.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReplicaFacts {
    pub applied_seq: u64,
    pub head_seq: u64,
    pub files_pending: u32,
    pub reported_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ParticipantQuality {
    pub rtt_ms: f32,
    pub jitter_ms: f32,
    pub loss_pct: f32,
    pub late_pct: f32,
}

#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
pub struct VoiceFacts {
    pub measured: Vec<ParticipantQuality>,
    pub unmeasured: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Default, Serialize, Deserialize)]
pub struct CapacityFacts {
    pub upload_kbps: Option<u32>,
    pub cpu_free_pct: Option<u8>,
    pub free_disk_mb: Option<u64>,
    /// People the estimate is for. Zero means the default group size.
    pub participants: u32,
}

/// What this version of Relay can actually do. Set from the build, never by a
/// candidate, so a missing protocol is reported as missing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Abilities {
    pub planned_handover: bool,
    pub unexpected_takeover: bool,
}

#[derive(Clone, Debug)]
pub struct Facts<'a> {
    pub candidate: Option<&'a Candidate>,
    pub replica: Option<ReplicaFacts>,
    pub voice: Option<VoiceFacts>,
    pub capacity: Option<CapacityFacts>,
    pub abilities: Abilities,
    /// Approved full copies, counting the active host.
    pub approved_replicas: usize,
    pub now_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dimension {
    pub aspect: Aspect,
    /// False when the task does not depend on this aspect.
    pub required: bool,
    pub verdict: Verdict,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Assessment {
    pub task: Task,
    pub dimensions: Vec<Dimension>,
    pub overall: Verdict,
    pub summary: String,
}

fn dim(aspect: Aspect, required: bool, verdict: Verdict, text: impl Into<String>) -> Dimension {
    Dimension {
        aspect,
        required,
        verdict,
        text: text.into(),
    }
}

fn permission(task: Task, facts: &Facts) -> (Verdict, String) {
    let role = task.role();
    let Some(candidate) = facts.candidate else {
        return (
            Verdict::Red,
            format!("Not offered the \"{}\" role.", role.label()),
        );
    };
    if candidate.holds(role) {
        return (
            Verdict::Green,
            format!(
                "Approved for \"{}\" by the member and an administrator.",
                role.label()
            ),
        );
    }
    let text = match candidate.state {
        CandidateState::Pending => "The member has not answered the offer yet.".to_string(),
        CandidateState::Declined => "The member declined.".to_string(),
        CandidateState::Revoked => "The role was withdrawn.".to_string(),
        CandidateState::Approved if candidate.offered.contains(&role) => {
            format!("The member has not accepted \"{}\" yet.", role.label())
        }
        CandidateState::Approved => format!("\"{}\" was not offered.", role.label()),
    };
    (Verdict::Red, text)
}

/// True when the copy's own fresh report says it holds everything.
pub fn copy_ready(facts: &Facts) -> bool {
    data_copy(facts).0 == Verdict::Green
}

fn data_copy(facts: &Facts) -> (Verdict, String) {
    let Some(copy) = facts.replica else {
        return (Verdict::Unknown, "No data copy has reported yet.".into());
    };
    // A report dated after "now" cannot be aged, so it is not trusted as fresh.
    let Some(age) = facts.now_ms.checked_sub(copy.reported_ms) else {
        return (
            Verdict::Unknown,
            "The report is dated in the future, so its age cannot be trusted.".into(),
        );
    };
    if age > REPORT_FRESH_MS {
        return (
            Verdict::Unknown,
            format!(
                "The last report is {} min old, too old to trust.",
                age / 60_000
            ),
        );
    }
    if copy.applied_seq > copy.head_seq {
        return (
            Verdict::Red,
            "The copy claims newer data than the host. It needs a fresh snapshot.".into(),
        );
    }
    let behind = copy.head_seq - copy.applied_seq;
    match (behind, copy.files_pending) {
        (0, 0) => (
            Verdict::Green,
            "The copy reports that it is up to date. This is the copy's own report; it is not yet checked against a snapshot of the host.".into(),
        ),
        (0, files) => (
            Verdict::Yellow,
            format!("Messages are secured; {files} file(s) are still being copied."),
        ),
        (behind, _) if behind <= CATCH_UP_LIMIT => (
            Verdict::Yellow,
            format!("Missing {behind} change(s); catching up."),
        ),
        (behind, _) => (
            Verdict::Red,
            format!("{behind} change(s) behind. It cannot take over until it catches up."),
        ),
    }
}

fn takeover(task: Task, facts: &Facts) -> (Verdict, String) {
    match task {
        Task::PlannedHandover if !facts.abilities.planned_handover => (
            Verdict::Red,
            "Planned handover is not available in this version of Relay yet.".into(),
        ),
        Task::PlannedHandover => (
            Verdict::Green,
            "Can take over at an agreed time while the current host is online.".into(),
        ),
        Task::UnexpectedTakeover if !facts.abilities.unexpected_takeover => (
            Verdict::Red,
            "Taking over after an unexpected outage needs a voting protocol. This version does not have it, so no machine may continue alone.".into(),
        ),
        Task::UnexpectedTakeover if facts.approved_replicas < 3 => (
            Verdict::Red,
            format!(
                "Needs at least three approved full copies so two can agree. There are {}.",
                facts.approved_replicas
            ),
        ),
        Task::UnexpectedTakeover => (
            Verdict::Green,
            "Can continue when a majority of the approved copies agree.".into(),
        ),
        _ => (Verdict::Green, "Not needed for this role.".into()),
    }
}

fn voice_quality(facts: &Facts) -> (Verdict, String) {
    let voice = facts.voice.clone().unwrap_or_default();
    if voice.measured.is_empty() {
        return (
            Verdict::Unknown,
            "No participant has measured the connection to this machine yet.".into(),
        );
    }
    // The person with the worst connection decides, not the average.
    let mut worst = (Verdict::Green, String::new());
    for who in &voice.measured {
        let checks = [
            (who.rtt_ms, 60.0, 120.0, "ping", "ms"),
            (who.jitter_ms, 15.0, 30.0, "jitter", "ms"),
            (who.loss_pct, 1.0, 3.0, "packet loss", "%"),
            (who.late_pct, 2.0, 5.0, "late packets", "%"),
        ];
        for (value, green, yellow, name, unit) in checks {
            let verdict = if !value.is_finite() || value < 0.0 {
                Verdict::Unknown
            } else if value <= green {
                Verdict::Green
            } else if value <= yellow {
                Verdict::Yellow
            } else {
                Verdict::Red
            };
            if verdict.rank() > worst.0.rank() {
                worst = (
                    verdict,
                    format!("worst participant has {value:.0} {unit} {name}"),
                );
            }
        }
    }
    let missing = voice.unmeasured;
    match worst.0 {
        Verdict::Red => (Verdict::Red, format!("Poor connection: {}.", worst.1)),
        Verdict::Unknown => (Verdict::Unknown, "A measurement is unreadable.".into()),
        _ if missing > 0 => (
            Verdict::Unknown,
            format!(
                "{missing} participant(s) have not measured the connection yet. The {} measured so far look {}.",
                voice.measured.len(),
                if worst.0 == Verdict::Green { "fine" } else { "limited" }
            ),
        ),
        Verdict::Yellow => (Verdict::Yellow, format!("Usable, but {}.", worst.1)),
        _ => (
            Verdict::Green,
            format!("Stable for all {} participant(s) measured.", voice.measured.len()),
        ),
    }
}

/// ESTIMATED upload a gateway needs: each simultaneous speaker goes to every
/// other listener at `STREAM_KBPS`. This is not a guarantee and not a complete
/// traffic budget: repair data, signalling and other traffic are left out.
pub fn voice_upload_need_kbps(participants: u32) -> u32 {
    let listeners = participants.max(2) - 1;
    SIMULTANEOUS_SPEAKERS
        .saturating_mul(listeners)
        .saturating_mul(STREAM_KBPS)
}

/// Facts a task needs before capacity may be called ready. A missing or
/// unreadable one keeps the answer at "not measured".
fn capacity(task: Task, facts: &Facts) -> (Verdict, String) {
    let c = facts.capacity.unwrap_or_default();
    let assumed_people = c.participants == 0;
    let people = if assumed_people {
        DEFAULT_PARTICIPANTS
    } else {
        c.participants
    };
    let voice = task == Task::VoiceGateway;

    let mut measured: Vec<String> = Vec::new();
    let mut missing: Vec<String> = Vec::new();
    let mut blocking: Vec<String> = Vec::new();
    let mut limited: Vec<String> = Vec::new();

    // Processor, needed by every role.
    match c.cpu_free_pct {
        Some(cpu) if cpu <= 100 => {
            measured.push(format!("processor {cpu}% free"));
            if cpu < 10 {
                blocking.push(format!("only {cpu}% processor free"));
            } else if cpu < 30 {
                limited.push(format!("{cpu}% processor free"));
            }
        }
        Some(cpu) => missing.push(format!("processor (unreadable value {cpu})")),
        None => missing.push("processor".into()),
    }
    if voice {
        match c.upload_kbps {
            Some(up) => {
                let need = voice_upload_need_kbps(people);
                measured.push(format!("upload {up} kbit/s"));
                if up < need {
                    blocking.push(format!(
                        "upload {up} kbit/s is below the estimated {need} kbit/s"
                    ));
                } else if up < need.saturating_mul(3) / 2 {
                    limited.push(format!(
                        "upload {up} kbit/s leaves little room over the estimated {need} kbit/s"
                    ));
                }
            }
            None => missing.push("upload".into()),
        }
    } else {
        match c.free_disk_mb {
            Some(mb) => {
                measured.push(format!("storage {} MB free", mb));
                if mb < 1024 {
                    blocking.push(format!("only {mb} MB of storage free"));
                } else if mb < 5 * 1024 {
                    limited.push(format!("{} GB of storage free", mb / 1024));
                }
            }
            None => missing.push("storage".into()),
        }
    }

    let verdict = if !blocking.is_empty() {
        Verdict::Red
    } else if !missing.is_empty() {
        Verdict::Unknown
    } else if !limited.is_empty() {
        Verdict::Yellow
    } else {
        Verdict::Green
    };

    let mut text = match verdict {
        Verdict::Red => format!("Blocked by what was measured: {}.", blocking.join("; ")),
        Verdict::Unknown => format!("Not enough measured: {}.", missing.join(", ")),
        Verdict::Yellow => format!("Limited: {}.", limited.join("; ")),
        Verdict::Green => "Everything this role needs was measured and is sufficient.".into(),
    };
    if verdict != Verdict::Unknown && !missing.is_empty() {
        text.push_str(&format!(" Not measured: {}.", missing.join(", ")));
    }
    if measured.is_empty() {
        text.push_str(" Nothing has been measured yet.");
    } else {
        text.push_str(&format!(" Measured: {}.", measured.join(", ")));
    }
    if voice {
        text.push_str(&format!(
            " Upload need is an estimate for {people} people{}: {SIMULTANEOUS_SPEAKERS} speakers at once, each stream {STREAM_KBPS} kbit/s including an assumed {PACKET_OVERHEAD_BYTES} bytes of headers per packet. Real traffic also has repair data and signalling.",
            if assumed_people { " (assumed, not measured)" } else { "" }
        ));
    }
    (verdict, text)
}

pub fn assess(task: Task, facts: &Facts) -> Assessment {
    let mut dimensions = Vec::new();
    for aspect in Aspect::ALL {
        let required = task.needs(aspect);
        let (verdict, text) = match aspect {
            Aspect::Permission => permission(task, facts),
            Aspect::DataCopy => data_copy(facts),
            Aspect::Takeover => takeover(task, facts),
            Aspect::VoiceQuality => voice_quality(facts),
            Aspect::Capacity => capacity(task, facts),
        };
        dimensions.push(dim(aspect, required, verdict, text));
    }
    let overall = dimensions
        .iter()
        .filter(|d| d.required)
        .fold(Verdict::Green, |all, d| all.worst(d.verdict));
    let blocking = dimensions
        .iter()
        .filter(|d| d.required)
        .filter(|d| d.verdict == overall)
        .map(|d| d.text.clone())
        .next()
        .unwrap_or_default();
    let summary = match overall {
        Verdict::Green => format!("Recommended for \"{}\". {}", task.label(), blocking),
        _ => blocking,
    };
    Assessment {
        task,
        dimensions,
        overall,
        summary,
    }
}

// ---------------------------------------------------------------------------
// What the server sends to members

/// The machine that runs the community now.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveHost {
    pub name: String,
    pub instance_id: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CandidateView {
    pub candidate: Candidate,
    /// Name to show for the member.
    pub name: String,
    /// One assessment for every task the offered roles can serve.
    pub assessments: Vec<Assessment>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HostingView {
    pub schema: u32,
    pub revision: u64,
    pub active_host: ActiveHost,
    pub abilities: Abilities,
    /// The member asking.
    pub you: String,
    /// Administrators can offer and withdraw roles.
    pub can_manage: bool,
    pub candidates: Vec<CandidateView>,
    /// Whether this machine could continue the community after an unexpected
    /// outage of the active host. Older servers omit it: not set up.
    #[serde(default)]
    pub takeover: TakeoverStatus,
    /// How the host may change, and what that means right now.
    #[serde(default)]
    pub switching: crate::host_switch::SwitchPolicy,
    #[serde(default)]
    pub switch_advice: String,
    /// What still works for members if the host disappears now.
    #[serde(default)]
    pub outage: Vec<OutageOption>,
    /// Addresses of the approved co-hosts that could continue the community.
    /// The app remembers them and asks them for a majority mandate only when
    /// this host is unreachable; an address alone grants nothing.
    #[serde(default)]
    pub standby_urls: Vec<String>,
    /// The steps to continuing without the host, each with its live state.
    #[serde(default)]
    pub takeover_setup: Vec<SetupStep>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupStep {
    pub what: String,
    pub done: bool,
    pub text: String,
}

/// The takeover setup as a checklist. `approved` co-hosts hold the data-copy
/// role; `copies_ready` of them report a fresh, complete copy. Nothing here is
/// claimed beyond what the host measured.
pub fn takeover_setup(approved: usize, copies_ready: usize, takeover: &TakeoverStatus, abilities: Abilities) -> Vec<SetupStep> {
    const NEEDED: usize = 2;
    let step = |what: &str, done: bool, text: String| SetupStep {
        what: what.into(),
        done,
        text,
    };
    let armed = matches!(
        takeover.state,
        TakeoverState::ProtectedHost | TakeoverState::CopyUpToDate | TakeoverState::TookOver
    );
    vec![
        step(
            "Approve two co-hosts",
            approved >= NEEDED,
            if approved >= NEEDED {
                format!("{approved} members hold an approved data-copy role.")
            } else {
                format!("{approved} of {NEEDED}: offer the data-copy role under Ask a member to help host; each member accepts it themselves.")
            },
        ),
        step(
            "Each co-host keeps an up-to-date copy",
            copies_ready >= NEEDED,
            format!("{copies_ready} of {NEEDED} report a complete copy within the last two minutes. A co-host turns this on under Your host roles."),
        ),
        step(
            "Turn on takeover protection",
            armed,
            if armed {
                "On: every change is kept by at least two of three approved copies.".into()
            } else if abilities.unexpected_takeover {
                "Ready to turn on once both steps above are done.".into()
            } else {
                "Not available in this version: the host and both co-hosts cannot yet exchange the signed set, consent and first full copy.".into()
            },
        ),
    ]
}

/// One row of the outage matrix: what a member can still do, and why.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutageOption {
    pub what: String,
    pub available: bool,
    pub text: String,
}

/// What members can do after the active host disappears, for this build and
/// this community. Says "not yet" rather than promising what is not wired.
pub fn outage_options(
    takeover: &TakeoverStatus,
    abilities: Abilities,
    account_sign_in: bool,
) -> Vec<OutageOption> {
    let continues = abilities.unexpected_takeover && takeover.state == TakeoverState::TookOver;
    let row = |what: &str, available: bool, text: String| OutageOption {
        what: what.into(),
        available,
        text,
    };
    let not_yet = "Not yet: a co-host can prepare the community, but members' apps cannot follow a new host in this version.";
    vec![
        row(
            "Friends: messages and calls",
            true,
            "Works: Friends run on the separate Friends service, not on this community's host.".into(),
        ),
        row(
            "Read history already on your device",
            true,
            "Works: messages your app already received stay readable offline.".into(),
        ),
        row(
            "Keep chatting in this community",
            continues,
            if continues {
                "Works after you sign in again on the new host.".into()
            } else {
                not_yet.into()
            },
        ),
        row(
            "Sign in again with your Relay account",
            continues && account_sign_in,
            if !account_sign_in {
                "Not available: this community does not use Relay accounts.".into()
            } else if continues {
                "Works: sign-ins are not copied, so every device signs in once on the new host.".into()
            } else {
                not_yet.into()
            },
        ),
        row(
            "Sign in with a community password",
            false,
            "Not available: password hashes are never copied to another host, and no reset exists yet.".into(),
        ),
        row(
            "Add a new device",
            continues && account_sign_in,
            if continues && account_sign_in {
                "Works after signing in; encrypted history needs approval from one of your existing devices.".into()
            } else {
                "Not available until the new host runs and you can sign in there.".into()
            },
        ),
        row(
            "Invite a new member",
            continues,
            if continues {
                "Works: open invitations are part of the copy.".into()
            } else {
                not_yet.into()
            },
        ),
    ]
}

/// What a member's or co-host's app measured, for the host overview.
pub const MAX_VOICE_SAMPLES: usize = 16;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoiceSample {
    /// The co-host whose machine was measured.
    pub host_user_id: String,
    pub quality: ParticipantQuality,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeasurementReport {
    /// The reporter's own machine. Group size is counted by the server.
    #[serde(default)]
    pub capacity: Option<CapacityFacts>,
    #[serde(default)]
    pub voice: Vec<VoiceSample>,
}

impl MeasurementReport {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.capacity.is_none() && self.voice.is_empty() {
            return Err("The report is empty");
        }
        if let Some(c) = self.capacity {
            if c.cpu_free_pct.is_some_and(|v| v > 100) {
                return Err("Free processor must be 0-100 %");
            }
        }
        if self.voice.len() > MAX_VOICE_SAMPLES {
            return Err("Too many voice measurements");
        }
        let mut hosts: Vec<&str> = Vec::new();
        for s in &self.voice {
            let q = s.quality;
            let ok = |v: f32, max: f32| v.is_finite() && (0.0..=max).contains(&v);
            if s.host_user_id.is_empty()
                || s.host_user_id.len() > 64
                || hosts.contains(&s.host_user_id.as_str())
                || !ok(q.rtt_ms, 10_000.0)
                || !ok(q.jitter_ms, 10_000.0)
                || !ok(q.loss_pct, 100.0)
                || !ok(q.late_pct, 100.0)
            {
                return Err("A voice measurement is invalid");
            }
            hosts.push(&s.host_user_id);
        }
        Ok(())
    }
}

/// How long the active host must be silent before an approved co-host may ask
/// for a takeover. A short network blip never moves the community.
pub const TAKEOVER_SILENCE_SECONDS: u64 = 30;

/// One state per reason, so the client never guesses from free text.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TakeoverState {
    /// This machine holds no approved full copy for unexpected takeover.
    #[default]
    NotSetUp,
    /// This machine is the active host and writes only with a majority of copies.
    ProtectedHost,
    /// Protected, but no co-host is confirming changes right now: changes wait.
    Paused,
    /// The active host stopped this machine: a majority chose another host.
    Fenced,
    /// The copy has every confirmed change; a takeover is possible if the host disappears.
    CopyUpToDate,
    /// Changes are missing or not yet applied; this copy may not take over.
    CopyOutOfDate,
    /// The copy failed a check and must be repaired before it can take over.
    CopyNeedsRepair,
    /// The host was silent, but too few approved copies agreed.
    NoMajority,
    /// A majority of approved copies chose this machine.
    TookOver,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TakeoverStatus {
    pub state: TakeoverState,
    /// Term of the latest election this machine knows of.
    #[serde(default)]
    pub term: u64,
    /// Approved full copies that agreed in the latest attempt, this one included.
    #[serde(default)]
    pub votes: u32,
}

impl TakeoverStatus {
    /// Approved full copies needed: the active host plus two co-hosts, 2 of 3.
    pub const NEEDED: u32 = 2;

    /// The one sentence the client shows. Never claims more than the state proves.
    pub fn text(&self) -> String {
        match self.state {
            TakeoverState::NotSetUp => {
                "Not set up yet: continuing without the host needs two approved co-hosts with a full data copy. Setting that up between machines is not available in this version.".into()
            }
            TakeoverState::ProtectedHost => format!(
                "Protected: every change is kept by at least {} of 3 approved copies",
                Self::NEEDED
            ),
            TakeoverState::Paused => "Paused: waiting for a co-host to confirm changes".into(),
            TakeoverState::Fenced => {
                "Stopped: approved co-hosts chose a new host while this one was unreachable".into()
            }
            TakeoverState::CopyUpToDate => "Data copy up to date".into(),
            TakeoverState::CopyOutOfDate => "Can't take over: copy is out of date".into(),
            TakeoverState::CopyNeedsRepair => "Can't take over: copy needs repair".into(),
            TakeoverState::NoMajority => format!(
                "Can't take over: only {} of {} approved copies agreed",
                self.votes,
                Self::NEEDED
            ),
            TakeoverState::TookOver => format!(
                "Took over: {} of 3 approved copies chose this machine (term {})",
                self.votes, self.term
            ),
        }
    }

    /// True for states that block every write or takeover until someone acts.
    pub fn is_problem(&self) -> bool {
        matches!(
            self.state,
            TakeoverState::Paused
                | TakeoverState::Fenced
                | TakeoverState::CopyOutOfDate
                | TakeoverState::CopyNeedsRepair
                | TakeoverState::NoMajority
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_000_000;

    fn approved(roles: &[HostRole]) -> Roster {
        let mut roster = Roster::default();
        roster.offer("bo", roles, "anna", NOW).unwrap();
        roster.respond("bo", roles, NOW).unwrap();
        roster
    }

    fn facts<'a>(roster: &'a Roster) -> Facts<'a> {
        Facts {
            candidate: roster.candidate("bo"),
            replica: None,
            voice: None,
            capacity: None,
            abilities: Abilities::default(),
            approved_replicas: 1,
            now_ms: NOW,
        }
    }

    fn good_voice() -> VoiceFacts {
        VoiceFacts {
            measured: vec![ParticipantQuality {
                rtt_ms: 24.0,
                jitter_ms: 4.0,
                loss_pct: 0.0,
                late_pct: 0.5,
            }],
            unmeasured: 0,
        }
    }

    #[test]
    fn an_offer_waits_for_the_member_and_nothing_is_effective_until_accepted() {
        let mut roster = Roster::default();
        roster
            .offer("bo", &[HostRole::Replica, HostRole::Replica], "anna", NOW)
            .unwrap();
        let bo = roster.candidate("bo").unwrap();
        assert_eq!(bo.state, CandidateState::Pending);
        assert_eq!(bo.offered, vec![HostRole::Replica]);
        assert!(bo.effective().is_empty() && !bo.holds(HostRole::Replica));
        assert_eq!(bo.waiting_for_acceptance(), vec![HostRole::Replica]);
        roster.respond("bo", &[HostRole::Replica], NOW + 1).unwrap();
        assert!(roster.candidate("bo").unwrap().holds(HostRole::Replica));
        assert_eq!(roster.revision, 2);
    }

    #[test]
    fn a_member_cannot_accept_what_was_not_offered_and_empty_offers_are_refused() {
        let mut roster = Roster::default();
        assert_eq!(roster.offer("bo", &[], "anna", NOW), Err(Error::NoRoles));
        roster
            .offer("bo", &[HostRole::VoiceGateway], "anna", NOW)
            .unwrap();
        assert_eq!(
            roster.respond("bo", &[HostRole::Replica], NOW),
            Err(Error::NotOffered)
        );
        assert_eq!(
            roster.respond("clara", &[], NOW),
            Err(Error::UnknownCandidate)
        );
        assert_eq!(roster.revision, 1, "refused changes do not count");
    }

    #[test]
    fn adding_a_role_needs_a_new_answer_but_taking_one_away_is_immediate() {
        let mut roster = approved(&[HostRole::Replica]);
        roster
            .offer(
                "bo",
                &[HostRole::Replica, HostRole::VoiceGateway],
                "anna",
                NOW,
            )
            .unwrap();
        let bo = roster.candidate("bo").unwrap();
        assert_eq!(bo.state, CandidateState::Approved);
        assert!(bo.holds(HostRole::Replica) && !bo.holds(HostRole::VoiceGateway));
        assert_eq!(bo.waiting_for_acceptance(), vec![HostRole::VoiceGateway]);
        roster
            .offer("bo", &[HostRole::VoiceGateway], "anna", NOW)
            .unwrap();
        let bo = roster.candidate("bo").unwrap();
        assert_eq!(bo.state, CandidateState::Pending);
        assert!(!bo.holds(HostRole::Replica));
    }

    #[test]
    fn declining_stepping_down_and_revoking_end_every_role_and_can_be_offered_again() {
        let mut roster = Roster::default();
        roster
            .offer("bo", &[HostRole::Replica], "anna", NOW)
            .unwrap();
        roster.respond("bo", &[], NOW).unwrap();
        assert_eq!(
            roster.candidate("bo").unwrap().state,
            CandidateState::Declined
        );
        assert!(roster.respond("bo", &[HostRole::Replica], NOW).is_err());
        roster
            .offer("bo", &[HostRole::Replica], "anna", NOW)
            .unwrap();
        roster.respond("bo", &[HostRole::Replica], NOW).unwrap();
        roster.respond("bo", &[], NOW).unwrap();
        let bo = roster.candidate("bo").unwrap();
        assert_eq!(bo.state, CandidateState::Revoked);
        assert!(bo.effective().is_empty());
        roster
            .offer("bo", &[HostRole::Replica], "anna", NOW)
            .unwrap();
        roster.respond("bo", &[HostRole::Replica], NOW).unwrap();
        roster.revoke("bo", NOW).unwrap();
        assert!(roster.holders(HostRole::Replica).is_empty());
        assert!(roster.revoke("bo", NOW).is_err());
        assert!(roster.forget("bo") && roster.candidate("bo").is_none());
        assert!(!roster.forget("bo"));
    }

    #[test]
    fn the_candidate_list_is_bounded() {
        let mut roster = Roster::default();
        for n in 0..MAX_CANDIDATES {
            roster
                .offer(&format!("u{n}"), &[HostRole::Replica], "anna", NOW)
                .unwrap();
        }
        assert_eq!(
            roster.offer("extra", &[HostRole::Replica], "anna", NOW),
            Err(Error::TooManyCandidates)
        );
        roster
            .offer("u0", &[HostRole::VoiceGateway], "anna", NOW)
            .unwrap();
    }

    #[test]
    fn a_good_ping_never_hides_a_missing_data_copy() {
        let roster = approved(&[HostRole::Replica, HostRole::VoiceGateway]);
        let mut f = facts(&roster);
        f.voice = Some(good_voice());
        f.capacity = Some(CapacityFacts {
            upload_kbps: Some(50_000),
            cpu_free_pct: Some(80),
            free_disk_mb: Some(100_000),
            participants: 8,
        });
        let voice = assess(Task::VoiceGateway, &f);
        assert_eq!(voice.overall, Verdict::Green, "{voice:?}");
        let standby = assess(Task::DataStandby, &f);
        assert_eq!(standby.overall, Verdict::Unknown);
        assert_eq!(
            standby.summary,
            "No data copy has reported yet.".to_string()
        );
    }

    #[test]
    fn data_copy_states_are_exact() {
        let roster = approved(&[HostRole::Replica]);
        let report = |applied, head, files, at| ReplicaFacts {
            applied_seq: applied,
            head_seq: head,
            files_pending: files,
            reported_ms: at,
        };
        let verdict = |copy: ReplicaFacts| {
            let mut f = facts(&roster);
            f.replica = Some(copy);
            data_copy(&f)
        };
        assert_eq!(verdict(report(10, 10, 0, NOW)).0, Verdict::Green);
        let files = verdict(report(10, 10, 3, NOW));
        assert_eq!(files.0, Verdict::Yellow);
        assert!(files.1.contains("3 file"));
        let behind = verdict(report(10, 150, 0, NOW));
        assert_eq!(
            (behind.0, behind.1.as_str()),
            (Verdict::Yellow, "Missing 140 change(s); catching up.")
        );
        assert_eq!(verdict(report(10, 10_000, 0, NOW)).0, Verdict::Red);
        assert_eq!(verdict(report(20, 10, 0, NOW)).0, Verdict::Red);
        let old = verdict(report(10, 10, 0, NOW - REPORT_FRESH_MS - 60_000));
        assert_eq!(old.0, Verdict::Unknown);
        assert!(old.1.contains("3 min"));
    }

    #[test]
    fn takeover_is_blocked_with_a_reason_until_the_protocol_exists() {
        let roster = approved(&[HostRole::Replica]);
        let mut f = facts(&roster);
        f.replica = Some(ReplicaFacts {
            applied_seq: 5,
            head_seq: 5,
            files_pending: 0,
            reported_ms: NOW,
        });
        f.capacity = Some(CapacityFacts {
            cpu_free_pct: Some(70),
            free_disk_mb: Some(50_000),
            ..Default::default()
        });
        let planned = assess(Task::PlannedHandover, &f);
        assert_eq!(planned.overall, Verdict::Red);
        assert!(planned.summary.contains("not available in this version"));
        f.abilities.planned_handover = true;
        assert_eq!(assess(Task::PlannedHandover, &f).overall, Verdict::Green);
        let outage = assess(Task::UnexpectedTakeover, &f);
        assert!(outage.summary.contains("voting protocol"));
        f.abilities.unexpected_takeover = true;
        f.approved_replicas = 2;
        assert!(assess(Task::UnexpectedTakeover, &f)
            .summary
            .contains("three approved"));
        f.approved_replicas = 3;
        assert_eq!(assess(Task::UnexpectedTakeover, &f).overall, Verdict::Green);
    }

    #[test]
    fn the_worst_participant_decides_voice_quality() {
        let roster = approved(&[HostRole::VoiceGateway]);
        let mut f = facts(&roster);
        let mut voice = good_voice();
        voice.measured.push(ParticipantQuality {
            rtt_ms: 180.0,
            jitter_ms: 5.0,
            loss_pct: 0.0,
            late_pct: 0.0,
        });
        f.voice = Some(voice.clone());
        let (verdict, text) = voice_quality(&f);
        assert_eq!(verdict, Verdict::Red);
        assert!(text.contains("180 ms ping"), "{text}");
        voice.measured[1].rtt_ms = 90.0;
        f.voice = Some(voice.clone());
        assert_eq!(voice_quality(&f).0, Verdict::Yellow);
        voice.measured[1].rtt_ms = 30.0;
        voice.unmeasured = 2;
        f.voice = Some(voice);
        let (verdict, text) = voice_quality(&f);
        assert_eq!(
            verdict,
            Verdict::Unknown,
            "partial measurements cannot be green"
        );
        assert!(text.contains("2 participant(s) have not measured"));
        f.voice = None;
        assert_eq!(voice_quality(&f).0, Verdict::Unknown);
    }

    fn with_capacity(roster: &Roster, c: CapacityFacts, task: Task) -> (Verdict, String) {
        let mut f = facts(roster);
        f.capacity = Some(c);
        capacity(task, &f)
    }

    #[test]
    fn capacity_compares_upload_with_the_estimate_for_the_group() {
        let roster = approved(&[HostRole::VoiceGateway]);
        assert_eq!(STREAM_KBPS, 96 + 48, "audio plus the assumed headers");
        assert_eq!(voice_upload_need_kbps(8), 2 * 7 * STREAM_KBPS);
        let measure = |upload| CapacityFacts {
            upload_kbps: Some(upload),
            cpu_free_pct: Some(60),
            free_disk_mb: None,
            participants: 8,
        };
        let need = voice_upload_need_kbps(8);
        let red = with_capacity(&roster, measure(need - 1), Task::VoiceGateway);
        assert_eq!(red.0, Verdict::Red);
        assert!(red.1.contains("below the estimated"), "{}", red.1);
        let yellow = with_capacity(&roster, measure(need + 100), Task::VoiceGateway);
        assert_eq!(yellow.0, Verdict::Yellow);
        let green = with_capacity(&roster, measure(need * 4), Task::VoiceGateway);
        assert_eq!(green.0, Verdict::Green);
        assert!(green.1.contains("Measured: processor 60% free, upload"));
        assert!(green.1.contains("estimate for 8 people"));
        assert!(
            !green.1.contains("assumed, not measured"),
            "the group size was given"
        );
    }

    #[test]
    fn a_missing_required_fact_is_never_ready() {
        let roster = approved(&[HostRole::Replica, HostRole::VoiceGateway]);
        let cpu_only = CapacityFacts {
            cpu_free_pct: Some(80),
            ..Default::default()
        };
        // A gateway with only a processor reading lacks its upload figure.
        let gateway = with_capacity(&roster, cpu_only, Task::VoiceGateway);
        assert_eq!(gateway.0, Verdict::Unknown);
        assert!(
            gateway.1.starts_with("Not enough measured: upload."),
            "{}",
            gateway.1
        );
        assert!(gateway.1.contains("Measured: processor 80% free"));
        assert!(
            gateway.1.contains("assumed, not measured"),
            "default group size is named"
        );
        // A standby with only a processor reading lacks storage.
        for task in [
            Task::DataStandby,
            Task::PlannedHandover,
            Task::UnexpectedTakeover,
        ] {
            let standby = with_capacity(&roster, cpu_only, task);
            assert_eq!(standby.0, Verdict::Unknown, "{task:?}");
            assert!(standby.1.starts_with("Not enough measured: storage."));
        }
        // Nothing measured at all.
        let none = with_capacity(&roster, CapacityFacts::default(), Task::DataStandby);
        assert_eq!(none.0, Verdict::Unknown);
        assert!(none.1.contains("processor, storage"));
        assert!(none.1.contains("Nothing has been measured yet."));
        let mut f = facts(&roster);
        f.capacity = None;
        assert_eq!(capacity(Task::VoiceGateway, &f).0, Verdict::Unknown);
    }

    #[test]
    fn each_task_needs_exactly_its_own_facts_to_be_ready() {
        let roster = approved(&[HostRole::Replica, HostRole::VoiceGateway]);
        let full = CapacityFacts {
            upload_kbps: Some(50_000),
            cpu_free_pct: Some(80),
            free_disk_mb: Some(100_000),
            participants: 4,
        };
        for task in Task::ALL {
            assert_eq!(
                with_capacity(&roster, full, task).0,
                Verdict::Green,
                "{task:?}"
            );
            // Drop one fact at a time: only the facts the task needs matter.
            let without_cpu = CapacityFacts {
                cpu_free_pct: None,
                ..full
            };
            let without_upload = CapacityFacts {
                upload_kbps: None,
                ..full
            };
            let without_disk = CapacityFacts {
                free_disk_mb: None,
                ..full
            };
            assert_eq!(
                with_capacity(&roster, without_cpu, task).0,
                Verdict::Unknown
            );
            let voice = task == Task::VoiceGateway;
            assert_eq!(
                with_capacity(&roster, without_upload, task).0,
                if voice {
                    Verdict::Unknown
                } else {
                    Verdict::Green
                },
                "{task:?} upload"
            );
            assert_eq!(
                with_capacity(&roster, without_disk, task).0,
                if voice {
                    Verdict::Green
                } else {
                    Verdict::Unknown
                },
                "{task:?} storage"
            );
        }
    }

    #[test]
    fn a_measured_blocking_failure_stays_red_even_when_other_facts_are_unknown() {
        let roster = approved(&[HostRole::Replica, HostRole::VoiceGateway]);
        let slow = CapacityFacts {
            upload_kbps: Some(100),
            ..Default::default()
        };
        let gateway = with_capacity(&roster, slow, Task::VoiceGateway);
        assert_eq!(gateway.0, Verdict::Red);
        assert!(
            gateway.1.contains("Not measured: processor."),
            "{}",
            gateway.1
        );
        let full_disk = CapacityFacts {
            free_disk_mb: Some(200),
            ..Default::default()
        };
        let standby = with_capacity(&roster, full_disk, Task::DataStandby);
        assert_eq!(standby.0, Verdict::Red);
        assert!(standby.1.contains("only 200 MB of storage free"));
        let busy = CapacityFacts {
            cpu_free_pct: Some(3),
            ..Default::default()
        };
        assert_eq!(
            with_capacity(&roster, busy, Task::VoiceGateway).0,
            Verdict::Red
        );
        // Storage is irrelevant to a voice gateway, so it cannot make one Red.
        let disk_only = with_capacity(&roster, full_disk, Task::VoiceGateway);
        assert_eq!(disk_only.0, Verdict::Unknown);
    }

    #[test]
    fn an_unreadable_processor_reading_is_not_a_measurement() {
        let roster = approved(&[HostRole::Replica]);
        for bad in [101_u8, 150, 255] {
            let c = CapacityFacts {
                cpu_free_pct: Some(bad),
                free_disk_mb: Some(100_000),
                ..Default::default()
            };
            let (verdict, text) = with_capacity(&roster, c, Task::DataStandby);
            assert_eq!(verdict, Verdict::Unknown, "{bad}");
            assert!(text.contains(&format!("unreadable value {bad}")), "{text}");
        }
        let edge = CapacityFacts {
            cpu_free_pct: Some(100),
            free_disk_mb: Some(100_000),
            ..Default::default()
        };
        assert_eq!(
            with_capacity(&roster, edge, Task::DataStandby).0,
            Verdict::Green
        );
    }

    #[test]
    fn a_report_from_the_future_is_unknown_not_fresh() {
        let roster = approved(&[HostRole::Replica]);
        let mut f = facts(&roster);
        for reported_ms in [NOW + 1, NOW + 10 * 60_000, u64::MAX] {
            f.replica = Some(ReplicaFacts {
                applied_seq: 5,
                head_seq: 5,
                files_pending: 0,
                reported_ms,
            });
            let (verdict, text) = data_copy(&f);
            assert_eq!(verdict, Verdict::Unknown, "{reported_ms}");
            assert!(text.contains("dated in the future"), "{text}");
        }
        // Exactly "now" is still a valid, fresh report.
        f.replica = Some(ReplicaFacts {
            applied_seq: 5,
            head_seq: 5,
            files_pending: 0,
            reported_ms: NOW,
        });
        let (verdict, text) = data_copy(&f);
        assert_eq!(verdict, Verdict::Green);
        assert!(text.contains("its own report") || text.contains("copy's own report"));
        assert!(text.contains("not yet checked against a snapshot"));
    }

    #[test]
    fn nothing_claims_that_copying_or_routing_is_already_running() {
        for role in HostRole::ALL {
            let words = role.explanation();
            // The work exists now, but it never starts by itself and never
            // makes the member a host: the words must keep saying so.
            assert!(words.contains("only when"), "{words}");
            assert!(
                words.contains("does not make this PC a host")
                    || words.contains("does not store messages"),
                "{words}"
            );
            assert!(!words.starts_with("Keeps") && !words.starts_with("Can relay"));
        }
    }

    #[test]
    fn permission_explains_why_a_role_is_missing() {
        let mut roster = Roster::default();
        let text = |roster: &Roster| permission(Task::DataStandby, &facts(roster)).1;
        assert!(text(&roster).contains("Not offered"));
        roster
            .offer("bo", &[HostRole::Replica], "anna", NOW)
            .unwrap();
        assert_eq!(text(&roster), "The member has not answered the offer yet.");
        roster.respond("bo", &[HostRole::Replica], NOW).unwrap();
        assert_eq!(
            permission(Task::DataStandby, &facts(&roster)).0,
            Verdict::Green
        );
        assert_eq!(
            permission(Task::VoiceGateway, &facts(&roster)).0,
            Verdict::Red
        );
    }

    #[test]
    fn red_outranks_unknown_outranks_limited() {
        use Verdict::*;
        assert_eq!(Green.worst(Yellow), Yellow);
        assert_eq!(Yellow.worst(Unknown), Unknown);
        assert_eq!(Unknown.worst(Red), Red);
        assert_eq!(Red.worst(Green), Red);
        for v in [Green, Yellow, Red, Unknown] {
            assert!(!v.word().is_empty());
        }
    }
}
