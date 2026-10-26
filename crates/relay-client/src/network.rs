use crate::{chat_media, media::CaptureFrame};
#[path = "network_account_login.rs"]
mod account_login;
#[path = "network_call_switch.rs"]
pub(crate) mod call_switch;
#[path = "network_game_activity.rs"]
mod game_activity;
#[path = "network_handover.rs"]
mod handover_work;
#[path = "network_host_resume.rs"]
mod host_resume;
#[path = "network_host_locator.rs"]
mod host_locator_publish;
#[path = "network_lan_pairing.rs"]
mod lan_pairing_work;
#[path = "network_people_call.rs"]
mod people_call;
use relay_protocol::*;
use reqwest::blocking::{Client, RequestBuilder};
use serde::{de::DeserializeOwned, Serialize};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc, Arc, Condvar, OnceLock,
    },
    time::{Duration, Instant},
};

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Target {
    Channel(String),
    Direct(String),
}

/// The identity visible when an account-bound action was queued. IDs alone are
/// insufficient when an account or community switch precedes that action.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CommandOrigin {
    pub url: String,
    pub server_id: String,
    pub user_id: String,
}

/// The protection and local device selected when a message was staged. A
/// restored retry must not silently become a new send under another identity.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SendBinding {
    pub protection: CommunityProtection,
    pub plain: bool,
    pub device_id: String,
    pub fingerprint: String,
}

impl From<&Connection> for CommandOrigin {
    fn from(connection: &Connection) -> Self {
        Self {
            url: connection.url.clone(),
            server_id: connection.server_id.clone(),
            user_id: connection.session.user.id.clone(),
        }
    }
}

pub enum Command {
    CreateLanPairing {origin:CommandOrigin,request_id:u64,cancel:Arc<AtomicBool>},
    RevokeLanPairing {origin:CommandOrigin,id:String},
    RestoreHandoverHost,
    Handover {
        origin: CommandOrigin,
        action: crate::handover_client::Action,
        request_id: u64,
    },
    StopHandover {
        origin: CommandOrigin,
        account_change: bool,
    },
    AccountLoginBegin {
        authority: Option<handover::RoutePin>,
        url: String,
        expected_server_id: Option<String>,
        invite: zeroize::Zeroizing<String>,
        register: bool,
        initial_protection: Option<ProtectionLevel>,
        request_id: u64,
    },
    AccountLoginFinish {
        authority: Option<handover::RoutePin>,
        url: String,
        expected_server_id: String,
        challenge_id: String,
        proof: zeroize::Zeroizing<String>,
        cancel: Arc<AtomicBool>,
        request_id: u64,
    },
    HistoryBackup {
        origin: Option<CommandOrigin>,
        path: PathBuf,
        password: zeroize::Zeroizing<String>,
        export: bool,
        request_id: u64,
    },
    PrepareWhisper {
        origin: CommandOrigin,
        channel_id: String,
        peer: String,
    },
    CryptoRefresh,
    CryptoResetDevice {
        confirmation: String,
        url: String,
        user_id: String,
        expected_device_id: String,
    },
    CryptoRevokeOwnDevice {
        device_id: String,
        url: String,
        user_id: String,
    },
    CryptoVerify {
        origin: CommandOrigin,
        user_id: String,
        device_id: String,
        fingerprint: String,
    },
    CryptoUnverify {
        origin: CommandOrigin,
        user_id: String,
        device_id: String,
    },
    FetchUserProfile {
        user_id: String,
        request_id: u64,
    },
    SaveProfile {
        origin: CommandOrigin,
        display_name: String,
        bio: String,
        appearance: Option<ProfileAppearance>,
        avatar: ProfileAvatarChange,
        request_id: u64,
    },
    FetchAvatar {
        user: User,
    },
    FetchAsset {
        asset: MediaAsset,
    },
    Auth {
        authority: Option<handover::RoutePin>,
        url: String,
        expected_server_id: Option<String>,
        request: AuthRequest,
        register: bool,
        /// A failure is reported as `RequestFailed` with this id, so the form
        /// that sent it can place a field error without guessing.
        request_id: u64,
    },
    CheckServer {
        locator_invite: Option<String>,
        cancel: Arc<AtomicBool>,
        authority: Option<handover::RoutePin>,
        url: String,
        request_id: u64,
    },
    Logout,
    #[cfg(test)]
    Select(Target),
    SelectConversation(Target),
    ClearSelection,
    MentionInbox {
        origin: CommandOrigin,
        remaining: Option<Vec<(Target, Option<u64>)>>,
        request_id: u64,
    },
    CommunitySearch {
        origin: CommandOrigin,
        query: String,
        filters: crate::search_filters::SearchFilters,
        remaining: Option<Vec<(Target, Option<u64>)>>,
        request_id: u64,
    },
    #[cfg(test)]
    Send {
        target: Target,
        content: String,
        attachment_ids: Vec<String>,
    },
    EditMessage {
        origin: CommandOrigin,
        message_id: String,
        content: String,
    },
    #[cfg(test)]
    Upload {
        target: Target,
        path: PathBuf,
    },
    StartDownload {
        origin: CommandOrigin,
        attachment: Attachment,
        path: PathBuf,
        operation_id: String,
    },
    FetchChatMedia {
        original: bool,
        origin: CommandOrigin,
        attachment: Attachment,
    },
    CreateChannel {
        origin: CommandOrigin,
        request: CreateChannel,
    },
    Invite {
        origin: CommandOrigin,
        request_id: Option<u64>,
    },
    /// Sent once after joining through a link that carries device verification:
    /// proves, with the link's one-time secret, that this device came with that
    /// invitation, and pins the host's device named in the link. Best effort.
    AttestInvitation {
        origin: CommandOrigin,
        proof: crate::invite_proof::InviteProof,
    },
    /// The user's decision about sending without Relay's end-to-end layer in this
    /// community: `Some(revision)` accepts exactly that revision, `None` keeps encryption.
    OpenConsent {
        origin: CommandOrigin,
        revision: Option<u64>,
    },
    /// Administrator only: changes the community's protection mode.
    SetProtection {
        origin: CommandOrigin,
        level: ProtectionLevel,
        expected_revision: u64,
    },
    React {
        origin: CommandOrigin,
        target: Target,
        message_id: String,
        target_client_id: Option<String>,
        emoji: String,
        active: bool,
    },
    DeleteMessage(String),
    JoinVoiceRequest {
        channel_id: String,
        request_id: u64,
    },
    LeaveVoice,
    LeaveAndAnswer(call_switch::Request),
    CancelSwitchedCall {
        origin: CommandOrigin,
        instance: String,
        session: u64,
        channel: String,
        device: String,
        open_revision: Option<u64>,
    },
    VoiceState(VoiceState),
    StopCapture(String),
    RestoreLocal {
        server: PathBuf,
        data: PathBuf,
        expected: String,
    },
    HostLocal {
        server: PathBuf,
        data: PathBuf,
    },
    CreateLocal { server: PathBuf, request: String },
    OpenLocal { server: PathBuf, server_id: String },
    HostTailscale {
        server: PathBuf,
        data: PathBuf,
        address: String,
    },
    HostEmbedded {
        server: PathBuf,
        data: PathBuf,
        address: String,
    },
    /// Lets friends in by key, with no account for anyone.
    HostIroh {
        server: PathBuf,
        data: PathBuf,
    },
    RotateIrohToken {
        origin: CommandOrigin,
    },
    StopLocal,
    History {
        target: Target,
        before: Option<u64>,
        request_id: u64,
    },
    HistoryAfter {
        origin: CommandOrigin,
        target: Target,
        after: u64,
        request_id: u64,
    },
    AroundMessage {
        target: Target,
        message_id: String,
        request_id: u64,
    },
    Search {
        target: Target,
        query: String,
        before: Option<u64>,
        request_id: u64,
    },
    MarkRead {
        origin: CommandOrigin,
        target: Target,
        last_read_sequence: u64,
    },
    MarkCommunityRead {
        origin: CommandOrigin,
    },
    /// Narrow activity write on this exact saved Friends session; it never
    /// switches the selected community or borrows its credentials.
    SetGameActivity {
        origin: CommandOrigin,
        share: Option<relay_protocol::game_activity::Share>,
        request_id: u64,
    },
    /// Decline only: a background call must never switch the active connection
    /// or borrow its credentials. Accepting still uses the ordinary call UI.
    DeclinePeopleCall {
        origin: CommandOrigin,
        call_id: String,
        request_id: u64,
    },
    FetchPins {
        channel_id: String,
        request_id: u64,
    },
    SetPin {
        channel_id: String,
        message_id: String,
        pinned: bool,
        request_id: u64,
    },
    Typing {
        target: Target,
        typing: bool,
    },
    SendRich {
        origin: CommandOrigin,
        binding: Option<SendBinding>,
        target: Target,
        content: String,
        attachment_ids: Vec<String>,
        client_id: String,
        reply_to_id: Option<String>,
        mentions: Vec<String>,
    },
    StartUpload {
        origin: CommandOrigin,
        target: Target,
        path: PathBuf,
        operation_id: String,
    },
    CancelUpload {
        operation_id: String,
    },
    SwitchCommunity {
        url: String,
    },
    ForgetCommunity {
        url: String,
    },
    RememberSession {
        enabled: bool,
    },
    /// Whether the next login is remembered. Unlike RememberSession it never
    /// forgets the current server's remembered login.
    RememberNextLogins {
        enabled: bool,
    },
    /// Keep this server's current login on this PC: a host paused for a move
    /// issues no new sessions, so the people moving it must be able to resume.
    KeepSignedInForMove {
        origin: CommandOrigin,
    },
    Resume {
        url: String,
        expected_server_id: Option<String>,
    },
    DiscordHost {
        origin: CommandOrigin,
        action: DiscordHostAction,
        request_id: u64,
    },
    Admin {
        origin: CommandOrigin,
        method: AdminMethod,
        path: String,
        body: serde_json::Value,
        request_id: u64,
    },
    /// The member's own data-copy token: minted into the OS vault or revoked.
    /// It has its own command so the token never travels as an admin result.
    ReplicaToken {
        origin: CommandOrigin,
        action: ReplicaTokenAction,
        replica_id: String,
        request_id: u64,
    },
    SaveBackup {
        path: PathBuf,
        operation_id: String,
    },
    #[cfg_attr(not(feature = "native-rtc"), allow(dead_code))]
    RtcToken {
        channel_id: String,
        request_id: u64,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ProfileAvatarChange {
    #[default]
    Keep,
    Remove,
    Upload(PathBuf),
    Prepared(Vec<u8>),
}

pub enum DiscordHostAction {
    Status,
    Save(zeroize::Zeroizing<String>),
    Remove,
    Retry,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplicaTokenAction {
    Mint,
    Revoke,
}

/// What the app learns about the copy access. Never the token itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplicaTokenOutcome {
    Minted {
        expires: u64,
    },
    /// `remote` is whether the community confirmed the revocation. The access
    /// on this PC is gone either way.
    Revoked {
        remote: bool,
    },
}

#[derive(Clone, Copy, Debug)]
pub enum AdminMethod {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectionStatus {
    Connected,
    Reconnecting,
    Offline,
    SignInRequired,
}

pub fn operation_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// A protected send that the encryption code chose to hold back, rather than
/// a failure. Set only at the places that decide to wait; the composer
/// explains it next to the kept message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendWait {
    /// This device is not yet approved into the conversation's group.
    DeviceApproval,
    /// The key manager has not finished adding this account's devices.
    AddingDevices,
    /// A device left; the key manager must replace the keys.
    KeyRotation,
    /// Key updates are queued for this conversation.
    KeyUpdates,
}

pub enum Update {
    LanPairCreated {origin:CommandOrigin,request_id:u64,result:Result<crate::lan_pairing::Ready,String>},
    HandoverHostReady {
        remembered: bool,
    },
    Handover(crate::handover_client::View),
    AccountLoginChallenge {
        url: String,
        data: serde_json::Value,
        request_id: u64,
    },
    AccountLoginComplete {
        url: String,
        session: Session,
        bootstrap: Box<Bootstrap>,
        request_id: u64,
    },
    HistoryBackup {
        origin: Option<CommandOrigin>,
        request_id: u64,
        result: Result<crate::recovery::BackupResult, String>,
    },
    WhisperKeys {
        origin: CommandOrigin,
        channel_id: String,
        peer: String,
        keys: Result<Arc<crate::e2ee::CallKeys>, String>,
    },
    WhisperInvalidated {
        peer: String,
    },
    ReactionSnapshot {
        url: String,
        target: Target,
        message_id: String,
        target_client_id: Option<String>,
        rows: Vec<Reaction>,
    },
    MessageProtections {
        origin: CommandOrigin,
        protections: Vec<(String, crate::e2ee::MessageProtection)>,
    },
    CryptoStatus {
        status: crate::e2ee::CryptoUiStatus,
    },
    VoiceEncryptionChanged {
        url: String,
        channel_id: String,
        ticket: VoiceTicket,
        keys: Arc<crate::e2ee::CallKeys>,
        gateway: Option<crate::gateway_control::Config>,
    },
    VoiceEncryptionPaused {
        channel_id: String,
        error: String,
        /// Only true for the worker's bounded reconnect operation.
        reconnecting: bool,
    },
    ProfileLoaded {
        url: String,
        user: User,
        request_id: u64,
    },
    ProfileSaved {
        url: String,
        user: User,
        request_id: u64,
    },
    ProfileSaveFailed {
        url: String,
        user: Option<User>,
        request_id: u64,
        error: String,
    },
    CommunityMoved {
        origin: CommandOrigin,
        url: String,
        session: Session,
        bootstrap: Box<Bootstrap>,
    },
    Connected {
        url: String,
        session: Session,
        bootstrap: Box<Bootstrap>,
    },
    Disconnected,
    Bootstrap(Bootstrap),
    ReadMarked {
        origin: CommandOrigin,
        target: Target,
        requested_sequence: u64,
        result: Result<ReadState, String>,
    },
    CommunitySearch {
        request_id: u64,
        query: String,
        messages: Vec<(Target, Message)>,
        remaining: Vec<(Target, Option<u64>)>,
        errors: Vec<String>,
    },
    MentionInbox {
        request_id: u64,
        messages: Vec<(Target, Message)>,
        remaining: Vec<(Target, Option<u64>)>,
        errors: Vec<String>,
    },
    Messages {
        target: Target,
        messages: Vec<Message>,
    },
    #[cfg(test)]
    Uploaded {
        target: Target,
        attachment: Attachment,
    },
    #[cfg(test)]
    Sent {
        target: Target,
        content: String,
        attachment_ids: Vec<String>,
    },
    SendFailed {
        target: Target,
        origin: Option<CommandOrigin>,
        error: String,
    },
    /// The send was held back by `SendWait`, for the connection it ran on.
    SendWaiting {
        target: Target,
        origin: CommandOrigin,
        wait: SendWait,
    },
    SendRateLimited {
        target: Target,
        /// The connection the rate-limited send ran on, when known.
        origin: Option<CommandOrigin>,
        error: String,
        code: String,
        retry_after_ms: u64,
    },
    Edited(String),
    EditFailed {
        message_id: String,
        error: String,
    },
    EditRateLimited {
        message_id: String,
        error: String,
        code: String,
        retry_after_ms: u64,
    },
    ChatMediaFetched {
        attachment_id: String,
        media: chat_media::DecodedMedia,
    },
    ChatMediaFailed {
        attachment_id: String,
        error: String,
    },
    Invite {
        origin: CommandOrigin,
        code: String,
        request_id: Option<u64>,
        /// Present when this device can verify the friend who uses the link.
        proof: Option<crate::invite_proof::InviteProof>,
    },
    InviteFailed {
        origin: CommandOrigin,
        request_id: u64,
        error: String,
    },
    ServerChecked {
        resolved: Option<String>,
        url: String,
        request_id: u64,
        result: Result<crate::connection_setup::ServerSummary, String>,
    },
    VoiceJoined {
        url: String,
        ticket: VoiceTicket,
        channel_id: String,
        request_id: u64,
        keys: Arc<crate::e2ee::CallKeys>,
        gateway: Option<crate::gateway_control::Config>,
    },
    VoiceJoinFailed {
        channel_id: String,
        error: String,
        request_id: u64,
    },
    VoiceLeft,
    CallSwitchResult {
        request: call_switch::Request,
        result: Result<Box<call_switch::Ready>, call_switch::Failure>,
    },
    // UI-local continuation after the existing Connected handler restores account UI.
    CallSwitchStart(Box<call_switch::Ready>),
    VoiceCallFailed {
        origin: CommandOrigin,
        channel_id: String,
        request_id: u64,
        error: String,
    },
    #[cfg(test)]
    Frames(Vec<(MediaFrame, Vec<u8>)>),
    DecodedFrames {
        generation: u64,
        channel_id: String,
        frames: Vec<(MediaFrame, Arc<eframe::egui::ColorImage>)>,
    },
    Notice(String),
    Error(String),
    LocalHostFailed(String),
    LocalOpened(crate::community::SavedCommunity),
    Busy(bool),
    LocalReady {
        created: bool,
        setup_required: Option<bool>,
    },
    LocalRestored {
        iroh_only: bool,
        address: Option<String>,
    },
    TailscaleReady {
        address: String,
    },
    /// Friends can now join by key. `only` is true when no private network is used.
    IrohReady {
        route: crate::iroh_route::SavedIroh,
        only: bool,
    },
    IrohInvitationsStopped {
        origin: CommandOrigin,
    },
    LocalStopped,
    History {
        target: Target,
        page: HistoryPage,
        request_id: u64,
    },
    UnreadHistory {
        origin: CommandOrigin,
        target: Target,
        page: HistoryPage,
        next_after: Option<u64>,
        request_id: u64,
    },
    SearchResults {
        target: Target,
        query: String,
        page: HistoryPage,
        request_id: u64,
    },
    PinsLoaded {
        url: String,
        channel_id: String,
        pins: Vec<PinnedMessage>,
        request_id: u64,
    },
    PinsChanged {
        url: String,
        channel_id: String,
    },
    Typing {
        url: String,
        target: Target,
        state: TypingState,
    },
    RequestFailed {
        request_id: u64,
        error: String,
    },
    SentRich {
        origin: CommandOrigin,
        target: Target,
        client_id: String,
    },
    UploadProgress {
        target: Target,
        operation_id: String,
        path: PathBuf,
        transferred: u64,
        total: u64,
    },
    UploadFinished {
        target: Target,
        operation_id: String,
        attachment: Attachment,
    },
    UploadFailed {
        target: Target,
        operation_id: String,
        error: String,
        cancelled: bool,
    },
    Communities(Vec<crate::community::CommunityInfo>),
    CommunityBootstrap {
        url: String,
        bootstrap: Bootstrap,
    },
    GameActivityResult {
        origin: CommandOrigin,
        request_id: u64,
        result: Result<Option<relay_protocol::game_activity::GameActivity>, String>,
    },
    PeopleCallDeclined {
        origin: CommandOrigin,
        call_id: String,
        request_id: u64,
        result: Result<(), String>,
    },
    Changes {
        origin: CommandOrigin,
        changes: Vec<MessageChange>,
    },
    ConnectionState {
        url: String,
        status: ConnectionStatus,
    },
    AdminResult {
        request_id: u64,
        data: serde_json::Value,
    },
    ReplicaToken {
        origin: CommandOrigin,
        request_id: u64,
        replica_id: String,
        outcome: ReplicaTokenOutcome,
    },
    BackupSaved {
        operation_id: String,
        path: PathBuf,
    },
    BackupFailed {
        operation_id: String,
        error: String,
        cancelled: bool,
    },
    DownloadProgress {
        operation_id: String,
        path: PathBuf,
        transferred: u64,
        total: u64,
    },
    DownloadFinished {
        operation_id: String,
        path: PathBuf,
    },
    DownloadFailed {
        operation_id: String,
        error: String,
        cancelled: bool,
    },
    RtcToken {
        ticket: relay_protocol::RtcTicket,
        request_id: u64,
        keys: Arc<crate::e2ee::CallKeys>,
    },
}

pub struct Network {
    call_alerts: Option<crate::call_alerts::Alerts>,
    tx: idle::Sender<Command>,
    pub rx: mpsc::Receiver<Update>,
    shutdown: Arc<AtomicBool>,
    wake: idle::Wake,
    capture_epochs: [Arc<AtomicU64>; 2],
    live_epoch: Arc<AtomicU64>,
    live_pending: Arc<AtomicBool>,
    work: Arc<shutdown::Work>,
    worker: Option<std::thread::JoinHandle<()>>,
    completed: mpsc::Receiver<shutdown::Report>,
    shutdown_started: Option<Instant>,
    shutdown_result: Option<Result<shutdown::Report, String>>,
    pending_updates: std::collections::VecDeque<Update>,
}

fn same_account_provider(a: Option<&AccountProvider>, b: Option<&AccountProvider>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => a.issuer == b.issuer && a.client_id == b.client_id,
        _ => false,
    }
}

struct MovingSession {
    origin: CommandOrigin,
    pin: relay_protocol::handover::RoutePin,
    provider: Option<AccountProvider>,
    aliases: Vec<String>,
    remembered: bool,
    resuming: bool,
}
struct PendingHostResume {
    origin: CommandOrigin,
    remembered: crate::credential_vault::RememberedSession,
    job: crate::host_route::Job,
}

struct Connection {
    url: String,
    session: Session,
    current: Option<Target>,
    voice_channel: Option<String>,
    /// UI correlation only, retained after pending admission completes.
    voice_attempt: Option<u64>,
    voice_admission: Option<voice_recovery::Admission>,
    voice_recovery: Option<voice_recovery::Recovery>,
    frame_ids: HashMap<String, String>,
    sharing: bool,
    camera: bool,
    server_id: String,
    name: String,
    change_cursor: u64,
    protocol_version: u32,
    remembered: bool,
    chat: extended::ChatSession,
    crypto: Option<crate::e2ee::CryptoSession>,
    crypto_registered: bool,
    crypto_call_epoch: Option<u64>,
    open_call_context: Option<Arc<crate::e2ee::CallKeys>>,
    crypto_pending_join: Option<(String, u64)>,
}

impl Connection {
    fn voice_device_id(&self) -> Option<String> {
        if self.chat.plain_allowed() {
            use sha2::{Digest, Sha256};
            Some(format!(
                "open-{:.32}",
                format!("{:x}", Sha256::digest(self.session.token.as_bytes()))
            ))
        } else {
            self.crypto
                .as_ref()
                .map(|crypto| crypto.device.identity().device_id.clone())
        }
    }
}

impl Network {
    #[cfg(test)]
    pub fn new(ctx: eframe::egui::Context) -> Self {
        Self::with_call_alerts(ctx, None)
    }
    pub(crate) fn with_call_alerts(
        ctx: eframe::egui::Context,
        call_alerts: Option<crate::call_alerts::Alerts>,
    ) -> Self {
        let (tx, commands) = mpsc::sync_channel(64);
        let (updates, rx) = mpsc::sync_channel(8);
        let (_frames, frame_rx) = mpsc::sync_channel::<CaptureFrame>(2);
        let refresh_pending = Arc::new(AtomicBool::new(false));
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_shutdown = shutdown.clone();
        let wake = idle::Wake::default();
        let thread_wake = wake.clone();
        let tx = idle::Sender::new(tx, wake.clone());
        let capture_epochs = [Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0))];
        let worker_epochs = capture_epochs.clone();
        let live_epoch = Arc::new(AtomicU64::new(0));
        let live_pending = Arc::new(AtomicBool::new(false));
        let worker_live_epoch = live_epoch.clone();
        let worker_live_pending = live_pending.clone();
        let work = Arc::new(shutdown::Work::default());
        let worker_work = work.clone();
        let thread_call_alerts = call_alerts.clone();
        let (completed_tx, completed) = mpsc::sync_channel(1);
        let worker_thread = std::thread::spawn(move || {
            let mut worker =
                match Worker::with_wake(updates, ctx, refresh_pending, thread_wake.clone()) {
                    Ok(w) => w,
                    Err(_) => {
                        let _ = completed_tx.send(shutdown::Report {
                            failed_requests: 1,
                            ..Default::default()
                        });
                        return;
                    }
                };
            worker.work = worker_work.clone();
            worker.call_alerts = thread_call_alerts;
            worker.archive_busy = worker_work.archive.clone();
            worker.transfer_active = worker_work.transfers.clone();
            worker.background_active = worker_work.background.clone();
            worker.server_check_active = worker_work.server_check.clone();
            worker.capture_epochs = worker_epochs;
            worker.live.capture_epochs = worker.capture_epochs.clone();
            worker.live.epoch = worker_live_epoch;
            worker.live.pending = worker_live_pending;
            worker.live.shutdown = thread_shutdown.clone();
            worker.live_frames = Some(Arc::new(std::sync::Mutex::new(frame_rx)));
            let mut refreshed = Instant::now();
            while !thread_shutdown.load(Ordering::Relaxed) {
                let observed = thread_wake.generation();
                if worker_work.hold.load(Ordering::Acquire)
                    && worker_work.commands.load(Ordering::Acquire) == 0
                {
                    worker_work.control.store(false, Ordering::Release);
                    thread_wake.wait(observed, None);
                    continue;
                }
                worker_work.control.store(true, Ordering::Release);
                let had_command = match commands.try_recv() {
                    Ok(command) => {
                        worker.command(command);
                        worker_work.commands.fetch_sub(1, Ordering::AcqRel);
                        true
                    }
                    Err(mpsc::TryRecvError::Disconnected) => break,
                    Err(mpsc::TryRecvError::Empty) => false,
                };
                worker.poll_host_resume();
                worker.poll_handover();
                worker.poll_host_locator();
                worker.poll_chat_media_results();
                worker.poll_transfers();
                worker.poll_legacy_reactions();
                worker.poll_chat_events();
                if worker.connection.is_some()
                    && (refreshed.elapsed() >= Duration::from_secs(5)
                        || (worker.refresh_pending.load(Ordering::Relaxed)
                            && refreshed.elapsed() >= Duration::from_millis(400)))
                {
                    worker.refresh_pending.store(false, Ordering::Relaxed);
                    worker.refresh();
                    refreshed = Instant::now();
                }
                worker.refresh_background();
                worker_work.control.store(false, Ordering::Release);
                if !had_command && !thread_shutdown.load(Ordering::Relaxed) {
                    thread_wake.wait(observed, worker.idle_delay(refreshed));
                }
            }
            let report = worker.finish_normal(Instant::now() + shutdown::HTTP_BUDGET);
            let completed_ctx = worker.ctx.clone();
            drop(worker);
            let _ = completed_tx.send(report);
            completed_ctx.request_repaint();
        });
        Self {
            call_alerts,
            tx,
            rx,
            shutdown,
            wake,
            capture_epochs,
            live_epoch,
            live_pending,
            work,
            worker: Some(worker_thread),
            completed,
            shutdown_started: None,
            shutdown_result: None,
            pending_updates: Default::default(),
        }
    }

    pub fn send(&self, command: Command) -> Result<(), String> {
        if self.shutdown.load(Ordering::Acquire) || self.work.hold.load(Ordering::Acquire) {
            return Err(
                "Relay is preparing to close its connection. Wait or cancel the installation."
                    .into(),
            );
        }
        // Reserve before enqueueing: the update gate sees a command even before
        // the worker starts it or publishes UI Busy.
        self.work.commands.fetch_add(1, Ordering::AcqRel);
        if self.tx.try_send(command).is_err() {
            self.work.commands.fetch_sub(1, Ordering::AcqRel);
            return Err("Connection is busy. Please try again.".into());
        }
        Ok(())
    }

    pub fn next_capture_generation(&self, camera: bool) -> u64 {
        self.capture_epochs[usize::from(camera)].fetch_add(1, Ordering::SeqCst) + 1
    }

    pub fn live_media_generation(&self) -> u64 {
        self.live_epoch.load(Ordering::SeqCst)
    }
    pub fn invalidate_live_media(&self) {
        self.live_epoch.fetch_add(1, Ordering::SeqCst);
    }
    pub fn acknowledge_live_media(&self) {
        self.live_pending.store(false, Ordering::SeqCst);
    }
}

