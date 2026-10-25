pub mod host_locator;
use serde::{Deserialize, Serialize};
pub mod direct_voice;
pub mod game_activity;
pub mod gateway_broker;
pub mod handover;
pub mod hosting;
pub mod installation;
pub mod lan_discovery;
pub mod lan_pairing;
pub mod takeover;

pub const CRYPTO_MAX_OPERATION_JSON_BYTES: usize = 1536 * 1024;
pub const CRYPTO_MAX_OPERATION_DECODED_BYTES: usize = 1024 * 1024;
pub const CRYPTO_MAX_EVENT_DECODED_BYTES: usize = 512 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct User {
    pub id: String,
    pub username: String,
    pub role: String,
    #[serde(default)]
    pub profile: UserProfile,
}

/// Community-scoped presentation. The stable user ID and login username are separate.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct UserProfile {
    #[serde(default)]
    pub appearance: ProfileAppearance,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub bio: String,
    #[serde(default)]
    pub avatar_url: Option<String>,
    #[serde(default)]
    pub version: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct UpdateProfile {
    pub display_name: String,
    pub bio: String,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub appearance: Option<ProfileAppearance>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ProfileAppearance {
    pub pronouns: String,
    pub accent: u32,
    pub banner: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effect: Option<String>,
}
impl Default for ProfileAppearance {
    fn default() -> Self {
        Self {
            pronouns: String::new(),
            accent: 0x5865f2,
            banner: 0x232428,
            effect: None,
        }
    }
}
impl ProfileAppearance {
    pub fn valid(&self) -> bool {
        self.pronouns.chars().count() <= 32
            && self.accent <= 0xffffff
            && self.banner <= 0xffffff
            && self
                .effect
                .as_deref()
                .is_none_or(|effect| matches!(effect, "none" | "glow" | "sparkle" | "wave"))
            && !self.pronouns.chars().any(|c| {
                c.is_control() || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
            })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ServerPresentation {
    pub name: String,
    pub description: String,
    pub accent: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub welcome_channel_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_channel_id: Option<String>,
}
impl Default for ServerPresentation {
    fn default() -> Self {
        Self {
            name: String::new(),
            description: String::new(),
            accent: 0x5865f2,
            welcome_channel_id: None,
            system_channel_id: None,
        }
    }
}
impl ServerPresentation {
    pub fn valid(&self) -> bool {
        !self.name.trim().is_empty()
            && self.name.chars().count() <= 100
            && self.description.chars().count() <= 300
            && self.accent <= 0xffffff
            && [&self.welcome_channel_id, &self.system_channel_id]
                .into_iter()
                .all(|id| id.as_ref().is_none_or(|id| id.len() <= 128))
            && !self.name.chars().any(|c| {
                c.is_control() || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
            })
            && !self.description.chars().any(|c| {
                (c.is_control() && c != '\n')
                    || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
            })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CryptoTarget {
    pub channel_id: Option<String>,
    pub peer_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CryptoDevice {
    pub user_id: String,
    pub device_id: String,
    /// Fingerprint of the client's signing identity; clients verify it.
    pub identity: String,
    /// Public MLS TLS KeyPackage, never device private state.
    pub key_package: String,
    /// Only in the community directory: the proof the device's owner made with
    /// the secret of the invitation they joined through. The server cannot
    /// create or check it; only the inviting host's client can.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invite_attestation: Option<InviteAttestation>,
}

/// `mac` is the lowercase hexadecimal HMAC-SHA-256 (64 characters) described in
/// `docs/encryption.md`, "Invitation-based verification". `invite_id` is the
/// identifier of the invitation the owner joined with (the server's `id`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InviteAttestation {
    pub invite_id: String,
    pub mac: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PutInviteAttestation {
    pub device_id: String,
    pub invite_id: String,
    pub mac: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct RegisterCryptoDevice {
    pub device_id: String,
    pub identity: String,
    pub key_package: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct PublishCryptoKeyPackages {
    pub device_id: String,
    pub packages: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ConsumeCryptoKeyPackage {
    pub target: CryptoTarget,
    pub device_id: String,
    /// SHA-256 of the decoded public TLS KeyPackage selected for this Welcome.
    pub package_hash: String,
    pub welcome_sequence: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CryptoKeyPackageStatus {
    pub device_id: String,
    pub unused: u32,
    pub reserved: u32,
    pub consumed: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CryptoEvent {
    pub sequence: u64,
    pub group_id: String,
    pub epoch: u64,
    pub kind: String,
    pub author_user_id: String,
    pub author_device_id: String,
    pub recipient_device_id: Option<String>,
    pub body: String,
    pub operation_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CryptoContext {
    pub server_id: String,
    pub target: CryptoTarget,
    pub roster: Vec<User>,
    pub devices: Vec<CryptoDevice>,
    pub group_id: Option<String>,
    /// Current durable head, which can be ahead of a bounded events page.
    pub sequence: u64,
    pub events: Vec<CryptoEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct CryptoEventBody {
    pub kind: String,
    pub recipient_device_id: Option<String>,
    pub body: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct AppendCryptoEvents {
    pub target: CryptoTarget,
    pub device_id: String,
    pub group_id: String,
    pub expected_sequence: u64,
    pub epoch: u64,
    pub operation_id: String,
    pub events: Vec<CryptoEventBody>,
}

/// Opaque authenticated MLS application control, separate from chat history.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CallKeyRequest {
    pub target: CryptoTarget,
    pub device_id: String,
    pub group_id: String,
    pub epoch: u64,
    pub operation_id: String,
    pub body: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CallKeyDelivery {
    pub server_id: String,
    /// Target relative to the authenticated caller, including for a direct call.
    pub target: CryptoTarget,
    pub author_user_id: String,
    pub request: CallKeyRequest,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AckCallKeyRequest {
    pub target: CryptoTarget,
    pub device_id: String,
    pub group_id: String,
    pub operation_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CallKeyTarget {
    pub server_id: String,
    pub target: CryptoTarget,
    pub group_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Channel {
    #[serde(default)]
    pub voice_parent_id: Option<String>,
    pub id: String,
    pub name: String,
    pub kind: String,
    pub topic: String,
    #[serde(default = "public_visibility")]
    pub visibility: String,
    #[serde(default)]
    pub category_id: Option<String>,
    #[serde(default)]
    pub position: i64,
    #[serde(default = "bool_true")]
    pub can_send: bool,
    #[serde(default = "bool_true")]
    pub can_join: bool,
    #[serde(default = "bool_true")]
    pub can_stream: bool,
    #[serde(default)]
    pub slowmode_seconds: u32,
    #[serde(default)]
    pub slowmode_bypass: bool,
    #[serde(default)]
    pub can_manage_pins: bool,
}

pub fn public_visibility() -> String {
    "public".into()
}
pub fn bool_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Attachment {
    pub id: String,
    pub name: String,
    pub mime: String,
    pub size: u64,
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Reaction {
    pub emoji: String,
    pub user_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Message {
    pub id: String,
    pub channel_id: Option<String>,
    pub recipient_id: Option<String>,
    pub author: User,
    pub content: String,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edited_at: Option<String>,
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    #[serde(default)]
    pub reactions: Vec<Reaction>,
    #[serde(default)]
    pub sequence: u64,
    #[serde(default)]
    pub reply_to_id: Option<String>,
    #[serde(default)]
    pub mentions: Vec<String>,
    #[serde(default)]
    pub client_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PinnedMessage {
    pub message: Message,
    pub pinned_by: User,
    pub pinned_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct TypingRequest {
    pub channel_id: Option<String>,
    pub peer_id: Option<String>,
    pub typing: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypingUser {
    pub user: User,
    pub expires_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TypingState {
    pub users: Vec<TypingUser>,
    #[serde(default)]
    pub server_time_ms: u64,
}

/// All HTTP errors preserve `error`. Cooldowns additionally include a stable
/// machine-readable code, milliseconds remaining and an HTTP Retry-After header.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ApiFailure {
    pub error: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct VoiceMember {
    pub user: User,
    pub channel_id: String,
    pub muted: bool,
    pub deafened: bool,
    pub sharing: bool,
    pub camera: bool,
    #[serde(default)]
    pub session_id: u64,
    #[serde(default)]
    pub device_id: Option<String>,
}

/// How a community protects message content. `Protected` is the default and
/// means Relay's own end-to-end layer. `Open` is the owner's explicit choice to
/// run without it; each member must still accept it before their client sends
/// anything unprotected.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ProtectionLevel {
    #[default]
    Protected,
    Open,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default, deny_unknown_fields)]
pub struct CommunityProtection {
    pub level: ProtectionLevel,
    /// Increases with every change; consent is given for one revision only.
    pub revision: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changed_by: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SetProtection {
    pub level: ProtectionLevel,
    pub expected_revision: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Bootstrap {
    /// A frozen source offers a read-only view for resuming/cancelling the move.
    /// It never renews a voice lease or permits a mutation.
    #[serde(default)]
    pub host_paused: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_authority: Option<handover::RoutePin>,
    #[serde(default)]
    pub account_login_only: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_provider: Option<AccountProvider>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub linked_identities: Vec<LinkedIdentity>,
    #[serde(default)]
    pub protection: CommunityProtection,
    #[serde(default)]
    pub server_presentation: ServerPresentation,
    #[serde(default)]
    pub assets: Vec<MediaAsset>,
    #[serde(default)]
    pub conversations: Vec<ConversationInfo>,
    /// Friends groups: invitations waiting for this account, and this account's own as an owner.
    #[serde(default)]
    pub group_invitations: Vec<GroupInvitation>,
    #[serde(default)]
    pub group_invitations_sent: Vec<SentGroupInvitation>,
    /// Coarse state of the other people in this account's own Friends groups.
    #[serde(default)]
    pub group_presence: Vec<GroupPresence>,
    #[serde(default)]
    pub scheduled_events: Vec<ScheduledEvent>,
    #[serde(default)]
    pub stage_members: Vec<StageMember>,
    #[serde(default)]
    pub social: SocialState,
    #[serde(default)]
    pub presence: Vec<UserPresence>,
    #[serde(default)]
    pub roles: Vec<CommunityRole>,
    #[serde(default)]
    pub member_roles: Vec<MemberRoles>,
    #[serde(default)]
    pub permissions: u32,
    pub server_name: String,
    pub user: User,
    pub channels: Vec<Channel>,
    pub members: Vec<User>,
    pub voice_members: Vec<VoiceMember>,
    #[serde(default)]
    pub server_id: String,
    /// Runtime generation, distinct from the durable community identity.
    #[serde(default)]
    pub instance_id: String,
    #[serde(default)]
    pub protocol_version: u32,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub read_states: Vec<ReadState>,
    #[serde(default)]
    pub latest_change_sequence: u64,
}

/// Optional operator-configured identity service. Never replaces local login.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AccountProvider {
    pub issuer: String,
    pub client_id: String,
    #[serde(default)]
    pub community_url: String,
}

/// This community confirmed the account link; this is not E2EE device trust.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LinkedIdentity {
    pub user_id: String,
    pub issuer: String,
    pub handle: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommunitySystemEvent {
    pub sequence: u64,
    pub user: User,
    pub kind: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct SocialState {
    /// Server Unix seconds at response construction, on the private-call expiry clock.
    /// Zero means the legacy peer did not provide a server-time sample.
    #[serde(default)]
    pub server_time: u64,
    pub friends: Vec<String>,
    pub incoming: Vec<String>,
    pub outgoing: Vec<String>,
    pub blocked: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub calls: Vec<PrivateCall>,
}

/// Short-lived signalling only. Accepting never grants voice/media admission.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PrivateCall {
    pub id: String,
    pub channel_id: String,
    pub caller_id: String,
    pub recipient_id: String,
    pub state: String,
    pub expires_at: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct ConversationInfo {
    pub channel_id: String,
    pub kind: String,
    pub owner_id: String,
    pub parent_id: Option<String>,
    pub archived: bool,
    pub member_ids: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct ScheduledEvent {
    #[serde(default)]
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub channel_id: String,
    pub starts_at: u64,
    pub ends_at: u64,
    #[serde(default)]
    pub interested: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct StageMember {
    pub channel_id: String,
    pub user_id: String,
    pub speaker: bool,
    pub raised: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct MediaAsset {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub owner_id: String,
    pub version: u64,
    pub url: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ImageCrop {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}
impl ImageCrop {
    pub fn valid(&self) -> bool {
        [self.x, self.y, self.width, self.height]
            .iter()
            .all(|v| v.is_finite())
            && self.x >= 0.0
            && self.y >= 0.0
            && self.width > 0.0
            && self.height > 0.0
            && self.x + self.width <= 1.000001
            && self.y + self.height <= 1.000001
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct UserPresence {
    #[serde(default)]
    pub user_id: String,
    pub status: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub expires_at: Option<u64>,
    /// Opt-in game activity, set by the Friends service for current friends only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<game_activity::GameActivity>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserReport {
    pub id: String,
    pub reporter_id: String,
    pub reported_id: String,
    pub reason: String,
    pub evidence: String,
    pub created_at: String,
    pub resolved: bool,
}

pub const PERMISSION_READ: u32 = 1;
pub const PERMISSION_SEND: u32 = 2;
pub const PERMISSION_JOIN: u32 = 4;
pub const PERMISSION_STREAM: u32 = 8;
pub const PERMISSION_CHANNELS: u32 = 16;
pub const PERMISSION_MESSAGES: u32 = 32;
pub const PERMISSION_MEMBERS: u32 = 64;
pub const PERMISSION_INVITES: u32 = 128;
pub const PERMISSION_ROLES: u32 = 256;
pub const PERMISSION_ADMIN: u32 = 512;
pub const PERMISSION_ALL: u32 = 1023;

impl Bootstrap {
    pub fn has_permission(&self, permission: u32) -> bool {
        let bits = if self.capabilities.iter().any(|c| c == "custom-roles-v1") {
            self.permissions
        } else {
            match self.user.role.as_str() {
                "admin" => PERMISSION_ALL,
                "moderator" => 15 | PERMISSION_MESSAGES | PERMISSION_MEMBERS | PERMISSION_INVITES,
                _ => 15,
            }
        };
        bits & PERMISSION_ADMIN != 0 || bits & permission == permission
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CommunityRole {
    #[serde(default)]
    pub id: String,
    pub name: String,
    pub color: u32,
    pub position: u32,
    pub permissions: u32,
}
impl CommunityRole {
    pub fn valid(&self) -> bool {
        !self.name.trim().is_empty()
            && self.name.chars().count() <= 64
            && !self.name.chars().any(|c| {
                c.is_control() || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
            })
            && self.color <= 0xffffff
            && (1..=1000).contains(&self.position)
            && self.permissions & !PERMISSION_ALL == 0
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct MemberRoles {
    pub user_id: String,
    pub role_ids: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    pub sequence: u64,
    pub actor_id: String,
    pub action: String,
    pub target_id: String,
    pub created_at: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct AuthRequest {
    /// Optional authenticator or single-use recovery code; never persisted by clients.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub second_factor: Option<String>,
    /// Applied atomically only when creating the first local owner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_protection: Option<ProtectionLevel>,
    pub username: String,
    pub password: String,
    #[serde(default)]
    pub invite: String,
}

impl std::fmt::Debug for AuthRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthRequest")
            .field("username", &self.username)
            .field("credentials", &"[redacted]")
            .finish_non_exhaustive()
    }
}

pub const AUTHENTICATOR_REQUIRED: &str = "Enter your authenticator code or a recovery code.";
pub const AUTHENTICATOR_INVALID: &str =
    "That authenticator or recovery code is not valid. Try a new code.";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub token: String,
    pub user: User,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SendMessage {
    pub channel_id: Option<String>,
    pub recipient_id: Option<String>,
    pub content: String,
    #[serde(default)]
    pub attachment_ids: Vec<String>,
    #[serde(default)]
    pub client_id: Option<String>,
    #[serde(default)]
    pub reply_to_id: Option<String>,
    #[serde(default)]
    pub mentions: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditMessage {
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateChannel {
    pub name: String,
    pub kind: String,
    #[serde(default)]
    pub topic: String,
    #[serde(default = "public_visibility")]
    pub visibility: String,
    #[serde(default)]
    pub category_id: Option<String>,
    #[serde(default)]
    pub position: i64,
    #[serde(default)]
    pub slowmode_seconds: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HistoryPage {
    pub messages: Vec<Message>,
    pub next_before: Option<u64>,
    pub latest_sequence: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ReadState {
    pub channel_id: Option<String>,
    pub peer_id: Option<String>,
    pub last_read_sequence: u64,
    pub unread_count: u64,
    pub mention_count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MarkRead {
    pub channel_id: Option<String>,
    pub peer_id: Option<String>,
    pub last_read_sequence: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MessageChange {
    pub sequence: u64,
    pub kind: String,
    pub message_id: String,
    pub channel_id: Option<String>,
    pub recipient_id: Option<String>,
    pub user_id: String,
    pub deleted: bool,
    pub message: Option<Message>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ChangePage {
    pub changes: Vec<MessageChange>,
    pub next_cursor: u64,
    pub has_more: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct EditChannel {
    pub name: Option<String>,
    pub topic: Option<String>,
    pub visibility: Option<String>,
    pub category_id: Option<String>,
    pub position: Option<i64>,
    pub slowmode_seconds: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ChannelPermission {
    pub subject_kind: String,
    pub subject_id: String,
    pub read: bool,
    pub send: bool,
    pub join: bool,
    pub stream: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct SetChannelPermissions {
    pub entries: Vec<ChannelPermission>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoiceJoin {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_revision: Option<u64>,
    pub channel_id: String,
    #[serde(default)]
    pub device_id: Option<String>,
}

/// Explicit user-confirmed replacement of one exact voice admission.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrivateCallJoin {
    pub instance_id: String,
    pub join: VoiceJoin,
    pub replace_session_id: Option<u64>,
    pub state: VoiceState,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct PrivateCallAdmission {
    pub call: PrivateCall,
    pub ticket: VoiceTicket,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoiceLeaveExact {
    pub instance_id: String,
    pub session_id: u64,
    pub channel_id: String,
    pub device_id: String,
    pub open_revision: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct VoiceState {
    pub muted: bool,
    pub deafened: bool,
    pub sharing: bool,
    pub camera: bool,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct VoiceResumeCapability {
    pub token: String,
    pub instance_id: String,
}
impl std::fmt::Debug for VoiceResumeCapability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VoiceResumeCapability")
            .field("token", &"[redacted]")
            .field("instance_id", &self.instance_id)
            .finish()
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoiceResume {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_revision: Option<u64>,
    pub channel_id: String,
    pub device_id: String,
    pub resume_token: String,
}

/// These are server-readable media keys, explicitly NOT end-to-end encryption.
/// No Debug implementation: secrets must not enter routine logs.
#[derive(Serialize, Deserialize)]
pub struct OpenCallMaterial {
    pub revision: u64,
    pub epoch: u64,
    pub group_id: String,
    pub senders: Vec<OpenCallSender>,
}
#[derive(Serialize, Deserialize)]
pub struct OpenCallSender {
    pub user_id: String,
    pub device_id: String,
    pub opus: String,
    pub rtc: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoiceTicket {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume: Option<VoiceResumeCapability>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resumed_state: Option<VoiceState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tunnel_url: Option<String>,
    pub session_id: u64,
    pub key: String,
    pub udp_port: u16,
    #[serde(default)]
    pub wire_version: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RtcTicket {
    pub url: String,
    pub token: String,
    pub room: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_bridge: Option<RtcMediaBridge>,
}

/// An optional fixed SFU route over the same HTTPS origin as signaling. Uses
/// a media-only call capability, never the community account bearer.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RtcMediaBridge {
    pub url: String,
    pub peer_ip: String,
    pub peer_port: u16,
    pub token: String,
}
impl std::fmt::Debug for RtcMediaBridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RtcMediaBridge")
            .field("url", &self.url)
            .field("peer_ip", &self.peer_ip)
            .field("peer_port", &self.peer_port)
            .field("token", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaFrame {
    pub user: User,
    pub kind: String,
    pub frame_id: String,
    pub url: String,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub kind: String,
    #[serde(default)]
    pub channel_id: Option<String>,
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipient_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub typing: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<u64>,
}

// UDP wire format: session id (u64 LE), sequence (u64 LE), then ChaCha20-Poly1305 ciphertext.
// The header is authenticated associated data. Nonce = 4-byte direction + sequence LE.
// Client->server direction is 0; server->client direction is 1. Session keys are unique.
// Plaintext: 0 = authenticated keepalive; 1 = Opus frame: sender id u64 LE + encoded mono 48 kHz audio.
pub const AUDIO_SAMPLE_RATE: u32 = 48_000;
pub const AUDIO_FRAME_SAMPLES: usize = 960;
pub const AUDIO_PACKET_V2: u8 = 2;
/// Opaque E2EE voice payload after the V2 routing/source prefix. Server rewrites
/// only the routing ticket ID; the signed account/epoch/device envelope is intact.
pub const AUDIO_PACKET_V3: u8 = 4;
pub const AUDIO_V2_HEADER_LEN: usize = 26;
/// Authenticated nine-byte probe: type plus little-endian u64 client ID. Echoes
/// only to its registered session source with a unique direction-one nonce.
pub const UDP_PING: u8 = 3;
/// Authenticated path challenge echoed to its source WITHOUT replacing the
/// selected voice address. Tests a candidate gateway while the call continues.
pub const UDP_PATH_PROBE: u8 = 8;
pub const API_PROTOCOL_VERSION: u32 = 2;
/// Friends group conversations. Only the Friends service advertises it.
pub const PEOPLE_GROUPS_CAPABILITY: &str = "people-groups-v1";
/// A Friends group has at most this many members, its creator included.
pub const PEOPLE_GROUP_MAX_MEMBERS: usize = 8;
/// Friends groups are joined by explicit invitation and acceptance.
pub const PEOPLE_GROUP_INVITES_CAPABILITY: &str = "people-group-invites-v1";
/// A waiting invitation lapses after this many days.
pub const PEOPLE_GROUP_INVITE_DAYS: u64 = 7;
/// One person can have this many invitations waiting at once.
pub const PEOPLE_GROUP_PENDING_PER_PERSON: usize = 50;
/// Members of an accepted Friends group see each other's coarse state there only.
pub const PEOPLE_GROUP_PRESENCE_CAPABILITY: &str = "people-group-presence-v1";
/// The event kind that says someone's coarse state changed in a group.
pub const GROUP_PRESENCE_EVENT: &str = "group-presence";

/// What an invited person may see before accepting: who invited them and the
/// group's name. Nothing about its members, messages, keys or files.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct GroupInvitation {
    pub channel_id: String,
    pub name: String,
    pub inviter_id: String,
}
/// One person's coarse state inside one group: `online`, `idle`, `dnd` or `offline`.
/// Never a status text, expiry, time, device, activity or anything global.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct GroupPresence {
    pub channel_id: String,
    pub user_id: String,
    pub status: String,
}
/// A group owner's own waiting invitation.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct SentGroupInvitation {
    pub channel_id: String,
    pub invitee_id: String,
}
pub const UDP_HEADER_LEN: usize = 16;
pub const UDP_MAX_PACKET: usize = 1500;
pub const MAX_UPLOAD_BYTES: usize = 10 * 1024 * 1024;
pub const MAX_FRAME_BYTES: usize = 512 * 1024;
pub const MAX_SLOWMODE_SECONDS: u32 = 21_600;
pub const MAX_CHANNEL_PINS: usize = 50;
pub const TYPING_TTL_MS: u64 = 8_000;

/// A targeted private voice frame. Its inner V3 frame uses a two-person MLS scope.
pub const AUDIO_WHISPER: u8 = 5;
pub fn wrap_whisper(target: &str, inner: &[u8]) -> Option<Vec<u8>> {
    if target.is_empty() || target.len() > 128 || inner.first() != Some(&AUDIO_PACKET_V3) {
        return None;
    }
    let mut result = vec![AUDIO_WHISPER];
    result.extend_from_slice(inner.get(1..9)?);
    result.push(target.len() as u8);
    result.extend_from_slice(target.as_bytes());
    result.extend_from_slice(inner);
    (result.len() + UDP_HEADER_LEN + 16 <= UDP_MAX_PACKET).then_some(result)
}
pub fn unwrap_whisper(packet: &[u8]) -> Option<(&str, &[u8])> {
    if packet.first() != Some(&AUDIO_WHISPER) {
        return None;
    }
    let size = *packet.get(9)? as usize;
    if !(1..=128).contains(&size) {
        return None;
    }
    let target = std::str::from_utf8(packet.get(10..10 + size)?).ok()?;
    let inner = packet.get(10 + size..)?;
    (inner.first() == Some(&AUDIO_PACKET_V3)).then_some((target, inner))
}
pub mod discord;
