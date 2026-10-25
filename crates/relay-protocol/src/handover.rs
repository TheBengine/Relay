//! Planned community handover. A preparation never grants a second writer.
use serde::{Deserialize, Serialize};

pub const SCHEMA: u32 = 1;
pub const MAX_ROUTE_MOVES: usize = 8;
/// Bounded authority/status IPC, including eight public delegation records.
pub const CONTROL_BYTES: u64 = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Offered,
    Accepted,
    Frozen,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Preparation {
    pub id: String,
    pub target_user_id: String,
    pub target_host_id: String,
    pub target_url: String,
    pub offered_by: String,
    pub phase: Phase,
    pub updated_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Status {
    pub server_id: String,
    pub source_host_id: String,
    pub term: u64,
    pub writes_allowed: bool,
    pub preparation: Option<Preparation>,
    /// Only true after the durable source retirement grant has been emitted.
    pub activation_available: bool,
    #[serde(default)]
    pub target_identity: Option<HostIdentity>,
    #[serde(default)]
    pub source_identity: Option<HostIdentity>,
    #[serde(default)]
    pub receipt_challenge: Option<String>,
    #[serde(default)]
    pub received: Option<Receipt>,
    #[serde(default)]
    pub retired: Option<ActivationGrant>,
    /// Target-signed access to the destination only; never a source login.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination: Option<DestinationCard>,
    /// Public source/target signatures connecting earlier hosts to this one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<ActivationGrant>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostIdentity {
    /// UUID v8 derived from the domain-separated SHA-256 of the signing key.
    pub host_id: String,
    pub public_key: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostProof {
    pub identity: HostIdentity,
    pub signature: String,
}

/// Fixed-order, domain-separated consent to this exact handover. Mutable phase
/// and progress timestamps are excluded so persisted proof remains verifiable.
pub fn acceptance_message(
    server_id: &str,
    source_host_id: &str,
    source_identity: &HostIdentity,
    term: u64,
    p: &Preparation,
) -> Vec<u8> {
    let mut message = b"relay-planned-handover-target-v2\0".to_vec();
    message.extend(term.to_be_bytes());
    // Length-prefixed UTF-8 avoids ambiguous concatenation, with a stable
    // encoding independent of JSON field order or serialization settings.
    for field in [
        server_id,
        source_host_id,
        &source_identity.host_id,
        &source_identity.public_key,
        &p.id,
        &p.target_user_id,
        &p.target_host_id,
        &p.target_url,
        &p.offered_by,
    ] {
        message.extend(
            u32::try_from(field.len())
                .expect("bounded handover field")
                .to_be_bytes(),
        );
        message.extend(field.as_bytes());
    }
    message
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Offer {
    pub server_id: String,
    pub target_user_id: String,
    pub target_host_id: String,
    pub target_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination: Option<DestinationCard>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Answer {
    pub server_id: String,
    pub preparation_id: String,
    /// This is separate consent from keeping a filtered member copy.
    pub accept_full_server_data: bool,
    /// Older clients may consent, but cannot freeze or activate an unbound host.
    #[serde(default)]
    pub target_proof: Option<HostProof>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Change {
    pub server_id: String,
    pub preparation_id: String,
}

/// Separate encryption key, signed by the destination's accepted host key.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recipient {
    pub public_key: String,
    pub signature: String,
}

pub fn recipient_message(status: &Status, public_key: &str) -> Option<Vec<u8>> {
    let p = status.preparation.as_ref()?;
    let mut message = b"relay-handover-encryption-recipient-v1\0".to_vec();
    message.extend(acceptance_message(
        &status.server_id,
        &status.source_host_id,
        status.source_identity.as_ref()?,
        status.term,
        p,
    ));
    message.extend(u32::try_from(public_key.len()).ok()?.to_be_bytes());
    message.extend(public_key.as_bytes());
    Some(message)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportRequest {
    pub change: Change,
    pub recipient: Recipient,
    #[serde(default)]
    pub retry: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HostRuntime {
    pub name: String,
    pub storage_limit_bytes: u64,
    pub user_storage_limit_bytes: u64,
    pub account_provider: Option<crate::AccountProvider>,
    /// Machine-local services/credentials require destination setup; never
    /// smuggle source loopback URLs or owner-control credentials to another PC.
    pub rtc_setup_required: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferContext {
    pub export_id: String,
    pub server_id: String,
    pub source_host_id: String,
    pub source_identity: HostIdentity,
    pub term: u64,
    pub preparation_id: String,
    pub target_identity: HostIdentity,
    pub target_url: String,
    pub recipient_key: String,
    pub schema_version: u32,
    pub database_bytes: u64,
    pub database_sha256: String,
    pub runtime: HostRuntime,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferPart {
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferManifest {
    pub context: TransferContext,
    pub parts: Vec<TransferPart>,
    pub signature: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ExportStatus {
    Building,
    Ready { manifest: TransferManifest },
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartRequest {
    pub request: ExportRequest,
    pub export_id: String,
    pub index: usize,
}

/// The destination attests to exactly the complete image it verified on disk.
/// A fresh source challenge prevents a receipt from a cancelled preparation
/// being reused; this is not permission to activate until the source retires.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReceiptBody {
    pub server_id: String,
    pub source_host_id: String,
    pub term: u64,
    pub preparation_id: String,
    pub export_id: String,
    pub target_identity: HostIdentity,
    pub target_url: String,
    pub database_sha256: String,
    pub database_bytes: u64,
    pub manifest_sha256: String,
    pub challenge: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub body: ReceiptBody,
    pub signature: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActivationGrant {
    pub receipt: Receipt,
    pub source_identity: HostIdentity,
    pub next_term: u64,
    /// New grants commit to the complete inherited ancestry. Legacy grants
    /// retain their exact v1 signature bytes when this field is absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_sha256: Option<String>,
    pub signature: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Retire {
    pub change: Change,
    /// The app must explain that source retirement is irreversible, even if
    /// the destination goes offline before it starts serving the community.
    pub confirm_stop_source: bool,
    /// Required before a new retirement. Old retries of a durable grant remain
    /// idempotent even after this short-lived preflight has expired.
    #[serde(default)]
    pub readiness: Option<ReadyProof>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadinessChallenge {
    pub source: RoutePin,
    pub source_host_id: String,
    pub preparation_id: String,
    pub receipt_sha256: String,
    pub challenge: String,
    /// Short-lived source authorization, bound to this exact received image.
    /// Legacy unsigned requests deserialize but cannot start a new preflight.
    #[serde(default)]
    pub expires_at: u64,
    #[serde(default)]
    pub signature: String,
}

pub fn readiness_challenge_message(request: &ReadinessChallenge) -> Vec<u8> {
    serde_json::to_vec(&(
        "relay-handover-readiness-request-v1",
        &request.source,
        &request.source_host_id,
        &request.preparation_id,
        &request.receipt_sha256,
        &request.challenge,
        request.expires_at,
    ))
    .expect("bounded readiness request")
}

/// Private short-lived access to a synthetic room, never community content.
/// Deliberately omits Debug because the ticket contains bearer capabilities.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RtcProbeBody {
    pub request: ReadinessChallenge,
    pub target: HostIdentity,
    pub target_url: String,
    pub instance_id: String,
    pub expires_at: u64,
    pub rtc: [crate::RtcTicket; 2],
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RtcProbeTicket {
    pub body: RtcProbeBody,
    pub signature: String,
}
pub fn rtc_probe_ticket_message(body: &RtcProbeBody) -> Vec<u8> {
    serde_json::to_vec(&("relay-handover-rtc-probe-ticket-v1", body))
        .expect("bounded private preflight ticket")
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadyBody {
    pub request: ReadinessChallenge,
    pub target: HostIdentity,
    pub target_url: String,
    pub instance_id: String,
    pub http_port: u16,
    pub voice_port: u16,
    /// Fresh authenticated SFU administration check on the destination. This
    /// does not claim media quality or reachability from a remote participant.
    pub rtc_ready: bool,
    /// This challenge opened SFU signaling and exchanged media packets through
    /// its fixed route. It makes no codec, device or WAN quality claim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rtc_media_ready: Option<bool>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadyProof {
    pub body: ReadyBody,
    pub signature: String,
}

pub fn ready_message(body: &ReadyBody) -> Vec<u8> {
    serde_json::to_vec(&("relay-handover-ready-v1", body)).expect("bounded readiness proof")
}

/// Learned only through an authenticated, already pinned community connection.
/// Knowing a server UUID or seeing a LAN announcement never establishes this pin.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoutePin {
    pub server_id: String,
    pub host: HostIdentity,
    pub term: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteChallenge {
    pub challenge: String,
}

/// No user, account, invitation secret or database content is published here.
/// A move is announced only after the irreversible source grant is durable.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteState {
    pub pin: RoutePin,
    pub challenge: String,
    pub writes_allowed: bool,
    pub moved: Option<ActivationGrant>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<ActivationGrant>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteProof {
    pub state: RouteState,
    pub signature: String,
}

pub fn receipt_message(body: &ReceiptBody) -> Vec<u8> {
    serde_json::to_vec(&("relay-handover-received-v1", body)).expect("bounded receipt")
}
pub fn grant_message(grant: &ActivationGrant) -> Vec<u8> {
    if let Some(history) = &grant.history_sha256 {
        return serde_json::to_vec(&(
            "relay-handover-retired-v2", &grant.receipt, &grant.source_identity,
            grant.next_term, history,
        )).expect("bounded grant");
    }
    serde_json::to_vec(&(
        "relay-handover-retired-v1",
        &grant.receipt,
        &grant.source_identity,
        grant.next_term,
    ))
    .expect("bounded grant")
}
pub fn history_message(history: &[ActivationGrant]) -> Vec<u8> {
    serde_json::to_vec(&("relay-handover-history-v1",history)).expect("bounded history")
}
pub fn route_message(state: &RouteState) -> Vec<u8> {
    serde_json::to_vec(&("relay-handover-route-v1", state)).expect("bounded route")
}

/// A member's setup card is only a proposed machine/address. It grants no data
/// access and is never permission to freeze or activate the community.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IrohDestination {
    pub endpoint_id: String,
    pub relay: Option<String>,
    pub token: String,
    /// This target's local transport namespace. Absent on legacy setup cards.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}
impl std::fmt::Debug for IrohDestination {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IrohDestination").field("endpoint_id", &self.endpoint_id)
            .field("token", &"<redacted>").finish_non_exhaustive()
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DestinationBody {
    pub source: RoutePin,
    pub user_id: String,
    pub target: HostIdentity,
    pub target_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub private_route: Option<IrohDestination>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DestinationCard {
    pub body: DestinationBody,
    pub signature: String,
}
pub fn destination_message(body: &DestinationBody) -> Vec<u8> {
    serde_json::to_vec(&("relay-handover-destination-card-v1", body))
        .expect("bounded destination card")
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetNetworkIdentity {
    pub target: HostIdentity,
    pub endpoint_id: Option<String>,
}
pub fn manifest_message(context: &TransferContext, parts: &[TransferPart]) -> Vec<u8> {
    serde_json::to_vec(&("relay-handover-source-image-v1", context, parts))
        .expect("bounded transfer manifest")
}

/// One bounded JSON command over the companion's owned stdin/stdout pipe.
/// No account bearer, password or private key is part of this contract.
/// A setup request may carry this destination's scoped connection invitation.
#[derive(Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub enum TargetCommand {
    Identity,
    NetworkIdentity,
    Plan {
        source: RoutePin,
        user_id: String,
        target_url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        private_route: Option<IrohDestination>,
    },
    Accept {
        status: Status,
    },
    Recipient {
        status: Status,
    },
    Receive {
        status: Status,
        cache: std::path::PathBuf,
    },
    Receipt {
        status: Status,
    },
    Activate {
        status: Status,
        grant: ActivationGrant,
    },
}