impl Drop for Network {
    fn drop(&mut self) {
        self.finish_drop();
    }
}

struct Worker {
    call_switch_preflight: bool,
    call_alerts: Option<crate::call_alerts::Alerts>,
    http: Client,
    work: Arc<shutdown::Work>,
    server_check_active: Arc<AtomicBool>,
    archive_busy: Arc<AtomicBool>,
    updates: mpsc::SyncSender<Update>,
    ctx: eframe::egui::Context,
    refresh_pending: Arc<AtomicBool>,
    /// Set by `send_waiting` during one command; read only for its own error.
    send_wait: Option<(SendWait, String)>,
    wake: idle::Wake,
    snapshots: std::sync::Mutex<snapshots::Snapshots>,
    message_repair_at: Instant,
    socket_stop: Arc<AtomicBool>,
    connection: Option<Connection>,
    pending_host_resume: Option<PendingHostResume>,
    handover_job: Option<(crate::handover_client::Job, u64)>,
    handover_runtime: Option<crate::handover_runtime::Runtime>,
    local_server: Option<std::process::Child>,
    owned_local: Option<local_lifecycle::OwnedLocal>,
    restoring_local: bool,
    local_sharing_instance: Option<(String, String)>,
    local_sharing_retry_at: Instant,
    host_locator_service: Option<String>,
    host_locator_retry_at: Instant,
    host_locator_more: Arc<AtomicBool>,
    host_locator_problem: Arc<AtomicBool>,
    local_sharing_failure_notified: bool,
    capture_epochs: [Arc<AtomicU64>; 2],
    chat_media_workers: Option<ChatMediaWorkers>,
    chat_media_epoch: Arc<AtomicU64>,
    live: LiveTransport,
    live_frames: Option<CaptureReceiver>,
    saved_connections: HashMap<String, SavedConnection>,
    transfer_epoch: Arc<AtomicU64>,
    transfer_http: Option<Client>,
    transfer_results: mpsc::Receiver<TransferResult>,
    transfer_completed: idle::Sender<TransferResult>,
    transfers: HashMap<String, Arc<AtomicBool>>,
    transfer_active: Arc<AtomicUsize>,
    transfer_commit: Arc<std::sync::Mutex<()>>,
    background_at: Instant,
    background_jobs: HashMap<String, String>,
    background_retry: HashMap<String, (u8, Instant)>,
    background_active: Arc<AtomicUsize>,
    background_results: mpsc::Receiver<BackgroundResult>,
    background_completed: idle::Sender<BackgroundResult>,
    background_pending: std::collections::VecDeque<background_crypto::Pending>,
    status: Option<ConnectionStatus>,
    remember_session: bool,
    legacy_reactions: legacy_reactions::LegacyReactions,
}

struct SavedConnection {
    connection: Connection,
    stop: Arc<AtomicBool>,
    dirty: Arc<AtomicBool>,
}

struct BackgroundResult {
    url: String,
    token: String,
    bootstrap: Result<Bootstrap, String>,
    changes: Option<Result<ChangePage, String>>,
    keys: Result<Vec<background_crypto::CatchUp>, String>,
    protections: Vec<(String, crate::e2ee::MessageProtection)>,
}

struct TransferResult {
    epoch: u64,
    operation_id: String,
    target: Option<Target>,
    path: PathBuf,
    result: TransferOutcome,
}

enum TransferOutcome {
    Profile {
        url: String,
        request_id: u64,
        user: Option<User>,
        error: Option<String>,
    },
    Progress {
        transferred: u64,
        total: u64,
    },
    DownloadProgress {
        transferred: u64,
        total: u64,
    },
    Downloaded(Result<(), String>),
    Uploaded(Result<(Attachment, Option<crate::e2ee::EncryptedAttachment>), String>),
    Backup(Result<(), String>),
}

type CaptureReceiver = Arc<std::sync::Mutex<mpsc::Receiver<CaptureFrame>>>;

#[derive(Clone)]
struct LiveConnection {
    url: String,
    token: String,
    channel_id: String,
    generation: u64,
    sharing: bool,
    camera: bool,
}

#[derive(Clone)]
struct LiveTransport {
    context: Arc<std::sync::Mutex<Option<LiveConnection>>>,
    gate: Arc<std::sync::Mutex<()>>,
    epoch: Arc<AtomicU64>,
    pending: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    capture_epochs: [Arc<AtomicU64>; 2],
    http: Arc<OnceLock<Client>>,
    updates: mpsc::SyncSender<Update>,
    ctx: eframe::egui::Context,
    poll_cursor: Arc<AtomicU64>,
    activity: Arc<Condvar>,
    #[cfg(test)]
    fail_next_start: Arc<AtomicBool>,
}

impl LiveTransport {
    fn initialize_http(&self) -> Result<(), reqwest::Error> {
        if self.http.get().is_none() {
            let http = Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(2))
                .connect_timeout(Duration::from_secs(1))
                .redirect(reqwest::redirect::Policy::none())
                .build()?;
            let _ = self.http.set(http);
        }
        Ok(())
    }

    fn wait_for_activity_until(&self, stopped: &AtomicBool) -> bool {
        let Ok(context) = self.context.lock() else {
            return false;
        };
        let Ok(_context) = self.activity.wait_while(context, |context| {
            !self.shutdown.load(Ordering::Acquire)
                && !stopped.load(Ordering::Acquire)
                && context
                    .as_ref()
                    .is_none_or(|c| c.generation != self.epoch.load(Ordering::SeqCst))
        }) else {
            return false;
        };
        !self.shutdown.load(Ordering::Acquire) && !stopped.load(Ordering::Acquire)
    }

    fn snapshot(&self) -> Option<LiveConnection> {
        self.context
            .lock()
            .ok()?
            .clone()
            .filter(|c| c.generation == self.epoch.load(Ordering::SeqCst))
    }

    fn start(&self, frames: CaptureReceiver) -> Result<(), std::io::Error> {
        #[cfg(test)]
        if self.fail_next_start.swap(false, Ordering::SeqCst) {
            return Err(std::io::Error::other(
                "fixture rejected native worker startup",
            ));
        }
        let stopped = Arc::new(AtomicBool::new(false));
        let publisher_stop = stopped.clone();
        let publisher = self.clone();
        let publisher_task = std::thread::Builder::new()
            .name("relay-frame-publisher".into())
            .spawn(move || {
                while publisher.wait_for_activity_until(&publisher_stop) {
                    let Ok(receiver) = frames.lock() else { break };
                    let first = match receiver.recv_timeout(Duration::from_millis(20)) {
                        Ok(frame) => frame,
                        Err(mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    };
                    let mut newest = HashMap::from([(first.kind.clone(), first)]);
                    while let Ok(frame) = receiver.try_recv() {
                        newest.insert(frame.kind.clone(), frame);
                    }
                    drop(receiver);
                    if publisher_stop.load(Ordering::Acquire) {
                        break;
                    }
                    for frame in newest.into_values() {
                        publisher.publish_until(frame, Some(&publisher_stop));
                    }
                }
            })?;
        let viewer = self.clone();
        let viewer_stop = stopped.clone();
        let viewer_task = std::thread::Builder::new()
            .name("relay-frame-viewer".into())
            .spawn(move || {
                let mut ids = HashMap::new();
                let mut generation = u64::MAX;
                while viewer.wait_for_activity_until(&viewer_stop) {
                    let started = Instant::now();
                    if let Some(context) = viewer.snapshot() {
                        if generation != context.generation {
                            ids.clear();
                            generation = context.generation;
                        }
                        if !viewer.pending.load(Ordering::SeqCst) {
                            viewer.poll(&context, &mut ids);
                        }
                    }
                    std::thread::sleep(Duration::from_millis(33).saturating_sub(started.elapsed()));
                }
            });
        if let Err(error) = viewer_task {
            stopped.store(true, Ordering::Release);
            let _context = self.context.lock().unwrap_or_else(|e| e.into_inner());
            self.activity.notify_all();
            drop(publisher_task);
            return Err(error);
        }
        Ok(())
    }

    #[cfg(test)]
    fn publish(&self, frame: CaptureFrame) {
        self.publish_until(frame, None);
    }

    fn publish_until(&self, frame: CaptureFrame, stopped: Option<&AtomicBool>) {
        // The control worker waits on this gate before DELETE/leave/logout/join,
        // so an already in-flight old POST finishes before the revocation request.
        let Ok(_gate) = self.gate.lock() else { return };
        if stopped.is_some_and(|stop| stop.load(Ordering::Acquire)) {
            return;
        }
        let Some(context) = self.snapshot() else {
            return;
        };
        let index = match frame.kind.as_str() {
            "screen" => 0,
            "camera" => 1,
            _ => return,
        };
        if frame.generation == 0
            || frame.generation != self.capture_epochs[index].load(Ordering::SeqCst)
            || (index == 0 && !context.sharing)
            || (index == 1 && !context.camera)
            || frame.jpeg.len() > MAX_FRAME_BYTES
        {
            return;
        }
        let Some(http) = self.http.get() else { return };
        let result = http
            .post(crate::embedded_network::endpoint(&format!(
                "{}/api/media/frame",
                context.url
            )))
            .bearer_auth(&context.token)
            .query(&[
                ("kind", frame.kind),
                ("width", frame.width.to_string()),
                ("height", frame.height.to_string()),
            ])
            .header("Content-Type", "image/jpeg")
            .body(frame.jpeg)
            .send()
            .map_err(network_error)
            .and_then(checked);
        if let Err(error) = result {
            if self.epoch.load(Ordering::SeqCst) == context.generation {
                let _ = self.updates.try_send(Update::Error(error));
                self.ctx.request_repaint();
            }
        }
    }

    fn poll(&self, context: &LiveConnection, ids: &mut HashMap<String, String>) {
        let Some(http) = self.http.get() else { return };
        let result = http
            .get(crate::embedded_network::endpoint(&format!(
                "{}/api/media/frames",
                context.url
            )))
            .bearer_auth(&context.token)
            .query(&[("channel_id", &context.channel_id)])
            .send()
            .map_err(network_error)
            .and_then(parse::<Vec<MediaFrame>>);
        let Ok(mut list) = result else { return };
        let active: std::collections::HashSet<_> = list
            .iter()
            .map(|f| format!("{}:{}", f.user.id, f.kind))
            .collect();
        ids.retain(|key, _| active.contains(key));
        if !list.is_empty() {
            let offset = self.poll_cursor.fetch_add(1, Ordering::Relaxed) as usize % list.len();
            list.rotate_left(offset);
        }
        let mut frames = Vec::new();
        let mut decoded_bytes = 0usize;
        let mut fetched_ids = Vec::new();
        for mut frame in list.into_iter().take(16) {
            if self.epoch.load(Ordering::SeqCst) != context.generation {
                return;
            }
            if !frame
                .user
                .id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                || frame.user.id.is_empty()
                || !matches!(frame.kind.as_str(), "screen" | "camera")
            {
                continue;
            }
            let key = format!("{}:{}", frame.user.id, frame.kind);
            if ids.get(&key).is_some_and(|id| id == &frame.frame_id) {
                continue;
            }
            let estimated_bytes = u64::from(frame.width)
                .saturating_mul(u64::from(frame.height))
                .saturating_mul(4);
            if estimated_bytes > (16 * 1024 * 1024 - decoded_bytes) as u64 {
                continue;
            }
            // A stable authenticated route survives replacement between metadata
            // listing and JPEG fetch. The response identifies the actual JPEG.
            let response = http
                .get(crate::embedded_network::endpoint(&format!(
                    "{}/api/media/live/{}/{}",
                    context.url, frame.user.id, frame.kind
                )))
                .bearer_auth(&context.token)
                .send()
                .map_err(network_error)
                .and_then(checked);
            let Ok(mut response) = response else { continue };
            if response
                .content_length()
                .is_some_and(|size| size > MAX_FRAME_BYTES as u64)
            {
                continue;
            }
            if let Some(id) = response
                .headers()
                .get("x-relay-frame-id")
                .and_then(|v| v.to_str().ok())
                .filter(|s| s.len() <= 128)
            {
                frame.frame_id = id.to_owned();
            }
            let mut bytes = Vec::new();
            if std::io::Read::read_to_end(
                &mut std::io::Read::take(&mut response, (MAX_FRAME_BYTES + 1) as u64),
                &mut bytes,
            )
            .is_err()
                || bytes.len() > MAX_FRAME_BYTES
                || !bytes.starts_with(&[0xff, 0xd8])
            {
                continue;
            }
            let Ok(decoded) = chat_media::decode(&bytes) else {
                continue;
            };
            if decoded.frames.len() != 1
                || decoded_bytes.saturating_add(decoded.decoded_bytes) > 16 * 1024 * 1024
            {
                continue;
            }
            decoded_bytes += decoded.decoded_bytes;
            let image = decoded
                .frames
                .into_iter()
                .next()
                .expect("decoded images contain a frame")
                .image;
            frame.width = image.size[0] as u32;
            frame.height = image.size[1] as u32;
            fetched_ids.push((key, frame.frame_id.clone()));
            frames.push((frame, image));
        }
        if frames.is_empty() || self.epoch.load(Ordering::SeqCst) != context.generation {
            return;
        }
        if self
            .pending
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }
        let update = Update::DecodedFrames {
            generation: context.generation,
            channel_id: context.channel_id.clone(),
            frames,
        };
        if self.updates.try_send(update).is_ok() {
            ids.extend(fetched_ids);
            self.ctx.request_repaint();
        } else {
            self.pending.store(false, Ordering::SeqCst);
        }
    }
}

struct ChatMediaJob {
    _activity: Option<shutdown::Lease>,
    original: bool,
    avatar: bool,
    attachment: Attachment,
    url: String,
    token: String,
    epoch: u64,
    encrypted: Option<crate::e2ee::EncryptedAttachment>,
    /// The community runs without the extra layer, so a file without a key is
    /// served as it is.
    plain: bool,
}

struct ChatMediaResult {
    attachment_id: String,
    epoch: u64,
    result: Result<chat_media::DecodedMedia, String>,
}

struct ChatMediaWorkers {
    jobs: mpsc::SyncSender<ChatMediaJob>,
    results: mpsc::Receiver<ChatMediaResult>,
}

fn start_chat_media_workers(
    epoch: Arc<AtomicU64>,
    wake: idle::Wake,
) -> Result<ChatMediaWorkers, reqwest::Error> {
    let http = Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(8))
        .connect_timeout(Duration::from_secs(4))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let (jobs, receiver) = mpsc::sync_channel::<ChatMediaJob>(4);
    let (results, completed) = mpsc::sync_channel(1);
    let results = idle::Sender::new(results, wake);
    let receiver = Arc::new(std::sync::Mutex::new(receiver));
    for _ in 0..2 {
        let receiver = receiver.clone();
        let epoch = epoch.clone();
        let results = results.clone();
        let http = http.clone();
        std::thread::spawn(move || loop {
            let job = match receiver.lock() {
                Ok(receiver) => receiver.recv(),
                Err(_) => return,
            };
            let Ok(job) = job else { return };
            if epoch.load(Ordering::SeqCst) != job.epoch {
                continue;
            }
            let result = fetch_chat_media(&http, &job);
            if epoch.load(Ordering::SeqCst) != job.epoch {
                continue;
            }
            if results
                .send(ChatMediaResult {
                    attachment_id: job.attachment.id,
                    epoch: job.epoch,
                    result,
                })
                .is_err()
            {
                return;
            }
        });
    }
    Ok(ChatMediaWorkers {
        jobs,
        results: completed,
    })
}

fn validate_chat_attachment(attachment: &Attachment) -> Result<(), String> {
    // Canonical server upload routes only: never fetch an external URL, a different
    // authenticated API, a path traversal, or an HTTP redirect with the bearer token.
    if attachment.id.is_empty()
        || !attachment
            .id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-')
        || attachment.url != format!("/api/uploads/{}", attachment.id)
    {
        return Err("The image attachment address is invalid.".into());
    }
    if attachment.size > chat_media::MAX_COMPRESSED_BYTES as u64 {
        return Err("The image exceeds the 10 MB attachment limit.".into());
    }
    Ok(())
}

pub fn avatar_attachment_id(user: &User) -> String {
    format!("avatar:{}:{}", user.id, user.profile.version)
}

fn validate_avatar_attachment(attachment: &Attachment) -> Result<(), String> {
    if attachment.id.starts_with("asset:") {
        return validate_asset_attachment(attachment);
    }
    let mut parts = attachment.id.split(':');
    let prefix = parts.next();
    let user_id = parts.next().unwrap_or_default();
    let version = parts.next().and_then(|value| value.parse::<u64>().ok());
    if prefix != Some("avatar")
        || user_id.is_empty()
        || !user_id
            .bytes()
            .all(|value| value.is_ascii_alphanumeric() || value == b'-')
        || parts.next().is_some()
    {
        return Err("The avatar address is invalid.".into());
    }
    let Some(version) = version else {
        return Err("The avatar version is invalid.".into());
    };
    if attachment.url != format!("/api/profiles/{user_id}/avatar?v={version}")
        || attachment.mime != "image/png"
    {
        return Err("The avatar address is invalid.".into());
    }
    Ok(())
}
pub(super) fn asset_attachment_id(asset: &MediaAsset) -> String {
    format!(
        "asset:{}:{}:{}:{}",
        asset.kind, asset.name, asset.version, asset.id
    )
}
fn validate_asset_attachment(attachment: &Attachment) -> Result<(), String> {
    let parts: Vec<_> = attachment.id.split(':').collect();
    if parts.len() != 5
        || parts[0] != "asset"
        || !matches!(
            parts[1],
            "server_icon" | "server_banner" | "profile_banner" | "emoji" | "sticker"
        )
        || parts[2].is_empty()
        || parts[2].len() > 128
        || !parts[2]
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
        || parts[3].parse::<u64>().is_err()
        || parts[4].is_empty()
        || !parts[4]
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        || attachment.url != format!("/api/assets/{}/{}?v={}", parts[1], parts[2], parts[3])
        || attachment.mime != "image/png"
    {
        return Err("The community image address is invalid.".into());
    }
    Ok(())
}

fn fetch_chat_media(http: &Client, job: &ChatMediaJob) -> Result<chat_media::DecodedMedia, String> {
    if job.avatar {
        validate_avatar_attachment(&job.attachment)?;
    } else {
        validate_chat_attachment(&job.attachment)?;
        if job.encrypted.is_none() && !job.plain {
            return Err(
                "This attachment has no verified encryption key. Plaintext fallback is disabled."
                    .into(),
            );
        }
    }
    let mut response = checked(
        http.get(crate::embedded_network::endpoint(&format!(
            "{}{}",
            job.url, job.attachment.url
        )))
        .bearer_auth(&job.token)
        .send()
        .map_err(network_error)?,
    )?;
    if response
        .content_length()
        .is_some_and(|length| length > chat_media::MAX_COMPRESSED_BYTES as u64)
    {
        return Err("The image exceeds the 10 MB attachment limit.".into());
    }
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(
        &mut std::io::Read::take(
            &mut response,
            if job.avatar {
                2 * 1024 * 1024 + 1
            } else {
                (chat_media::MAX_COMPRESSED_BYTES + 1) as u64
            },
        ),
        &mut bytes,
    )
    .map_err(|_| "The image download failed.".to_string())?;
    if job.avatar {
        if bytes.len() > 2 * 1024 * 1024
            || !matches!(image::guess_format(&bytes), Ok(image::ImageFormat::Png))
        {
            return Err("The avatar response is not a bounded PNG.".into());
        }
        let (width, height) =
            image::io::Reader::with_format(std::io::Cursor::new(&bytes), image::ImageFormat::Png)
                .into_dimensions()
                .map_err(|_| "The avatar PNG is invalid.")?;
        let (max_width, max_height) = if job.attachment.id.starts_with("asset:") {
            (1200, 480)
        } else {
            (256, 256)
        };
        if width > max_width || height > max_height {
            return Err("The avatar exceeds 256 pixels.".into());
        }
    }
    if job.avatar {
        chat_media::decode(&bytes)
    } else {
        let decrypted;
        let plain: &[u8] = match job.encrypted.as_ref() {
            Some(metadata) => {
                if metadata.id != job.attachment.id || metadata.file.size != job.attachment.size {
                    return Err(
                        "The attachment metadata does not match its verified envelope.".into(),
                    );
                }
                decrypted =
                    relay_crypto::files::decrypt(&bytes, &metadata.file, &metadata.file.binding)?;
                &decrypted[..]
            }
            // Checked above: a file without a key is served only in an open community.
            None => {
                if bytes.len() as u64 != job.attachment.size {
                    return Err("The attachment size changed. Refresh and try again.".into());
                }
                if relay_crypto::files::looks_encrypted(&bytes) {
                    return Err("This file is encrypted, but its key is not on this device.".into());
                }
                &bytes[..]
            }
        };
        if job.original {
            chat_media::decode(plain)
        } else {
            chat_media::inline_preview(plain)
        }
    }
}

/// Tells this PC's own server another address it is reached at (the
/// account-free iroh address). Loopback only, with the server's own local
/// secret from its runtime record; no proxy or DNS is ever involved.
fn register_account_destination(
    runtime: &serde_json::Value,
    identity: &crate::local_host::Identity,
    url: &str,
) -> Result<(), String> {
    let control = runtime["shutdown_token"]
        .as_str()
        .filter(|t| (32..=128).contains(&t.len()))
        .ok_or("The local server proof is unavailable.")?;
    let mut control =
        reqwest::header::HeaderValue::from_str(control).map_err(|_| "Invalid local proof.")?;
    control.set_sensitive(true);
    let http = Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(1))
        .timeout(Duration::from_secs(4))
        .build()
        .map_err(|_| "Cannot reach the local server.")?;
    let response = http
        .post("http://127.0.0.1:4580/api/local/account-destination")
        .header("x-relay-local-control", control)
        .header("x-relay-instance-id", identity.instance_id.as_str())
        .header("x-relay-server-id", identity.server_id.as_str())
        .json(&serde_json::json!({ "url": url }))
        .send()
        .map_err(|_| "Cannot reach the local server.")?;
    if response.status().is_success() {
        Ok(())
    } else {
        Err("The local server did not accept its address.".into())
    }
}

impl Worker {
    #[cfg(test)]
    fn new(
        updates: mpsc::SyncSender<Update>,
        ctx: eframe::egui::Context,
        refresh_pending: Arc<AtomicBool>,
    ) -> Result<Self, reqwest::Error> {
        Self::with_wake(updates, ctx, refresh_pending, idle::Wake::default())
    }
    fn with_wake(
        updates: mpsc::SyncSender<Update>,
        ctx: eframe::egui::Context,
        refresh_pending: Arc<AtomicBool>,
        wake: idle::Wake,
    ) -> Result<Self, reqwest::Error> {
        let (transfer_completed, transfer_results) = mpsc::sync_channel(16);
        let (background_completed, background_results) = mpsc::sync_channel(4);
        let transfer_completed = idle::Sender::new(transfer_completed, wake.clone());
        let background_completed = idle::Sender::new(background_completed, wake.clone());
        let chat_media_epoch = Arc::new(AtomicU64::new(0));
        let capture_epochs = [Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0))];
        let live = LiveTransport {
            context: Arc::new(std::sync::Mutex::new(None)),
            gate: Arc::new(std::sync::Mutex::new(())),
            epoch: Arc::new(AtomicU64::new(0)),
            pending: Arc::new(AtomicBool::new(false)),
            shutdown: Arc::new(AtomicBool::new(false)),
            capture_epochs: capture_epochs.clone(),
            http: Arc::new(OnceLock::new()),
            updates: updates.clone(),
            ctx: ctx.clone(),
            poll_cursor: Arc::new(AtomicU64::new(0)),
            activity: Arc::new(Condvar::new()),
            #[cfg(test)]
            fail_next_start: Arc::new(AtomicBool::new(false)),
        };
        let work = Arc::new(shutdown::Work::default());
        Ok(Self {
            http: Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(8))
                .connect_timeout(Duration::from_secs(4))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            updates,
            send_wait: None,
            work: work.clone(),
            server_check_active: work.server_check.clone(),
            archive_busy: work.archive.clone(),
            ctx,
            refresh_pending,
            wake,
            snapshots: std::sync::Mutex::new(snapshots::Snapshots::default()),
            message_repair_at: Instant::now(),
            socket_stop: Arc::new(AtomicBool::new(false)),
            connection: None,
            pending_host_resume: None,
            handover_job: None,
            handover_runtime: None,
            local_server: None,
            owned_local: None,
            restoring_local: false,
            local_sharing_instance: None,
            local_sharing_retry_at: Instant::now(),
            host_locator_service: crate::host_locator::directory(),
            host_locator_retry_at: Instant::now(),
            host_locator_more: Arc::new(AtomicBool::new(false)),
            host_locator_problem: Arc::new(AtomicBool::new(false)),
            local_sharing_failure_notified: false,
            capture_epochs,
            chat_media_workers: None,
            chat_media_epoch,
            live,
            live_frames: None,
            saved_connections: HashMap::new(),
            call_alerts: None,
            transfer_epoch: Arc::new(AtomicU64::new(0)),
            transfer_http: None,
            transfer_results,
            transfer_completed,
            transfers: HashMap::new(),
            transfer_active: work.transfers.clone(),
            transfer_commit: Arc::new(std::sync::Mutex::new(())),
            background_at: Instant::now(),
            background_jobs: HashMap::new(),
            background_retry: HashMap::new(),
            background_active: work.background.clone(),
            background_results,
            background_completed,
            background_pending: std::collections::VecDeque::new(),
            status: None,
            remember_session: false,
            legacy_reactions: legacy_reactions::LegacyReactions::default(),
            call_switch_preflight: false,
        })
    }

    fn emit(&self, update: Update) {
        // A temporary exact destination context may prepare keys, but it must
        // never publish unscoped encryption/history UI into the source account.
        if self.call_switch_preflight {
            return;
        }
        if self.restoring_local
            && matches!(
                &update,
                Update::LocalReady { .. }
                    | Update::IrohReady { .. }
                    | Update::TailscaleReady { .. }
            )
        {
            return;
        }
        if !self
            .snapshots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .should_emit(&update)
        {
            return;
        }
        let _ = self.updates.send(update);
        self.ctx.request_repaint();
    }

    fn request(&self, method: reqwest::Method, path: &str) -> Result<RequestBuilder, String> {
        if self.live.shutdown.load(Ordering::Acquire) {
            return Err("The connection is closing.".into());
        }
        let c = self
            .connection
            .as_ref()
            .ok_or("Connect to a server first.")?;
        Ok(self
            .http
            .request(
                method,
                crate::embedded_network::endpoint(&format!("{}{}", c.url, path)),
            )
            .bearer_auth(&c.session.token))
    }

    fn voice_gateway(&self, ticket: &VoiceTicket) -> Option<crate::gateway_control::Config> {
        let c = self.connection.as_ref()?;
        let supported = if c.chat.supports("people-v1") {
            c.chat.supports("friends-gateway-broker-v1")
        } else {
            c.chat.supports("gateway-broker-v1")
        };
        let private_route = crate::iroh_route::saved_for(&c.url);
        if !supported
            || crate::community::account_destination(&c.url).is_err()
            || (crate::embedded_network::routed(&c.url) && private_route.is_none())
        { return None; }
        let config = crate::gateway_control::Config::new(ticket.session_id, |method, path| self.request(method, path))?;
        Some(match private_route {
            Some(saved) => config.with_private_route(saved, c.server_id.clone()),
            None => config,
        })
    }

    fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, String> {
        parse(
            self.request(reqwest::Method::GET, path)?
                .send()
                .map_err(network_error)?,
        )
    }
    fn post<T: DeserializeOwned, B: Serialize>(&self, path: &str, body: &B) -> Result<T, String> {
        parse(
            self.request(reqwest::Method::POST, path)?
                .json(body)
                .send()
                .map_err(network_error)?,
        )
    }

    fn patch<T: DeserializeOwned, B: Serialize>(&self, path: &str, body: &B) -> Result<T, String> {
        parse(
            self.request(reqwest::Method::PATCH, path)?
                .json(body)
                .send()
                .map_err(network_error)?,
        )
    }

    fn command(&mut self, command: Command) {
        if let Command::MarkRead { origin, target, last_read_sequence } = command {
            let result = self.mark_read(&origin, &target, last_read_sequence);
            // Automatic read receipts must not flicker the global busy state or
            // turn every retry into an unrelated dismiss-only error banner.
            self.emit(Update::ReadMarked {
                origin, target, requested_sequence: last_read_sequence, result,
            });
            return;
        }
        if matches!(&command, Command::RestoreHandoverHost) {
            if let Err(error) = self.restore_handover_host() {
                self.emit(Update::Error(error));
            }
            return;
        }
        if matches!(&command, Command::StopLocal) && self.handover_runtime.is_some() {
            self.stop_handover_host();
            return;
        }
        if matches!(&command, Command::HostIroh { .. }) && self.handover_runtime.is_some() {
            if let Err(error)=self.share_handover_host() { self.emit(Update::Error(error)); }
            return;
        }
        if matches!(
            &command,
            Command::HostLocal { .. }
                | Command::CreateLocal { .. }
                | Command::OpenLocal { .. }
                | Command::HostIroh { .. }
                | Command::HostTailscale { .. }
                | Command::HostEmbedded { .. }
                | Command::RestoreLocal { .. }
        ) && (self.handover_runtime.is_some()
            || self
                .handover_job
                .as_ref()
                .is_some_and(|(j, _)| j.is_hosting()))
        {
            self.emit(Update::Error("This PC is preparing or running the transferred community. Stop that host before starting another local server.".into()));
            return;
        }
        if let Command::Handover {
            origin,
            action,
            request_id,
        } = command
        {
            self.start_handover(origin, action, request_id);
            return;
        }
        if let Command::StopHandover {
            origin,
            account_change,
        } = command
            {
            if self.handover_job.as_ref().is_some_and(|(j, _)| {
                j.view().origin == origin && (!account_change || !j.is_restore())
            }) {
                if let Some((job, _)) = &self.handover_job {
                    job.cancel();
                }
            }
            return;
        }
        if let Command::LeaveAndAnswer(request) = command {
            let result = self.leave_and_answer(&request).map(Box::new);
            self.emit(Update::CallSwitchResult { request, result });
            return;
        }
        if let Command::CancelSwitchedCall {
            origin,
            instance,
            session,
            channel,
            device,
            open_revision,
        } = command
        {
            let current = self.connection.as_ref().is_some_and(|c| {
                CommandOrigin::from(c) == origin
                    && c.voice_channel.as_deref() == Some(channel.as_str())
                    && c.voice_device_id().as_deref() == Some(device.as_str())
                    && c.chat.open_revision() == open_revision
                    && c.voice_admission
                        .as_ref()
                        .is_some_and(|a| a.session == session && a.instance == instance)
            });
            let _ = self.release_switched_call(
                &origin, &instance, session, &channel, &device, open_revision,
            );
            if current {
                self.clear_voice_intent();
            }
            return;
        }
        if let Command::SetGameActivity {
            origin,
            share,
            request_id,
        } = command
        {
            let result = self.set_game_activity(&origin, share.as_ref());
            self.emit(Update::GameActivityResult {
                origin,
                request_id,
                result,
            });
            return;
        }
        if let Command::DeclinePeopleCall {
            origin,
            call_id,
            request_id,
        } = command
        {
            let result = self.decline_people_call(&origin, &call_id);
            // This result belongs to its call notice, not the foreground form.
            // In particular, do not publish Busy/Error for another community.
            self.emit(Update::PeopleCallDeclined {
                origin,
                call_id,
                request_id,
                result,
            });
            return;
        }
        if let Command::HistoryBackup {
            origin,
            path,
            password,
            export,
            request_id,
        } = command
        {
            self.history_backup(origin, path, password, export, request_id);
            return;
        }
        if let Command::FetchAsset { asset } = &command {
            if let Err(error) = self.queue_asset(asset.clone()) {
                self.emit(Update::ChatMediaFailed {
                    attachment_id: asset_attachment_id(asset),
                    error,
                });
            }
            return;
        }
        if let Command::FetchAvatar { user } = command {
            let id = avatar_attachment_id(&user);
            if let Err(error) = self.queue_avatar(user) {
                self.emit(Update::ChatMediaFailed {
                    attachment_id: id,
                    error,
                });
            }
            return;
        }
        if let Command::FetchChatMedia {
            origin,
            attachment,
            original,
        } = command
        {
            if self
                .connection
                .as_ref()
                .is_none_or(|connection| origin != CommandOrigin::from(connection))
            {
                // The replacement UI may have the same attachment ID. A stale
                // failure must not poison that account's preview cache either.
                return;
            }
            let attachment_id = attachment.id.clone();
            if let Err(error) = if original {
                self.queue_chat_media_original(&origin, attachment)
            } else {
                self.queue_chat_media(&origin, attachment)
            } {
                self.emit(Update::ChatMediaFailed {
                    attachment_id,
                    error,
                });
            }
            return;
        }
        // Typing is ephemeral: it must not replace visible request errors or
        // make the composer look busy for every heartbeat.
        // A saved consent is handed over silently for the same reason: it must not
        // make the interface look busy. Failing to hand it over only keeps sending
        // encrypted messages, which is the safe side.
        if matches!(
            &command,
            Command::Typing { .. } | Command::OpenConsent { .. }
        ) {
            let _ = self.handle(command);
            return;
        }
        self.emit(Update::Busy(true));
        let send_target = match &command {
            #[cfg(test)]
            Command::Send { target, .. } => Some(target.clone()),
            Command::SendRich { target, .. } => Some(target.clone()),
            _ => None,
        };
        let send_origin = match &command {
            Command::SendRich { origin, .. } => Some(origin.clone()),
            #[cfg(test)]
            Command::Send { .. } => self.connection.as_ref().map(CommandOrigin::from),
            _ => None,
        };
        let edit_message_id = match &command {
            Command::EditMessage { message_id, .. } => Some(message_id.clone()),
            _ => None,
        };
        let join_channel_id = match &command {
            Command::JoinVoiceRequest {
                channel_id,
                request_id,
            } => Some((channel_id.clone(), *request_id)),
            _ => None,
        };
        let request_id = match &command {
            Command::History { request_id, .. }
            | Command::HistoryAfter { request_id, .. }
            | Command::AroundMessage { request_id, .. }
            | Command::Search { request_id, .. }
            | Command::FetchPins { request_id, .. }
            | Command::SetPin { request_id, .. }
            | Command::FetchUserProfile { request_id, .. }
            | Command::SaveProfile { request_id, .. }
            | Command::Admin { request_id, .. }
            | Command::ReplicaToken { request_id, .. }
            | Command::DiscordHost { request_id, .. }
            | Command::MentionInbox { request_id, .. }
            | Command::CommunitySearch { request_id, .. }
            | Command::RtcToken { request_id, .. }
            | Command::Auth { request_id, .. }
            | Command::AccountLoginBegin { request_id, .. }
            | Command::AccountLoginFinish { request_id, .. } => Some(*request_id),
            _ => None,
        };
        let local_hosting = matches!(&command,
            Command::HostLocal { .. } | Command::CreateLocal { .. } | Command::OpenLocal { .. } | Command::HostIroh { .. }
                | Command::HostEmbedded { .. } | Command::HostTailscale { .. }
                | Command::RestoreLocal { .. });
        self.send_wait = None;
        let result = self.handle(command);
        if let Err(error) = result {
            if let Some(target) = send_target {
                let wait = self.send_wait.take();
                self.emit(send_failure(target, error, wait, send_origin));
            } else if let Some(message_id) = edit_message_id {
                self.emit(Update::EditFailed { message_id, error });
            } else if let Some((channel_id, request_id)) = join_channel_id {
                self.emit(Update::VoiceJoinFailed {
                    channel_id,
                    request_id,
                    error,
                });
            } else if let Some(request_id) = request_id {
                self.emit(Update::RequestFailed { request_id, error });
            } else if local_hosting {
                self.emit(Update::LocalHostFailed(error));
            } else {
                self.emit(Update::Error(error));
            }
        }
        self.emit(Update::Busy(false));
    }

    fn post_empty<B: Serialize>(&self, path: &str, body: &B) -> Result<(), String> {
        checked(
            self.request(reqwest::Method::POST, path)?
                .json(body)
                .send()
                .map_err(network_error)?,
        )?;
        Ok(())
    }

    fn adopt_session(
        &mut self,
        url: String,
        expected_server_id: Option<String>,
        session: Session,
    ) -> Result<(), String> {
        self.adopt_session_checked(url, expected_server_id, session, None, None, None)
    }

    fn adopt_session_checked(
        &mut self,
        url: String,
        expected_server_id: Option<String>,
        session: Session,
        account_attempt: Option<(Arc<AtomicBool>, u64)>,
        moved: Option<MovingSession>,
        login_authority: Option<handover::RoutePin>,
    ) -> Result<(), String> {
        let call_alerts::BootstrapReply {
            bootstrap,
            cursor: alert_cursor,
        } = parse::<call_alerts::BootstrapReply>(
            self.http
                .get(crate::embedded_network::endpoint(&format!(
                    "{url}/api/bootstrap"
                )))
                .bearer_auth(&session.token)
                .send()
                .map_err(network_error)?,
        )?;
        let call_received = Instant::now();
        if account_attempt
            .as_ref()
            .is_some_and(|(cancel, _)| cancel.load(Ordering::Acquire))
        {
            let _ = self
                .http
                .post(crate::embedded_network::endpoint(&format!(
                    "{url}/api/logout"
                )))
                .bearer_auth(&session.token)
                .send();
            return Ok(());
        }
        if expected_server_id
            .as_ref()
            .is_some_and(|expected| expected != &bootstrap.server_id)
        {
            // Do not replace the active account or initialize its encryption.
            // Best-effort revoke the new session if the identity changed mid-login.
            if moved.is_none() {
                let _ = self
                    .http
                    .post(crate::embedded_network::endpoint(&format!(
                        "{url}/api/logout"
                    )))
                    .bearer_auth(&session.token)
                    .send();
            }
            return Err(
                "The community changed during sign-in. Ask the host for a new invitation.".into(),
            );
        }
        if session.token.is_empty()
            || session.user.id.is_empty()
            || bootstrap.user.id != session.user.id
        {
            return Err("The community returned a different account during sign-in. Confirm its address and try again.".into());
        }
        let previous_pin = moved.as_ref().map(|m| &m.pin).or(login_authority.as_ref()).or_else(|| {
            self.connection
                .as_ref()
                .filter(|c| c.url == url)
                .and_then(|c| c.chat.authority.as_ref())
                .or_else(|| {
                    self.saved_connections
                        .get(&url)
                        .and_then(|c| c.connection.chat.authority.as_ref())
                })
        });
        let authority = crate::host_route::bootstrap_pin(
            &url,
            &bootstrap.server_id,
            bootstrap.host_authority.as_ref(),
            previous_pin,
        )?;
        if let Some(moving) = &moved {
            let origin = &moving.origin;
            validate_bootstrap_identity(&origin.server_id, &origin.user_id, &bootstrap)?;
            if !moving.resuming {
                self.command_connection(origin)?;
            }
            if !same_account_provider(
                moving.provider.as_ref(),
                bootstrap.account_provider.as_ref(),
            ) {
                return Err(crate::host_route::REFUSED.into());
            }
        }
        let remember = moved
            .as_ref()
            .map_or(self.remember_session, |m| m.remembered);
        crate::embedded_network::bind_identity(&url, &bootstrap.server_id)?;
        if moved.as_ref().is_some_and(|m| m.origin.url == url)
            && self.connection.as_ref().is_some_and(|c| c.url == url)
        {
            // A verified chain may return to an old address under a new term.
            // Dropping the old transport must not revoke the transferred token.
            self.cancel_transfers();
            self.socket_stop.store(true, Ordering::SeqCst);
            self.connection = None;
        }
        self.park_connection(&url);
        self.chat_media_epoch.fetch_add(1, Ordering::SeqCst);
        self.chat_media_workers = None;
        self.legacy_reactions.clear();
        self.socket_stop = Arc::new(AtomicBool::new(false));
        self.refresh_pending = Arc::new(AtomicBool::new(false));
        self.connection = Some(Connection {
            url: url.clone(),
            session: session.clone(),
            current: None,
            voice_channel: None,
            voice_attempt: None,
            voice_admission: None,
            voice_recovery: None,
            frame_ids: HashMap::new(),
            sharing: false,
            camera: false,
            server_id: bootstrap.server_id.clone(),
            name: bootstrap.server_name.clone(),
            change_cursor: bootstrap.latest_change_sequence,
            protocol_version: bootstrap.protocol_version,
            remembered: remember,
            chat: extended::ChatSession::new(&bootstrap),
            crypto: None,
            crypto_registered: false,
            crypto_call_epoch: None,
            open_call_context: None,
            crypto_pending_join: None,
        });
        self.connection.as_mut().unwrap().chat.authority = authority;
        if let Some(m) = &moved {
            self.connection.as_mut().unwrap().chat.authority_aliases = m.aliases.clone();
        }
        self.live.epoch.fetch_add(1, Ordering::SeqCst);
        self.sync_live_transport()?;
        if let Some(m) = &moved {
            if let Some(old) = self.saved_connections.remove(&m.origin.url) {
                old.stop.store(true, Ordering::SeqCst);
                // This token moved with the database. Never log it out on either
                // host merely because the old parked transport is being dropped.
            }
        }
        spawn_events_with_alerts(
            url.clone(),
            session.token.clone(),
            self.refresh_pending.clone(),
            self.socket_stop.clone(),
            self.connection.as_ref().unwrap().chat.events.clone(),
            self.wake.clone(),
            self.alert_watch(&bootstrap, call_received, alert_cursor),
        );
        // Persist a successful remembered destination before publishing the
        // URL change. A crash after the UI/preferences switch must not point
        // at an address whose corresponding vault record was never written.
        let mut remember_error = None;
        if remember {
            if let Some(c) = &mut self.connection {
                let authority =
                    c.chat
                        .authority
                        .as_ref()
                        .map(|pin| crate::host_route::RememberedAuthority {
                            pin: pin.clone(),
                            account_provider: c.chat.account_provider.clone(),
                            previous_urls: c.chat.authority_aliases.clone(),
                        });
                if let Err(error) = crate::credential_vault::save_bound(
                    &c.url,
                    &c.server_id,
                    &c.session,
                    authority.as_ref(),
                ) {
                    c.remembered = false;
                    remember_error = Some(format!(
                        "You are signed in, but this login could not be remembered: {error}"
                    ));
                }
            }
        }
        self.emit(if let Some(m) = moved {
            Update::CommunityMoved {
                origin: m.origin,
                url: url.clone(),
                session: session.clone(),
                bootstrap: Box::new(bootstrap),
            }
        } else if let Some((_, request_id)) = account_attempt {
            Update::AccountLoginComplete {
                url: url.clone(),
                session: session.clone(),
                bootstrap: Box::new(bootstrap),
                request_id,
            }
        } else {
            Update::Connected {
                url: url.clone(),
                session: session.clone(),
                bootstrap: Box::new(bootstrap),
            }
        });
        if let Some(error) = remember_error {
            self.emit(Update::Error(error));
        }
        self.publish_communities();
        Ok(())
    }

    fn handle(&mut self, command: Command) -> Result<(), String> {
        if matches!(
            &command,
            Command::Resume { .. }
                | Command::SwitchCommunity { .. }
                | Command::Auth { .. }
                | Command::AccountLoginBegin { .. }
                | Command::AccountLoginFinish { .. }
                | Command::Logout
                | Command::ForgetCommunity { .. }
        ) {
            self.pending_host_resume = None;
            if let Some((job, _)) = &self.handover_job {
                if !job.is_restore() {
                job.cancel();
                }
            }
        }
        if let Command::RestoreLocal {
            server,
            data,
            expected,
        } = command
        {
            return self.restore_app_host(server, data, expected);
        }
        if let Command::OpenLocal { server, server_id } = command {
            let data = crate::local_host::community_directory(&server_id)?;
            let previous = crate::local_host::data_directory().ok()
                .and_then(|data| crate::local_host::AppHost::load(&data)).map(|plan| plan.server_id);
            if self.local_server_ready()
                && !self.local_identity().is_some_and(|live| live.server_id == server_id)
            {
                if self.owned_local.is_none() {
                    return Err("Another local host is running. Close it normally before opening this server.".into());
                }
                self.close_app_host()?;
            }
            self.detach_local_connections()?;
            let saved = crate::local_host::select_community(&server_id)?;
            if let Err(error) = self.restore_app_host(server, data, server_id.clone()) {
                if let Some(previous) = previous { let _ = crate::local_host::select_community(&previous); }
                return Err(error);
            }
            if !self.local_identity().is_some_and(|live| live.server_id == server_id) {
                return Err("A different local server answered. Your account was not connected.".into());
            }
            self.emit(Update::LocalOpened(saved));
            return Ok(());
        }
        match &command {
            Command::Auth {
                url,
                expected_server_id,
                ..
            }
            | Command::Resume {
                url,
                expected_server_id,
            } => self.ensure_app_host(url, expected_server_id.as_deref())?,
            Command::AccountLoginBegin {
                url,
                expected_server_id,
                ..
            } => self.ensure_app_host(url, expected_server_id.as_deref())?,
            Command::AccountLoginFinish {
                url,
                expected_server_id,
                ..
            } => self.ensure_app_host(url, Some(expected_server_id))?,
            Command::SwitchCommunity { url } => {
                let expected = self
                    .saved_connections
                    .get(url)
                    .map(|s| s.connection.server_id.clone());
                self.ensure_app_host(url, expected.as_deref())?;
            }
            _ => {}
        }
        let hosting_target = match &command {
            Command::HostLocal { server, data }
            | Command::HostIroh { server, data }
            | Command::HostEmbedded { server, data, .. }
            | Command::HostTailscale { server, data, .. } => Some((server.clone(), data.clone())),
            _ => None,
        };
        if let Command::AttestInvitation { origin, proof } = &command {
            self.command_connection(origin)?;
            // Best effort: a failure only means the friend verifies the host by hand.
            let _ = self.attest_invitation(proof);
            return Ok(());
        }
        if let Command::OpenConsent { origin, revision } = &command {
            self.command_connection(origin)?;
            let leave = if let Some(c) = self.connection.as_mut() {
                c.chat.set_open_consent(*revision);
                c.chat.is_open()
                    && !c.chat.plain_allowed()
                    && (c.voice_channel.is_some() || c.crypto_pending_join.is_some())
            } else {
                false
            };
            if leave {
                // Withdrawal is local and immediate, even during an outage.
                // Never retain an admission that could resume this Open call.
                self.clear_voice_intent();
                self.emit(Update::VoiceLeft);
                if let Ok(request) = self.request(reqwest::Method::POST, "/api/voice/leave") {
                    let _ = request
                        .timeout(Duration::from_secs(2))
                        .json(&serde_json::json!({}))
                        .send();
                }
            }
            return Ok(());
        }
        if let Command::SetProtection {
            origin,
            level,
            expected_revision,
        } = &command
        {
            self.command_connection(origin)?;
            let _: CommunityProtection = parse(
                self.request(reqwest::Method::PUT, "/api/protection")?
                    .json(&SetProtection {
                        level: *level,
                        expected_revision: *expected_revision,
                    })
                    .send()
                    .map_err(network_error)?,
            )?;
            // Everyone, including this client, learns the result from the bootstrap.
            self.refresh();
            return Ok(());
        }
        match &command {
            Command::Auth { url, .. }
            | Command::AccountLoginBegin { url, .. }
            | Command::AccountLoginFinish { url, .. }
            | Command::Resume { url, .. }
            | Command::SwitchCommunity { url } => {
                crate::tailscale_connect::ensure_private_connection(url)?;
            }
            _ => {}
        }
        if matches!(
            &command,
            Command::CryptoRefresh
                | Command::CryptoVerify { .. }
                | Command::CryptoUnverify { .. }
                | Command::CryptoResetDevice { .. }
                | Command::CryptoRevokeOwnDevice { .. }
        ) {
            return self.handle_crypto_command(command);
        }
        // Prefer the live in-memory session over reloading the same credential:
        // replacing that connection would revoke the token being resumed.
        let command = match command {
            Command::Resume {
                url,
                expected_server_id,
            } => {
                let url = crate::community::normalize_url(&url)?;
                let known = self
                    .connection
                    .as_ref()
                    .filter(|c| c.url == url)
                    .or_else(|| {
                        self.saved_connections
                            .get(&url)
                            .map(|saved| &saved.connection)
                    });
                if known.is_some_and(|c| {
                    expected_server_id
                        .as_ref()
                        .is_some_and(|id| id != &c.server_id)
                }) {
                    return Err(IDENTITY_CHANGED.into());
                }
                if self.connection.as_ref().is_some_and(|c| c.url == url) {
                    return Ok(());
                }
                if self.saved_connections.contains_key(&url) {
                    Command::SwitchCommunity { url }
                } else {
                    Command::Resume {
                        url,
                        expected_server_id,
                    }
                }
            }
            command => command,
        };
        let barrier = matches!(
            &command,
            Command::Auth { .. }
                | Command::AccountLoginFinish { .. }
                | Command::Resume { .. }
                | Command::Logout
                | Command::JoinVoiceRequest { .. }
                | Command::LeaveVoice
                | Command::VoiceState(_)
                | Command::StopCapture(_)
                | Command::StopLocal
                | Command::HostEmbedded { .. }
                | Command::HostIroh { .. }
        );
        if matches!(
            &command,
            Command::Logout
                | Command::JoinVoiceRequest { .. }
                | Command::LeaveVoice
                | Command::StopLocal
        ) {
            self.live.epoch.fetch_add(1, Ordering::SeqCst);
            if let Ok(mut context) = self.live.context.lock() {
                *context = None;
            }
        }
        let gate = self.live.gate.clone();
        let _gate = if barrier {
            Some(
                gate.lock()
                    .map_err(|_| "The media worker stopped.".to_string())?,
            )
        } else {
            None
        };
        match command {
            Command::Auth {
                authority,
                url,
                expected_server_id,
                request,
                register,
                request_id: _,
            } => {
                let url = crate::community::normalize_url(&url)?;
                if let Some(expected) = expected_server_id.as_deref() {
                    uuid::Uuid::parse_str(expected)
                        .map_err(|_| "This invitation has an invalid community identity.")?;
                }
                let private_service = url::Url::parse(&url).ok().is_some_and(|address| {
                    address.scheme() == "http"
                        && address.port_or_known_default() == Some(4580)
                        && address
                            .host_str()
                            .is_some_and(crate::tailscale_connect::valid_address)
                });
                if expected_server_id.is_some()
                    || private_service
                    || crate::iroh_route::is_iroh(&url)
                {
                    let summary = check_server(&self.http, &url)?;
                    summary.validate(expected_server_id.as_deref())?;
                    crate::embedded_network::bind_identity(&url, &summary.server_id)?;
                }
                let authority = self.confirm_login_authority(&url,expected_server_id.as_deref(),authority.as_ref())?;
                let endpoint = if register {
                    "/api/register"
                } else {
                    "/api/login"
                };
                let session: Session = parse(
                    self.http
                        .post(crate::embedded_network::endpoint(&format!(
                            "{url}{endpoint}"
                        )))
                        .json(&request)
                        .send()
                        .map_err(network_error)?,
                )?;
                self.adopt_session_checked(url, expected_server_id, session, None, None, authority)?;
            }
            Command::Logout => {
                self.cancel_transfers();
                if let Some(c) = &self.connection {
                    let _ = crate::credential_vault::forget(&c.url);
                }
                self.chat_media_epoch.fetch_add(1, Ordering::SeqCst);
                self.chat_media_workers = None;
                self.legacy_reactions.clear();
                if self.connection.is_some() {
                    let _ = self.post_empty("/api/voice/leave", &serde_json::json!({}));
                    let _ = self.post_empty("/api/logout", &serde_json::json!({}));
                }
                self.connection = None;
                self.socket_stop.store(true, Ordering::Relaxed);
                self.emit(Update::Disconnected);
                self.publish_communities();
            }
            #[cfg(test)]
            Command::Select(target) => {
                if let Some(c) = &mut self.connection {
                    c.current = Some(target);
                }
                self.load_messages()?;
            }
            Command::ClearSelection => {
                self.clear_typing_selection();
                if let Some(c) = &mut self.connection {
                    c.current = None;
                }
            }
            #[cfg(test)]
            Command::Send {
                target,
                content,
                attachment_ids,
            } => {
                let acknowledgement = Update::Sent {
                    target: target.clone(),
                    content: content.clone(),
                    attachment_ids: attachment_ids.clone(),
                };
                let (channel_id, recipient_id) = match target {
                    Target::Channel(id) => (Some(id), None),
                    Target::Direct(id) => (None, Some(id)),
                };
                let _: Message = self.post(
                    "/api/messages",
                    &SendMessage {
                        channel_id,
                        recipient_id,
                        content,
                        attachment_ids,
                        ..Default::default()
                    },
                )?;
                self.emit(acknowledgement);
                // The send already committed. A refresh failure must not look like a failed send.
                if let Err(error) = self.load_messages() {
                    self.emit(Update::Error(error));
                }
            }
            Command::EditMessage {
                origin,
                message_id,
                content,
            } => {
                self.command_connection(&origin)?;
                if !self.edit_message(&message_id, content)? {
                    return Ok(());
                }
                self.emit(Update::Edited(message_id));
                // The edit committed. Refresh failures must not be reported as failed edits.
                if let Err(error) = self.load_messages() {
                    self.emit(Update::Error(error));
                }
            }
            #[cfg(test)]
            Command::Upload { target, path } => {
                let size = std::fs::metadata(&path)
                    .map_err(|_| "The selected file cannot be read.")?
                    .len();
                if size > MAX_UPLOAD_BYTES as u64 {
                    return Err("Files can be up to 10 MB in this version.".into());
                }
                let file =
                    std::fs::File::open(&path).map_err(|_| "The selected file cannot be read.")?;
                let mut reader = std::io::Read::take(file, (MAX_UPLOAD_BYTES + 1) as u64);
                let mut bytes = Vec::new();
                std::io::Read::read_to_end(&mut reader, &mut bytes)
                    .map_err(|_| "The selected file cannot be read.")?;
                if bytes.len() > MAX_UPLOAD_BYTES {
                    return Err("Files can be up to 10 MB in this version.".into());
                }
                let name = path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("attachment");
                let attachment: Attachment = parse(
                    self.request(reqwest::Method::POST, "/api/uploads")?
                        .query(&[("name", name), ("mime", "application/octet-stream")])
                        .header("Content-Type", "application/octet-stream")
                        .body(bytes)
                        .send()
                        .map_err(network_error)?,
                )?;
                self.emit(Update::Uploaded { target, attachment });
            }
            Command::FetchChatMedia {
                origin,
                attachment,
                original,
            } => {
                if original {
                    self.queue_chat_media_original(&origin, attachment)
                } else {
                    self.queue_chat_media(&origin, attachment)
                }?
            }
            Command::CreateChannel { origin, request } => {
                self.command_connection(&origin)?;
                let _: Channel = self.post("/api/channels", &request)?;
                self.refresh();
            }
            Command::CheckServer { url, request_id, authority, locator_invite, cancel } => {
                let url = crate::community::normalize_url(&url)?;
                if self.server_check_active.swap(true, Ordering::SeqCst) {
                    self.emit(Update::ServerChecked {
                        resolved: None,
                        url,
                        request_id,
                        result: Err(
                            "Another connection check is running. Try again shortly.".into()
                        ),
                    });
                    return Ok(());
                }
                let http = self.http.clone();
                let updates = self.updates.clone();
                let ctx = self.ctx.clone();
                let active = self.server_check_active.clone();
                let target = url.clone();
                if std::thread::Builder::new()
                    .name("relay-server-check".into())
                    .spawn(move || {
                        let checked = (|| {
                            if cancel.load(Ordering::Acquire){return Err(crate::host_route::UNAVAILABLE.into());}
                            let resolved=if let Some(pin)=&authority {
                                match crate::host_route::discover(&target,pin,&cancel) {
                                    Ok(value)=>value.map(|v|v.url),
                                    Err(error) if error == crate::host_route::UNAVAILABLE && locator_invite.as_ref().is_some_and(|v|!v.is_empty()) =>
                                        Some(crate::host_locator::discover(pin,locator_invite.as_deref().unwrap(),&cancel)?.url),
                                    Err(error)=>return Err(error),
                                }
                            } else {None};
                            let summary=check_server(&http,resolved.as_deref().unwrap_or(&target))?;
                            summary.validate(authority.as_ref().map(|p|p.server_id.as_str()))?;
                            Ok::<_,String>((summary,resolved))
                        })();
                        let (result,resolved)=match checked {Ok((s,r))=>(Ok(s),r),Err(e)=>(Err(e),None)};
                        let _ = updates.send(Update::ServerChecked {
                            resolved,
                            url: target,
                            request_id,
                            result,
                        });
                        active.store(false, Ordering::SeqCst);
                        ctx.request_repaint();
                    })
                    .is_err()
                {
                    self.server_check_active.store(false, Ordering::SeqCst);
                    self.emit(Update::ServerChecked {
                        resolved: None,
                        url,
                        request_id,
                        result: Err("Unable to start a connection check. Try again.".into()),
                    });
                }
            }
            Command::CreateLanPairing {origin,request_id,cancel} => {
                if let Err(error)=self.create_lan_pairing(origin.clone(),request_id,cancel){
                    self.emit(Update::LanPairCreated{origin,request_id,result:Err(error)});
                }
            }
            Command::RevokeLanPairing {origin,id} => {self.revoke_lan_pairing(&origin,&id);}
            Command::Invite { origin, request_id } => {
                let result =
                    (|| -> Result<(String, Option<crate::invite_proof::InviteProof>), String> {
                        self.command_connection(&origin)?;
                        // A verifiable invitation needs this device's encryption keys. If
                        // they are not ready the invitation is created as before, without.
                        let verifiable =
                            !self.connection.as_ref().is_some_and(|c| c.chat.is_open())
                                && self.ensure_crypto().is_ok();
                        let body = if verifiable {
                            // One use, stated explicitly; the server's default lifetime is 24 hours.
                            serde_json::json!({"max_uses": 1})
                        } else {
                            serde_json::json!({})
                        };
                        let value: serde_json::Value = self.post("/api/invites", &body)?;
                        let code: String = value["code"].as_str().unwrap_or_default().into();
                        let proof = if verifiable {
                            self.issue_invitation_proof(&value)
                        } else {
                            None
                        };
                        self.publish_invitation_locator(&code);
                        Ok((code, proof))
                    })();
                match result {
                    Ok((code, proof)) => self.emit(Update::Invite {
                        origin,
                        code,
                        request_id,
                        proof,
                    }),
                    Err(error) => {
                        if let Some(request_id) = request_id {
                            self.emit(Update::InviteFailed {
                                origin,
                                request_id,
                                error,
                            });
                        } else {
                            return Err(error);
                        }
                    }
                }
            }
            Command::React {
                origin,
                target,
                message_id,
                target_client_id,
                emoji,
                active,
            } => {
                self.command_connection(&origin)?;
                self.send_reaction(target, message_id, target_client_id, emoji, active)?;
            }
            Command::DeleteMessage(id) => {
                checked(
                    self.request(reqwest::Method::DELETE, &format!("/api/messages/{id}"))?
                        .send()
                        .map_err(network_error)?,
                )?;
                self.load_messages()?;
            }
            Command::PrepareWhisper {
                origin,
                channel_id,
                peer,
            } => {
                self.command_connection(&origin)?;
                if self
                    .connection
                    .as_ref()
                    .and_then(|c| c.voice_channel.as_ref())
                    != Some(&channel_id)
                {
                    return Ok(());
                }
                let keys = self.protected_whisper_keys(&peer);
                self.emit(Update::WhisperKeys {
                    origin,
                    channel_id,
                    peer,
                    keys,
                });
            }
            Command::JoinVoiceRequest {
                channel_id,
                request_id,
            } => {
                self.clear_voice_intent();
                if self.connection.as_ref().is_some_and(|c| c.chat.is_open()) {
                    if !self
                        .connection
                        .as_ref()
                        .is_some_and(|c| c.chat.plain_allowed())
                    {
                        return Err(
                            "Accept this server's encryption choice before joining voice.".into(),
                        );
                    }
                } else {
                    self.ensure_crypto()?;
                }
                let mut ticket: VoiceTicket = self.post(
                    "/api/voice/join",
                    &VoiceJoin {
                        open_revision: self
                            .connection
                            .as_ref()
                            .and_then(|c| c.chat.open_revision()),
                        channel_id: channel_id.clone(),
                        device_id: self
                            .connection
                            .as_ref()
                            .and_then(Connection::voice_device_id),
                    },
                )?;
                let c = self.connection.as_mut().ok_or("Not connected.")?;
                c.voice_channel = Some(channel_id.clone());
                c.voice_attempt = Some(request_id);
                c.frame_ids.clear();
                c.sharing = false;
                c.camera = false;
                c.crypto_pending_join = Some((channel_id.clone(), request_id));
                c.crypto_call_epoch = None;
                self.remember_voice_admission(&ticket);
                let c = self.connection.as_mut().unwrap();
                let url = c.url.clone();
                let keys = match self.protected_call_keys(&channel_id) {
                    Ok(keys) => keys,
                    Err(error) => {
                        self.emit(Update::VoiceEncryptionPaused {
                            channel_id,
                            error,
                            reconnecting: false,
                        });
                        self.refresh();
                        return Ok(());
                    }
                };
                if let Err(error) = self.sync_live_transport() {
                    // Roll back a successful remote join if local runtime/thread setup
                    // fails, before sending a failed acknowledgement to the UI.
                    let _ = self.post_empty("/api/voice/leave", &serde_json::json!({}));
                    if let Some(c) = &mut self.connection {
                        c.voice_channel = None;
                        c.voice_attempt = None;
                        c.frame_ids.clear();
                        c.sharing = false;
                        c.camera = false;
                    }
                    let mut context = self.live.context.lock().unwrap_or_else(|e| e.into_inner());
                    *context = None;
                    self.live.activity.notify_all();
                    drop(context);
                    if self.live_frames.is_some() {
                        self.live.http = Arc::new(OnceLock::new());
                    }
                    return Err(error);
                }
                if let Some(c) = &mut self.connection {
                    c.crypto_pending_join = None;
                    c.crypto_call_epoch = Some(keys.epoch);
                }
                ticket.tunnel_url = self
                    .connection
                    .as_ref()
                    .map(|c| format!("{}/api/voice/tunnel", c.url.trim_end_matches('/')));
                self.emit(Update::VoiceJoined {
                    gateway: self.voice_gateway(&ticket),
                    url,
                    ticket,
                    channel_id,
                    request_id,
                    keys,
                });
                self.refresh();
            }
            Command::LeaveVoice => {
                // Leaving is a local decision. A failed remote acknowledgement
                // must never leave an intent that the encryption poller can join
                // again when the server comes back.
                self.clear_voice_intent();
                self.emit(Update::VoiceLeft);
                self.post_empty("/api/voice/leave", &serde_json::json!({}))?;
                self.refresh();
            }
            Command::VoiceState(state) => {
                if let Some(c) = &mut self.connection {
                    c.sharing = state.sharing;
                    c.camera = state.camera;
                }
                self.post_empty("/api/voice/state", &state)?;
                self.sync_live_transport()?;
            }
            Command::StopCapture(kind) => {
                if let Some(c) = &mut self.connection {
                    if kind == "screen" {
                        c.sharing = false;
                    } else {
                        c.camera = false;
                    }
                }
                checked(
                    self.request(reqwest::Method::DELETE, "/api/media/frame")?
                        .query(&[("kind", kind)])
                        .send()
                        .map_err(network_error)?,
                )?;
                self.sync_live_transport()?;
            }
            Command::CreateLocal { server, request } => {
                if self.local_server_ready() {
                    if self.owned_local.is_none() {
                        return Err("Another local host is running. Close it normally before creating a new server.".into());
                    }
                    self.close_app_host()?;
                }
                self.detach_local_connections()?;
                let data = crate::local_host::create_directory(&request)?;
                self.host_local(server.clone(), data.clone())?;
                self.remember_local_host(server, data)?;
            }
            Command::HostLocal { server, data } => self.host_local(server, data)?,
            Command::HostTailscale {
                server,
                data,
                address,
            } => {
                if !crate::tailscale_connect::valid_address(&address) {
                    return Err("Check your own Tailscale connection before hosting.".into());
                }
                if self.local_server_ready() {
                    return Err("Stop the local server in Server Settings before switching to Tailscale hosting. Your community data is kept.".into());
                }
                let package = server
                    .parent()
                    .and_then(|path| path.parent())
                    .ok_or("Cannot locate the Relay package.")?;
                let stack = package.join("scripts/launch-stack.ps1");
                let sfu = server
                    .parent()
                    .ok_or("Cannot locate the video server.")?
                    .join("livekit-server.exe");
                if !stack.is_file() || !sfu.is_file() {
                    return Err("Extract the complete Relay package before hosting.".into());
                }
                self.host_stack(server, data, stack, sfu, Some(address), None)?;
            }
            Command::HostEmbedded {
                server,
                data,
                address,
            } => {
                let status = crate::embedded_network::status()?;
                if status.phase != "ready" || status.address.as_deref() != Some(address.as_str()) {
                    return Err("Connect Relay's built-in private network before hosting.".into());
                }
                if self.local_server_ready() {
                    let identity = self
                        .local_identity()
                        .ok_or("Cannot confirm your local server.")?;
                    if crate::local_host::embedded_address(&identity.server_id).as_deref()
                        == Some(format!("http://{address}:4580").as_str())
                    {
                        self.attach_embedded_host(&data, &address, false)?;
                        // Tailscale is the one way now; the account-free key stops.
                        let _ = crate::iroh_route::unhost(&identity.server_id);
                    } else {
                        self.switch_to_embedded_host(server, data, address)?;
                    }
                } else {
                    let package = server
                        .parent()
                        .and_then(|p| p.parent())
                        .ok_or("Cannot locate the Relay package.")?;
                    let stack = package.join("scripts/launch-stack.ps1");
                    let sfu = server
                        .parent()
                        .ok_or("Cannot locate the video server.")?
                        .join("livekit-server.exe");
                    if !stack.is_file() || !sfu.is_file() {
                        return Err("Extract the complete Relay package before hosting.".into());
                    }
                    self.host_stack(server, data, stack, sfu, None, Some(address))?;
                }
            }
            Command::HostIroh { server, data } => {
                if !crate::iroh_route::available() {
                    return Err(
                        "This copy of Relay does not include the account-free connection.".into(),
                    );
                }
                if self.local_server_ready() {
                    let identity = self
                        .local_identity()
                        .ok_or("Cannot confirm your local server.")?;
                    let existing = crate::local_host::embedded_address(&identity.server_id)
                        .and_then(|url| {
                            url::Url::parse(&url)
                                .ok()
                                .and_then(|u| u.host_str().map(str::to_owned))
                        });
                    match crate::iroh_route::host_plan(existing.as_deref()) {
                        crate::iroh_route::HostPlan::Attach => {
                            self.attach_embedded_host(&data, crate::iroh_route::ONLY_ADDRESS, true)?
                        }
                        // Shared through Tailscale (or not shared): the server restarts on
                        // the placeholder address, which also lets go of Tailscale. The
                        // community, accounts, history and keys are the same data folder.
                        crate::iroh_route::HostPlan::Restart => self.switch_to_embedded_host(
                            server,
                            data,
                            crate::iroh_route::ONLY_ADDRESS.into(),
                        )?,
                    }
                } else {
                    let package = server
                        .parent()
                        .and_then(|p| p.parent())
                        .ok_or("Cannot locate the Relay package.")?;
                    let stack = package.join("scripts/launch-stack.ps1");
                    let sfu = server
                        .parent()
                        .ok_or("Cannot locate the video server.")?
                        .join("livekit-server.exe");
                    if !stack.is_file() || !sfu.is_file() {
                        return Err("Extract the complete Relay package before hosting.".into());
                    }
                    self.host_stack(
                        server,
                        data,
                        stack,
                        sfu,
                        None,
                        Some(crate::iroh_route::ONLY_ADDRESS.into()),
                    )?;
                }
            }
            Command::RotateIrohToken { origin } => {
                let connection = self.command_connection(&origin)?;
                let transferred=self.handover_runtime.as_ref().is_some_and(|r|
                    r.matches_connection(&origin.server_id,&connection.url));
                if !transferred && (!crate::local_host::is_local_url(&connection.url)
                    || self.local_identity().as_ref().map(|id| &id.server_id)
                        != Some(&origin.server_id))
                {
                    return Err(
                        "Only the PC hosting this server can stop its iroh invitations.".into(),
                    );
                }
                let bootstrap: Bootstrap = self.get("/api/bootstrap")?;
                validate_bootstrap_identity(&origin.server_id, &origin.user_id, &bootstrap)?;
                if !bootstrap.has_permission(PERMISSION_ADMIN) {
                    return Err("Only a server administrator can stop invitations.".into());
                }
                crate::iroh_route::rotate(&origin.server_id)?;
                self.emit(Update::IrohInvitationsStopped { origin });
            }
            Command::StopLocal => {
                let local_connection = self
                    .connection
                    .as_ref()
                    .is_some_and(|c| crate::local_host::is_local_url(&c.url));
                if local_connection {
                    let identity = self.local_identity().ok_or(
                        "Cannot confirm which local server is running. Refresh and try again.",
                    )?;
                    // Both identifiers must match the currently running instance.
                    // Authenticated server-side administration decides ownership.
                    self.post_empty("/api/admin/shutdown", &serde_json::json!({"instance_id":identity.instance_id,"confirm_server_id":identity.server_id}))?;
                    let _ = crate::iroh_route::unhost(&identity.server_id);
                    if crate::embedded_network::unhost(&identity.server_id).is_err() {
                        self.emit(Update::Error("The server stopped, but its private connection could not be released. Restart Relay before hosting again.".into()));
                    }
                    self.local_server = None;
                } else if let Some(mut child) = self.local_server.take() {
                    if child
                        .try_wait()
                        .map_err(|_| "Cannot check this server process.")?
                        .is_none()
                    {
                        child.kill().map_err(|_| "Cannot stop this local server.")?;
                        let _ = child.wait();
                    }
                } else {
                    return Err(
                        "Sign in as an administrator of this PC's server to stop it.".into(),
                    );
                }
                if local_connection {
                    self.cancel_transfers();
                    self.connection = None;
                    self.chat_media_epoch.fetch_add(1, Ordering::SeqCst);
                    self.chat_media_workers = None;
                    self.legacy_reactions.clear();
                    self.socket_stop.store(true, Ordering::Relaxed);
                    self.emit(Update::Disconnected);
                }
                self.owned_local = None;
                self.emit(Update::LocalStopped);
            }
            command => self.handle_extended(command)?,
        }
        if let Some((server, data)) = hosting_target {
            self.remember_local_host(server, data)?;
        }
        Ok(())
    }

    fn sync_live_transport(&mut self) -> Result<(), String> {
        if self.connection.as_ref().is_some_and(|c| c.crypto.is_some()) {
            self.live.epoch.fetch_add(1, Ordering::SeqCst);
            let mut context = self
                .live
                .context
                .lock()
                .map_err(|_| "The media worker stopped.")?;
            *context = None;
            self.live.activity.notify_all();
            return Ok(());
        }
        let context = self.connection.as_ref().and_then(|c| {
            c.voice_channel.as_ref().map(|channel| LiveConnection {
                url: c.url.clone(),
                token: c.session.token.clone(),
                channel_id: channel.clone(),
                generation: self.live.epoch.load(Ordering::SeqCst),
                sharing: c.sharing,
                camera: c.camera,
            })
        });
        let active = context.is_some();
        if active {
            self.live
                .initialize_http()
                .map_err(|error| format!("Cannot start live media: {error}"))?;
        }
        let mut current = self
            .live
            .context
            .lock()
            .map_err(|_| "The media worker stopped.")?;
        *current = context;
        self.live.activity.notify_all();
        drop(current);
        if active {
            if let Some(frames) = &self.live_frames {
                self.live
                    .start(frames.clone())
                    .map_err(|error| format!("Cannot start live media: {error}"))?;
                self.live_frames = None;
            }
        }
        Ok(())
    }

    fn command_connection(&self, origin: &CommandOrigin) -> Result<&Connection, String> {
        let connection = self
            .connection
            .as_ref()
            .ok_or("Connect to a community first.")?;
        if origin != &CommandOrigin::from(connection) {
            return Err("This request belongs to another account or community.".into());
        }
        if let Some(crypto) = &connection.crypto {
            if crypto.device.identity().server_id != connection.server_id
                || crypto.device.identity().user_id != connection.session.user.id
            {
                return Err("The encryption actor belongs to another account or community.".into());
            }
        }
        Ok(connection)
    }

    fn queue_chat_media(
        &mut self,
        origin: &CommandOrigin,
        attachment: Attachment,
    ) -> Result<(), String> {
        self.command_connection(origin)?;
        self.queue_image(attachment, false)
    }

    fn queue_chat_media_original(
        &mut self,
        origin: &CommandOrigin,
        attachment: Attachment,
    ) -> Result<(), String> {
        self.command_connection(origin)?;
        self.queue_image_size(attachment, false, true)
    }
    fn queue_avatar(&mut self, user: User) -> Result<(), String> {
        let attachment = Attachment {
            id: avatar_attachment_id(&user),
            name: "Avatar".into(),
            mime: "image/png".into(),
            size: 0,
            url: user
                .profile
                .avatar_url
                .ok_or("This profile has no avatar.")?,
        };
        self.queue_image(attachment, true)
    }
    fn queue_asset(&mut self, asset: MediaAsset) -> Result<(), String> {
        self.queue_image(
            Attachment {
                id: asset_attachment_id(&asset),
                name: asset.name,
                mime: "image/png".into(),
                size: 0,
                url: asset.url,
            },
            true,
        )
    }

    fn queue_image(&mut self, attachment: Attachment, avatar: bool) -> Result<(), String> {
        self.queue_image_size(attachment, avatar, false)
    }
    fn queue_image_size(
        &mut self,
        attachment: Attachment,
        avatar: bool,
        original: bool,
    ) -> Result<(), String> {
        let connection = self
            .connection
            .as_ref()
            .ok_or("Connect to a server first.")?;
        if avatar {
            validate_avatar_attachment(&attachment)?;
        } else {
            validate_chat_attachment(&attachment)?;
        }
        let plain_files = !avatar && connection.chat.plain_files_readable();
        let encrypted = if avatar {
            None
        } else {
            let key = match connection.crypto.as_ref() {
                Some(crypto) => crypto.file(&attachment.id)?,
                None => None,
            };
            match key {
                Some(key) => Some(key),
                None if plain_files => None,
                None if connection.crypto.is_none() => {
                    return Err("Encryption keys are not ready.".into())
                }
                None => return Err("This attachment has no verified encryption key. Plaintext fallback is disabled.".into()),
            }
        };
        if self.chat_media_workers.is_none() {
            self.chat_media_workers = Some(
                start_chat_media_workers(self.chat_media_epoch.clone(), self.wake.clone())
                    .map_err(|error| format!("Cannot start image previews: {error}"))?,
            );
        }
        self.chat_media_workers
            .as_ref()
            .expect("image workers initialized before submission")
            .jobs
            .try_send(ChatMediaJob {
                _activity: Some(shutdown::Lease::acquire(self.work.media.clone())),
                original,
                avatar,
                attachment,
                url: connection.url.clone(),
                token: connection.session.token.clone(),
                epoch: self.chat_media_epoch.load(Ordering::SeqCst),
                encrypted,
                plain: plain_files,
            })
            .map_err(|_| "Image previews are busy. Try again shortly.".into())
    }

    fn poll_chat_media_results(&self) {
        let Some(workers) = &self.chat_media_workers else {
            return;
        };
        while let Ok(result) = workers.results.try_recv() {
            // Delivery is serialized with Auth/Logout on this control worker. An
            // old result cannot arrive after a new session's Connected update.
            if result.epoch != self.chat_media_epoch.load(Ordering::SeqCst) {
                continue;
            }
            self.emit(match result.result {
                Ok(media) => Update::ChatMediaFetched {
                    attachment_id: result.attachment_id,
                    media,
                },
                Err(error) => Update::ChatMediaFailed {
                    attachment_id: result.attachment_id,
                    error,
                },
            });
        }
    }

    fn detach_local_connections(&mut self) -> Result<(), String> {
        if self.connection.as_ref().is_some_and(|c| crate::local_host::is_local_url(&c.url)) {
            self.cancel_transfers();
            self.live.epoch.fetch_add(1, Ordering::SeqCst);
            if let Ok(mut context) = self.live.context.lock() { *context = None; }
            let gate = self.live.gate.clone();
            let _guard = gate.lock().map_err(|_| "The media worker stopped.")?;
            self.clear_voice_intent();
            self.chat_media_epoch.fetch_add(1, Ordering::SeqCst);
            self.chat_media_workers = None;
            self.legacy_reactions.clear();
            self.connection = None;
            self.socket_stop.store(true, Ordering::SeqCst);
            self.emit(Update::Disconnected);
        }
        let local: Vec<_> = self.saved_connections.keys()
            .filter(|url| crate::local_host::is_local_url(url)).cloned().collect();
        for url in local {
            if let Some(saved) = self.saved_connections.remove(&url) {
                saved.stop.store(true, Ordering::SeqCst);
            }
        }
        self.publish_communities();
        Ok(())
    }

    fn host_local(&mut self, server: PathBuf, data: PathBuf) -> Result<(), String> {
        if self.local_server_ready() {
            self.emit_local_ready(false);
            return Ok(());
        }
        std::fs::create_dir_all(&data).map_err(|_| "Cannot create server storage.")?;
        if let Some(package) = server.parent().and_then(|path| path.parent()) {
            let stack = package.join("scripts").join("launch-stack.ps1");
            if stack.is_file() {
                let sfu = server
                    .parent()
                    .ok_or("Cannot locate the video server.")?
                    .join("livekit-server.exe");
                if !sfu.is_file() {
                    return Err("The bundled video server is missing. Extract the complete Relay package and try again.".into());
                }
                return self.host_stack(server, data, stack, sfu, None, None);
            }
        }
        let log_dir = data.join("logs");
        std::fs::create_dir_all(&log_dir).map_err(|_| "Cannot create server logs.")?;
        // The member's saved opt-in, and only when the owner account already
        // exists: the server refuses a network bind before that, and a start
        // never listens more widely than the member chose.
        let lan = crate::local_host::lan_choice(&data)
            && crate::local_host::owner_ready(&server, &data);
        let (bind, extra) = crate::local_host::local_launch(lan);
        let mut command = std::process::Command::new(&server);
        if let Ok(Some(provider)) = crate::central_account::Provider::configured() {
            provider.configure_host(&mut command);
        }
        command
            .args([
                "--bind",
                bind,
                "--port",
                "4580",
                "--udp-port",
                "4581",
                "--name",
                "Relay House",
                "--data",
            ])
            .arg(&data)
            .args(extra);
        let log = std::fs::File::create(log_dir.join("server.log"))
            .map_err(|_| "Cannot create the server log.")?;
        let error_log = log
            .try_clone()
            .map_err(|_| "Cannot create the server log.")?;
        command.stdout(log).stderr(error_log);
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        let mut child = command
            .spawn()
            .map_err(|_| "Cannot start the local server executable.")?;
        for _ in 0..40 {
            if child
                .try_wait()
                .map_err(|_| "Cannot check the server process.")?
                .is_some()
            {
                return Err("The local server exited. Check server.log for details.".into());
            }
            if let Some(identity) = self
                .local_identity()
                .filter(|identity| identity.pid == child.id())
            {
                let _ = std::fs::write(log_dir.join("server.pid"), child.id().to_string());
                if let Err(error) = identity.save(&data, &server) {
                    self.emit(Update::Error(error));
                }
                self.local_server = Some(child);
                self.emit_local_ready(true);
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = child.kill();
        let _ = child.wait();
        Err("The local server did not become ready. Check server.log.".into())
    }

    #[cfg(windows)]
    fn host_stack(
        &mut self,
        server: PathBuf,
        data: PathBuf,
        script: PathBuf,
        sfu: PathBuf,
        tailscale: Option<String>,
        embedded: Option<String>,
    ) -> Result<(), String> {
        use std::os::windows::process::CommandExt;
        let logs = data.join("logs");
        std::fs::create_dir_all(&logs).map_err(|_| "Cannot create server logs.")?;
        let log = std::fs::File::create(logs.join("host-launch.log"))
            .map_err(|_| "Cannot create the hosting log.")?;
        let error_log = log
            .try_clone()
            .map_err(|_| "Cannot create the hosting log.")?;
        let mut command = std::process::Command::new("powershell.exe");
        if let Ok(Some(provider)) = crate::central_account::Provider::configured() {
            provider.configure_host(&mut command);
        }
        command
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
            ])
            .arg(script)
            .arg("-DataDirectory")
            .arg(&data)
            .arg("-ServerBinary")
            .arg(&server)
            .arg("-SfuBinary")
            .arg(sfu)
            .stdout(log)
            .stderr(error_log)
            .creation_flags(0x08000000);
        if let Some(address) = &tailscale {
            command.arg("-TailscaleIPv4").arg(address);
        }
        if let Some(address) = &embedded {
            let listener = std::net::TcpListener::bind("127.0.0.1:0")
                .map_err(|_| "Cannot reserve a private server port.")?;
            let port = listener
                .local_addr()
                .map_err(|_| "Cannot read the private server port.")?
                .port();
            drop(listener);
            command
                .arg("-EmbeddedIPv4")
                .arg(address)
                .arg("-NetworkPeerPort")
                .arg(port.to_string());
        }
        // Plain and iroh hosts keep their ordinary ports on loopback. LAN
        // opt-in adds only the code redemption listener; Tailscale keeps its
        // existing network binding and does not use this mode.
        let lan = tailscale.is_none()
            && crate::local_host::lan_choice(&data)
            && crate::local_host::owner_ready(&server, &data);
        if lan {
            command.arg("-LanDiscovery");
        }
        let mut launcher = command
            .spawn()
            .map_err(|_| "Cannot launch the local hosting stack.")?;
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline {
            if let Some(exit) = launcher
                .try_wait()
                .map_err(|_| "Cannot check the hosting launcher.")?
            {
                if !exit.success() {
                    return Err(format!(
                        "Local hosting failed. Read {}.",
                        logs.join("host-launch.log").display()
                    ));
                }
                // The server is ours even if its sharing helper cannot connect.
                // Remember it before attaching that helper so normal app close
                // also cleans up a partially successful hosting attempt.
                self.remember_local_host(server.clone(), data.clone())?;
                if tailscale.is_none() && embedded.is_none() && self.local_identity().is_some() {
                    self.emit_local_ready(true);
                    return Ok(());
                }
                if let Some(address) = &tailscale {
                    let identity = self
                        .local_identity()
                        .ok_or("The Tailscale server did not become ready.")?;
                    let record = crate::local_host::read_record(&data.join("stack-instance.json"))
                        .map_err(|_| "Cannot confirm the Tailscale host.")?;
                    let shared_url = format!("http://{address}:4580");
                    if record["relay_pid"].as_u64() != Some(identity.pid.into())
                        || record["hosting_mode"].as_str() != Some("tailscale")
                        || record["advertised_url"].as_str() != Some(shared_url.as_str())
                    {
                        return Err(
                            "The running server is not the requested Tailscale host.".into()
                        );
                    }
                    self.emit(Update::TailscaleReady {
                        address: shared_url,
                    });
                    return Ok(());
                }
                if let Some(address) = &embedded {
                    self.attach_embedded_host(
                        &data,
                        address,
                        address == crate::iroh_route::ONLY_ADDRESS,
                    )?;
                    return Ok(());
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        // The launcher may be finishing cleanup. Keep its exact owned handle;
        // do not kill unrelated processes using a saved PID.
        Err(format!(
            "The hosting stack did not become ready. Read {}.",
            logs.join("host-launch.log").display()
        ))
    }

    #[cfg(not(windows))]
    fn host_stack(
        &mut self,
        _server: PathBuf,
        _data: PathBuf,
        _script: PathBuf,
        _sfu: PathBuf,
        _tailscale: Option<String>,
        _embedded: Option<String>,
    ) -> Result<(), String> {
        Err("Use the platform hosting instructions to start this server.".into())
    }

    /// Connects a running built-in-network server to the route friends use.
    /// `iroh` serves the account-free route instead of the Tailscale one.
    fn attach_embedded_host(
        &mut self,
        data: &std::path::Path,
        address: &str,
        iroh: bool,
    ) -> Result<(), String> {
        let identity = self
            .local_identity()
            .ok_or("Cannot confirm the local server.")?;
        let read = |name: &str| -> Result<serde_json::Value, String> {
            crate::local_host::read_record(&data.join(name))
                .map_err(|_| "The private server configuration is missing or invalid.".into())
        };
        let stack = read("stack-instance.json")?;
        let runtime = read("server-runtime.json")?;
        if stack["hosting_mode"] != "embedded"
            || stack["embedded_ipv4"].as_str() != Some(address)
            || stack["relay_pid"].as_u64() != Some(identity.pid.into())
            || stack["server_id"].as_str() != Some(identity.server_id.as_str())
            || runtime["server_id"].as_str() != Some(identity.server_id.as_str())
            || runtime["instance_id"].as_str() != Some(identity.instance_id.as_str())
        {
            return Err("Stop this server in Server Settings before switching its private connection. The existing community is kept.".into());
        }
        let port = runtime["network_peer_port"]
            .as_u64()
            .and_then(|v| u16::try_from(v).ok())
            .filter(|p| *p != 0)
            .ok_or("The server needs the latest built-in connection support.")?;
        let token = runtime["network_ingress_token"]
            .as_str()
            .filter(|t| t.len() >= 32 && t.len() <= 128)
            .ok_or("The private server proof is unavailable.")?;
        if iroh {
            let route = crate::iroh_route::host(port, token, &identity.server_id)?;
            // Relay account sign-in through this address must be bound to it, so the
            // server learns it is its own. Older servers or ones without shared
            // accounts refuse; nothing else depends on it.
            let _ = register_account_destination(&runtime, &identity, &route.address());
            self.local_sharing_instance =
                Some((identity.server_id.clone(), identity.instance_id.clone()));
            self.emit(Update::IrohReady {
                route,
                only: address == crate::iroh_route::ONLY_ADDRESS,
            });
            return Ok(());
        }
        let status = crate::embedded_network::host(port, token, &identity.server_id)?;
        if status.phase != "ready" || status.address.as_deref() != Some(address) {
            return Err("The private server could not connect.".into());
        }
        self.local_sharing_instance = Some((identity.server_id.clone(), identity.instance_id));
        self.emit(Update::TailscaleReady {
            address: format!("http://{address}:4580"),
        });
        Ok(())
    }

    /// Reattach the configured sharing route after sign-in or helper failure.
    fn restore_local_sharing(&mut self, bootstrap: &Bootstrap) {
        let Some(connection) = self.connection.as_ref() else {
            return;
        };
        if !crate::local_host::is_local_url(&connection.url)
            || !bootstrap.has_permission(PERMISSION_ADMIN)
            || Instant::now() < self.local_sharing_retry_at
        {
            return;
        }
        // Bound retries so a broken helper does not block every refresh or
        // repeatedly interrupt the user with the same notice.
        self.local_sharing_retry_at = Instant::now() + Duration::from_secs(30);
        let restore =
            || -> Result<Option<(PathBuf, String, crate::local_host::Identity)>, String> {
                let data = crate::local_host::data_directory()?;
                let stack_path = data.join("stack-instance.json");
                if !stack_path.is_file() {
                    return Ok(None);
                }
                let stack = crate::local_host::read_record(&stack_path)
                    .map_err(|_| "Cannot read your server's sharing settings.")?;
                if stack["hosting_mode"] != "embedded" {
                    return Ok(None);
                }
                let identity = self
                    .local_identity()
                    .ok_or("Cannot confirm your local server.")?;
                let managed = crate::local_host::read_record(&data.join("managed-instance.json"))
                    .map_err(|_| "Cannot confirm your server's saved identity.")?;
                if managed["data"]
                    .as_str()
                    .and_then(|path| PathBuf::from(path).canonicalize().ok())
                    != Some(
                        data.canonicalize()
                            .map_err(|_| "Cannot confirm your server's data folder.")?,
                    )
                {
                    return Err(
                        "This server uses another data folder. Open its matching Relay profile."
                            .into(),
                    );
                }
                let address = crate::local_host::sharing_restore_address(
                    &bootstrap.server_id,
                    &identity,
                    &managed,
                    &stack,
                )?;
                Ok(address.map(|address| (data, address, identity)))
            };
        let result = restore().and_then(|plan| {
            let Some((data, address, identity)) = plan else {
                return Ok(());
            };
            let iroh = address == crate::iroh_route::ONLY_ADDRESS;
            let running = if iroh {
                crate::iroh_route::hosted(&bootstrap.server_id).is_some()
            } else {
                crate::embedded_network::shared_address(&bootstrap.server_id).is_some()
            };
            let current = (identity.server_id, identity.instance_id);
            if running && self.local_sharing_instance.as_ref() == Some(&current) {
                return Ok(());
            }
            self.attach_embedded_host(&data, &address, iroh)
        });
        match result {
            Ok(()) => {
                self.local_sharing_retry_at = Instant::now();
                self.local_sharing_failure_notified = false;
            }
            Err(error) if !self.local_sharing_failure_notified => {
                self.local_sharing_failure_notified = true;
                self.emit(Update::Notice(format!(
                    "Your server is running, but sharing could not reconnect. Open Invite friends to try again. {error}"
                )));
            }
            Err(_) => {}
        }
    }

    fn switch_to_embedded_host(
        &mut self,
        server: PathBuf,
        data: PathBuf,
        address: String,
    ) -> Result<(), String> {
        let c = self
            .connection
            .as_ref()
            .ok_or("Sign in to your server before inviting friends.")?;
        if !crate::local_host::is_local_url(&c.url) {
            return Err("Return to your own server before changing its sharing.".into());
        }
        if c.voice_channel.is_some()
            || c.sharing
            || c.camera
            || self.transfer_active.load(Ordering::Acquire) != 0
            || self.archive_busy.load(Ordering::Acquire)
        {
            return Err("Leave the call and wait for file transfers or backups to finish, then enable sharing.".into());
        }
        let expected = c.server_id.clone();
        let identity = self
            .local_identity()
            .ok_or("Cannot confirm your local server. It was left running.")?;
        let read = |name: &str| -> Result<serde_json::Value, String> {
            crate::local_host::read_record(&data.join(name))
                .map_err(|_| "Cannot read the server's saved settings. It was left running.".into())
        };
        let managed = read("managed-instance.json")?;
        crate::local_host::validate_sharing_switch(
            &expected,
            &identity,
            &managed,
            &read("stack-instance.json")?,
        )?;
        let saved_data = managed["data"]
            .as_str()
            .map(PathBuf::from)
            .and_then(|p| p.canonicalize().ok());
        if saved_data.is_none() || saved_data != data.canonicalize().ok() {
            return Err("This server uses another data folder. It was left running; open the matching Relay profile before sharing.".into());
        }
        let package = server
            .parent()
            .and_then(|p| p.parent())
            .ok_or("Cannot locate the Relay package.")?;
        let stack = package.join("scripts/launch-stack.ps1");
        let sfu = server
            .parent()
            .ok_or("Cannot locate the video server.")?
            .join("livekit-server.exe");
        if !server.is_file() || !stack.is_file() || !sfu.is_file() {
            return Err(
                "Extract the complete Relay package before sharing. Your server was left running."
                    .into(),
            );
        }
        // The server authenticates administration and matches both IDs. Keep the
        // live session/crypto actor and its websocket retry loop across restart.
        self.post_empty("/api/admin/shutdown", &serde_json::json!({"instance_id":identity.instance_id,"confirm_server_id":identity.server_id}))?;
        self.set_status(ConnectionStatus::Reconnecting);
        let deadline = Instant::now() + Duration::from_secs(12);
        while self.local_server_ready() {
            if Instant::now() >= deadline {
                return Err(
                    "Your server is still closing. Wait a moment, then try sharing again.".into(),
                );
            }
            if self
                .local_identity()
                .is_some_and(|current| current.instance_id != identity.instance_id)
            {
                return Err(
                    "A different local server started during setup. Relay left it running.".into(),
                );
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        self.local_server = None;
        let _ = crate::iroh_route::unhost(&expected);
        let result = crate::embedded_network::unhost(&expected).and_then(|()| {
            self.host_stack(
                server.clone(),
                data.clone(),
                stack.clone(),
                sfu.clone(),
                None,
                Some(address),
            )
        });
        if let Err(error) = result {
            if !self.local_server_ready() {
                let restored = self
                    .host_stack(server, data, stack, sfu, None, None)
                    .is_ok();
                return Err(if restored {
                    format!("Sharing couldn't be enabled. Your server is running locally again. Try sharing again. Details: {error}")
                } else {
                    format!("Sharing couldn't be enabled. Your data is kept; start your server again. Details: {error}")
                });
            }
            return Err(error);
        }
        let current = self
            .local_identity()
            .ok_or("Your server didn't respond after setup. Your data is kept.")?;
        if current.server_id != expected {
            self.reject_active_identity("The server identity changed during setup. Sign in again after confirming your server.".into());
            return Err(IDENTITY_CHANGED.into());
        }
        self.refresh();
        Ok(())
    }

    fn emit_local_ready(&self, created: bool) {
        // Credential-free, bounded health validation. Only the local-start
        // result uses this hint; an invitation cannot change another form.
        let setup_required = check_server(&self.http, "http://127.0.0.1:4580")
            .ok()
            .and_then(|summary| summary.setup_required);
        self.emit(Update::LocalReady {
            created,
            setup_required,
        });
    }

    fn local_server_ready(&self) -> bool {
        self.http
            .get("http://127.0.0.1:4580/health")
            .timeout(Duration::from_millis(300))
            .send()
            .ok()
            .filter(|r| r.status().is_success())
            .and_then(|r| read_json::<serde_json::Value>(r, 16 * 1024).ok())
            .is_some_and(|v| {
                v["ok"] == true && v["server_name"].is_string() && v["version"].is_string()
            })
    }

    fn local_identity(&self) -> Option<crate::local_host::Identity> {
        let response = self
            .http
            .get("http://127.0.0.1:4580/health")
            .timeout(Duration::from_millis(500))
            .send()
            .ok()?;
        let value = read_json::<serde_json::Value>(response, 16 * 1024).ok()?;
        crate::local_host::Identity::parse(&value)
    }

    fn follow_host_move(&mut self) -> Result<bool, String> {
        let c = self.connection.as_mut().ok_or(crate::host_route::REFUSED)?;
        let Some(pin) = c.chat.authority.clone() else {
            return Ok(false);
        };
        let origin = CommandOrigin::from(&*c);
        let next = if let Some(job) = &c.chat.handover_probe {
            let Some(result) = job.poll() else {
                return Ok(false);
            };
            c.chat.handover_probe = None;
            match result {
                Ok(Some(next)) => next,
                Ok(None) => return Ok(false),
                Err(error)
                    if error == crate::host_route::UNAVAILABLE
                        || error == crate::host_route::PAUSED =>
                {
                    return Ok(false)
                }
                Err(error) => return Err(error),
            }
        } else {
            let now = Instant::now();
            if c.chat
                .handover_checked_at
                .is_some_and(|at| now.duration_since(at) < Duration::from_secs(5))
            {
                return Ok(false);
            }
            c.chat.handover_checked_at = Some(now);
            let wake = self.wake.clone();
            let refresh = self.refresh_pending.clone();
            c.chat.handover_probe = Some(crate::host_route::Job::start(
                origin.url.clone(),
                pin.clone(),
                crate::host_locator::ReadCapability::session(&pin, &c.session.token),
                self.background_active.clone(),
                move || {
                    refresh.store(true, Ordering::Release);
                    wake.notify();
                },
            )?);
            return Ok(false);
        };
        if self.saved_connections.contains_key(&next.url) {
            return Err("The new host is already open in another server session. Switch to that session to continue.".into());
        }
        let session = self.connection.as_ref().unwrap().session.clone();
        self.pause_voice_for_outage();
        self.adopt_session_checked(
            next.url,
            Some(origin.server_id.clone()),
            session,
            None,
            Some(MovingSession {
                provider: self
                    .connection
                    .as_ref()
                    .unwrap()
                    .chat
                    .account_provider
                    .clone(),
                aliases: crate::host_route::alias_history(
                    &origin.url,
                    &self.connection.as_ref().unwrap().chat.authority_aliases,
                ),
                origin,
                pin: next.pin,
                remembered: self.connection.as_ref().unwrap().remembered,
                resuming: false,
            }),
            None,
        )?;
        self.emit(Update::Notice("The community moved to its new host. Your conversations and drafts have been kept; join voice again when you are ready.".into()));
        Ok(true)
    }

    fn refresh(&mut self) {
        if self.connection.is_none() {
            return;
        }
        match self.get::<call_alerts::BootstrapReply>("/api/bootstrap") {
            Ok(call_alerts::BootstrapReply {
                bootstrap: mut b,
                cursor,
            }) => {
                let call_received = Instant::now();
                let c = self.connection.as_ref().expect("active refresh");
                if let Err(error) =
                    validate_bootstrap_identity(&c.server_id, &c.session.user.id, &b)
                {
                    self.reject_active_identity(error);
                    return;
                }
                match crate::host_route::bootstrap_pin(
                    &c.url,
                    &c.server_id,
                    b.host_authority.as_ref(),
                    c.chat.authority.as_ref(),
                ) {
                    Ok(pin) => self.connection.as_mut().unwrap().chat.authority = pin,
                    Err(error) => {
                        self.reject_active_identity(error);
                        return;
                    }
                }
                let c = self.connection.as_ref().unwrap();
                self.record_alert_bootstrap(&CommandOrigin::from(c), &b, call_received, cursor);
                if b.host_paused {
                    self.pause_voice_for_outage();
                    self.set_status(ConnectionStatus::Reconnecting);
                    if let Some(c) = &mut self.connection {
                        c.chat.update_bootstrap(&b);
                    }
                    self.emit(Update::Bootstrap(b));
                    return;
                }
                self.set_status(ConnectionStatus::Connected);
                self.restore_local_sharing(&b);
                if let Some(c) = &mut self.connection {
                    c.chat.update_bootstrap(&b);
                    c.name = b.server_name.clone();
                    c.server_id = b.server_id.clone();
                    c.protocol_version = b.protocol_version;
                    c.session.user = b.user.clone();
                    if c.change_cursor > b.latest_change_sequence {
                        c.change_cursor = b.latest_change_sequence;
                    }
                }
                if !self.recover_voice(&mut b) {
                    return;
                }
                if let Some(target) = self.connection.as_ref().and_then(|c| c.current.clone()) {
                    if self
                        .connection
                        .as_ref()
                        .is_some_and(|c| c.chat.supports("e2ee_mls_v1") && !c.chat.plain_allowed())
                    {
                        let _ = self.ensure_crypto_conversation(&target);
                    }
                }
                if let Some(c) = &self.connection {
                    encrypted::reconcile_read_states(c, &mut b.read_states);
                }
                let compatible = b.protocol_version >= API_PROTOCOL_VERSION;
                self.emit(Update::Bootstrap(b));
                let caught_up = self.load_changes();
                if !compatible || !caught_up {
                    self.message_repair_at = Instant::now() - Duration::from_secs(30);
                }
            }
            Err(e) => {
                // Only a durable source grant plus a fresh proof from the new
                // key may send this session to a different address.
                match self.follow_host_move() {
                    Ok(true) => return,
                    Err(error) => self.report_refresh_error(error),
                    Ok(false) => {}
                }
                if e != "Sign in again" {
                    self.pause_voice_for_outage();
                }
                if e == "Sign in again" {
                    self.cancel_transfers();
                    self.chat_media_epoch.fetch_add(1, Ordering::SeqCst);
                    self.chat_media_workers = None;
                    self.legacy_reactions.clear();
                    self.live.epoch.fetch_add(1, Ordering::SeqCst);
                    if let Ok(mut context) = self.live.context.lock() {
                        *context = None;
                    }
                    if let Some(c) = &mut self.connection {
                        c.chat.clear_ephemeral();
                        c.voice_channel = None;
                        c.voice_attempt = None;
                        c.voice_admission = None;
                        c.voice_recovery = None;
                        c.crypto_pending_join = None;
                        c.crypto_call_epoch = None;
                        c.sharing = false;
                        c.camera = false;
                        c.frame_ids.clear();
                        let _ = crate::credential_vault::forget(&c.url);
                        c.remembered = false;
                    }
                    self.live.activity.notify_all();
                }
                self.set_status(if e == "Sign in again" {
                    ConnectionStatus::SignInRequired
                } else {
                    ConnectionStatus::Reconnecting
                });
                self.report_refresh_error(e);
            }
        }
        self.refresh_encryption();
        // Deltas update the conversation immediately; separately repair missed
        // or legacy events instead of reloading 100 rows on every refresh.
        if self.message_repair_at.elapsed() >= Duration::from_secs(30) {
            if let Err(e) = self.load_messages() {
                self.report_refresh_error(e);
            }
        }
    }

    fn report_refresh_error(&self, error: String) {
        // Automatic reconnect already has a live status banner. A second,
        // dismiss-only timeout would remain red after the server recovers.
        // Explicit commands and non-transport failures still report errors.
        if self.status == Some(ConnectionStatus::Reconnecting)
            && matches!(error.as_str(), SERVER_UNREACHABLE | SERVER_TIMEOUT)
        {
            return;
        }
        self.emit(Update::Error(error));
    }

    fn reject_active_identity(&mut self, error: String) {
        // Local-only stop: never send logout/leave or further history requests
        // to an endpoint which has just returned another identity.
        self.cancel_transfers();
        self.chat_media_epoch.fetch_add(1, Ordering::SeqCst);
        self.chat_media_workers = None;
        self.legacy_reactions.clear();
        self.socket_stop.store(true, Ordering::SeqCst);
        self.refresh_pending.store(false, Ordering::SeqCst);
        self.live.epoch.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut context) = self.live.context.lock() {
            *context = None;
        }
        self.live.activity.notify_all();
        self.set_status(ConnectionStatus::SignInRequired);
        if let Some(c) = self.connection.take() {
            if c.remembered {
                let _ = crate::credential_vault::forget(&c.url);
            }
        }
        // SignInRequired stops media while preserving the user's drafts.
        // The private encryption actor is dropped; its on-disk store is kept.
        self.emit(Update::Error(error));
        self.publish_communities();
    }

    fn load_messages(&mut self) -> Result<(), String> {
        let target = match self.connection.as_ref().and_then(|c| c.current.clone()) {
            Some(t) => t,
            None => return Ok(()),
        };
        let request = self
            .request(reqwest::Method::GET, "/api/messages")?
            .query(&[("limit", "100")]);
        let request = match &target {
            Target::Channel(id) => request.query(&[("channel_id", id)]),
            Target::Direct(id) => request.query(&[("peer_id", id)]),
        };
        if self
            .connection
            .as_ref()
            .is_some_and(|c| c.chat.supports("e2ee_mls_v1") && !c.chat.plain_allowed())
        {
            if let Err(error) = self.ensure_crypto_conversation(&target) {
                self.emit(Update::Notice(error));
            }
        }
        let mut messages: Vec<Message> = read_json(
            checked(request.send().map_err(network_error)?)?,
            8 * 1024 * 1024,
        )?;
        self.decrypt_messages(&target, &mut messages);
        self.emit(Update::Messages { target, messages });
        self.message_repair_at = Instant::now();
        Ok(())
    }

    #[cfg(test)]
    fn upload_frame(&self, frame: CaptureFrame) {
        let camera = match frame.kind.as_str() {
            "screen" => false,
            "camera" => true,
            _ => return,
        };
        if frame.generation == 0
            || frame.generation != self.capture_epochs[usize::from(camera)].load(Ordering::SeqCst)
        {
            return;
        }
        if self.connection.as_ref().is_none_or(|c| {
            c.voice_channel.is_none()
                || (frame.kind == "screen" && !c.sharing)
                || (frame.kind == "camera" && !c.camera)
        }) {
            return;
        }
        let request = match self.request(reqwest::Method::POST, "/api/media/frame") {
            Ok(r) => r,
            Err(_) => return,
        };
        // Frame errors are surfaced once by the UI; no retry queue retains old frames.
        let result = request
            .query(&[
                ("kind", frame.kind),
                ("width", frame.width.to_string()),
                ("height", frame.height.to_string()),
            ])
            .header("Content-Type", "image/jpeg")
            .body(frame.jpeg)
            .send()
            .map_err(network_error)
            .and_then(checked);
        if let Err(error) = result {
            self.emit(Update::Error(error));
        }
    }

    #[cfg(test)]
    fn poll_frames(&mut self) {
        let channel = match self
            .connection
            .as_ref()
            .and_then(|c| c.voice_channel.clone())
        {
            Some(c) => c,
            None => return,
        };
        let result = (|| -> Result<Vec<(MediaFrame, Vec<u8>)>, String> {
            let list: Vec<MediaFrame> = parse(
                self.request(reqwest::Method::GET, "/api/media/frames")?
                    .query(&[("channel_id", channel)])
                    .send()
                    .map_err(network_error)?,
            )?;
            let mut updates = Vec::new();
            for frame in list.into_iter().take(16) {
                let key = format!("{}:{}", frame.user.id, frame.kind);
                if self
                    .connection
                    .as_ref()
                    .and_then(|c| c.frame_ids.get(&key))
                    .is_some_and(|id| id == &frame.frame_id)
                {
                    continue;
                }
                if !frame.url.starts_with("/api/") {
                    continue;
                }
                let response = self
                    .request(reqwest::Method::GET, &frame.url)?
                    .send()
                    .map_err(network_error)
                    .and_then(checked);
                let Ok(mut response) = response else { continue };
                let mut bytes = Vec::new();
                if std::io::Read::read_to_end(
                    &mut std::io::Read::take(&mut response, (MAX_FRAME_BYTES + 1) as u64),
                    &mut bytes,
                )
                .is_err()
                {
                    continue;
                }
                if bytes.len() > MAX_FRAME_BYTES {
                    continue;
                }
                if let Some(c) = &mut self.connection {
                    c.frame_ids.insert(key, frame.frame_id.clone());
                }
                updates.push((frame, bytes));
            }
            Ok(updates)
        })();
        if let Ok(frames) = result {
            if !frames.is_empty() {
                self.emit(Update::Frames(frames));
            }
        }
    }
}

const SERVER_UNREACHABLE: &str =
    "Cannot reach this server. Ask the host to check it is running and confirm the address.";
pub(crate) const SERVER_TIMEOUT: &str = "The server took too long to respond.";
const SERVER_REQUEST_FAILED: &str =
    "The request failed. Check your connection and server certificate.";

pub fn is_connection_error(error: &str) -> bool {
    matches!(
        error,
        SERVER_UNREACHABLE | SERVER_TIMEOUT | SERVER_REQUEST_FAILED
            | crate::connection_errors::WINDOWS_CONNECTION_BLOCKED
    )
}

fn network_error(error: reqwest::Error) -> String {
    if crate::connection_errors::windows_connection_blocked(&error) {
        crate::connection_errors::WINDOWS_CONNECTION_BLOCKED.into()
    } else if error.is_connect() {
        SERVER_UNREACHABLE.into()
    } else if error.is_timeout() {
        SERVER_TIMEOUT.into()
    } else {
        SERVER_REQUEST_FAILED.into()
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.cancel_transfers();
        for saved in self.saved_connections.values() {
            saved.stop.store(true, Ordering::SeqCst);
        }
        self.live.shutdown.store(true, Ordering::SeqCst);
        if let Ok(mut context) = self.live.context.lock() {
            *context = None;
            self.live.activity.notify_all();
        }
        self.live.epoch.fetch_add(1, Ordering::SeqCst);
        self.chat_media_epoch.fetch_add(1, Ordering::SeqCst);
        self.socket_stop.store(true, Ordering::Relaxed);
    }
}

/// A send that failed exactly with the error its `SendWait` was recorded for
/// is a wait; any other error, or a missing connection, stays a failure.
fn send_failure(
    target: Target,
    error: String,
    wait: Option<(SendWait, String)>,
    origin: Option<CommandOrigin>,
) -> Update {
    match (wait, origin) {
        (Some((wait, reported)), Some(origin)) if reported == error => Update::SendWaiting {
            target,
            origin,
            wait,
        },
        (_, origin) => Update::SendFailed {
            target,
            origin,
            error,
        },
    }
}

fn checked(response: reqwest::blocking::Response) -> Result<reqwest::blocking::Response, String> {
    if response.status().is_success() {
        return Ok(response);
    }
    let status = response.status();
    let value: serde_json::Value = read_json(response, 16 * 1024).unwrap_or_default();
    Err(value["error"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| format!("Server rejected the request ({status}).")))
}

const IDENTITY_CHANGED: &str = "The community or account identity changed. Connection stopped; confirm the address with the host before signing in again.";
const IDENTITY_UNCONFIRMED: &str =
    "This connection has no saved community identity. Sign in again to confirm the community.";

fn validate_bootstrap_identity(
    server_id: &str,
    user_id: &str,
    bootstrap: &Bootstrap,
) -> Result<(), String> {
    if !server_id.is_empty() && (bootstrap.server_id != server_id || bootstrap.user.id != user_id) {
        return Err(IDENTITY_CHANGED.into());
    }
    // Old protocol fixtures/servers without an identity cannot silently adopt
    // a modern encryption scope through refresh rather than a fresh sign-in.
    if server_id.is_empty() && bootstrap.protocol_version >= API_PROTOCOL_VERSION {
        return Err(IDENTITY_UNCONFIRMED.into());
    }
    Ok(())
}

fn resume_remembered(
    http: &Client,
    url: &str,
    remembered: &crate::credential_vault::RememberedSession,
    expected: Option<&str>,
) -> Result<(Session, Bootstrap, Instant, Option<u64>), String> {
    let pin = remembered.confirmed_server_id(expected)?;
    // Public probe first; a replaced community never receives a saved token.
    let summary = check_server(http, url)?;
    if summary.server_id != pin {
        return Err(IDENTITY_CHANGED.into());
    }
    crate::embedded_network::bind_identity(url, &pin)?;
    let call_alerts::BootstrapReply { bootstrap, cursor } = parse::<call_alerts::BootstrapReply>(
        http.get(crate::embedded_network::endpoint(&format!(
            "{url}/api/bootstrap"
        )))
        .bearer_auth(&remembered.session.token)
        .send()
        .map_err(network_error)?,
    )?;
    let received = Instant::now();
    validate_bootstrap_identity(&pin, &remembered.session.user.id, &bootstrap)?;
    if let Some(saved) = &remembered.authority {
        crate::host_route::bootstrap_pin(
            url,
            &pin,
            bootstrap.host_authority.as_ref(),
            Some(&saved.pin),
        )?;
        if !same_account_provider(
            saved.account_provider.as_ref(),
            bootstrap.account_provider.as_ref(),
        ) {
            return Err(crate::host_route::REFUSED.into());
        }
    }
    let mut session = remembered.session.clone();
    session.user = bootstrap.user.clone();
    Ok((session, bootstrap, received, cursor))
}

fn check_server(
    http: &Client,
    url: &str,
) -> Result<crate::connection_setup::ServerSummary, String> {
    crate::tailscale_connect::ensure_private_connection(url)?;
    // Public, credential-free and bounded. Never follow an invite to a redirect.
    let response = http
        .get(crate::embedded_network::endpoint(&format!(
            "{url}/api/health"
        )))
        .timeout(Duration::from_secs(2))
        .send()
        .map_err(network_error)?;
    if !response.status().is_success() {
        return Err(
            "The address did not return a Relay community. Check the address with the host.".into(),
        );
    }
    let summary: crate::connection_setup::ServerSummary = read_json(response, 16 * 1024)?;
    summary.validate(None)?;
    Ok(summary)
}

fn parse<T: DeserializeOwned>(response: reqwest::blocking::Response) -> Result<T, String> {
    read_json(checked(response)?, 2 * 1024 * 1024)
}

fn read_json<T: DeserializeOwned>(
    response: reqwest::blocking::Response,
    limit: usize,
) -> Result<T, String> {
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(
        &mut std::io::Read::take(response, (limit + 1) as u64),
        &mut bytes,
    )
    .map_err(|_| "Unable to read the server response.")?;
    if bytes.len() > limit {
        return Err("The server response is too large.".into());
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| "The server returned an incompatible response.".into())
}

#[cfg(test)]
fn spawn_events(
    base: String,
    token: String,
    refresh_pending: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    events: extended::ChatEventInbox,
    wake: idle::Wake,
) {
    spawn_events_with_alerts(base, token, refresh_pending, stop, events, wake, None);
}
fn spawn_events_with_alerts(
    base: String,
    token: String,
    refresh_pending: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    events: extended::ChatEventInbox,
    wake: idle::Wake,
    mut alerts: Option<call_alerts::Watch>,
) {
    std::thread::spawn(move || {
        let Ok(mut original) = url::Url::parse(&base) else {
            return;
        };
        let scheme = if original.scheme() == "https" {
            "wss"
        } else {
            "ws"
        };
        let _ = original.set_scheme(scheme);
        original.set_path(&format!(
            "{}/api/events",
            original.path().trim_end_matches('/')
        ));
        use tungstenite::client::IntoClientRequest;
        let mut retry = 1u32;
        while !stop.load(Ordering::Relaxed) {
            // Each retry checks the original private route's live helper and
            // generation instead of reusing a potentially reassigned local port.
            let endpoint = crate::embedded_network::endpoint(original.as_str());
            let Ok(mut request) = endpoint.as_str().into_client_request() else {
                return;
            };
            let Ok(value) = format!("Bearer {token}").parse() else {
                return;
            };
            request
                .headers_mut()
                .insert(tungstenite::http::header::AUTHORIZATION, value);
            let config = tungstenite::protocol::WebSocketConfig {
                max_message_size: Some(16 * 1024),
                max_frame_size: Some(16 * 1024),
                ..Default::default()
            };
            if let Ok((mut socket, _)) =
                tungstenite::client::connect_with_config(request, Some(config), 0)
            {
                retry = 1;
                refresh_pending.store(true, Ordering::Relaxed);
                wake.notify();
                match socket.get_mut() {
                    tungstenite::stream::MaybeTlsStream::Plain(s) => {
                        let _ = s.set_read_timeout(Some(Duration::from_secs(1)));
                    }
                    tungstenite::stream::MaybeTlsStream::Rustls(s) => {
                        let _ = s.sock.set_read_timeout(Some(Duration::from_secs(1)));
                    }
                    _ => {}
                }
                while !stop.load(Ordering::Relaxed) {
                    if let Some(alerts) = &mut alerts {
                        alerts.tick(false);
                    }
                    match socket.read() {
                        Ok(tungstenite::Message::Text(text)) => {
                            if stop.load(Ordering::SeqCst) {
                                break;
                            }
                            if let Ok(event) = serde_json::from_str::<Event>(&text) {
                                if matches!(event.kind.as_str(), "members" | "presence" | "messages") {
                                    if let Some(alerts) = &mut alerts {
                                        alerts.tick(true);
                                    }
                                }
                                extended::queue_chat_event(&events, &refresh_pending, event);
                                wake.notify();
                            }
                        }
                        Ok(tungstenite::Message::Ping(bytes)) => {
                            let _ = socket.send(tungstenite::Message::Pong(bytes));
                        }
                        Ok(tungstenite::Message::Close(_)) => break,
                        Ok(_) => {}
                        Err(tungstenite::Error::Io(e))
                            if matches!(
                                e.kind(),
                                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                            ) => {}
                        Err(_) => break,
                    }
                }
                let _ = socket.close(None);
            }
            // A parked community does not have the active poll timer. Wake its
            // bounded HTTP refresh on socket loss/failed reconnect so revoked
            // sessions and unavailable hosts cannot remain silently signed in.
            if !stop.load(Ordering::SeqCst) {
                refresh_pending.store(true, Ordering::Relaxed);
                wake.notify();
            }
            // Cancellation remains responsive without busy reconnecting.
            for _ in 0..retry * 10 {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                if let Some(alerts) = &mut alerts {
                    alerts.tick(false);
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            retry = (retry * 2).min(30);
        }
    });
}

#[path = "network_background_crypto.rs"]
mod background_crypto;
#[path = "network_call_alerts.rs"]
mod call_alerts;
#[path = "network_discord_host.rs"]
mod discord_host;
#[path = "network_e2ee.rs"]
mod encrypted;
#[path = "network_v2.rs"]
mod extended;
#[path = "network_history_backup.rs"]
mod history_backup;
#[path = "network_legacy_reactions.rs"]
mod legacy_reactions;
#[path = "network_reactions.rs"]
mod reactions;
#[path = "network_replica.rs"]
mod replica_token;
#[path = "network_voice_recovery.rs"]
mod voice_recovery;

#[cfg(test)]
#[path = "network_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "network_crypto_tests.rs"]
mod crypto_tests;

#[cfg(all(test, feature = "native-rtc"))]
#[path = "network_native_fixture.rs"]
pub(crate) mod native_fixture;

#[cfg(test)]
#[path = "network_gui_fixture_tests.rs"]
mod gui_fixture_tests;

#[path = "network_idle.rs"]
mod idle;
#[path = "network_local_lifecycle.rs"]
mod local_lifecycle;
#[path = "network_shutdown.rs"]
mod shutdown;
pub(crate) use shutdown::Poll as ShutdownPoll;
#[path = "network_snapshots.rs"]
mod snapshots;
