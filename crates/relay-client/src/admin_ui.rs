use super::*;
use crate::appearance::{TEXT_BODY, TEXT_CAPTION};
use serde_json::{json, Value};
include!("roles_ui.rs");
include!("social_ui.rs");
include!("friends_overview_ui.rs");
include!("conversations_ui.rs");
include!("polls_ui.rs");
include!("assets_ui.rs");
include!("interactions_ui.rs");
include!("welcome_ui.rs");
include!("discord_ui.rs");

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Section {
    #[default]
    Overview,
    Channels,
    Members,
    Roles,
    Audit,
    Reports,
    Expressions,
    Invites,
    Account,
    Backups,
    Discord,
}

#[derive(Default)]
pub(super) struct ServerUi {
    welcome_open: bool,
    system_open: bool,
    search_open: bool,
    search_focus: bool,
    search_query: String,
    search_applied: String,
    search_filters: crate::search_filters::SearchFilters,
    search_period: u8,
    search_applied_filters: crate::search_filters::SearchFilters,
    search_messages: Vec<(Target, Message)>,
    search_remaining: Vec<(Target, Option<u64>)>,
    search_errors: Vec<String>,
    pub(super) search: String,
    section: Section,
    channel: Option<Channel>,
    permissions: Vec<ChannelPermission>,
    permission_user: String,
    category_name: String,
    category_edits: HashMap<String, (String, i64)>,
    role_edits: HashMap<String, String>,
    member_confirm: Option<(String, String, String)>,
    delete_channel: bool,
    invite_label: String,
    invite_uses: u32,
    invite_seconds: u64,
    current_password: String,
    new_password: String,
    restore_id: String,
    restore_confirm: bool,
    stop_confirm: bool,
    presentation: Option<(ServerPresentation, ServerPresentation)>,
    discord_token: zeroize::Zeroizing<String>,
    discord_host_checked: bool,
    discord_message_id: String,
    discord: Option<(
        relay_protocol::discord::DiscordBridgeSettings,
        relay_protocol::discord::DiscordBridgeSettings,
    )>,
    custom_role_name: String,
    custom_role_edits: HashMap<String, CommunityRole>,
    member_role_edits: HashMap<String, Vec<String>>,
    role_delete: Option<String>,
    pub(super) presence_draft: Option<UserPresence>,
    friend_search: String,
    friend_tab: u8,
    friend_request_feedback: Option<(bool, String)>,
    pub(super) friend_all_servers: bool,
    report_target: Option<User>,
    report_reason: String,
    report_evidence: String,
    share_report_evidence: bool,
    conversation_create: bool,
    conversation_kind: String,
    conversation_name: String,
    conversation_parent: Option<String>,
    conversation_members: Vec<String>,
    conversation_member_search: String,
    conversation_edit: Option<String>,
    conversation_edit_name: String,
    conversation_edit_members: Vec<String>,
    event_draft: Option<ScheduledEvent>,
    event_start: String,
    event_end: String,
    inbox_messages: Vec<(Target, Message)>,
    inbox_tab: u8,
    inbox_filter: String,
    inbox_remaining: Vec<(Target, Option<u64>)>,
    inbox_errors: Vec<String>,
    inbox_loaded: bool,
    poll_draft: Option<PollContent>,
    image_picker: Option<ImagePicker>,
    asset_cache: HashMap<String, (egui::TextureHandle, Instant)>,
    asset_pending: HashSet<String>,
    asset_failed: HashSet<String>,
    expression_name: String,
    expression_picker: bool,
    expression_stickers: bool,
    pub(super) expression_query: String,
    pub(super) expression_category: Option<usize>,
    pub(super) expression_focus: bool,
    pub(super) expression_reaction: Option<(CommandOrigin, Target, Message)>,
}

impl ServerUi {
    pub(super) fn modal_open(&self) -> bool {
        self.welcome_open
            || self.system_open
            || self.search_open
            || self.report_target.is_some()
            || self.conversation_create
            || self.conversation_edit.is_some()
            || self.poll_draft.is_some()
            || self.image_picker.is_some()
    }
    pub(super) fn navigation(&mut self, ui: &mut egui::Ui, bootstrap: &Bootstrap, query: &str) {
        crate::settings_ui::nav_group(ui, &bootstrap.server_name);
        let admin = bootstrap.has_permission(PERMISSION_ADMIN);
        let staff = bootstrap.has_permission(PERMISSION_MEMBERS);
        for (section, label, allowed) in [
            (Section::Overview, "Overview", true),
            (
                Section::Channels,
                "Channels & Permissions",
                bootstrap.has_permission(PERMISSION_CHANNELS),
            ),
            (
                Section::Roles,
                "Roles",
                bootstrap.has_permission(PERMISSION_ROLES),
            ),
            (Section::Members, "Members & Moderation", staff),
            (Section::Audit, "Audit Log", staff),
            (Section::Reports, "Reports", staff),
            (
                Section::Expressions,
                "Emoji & Stickers",
                bootstrap.has_permission(PERMISSION_CHANNELS),
            ),
            (
                Section::Invites,
                "Invites",
                bootstrap.has_permission(PERMISSION_INVITES),
            ),
            (Section::Backups, "Backups & Restore", admin),
            (
                Section::Discord,
                "Discord",
                admin
                    && bootstrap
                        .capabilities
                        .iter()
                        .any(|c| c == "discord-command-v1"),
            ),
        ] {
            if allowed
                && (query.is_empty() || label.to_lowercase().contains(query))
                && crate::settings_ui::nav_item(
                    ui,
                    ("server-settings", label),
                    label,
                    self.section == section,
                )
                .clicked()
            {
                self.section = section;
            }
        }
    }
    pub(super) fn select_section(&mut self, section: &str) {
        self.section = match section {
            "channels" => Section::Channels,
            "members" => Section::Members,
            "roles" => Section::Roles,
            "audit" => Section::Audit,
            "reports" => Section::Reports,
            "expressions" | "emoji" => Section::Expressions,
            "invites" => Section::Invites,
            "account" => Section::Account,
            "backups" => Section::Backups,
            "discord" => Section::Discord,
            _ => Section::Overview,
        };
    }
}

impl RelayApp {
    pub(super) fn close_community_overlays(&mut self) -> bool {
        let open = self.admin_ui.welcome_open
            || self.admin_ui.system_open
            || self.admin_ui.search_open
            || self.admin_ui.conversation_create
            || self.admin_ui.conversation_edit.is_some()
            || self.admin_ui.expression_picker
            || self.admin_ui.report_target.is_some()
            || self.admin_ui.poll_draft.is_some()
            || self.admin_ui.image_picker.is_some();
        self.admin_ui.conversation_create = false;
        self.admin_ui.conversation_edit = None;
        self.admin_ui.expression_picker = false;
        self.admin_ui.expression_reaction = None;
        self.admin_ui.report_target = None;
        self.admin_ui.report_evidence.clear();
        self.admin_ui.share_report_evidence = false;
        self.admin_ui.poll_draft = None;
        self.admin_ui.image_picker = None;
        self.admin_ui.welcome_open = false;
        self.admin_ui.system_open = false;
        self.admin_ui.search_open = false;
        open
    }
    // Input-disabled captures render real widgets, with explicit synthetic display fixtures.
    // File data is explicit and bounded; this never joins voice or opens devices.
    pub(super) fn prepare_capture_panel(&mut self, panel: &str) {
        if self.capture_path.is_none() {
            return;
        }
        let Some(b) = self.bootstrap.clone() else {
            return;
        };
        self.profile_capture_panel(panel);
        if self.encryption_capture_panel(panel) {
            self.admin_ui.welcome_open = false;
        }
        match panel {
            "automatic-latency" => {
                self.prefs
                    .audio
                    .set_latency_mode(crate::audio_settings::LatencyMode::Adaptive);
                self.settings_open = true;
                self.settings_tab = SettingsTab::VoiceVideo;
                self.voice_video_tab = VoiceVideoTab::Voice;
            }
            "welcome" => self.admin_ui.welcome_open = true,
            "system-notices" => self.admin_ui.system_open = true,
            "community-search" => {
                self.admin_ui.search_open = true;
                self.admin_ui.search_query = "game".into();
                self.load_community_search(true);
            }
            "emoji" => self.open_expression_picker(),
            "emoji-completion" => {
                self.draft.clear();
                self.insert_composer_text(&self.capture_ctx.clone(), ":game_");
            }
            "mention-completion" => {
                self.draft.clear();
                self.insert_composer_text(&self.capture_ctx.clone(), "@mi");
            }
            "mention-picker" => {
                let editor_id = egui::Id::new(("composer-editor", self.current.clone()));
                self.capture_ctx.data_mut(|data| {
                    data.insert_temp(egui::Id::new("manual-mention-editor"), editor_id);
                    data.insert_temp(editor_id.with("mention-menu-focus"), true);
                });
            }
            "stickers" => self.open_sticker_picker(),
            "message-toolbar" => {
                self.message_menu = self.messages.last().map(|message| message.id.clone());
            }
            "delivery-pending" | "delivery-failed" => {
                if let Some(target) = self.current.clone() {
                    let emoji = b.assets.iter().find(|asset| asset.kind == "emoji");
                    let custom = emoji.map_or_else(String::new, |asset| {
                        format!(" <emoji:{}:{}>", asset.name, asset.id)
                    });
                    // Display-only fixture in input-disabled captures. It never sends a message.
                    let message = Message {
                        id: "local:owned-delivery-preview".into(),
                        client_id: Some("owned-delivery-preview".into()),
                        author: b.user.clone(),
                        content: format!(
                            "**Ready for game night** 😭{custom}\n__Underlined__ and *italic* · ||private surprise||\n> This quote remains formatted while the message is sending.\nNormal text after the quote."
                        ),
                        created_at: chrono::Utc::now().to_rfc3339(),
                        ..Default::default()
                    };
                    self.outgoing_messages.push((
                        target,
                        message,
                        panel == "delivery-failed",
                        false,
                    ));
                    self.scroll_to_end = true;
                }
            }
            "poll" => {
                self.admin_ui.poll_draft = Some(PollContent {
                    question: "What should we play on game night?".into(),
                    options: vec!["Co-op adventure".into(), "Community tournament".into()],
                    multiple: false,
                })
            }
            "group" => self.open_conversation_create("group", None),
            "conversation" => {
                if let Some(Target::Channel(id)) = self.current.clone() {
                    self.open_conversation_settings(&id);
                }
            }
            "voice-text" => {
                if let Some(Target::Channel(id)) = self.current.clone() {
                    if let Some(chat) = b
                        .channels
                        .iter()
                        .find(|c| c.voice_parent_id.as_deref() == Some(&id))
                    {
                        self.select(Target::Channel(chat.id.clone()));
                    }
                }
            }
            "profile-card" => {
                if let Some(user) = b.members.iter().find(|u| u.id != b.user.id).cloned() {
                    self.open_profile(user);
                }
            }
            "report" => {
                if let Some(user) = b.members.iter().find(|u| u.id != b.user.id).cloned() {
                    self.admin_ui.report_target = Some(user);
                    self.admin_ui.report_reason =
                        "Owned preview report; nothing is submitted.".into();
                }
            }
            "server-menu" => self.server_menu = true,
            "sound-check" | "voice-relay-hint" => {
                if panel == "voice-relay-hint" {
                    self.capture_ctx.data_mut(|d| {
                        d.insert_temp(egui::Id::new("capture-voice-relay-hint"), true)
                    });
                }
                if let Some(channel) = b.channels.iter().find(|channel| channel.kind == "voice") {
                    self.select(Target::Channel(channel.id.clone()));
                    // UI state only: no transport, voice join or device startup.
                    self.voice_channel = Some(channel.id.clone());
                    self.muted = true;
                    if let Some(first) = b
                        .voice_members
                        .iter()
                        .find(|m| m.channel_id == channel.id && m.user.id != b.user.id)
                    {
                        self.capture_heard.insert(first.user.id.clone());
                    }
                }
            }
            "call-active" => {
                if let Some(channel) = b.channels.iter().find(|channel| channel.kind == "voice") {
                    self.select(Target::Channel(channel.id.clone()));
                    // UI state only: no transport, voice join or device startup.
                    self.voice_channel = Some(channel.id.clone());
                    self.muted = true;
                    self.deafened = true;
                }
            }
            "quick-switch" => self.quick_switch = true,
            "create-channel" => self.create_channel_open = true,
            "connect-community" => self.add_community_open = true,
            "transfers" => self.transfers_open = true,
            "gif-library" => self.open_gif_picker(),
            "pins" => {
                self.pins.open = true;
                self.request_pins();
            }
            "invite" => self.open_invite("OWNED-PREVIEW-CODE"),
            "share-connect" => {
                self.capture_local_host = true;
                self.open_friend_sharing();
            }
            "sharing-status-off" | "sharing-status-on" => {
                // Display-only: the line under the server name.
                self.capture_local_host = true;
                if panel == "sharing-status-on" {
                    self.hosting_setup.capture_sharing = Some(host_setup_ui::Sharing::Iroh);
                }
            }
            "share-firewall" => {
                // Display-only: step 3 with the firewall card. Nothing is checked or changed.
                self.capture_local_host = true;
                self.open_friend_sharing();
                self.hosting_setup.capture_sharing = Some(host_setup_ui::Sharing::Iroh);
                self.hosting_setup.existing_access = true;
                self.hosting_setup.firewall = Some(crate::firewall_check::Firewall::Blocked);
            }
            "share-change" | "share-change-iroh" => {
                // Display-only: step 3 as it looks for each way of sharing, with
                // the choices open. Nothing is started and nothing is switched.
                self.capture_local_host = true;
                self.open_friend_sharing();
                self.hosting_setup.capture_sharing = Some(if panel == "share-change" {
                    host_setup_ui::Sharing::Tailscale
                } else {
                    host_setup_ui::Sharing::Iroh
                });
                self.hosting_setup.existing_access = true;
                self.hosting_setup.change_open = true;
            }
            "delete-message" => self.delete_confirm = self.messages.first().cloned(),
            "own-profile" => self.open_profile(b.user.clone()),
            "share-picker" | "camera-picker" => {
                // Capture only the dialog with an empty inventory, never personal sources.
                self.inventory_loaded = true;
                self.share_picker = Some(panel == "camera-picker");
            }
            "friends-home" | "inbox-home" | "inbox-mentions-home" | "events-home" => {
                self.selected_nav = panel == "events-home";
                self.admin_ui.inbox_tab = u8::from(panel == "inbox-mentions-home");
                self.home_page = Some(match panel {
                    "friends-home" => SettingsTab::Friends,
                    "inbox-home" | "inbox-mentions-home" => SettingsTab::Mentions,
                    _ => SettingsTab::Events,
                });
            }
            "crop" | "stream" | "stream-focus" | "stream-fullscreen" => {
                let Some(path) = std::env::var_os("RELAY_CAPTURE_IMAGE").map(PathBuf::from) else {
                    return;
                };
                let Ok((bytes, image)) = read_crop_image(&path) else {
                    return;
                };
                if panel == "crop" {
                    let Some(scope) = self.conversation_scope() else {
                        return;
                    };
                    let (tx, loaded) = mpsc::sync_channel(1);
                    let _ = tx.send(Ok((bytes, image)));
                    self.admin_ui.image_picker = Some(ImagePicker {
                        scope,
                        kind: "server_banner".into(),
                        name: "community".into(),
                        loaded,
                        bytes: vec![],
                        image: None,
                        texture: None,
                        zoom: 1.4,
                        x: 0.5,
                        y: 0.5,
                    });
                } else if let Some(member) = b.voice_members.first() {
                    let key = format!("{}:screen", member.user.id);
                    let texture = self.capture_ctx.load_texture(
                        &key,
                        egui::ColorImage::from_rgba_unmultiplied(
                            [image.width() as usize, image.height() as usize],
                            image.as_raw(),
                        ),
                        egui::TextureOptions::LINEAR,
                    );
                    self.video.insert(
                        key.clone(),
                        VideoTile {
                            metadata: MediaFrame {
                                user: member.user.clone(),
                                kind: "screen".into(),
                                url: "owned-capture://synthetic".into(),
                                frame_id: "generated-preview".into(),
                                width: image.width(),
                                height: image.height(),
                            },
                            texture,
                            updated: Instant::now(),
                        },
                    );
                    if panel == "stream-focus" {
                        self.focused_stream = Some(key.clone());
                    }
                    if panel == "stream-fullscreen" {
                        self.stream_preview = Some(key);
                        self.stream_fullscreen = true;
                    }
                }
            }
            _ => {}
        }
    }
    pub(super) fn open_channel_settings(&mut self, mut channel: Channel) {
        if let Some(parent) = &channel.voice_parent_id {
            if let Some(value) = self
                .bootstrap
                .as_ref()
                .and_then(|b| b.channels.iter().find(|c| &c.id == parent))
                .cloned()
            {
                channel = value;
            }
        }
        if let Some(info) = self.conversation_info(&channel.id).cloned() {
            if matches!(info.kind.as_str(), "group" | "group_voice" | "thread") {
                self.open_conversation_settings(
                    info.parent_id
                        .as_deref()
                        .filter(|_| info.kind == "group_voice")
                        .unwrap_or(&channel.id),
                );
                return;
            }
        }
        self.admin_ui.section = Section::Channels;
        self.admin_ui.permissions.clear();
        let id = channel.id.clone();
        self.admin_ui.channel = Some(channel);
        self.settings_tab = SettingsTab::Server;
        self.settings_open = true;
        self.refresh_admin_data(
            &format!("permissions:{id}"),
            &format!("/api/channels/{id}/permissions"),
        );
    }
    pub(super) fn admin_request(
        &mut self,
        method: AdminMethod,
        path: String,
        body: Value,
        key: String,
    ) {
        let Some(origin) = self.command_origin() else {
            self.error = Some("Connect to a community before changing its settings.".into());
            return;
        };
        let request_id = self.next_request();
        if let Err(error) = self.network.send(Command::Admin {
            origin,
            method,
            path,
            body,
            request_id,
        }) {
            self.error = Some(error);
        } else {
            self.admin_requests.insert(request_id, key);
        }
    }
    pub(super) fn ensure_admin_data(&mut self, key: &str, path: &str) {
        if !self.admin_data.contains_key(key)
            && !self.admin_requests.values().any(|pending| pending == key)
        {
            self.admin_request(AdminMethod::Get, path.into(), Value::Null, key.into());
        }
    }
    fn refresh_admin_data(&mut self, key: &str, path: &str) {
        self.admin_data.remove(key);
        self.ensure_admin_data(key, path);
    }
    pub(super) fn handle_admin_result(&mut self, key: String, data: Value) {
        if key == "action:people-request" {
            match serde_json::from_value::<SocialState>(data) {
                Ok(social) => {
                    if let Some(bootstrap) = self.bootstrap.as_mut().map(Arc::make_mut) {
                        bootstrap.social = social;
                        self.admin_ui.friend_request_feedback = Some((
                            true,
                            "Friend request saved. Open Pending or All to continue.".into(),
                        ));
                    }
                }
                Err(_) => self.people_request_failed(
                    "Relay could not read the response. Refresh Friends before trying again."
                        .into(),
                ),
            }
            return;
        }
        if key.starts_with("account-link:") {
            self.account_link_result(&key, data);
            return;
        }
        if key.starts_with("authenticator:") {
            self.authenticator_result(&key, data);
            return;
        }
        if key.starts_with("private-call:") {
            self.private_call_result(&key, data);
            return;
        }
        if key == "discord-host" || key == "action:discord-host" {
            self.admin_data.insert("discord-host".into(), data.clone());
            if key.starts_with("action:") {
                self.refresh_admin_data("discord", "/api/discord");
            }
        }
        if key == "inbox-read-states" {
            if let (Some(bootstrap), Ok(reads)) = (
                self.bootstrap.as_mut().map(Arc::make_mut),
                serde_json::from_value::<Vec<ReadState>>(data.clone()),
            ) {
                bootstrap.read_states = reads;
            }
        }
        if key == "social" || key == "action:social" {
            if let (Some(bootstrap), Ok(social)) = (
                self.bootstrap.as_mut().map(Arc::make_mut),
                serde_json::from_value::<SocialState>(data.clone()),
            ) {
                bootstrap.social = social;
            }
        }
        if key == "action:presence" {
            if let (Some(bootstrap), Ok(presence)) = (
                self.bootstrap.as_mut().map(Arc::make_mut),
                serde_json::from_value::<UserPresence>(data.clone()),
            ) {
                bootstrap.presence.retain(|p| p.user_id != presence.user_id);
                bootstrap.presence.push(presence);
                self.admin_ui.presence_draft = None;
            }
        }
        if key == "roles" {
            if let (Some(bootstrap), Ok(roles)) = (
                self.bootstrap.as_mut().map(Arc::make_mut),
                serde_json::from_value::<Vec<CommunityRole>>(data.clone()),
            ) {
                bootstrap.roles = roles;
            }
        }
        if key == "action:roles" {
            if let (Some(bootstrap), Ok(assignment)) = (
                self.bootstrap.as_mut().map(Arc::make_mut),
                serde_json::from_value::<MemberRoles>(data.clone()),
            ) {
                bootstrap
                    .member_roles
                    .retain(|entry| entry.user_id != assignment.user_id);
                bootstrap.member_roles.push(assignment);
            }
        }
        if key == "discord" || key == "action:discord" || key == "action:discord-status" {
            if let Ok(value) =
                serde_json::from_value::<relay_protocol::discord::DiscordBridgeView>(data.clone())
            {
                if key.starts_with("action:") || self.admin_ui.discord.is_none() {
                    self.admin_ui.discord = Some((value.settings.clone(), value.settings));
                }
                self.admin_data.insert("discord".into(), data.clone());
            }
        }
        if key == "presentation" || key == "action:presentation" {
            if let Ok(value) = serde_json::from_value::<ServerPresentation>(data.clone()) {
                if key.starts_with("action:") || self.admin_ui.presentation.is_none() {
                    self.admin_ui.presentation = Some((value.clone(), value));
                }
            }
        }
        if let Some(id) = key.strip_prefix("permissions:") {
            if self
                .admin_ui
                .channel
                .as_ref()
                .is_some_and(|channel| channel.id == id)
            {
                if let Ok(permissions) =
                    serde_json::from_value::<SetChannelPermissions>(data.clone())
                {
                    self.admin_ui.permissions = permissions.entries;
                }
            }
        }
        if let Some(action) = key.strip_prefix("action:") {
            let message = match action {
                "social" => "Friends updated",
                "presence" => "Status updated",
                _ => "Community settings saved",
            };
            self.notice = Some((message.into(), Instant::now()));
            match action {
                "reorder" => self.refresh_admin_data("categories", "/api/categories"),
                "assets" => {
                    self.admin_ui.image_picker = None;
                    self.refresh_admin_data("assets", "/api/assets");
                }
                "conversation:create" => {
                    self.admin_ui.conversation_create = false;
                    if let Some(id) = data.get("channel_id").and_then(Value::as_str) {
                        self.select(Target::Channel(id.into()));
                    }
                }
                "conversation:edit" => self.admin_ui.conversation_edit = None,
                "stage" => self.admin_data.retain(|key, _| !key.starts_with("stage:")),
                "event" => self.admin_ui.event_draft = None,
                "report" => {
                    self.admin_ui.report_target = None;
                    self.admin_ui.report_reason.clear();
                    self.admin_ui.report_evidence.clear();
                    self.admin_ui.share_report_evidence = false;
                }
                "reports" => self.refresh_admin_data("reports", "/api/reports"),
                "members" => self.refresh_admin_data("members", "/api/members"),
                "roles" => {
                    self.admin_ui.custom_role_edits.clear();
                    self.admin_ui.member_role_edits.clear();
                    self.admin_ui.role_delete = None;
                    self.refresh_admin_data("roles", "/api/roles");
                    if self
                        .bootstrap
                        .as_ref()
                        .is_some_and(|b| b.has_permission(PERMISSION_MEMBERS))
                    {
                        self.refresh_admin_data("audit", "/api/audit-log");
                    }
                }
                "invites" => {
                    if let Some(code) = data.get("code").and_then(Value::as_str) {
                        self.open_invite(code);
                    }
                    self.refresh_admin_data("invites", "/api/invites");
                }
                "categories" => {
                    self.admin_ui.category_edits.clear();
                    self.refresh_admin_data("categories", "/api/categories");
                }
                "backups" => self.refresh_admin_data("backups", "/api/admin/backups"),
                "sessions" => self.refresh_admin_data("sessions", "/api/sessions"),
                "password" => {
                    self.admin_ui.current_password.clear();
                    self.admin_ui.new_password.clear();
                    self.stop_media();
                    self.send(Command::Logout);
                    self.settings_open = false;
                }
                "restore" => {
                    self.stop_media();
                    self.send(Command::Logout);
                    self.settings_open = false;
                }
                "channels" => {
                    self.admin_ui.channel = None;
                    self.admin_ui.delete_channel = false;
                }
                _ => {}
            }
        } else {
            self.admin_data.insert(key, data);
        }
    }

    pub(super) fn server_settings_ui(&mut self, ui: &mut egui::Ui) {
        let Some(bootstrap) = self.bootstrap.clone() else {
            return;
        };
        let admin = bootstrap.has_permission(PERMISSION_ADMIN);
        let moderator = bootstrap.has_permission(PERMISSION_MEMBERS);
        crate::settings_ui::page_heading(ui, "Server Settings", &bootstrap.server_name);
        match self.admin_ui.section {
            Section::Overview => {
                self.server_presentation_ui(ui, &bootstrap);
                settings_heading(
                    ui,
                    &bootstrap.server_name,
                    "A community you can host yourself, or join through a friend.",
                );
                ui.label(
                    RichText::new(&self.login.url)
                        .size(font_size(13.0))
                        .color(c_muted()),
                );
                ui.horizontal_wrapped(|ui| {
                    if ui.add(secondary("Copy server address")).clicked() {
                        ui.output_mut(|output| output.copied_text = self.login.url.clone());
                    }
                    if ui.add(secondary("Connect another community")).clicked() {
                        self.open_community_dialog();
                    }
                    if ui.add(secondary("Sign out of this community")).clicked() {
                        self.save_preferences();
                        self.stop_media();
                        self.send(Command::Logout);
                        self.settings_open = false;
                    }
                });
                ui.label(
                    RichText::new("Other communities stay signed in while Relay is open.")
                        .size(font_size(TEXT_BODY))
                        .color(c_muted()),
                );
                if admin {
                    self.ensure_admin_data("metadata", "/api/admin/metadata");
                    ui.collapsing("Hosting status",|ui| {
                        if let Some(metadata)=self.admin_data.get("metadata").cloned() {
                            ui.label("Server is reachable");
                            if metadata.get("managed_instance").and_then(Value::as_bool)==Some(true) {
                                if ui.add(secondary("Stop this server")).clicked(){self.admin_ui.stop_confirm=true;}
                                if self.admin_ui.stop_confirm {
                                    ui.label(RichText::new("Everyone will disconnect until the host starts the server again.").color(c_bad()));
                                    ui.horizontal(|ui| {
                                        if ui.button("Confirm stop").clicked(){self.stop_media();if crate::local_host::is_local_url(&self.login.url){self.send(Command::StopLocal);}else{self.admin_request(AdminMethod::Post,"/api/admin/shutdown".into(),json!({"instance_id":metadata["instance_id"],"confirm_server_id":metadata["server_id"]}),"action:stop".into());}self.admin_ui.stop_confirm=false;}
                                        if ui.small_button("Cancel").clicked(){self.admin_ui.stop_confirm=false;}
                                    });
                                }
                            }
                        } else {ui.label(RichText::new("Checking server…").color(c_muted()));}
                        if ui.small_button("Refresh hosting status").clicked(){self.refresh_admin_data("metadata","/api/admin/metadata");}
                    });
                    egui::CollapsingHeader::new("Advanced")
                        .id_source(("server-advanced", &self.login.url, &bootstrap.server_id))
                        .default_open(false)
                        .show(ui, |ui| {
                            let server_id = self
                                .admin_data
                                .get("metadata")
                                .and_then(|metadata| metadata.get("server_id"))
                                .and_then(Value::as_str);
                            crate::settings_ui::nav_group(ui, "SERVER ID");
                            crate::settings_ui::description(ui, server_id.unwrap_or("Unavailable"));
                            if let Some(server_id) = server_id {
                                if ui.small_button("Copy server ID").clicked() {
                                    ui.output_mut(|output| {
                                        output.copied_text = server_id.to_owned()
                                    });
                                }
                            }
                        });
                }
                ui.add_space(14.0);
                ui.label(
                    RichText::new(format!(
                        "{} · Free and self-hosted",
                        crate::build_metadata::current().label()
                    ))
                    .size(font_size(TEXT_CAPTION))
                    .color(c_muted()),
                );
            }
            Section::Channels if bootstrap.has_permission(PERMISSION_CHANNELS) => {
                self.channel_settings_ui(ui, &bootstrap)
            }
            Section::Roles if bootstrap.has_permission(PERMISSION_ROLES) => {
                self.custom_roles_ui(ui, &bootstrap)
            }
            Section::Audit if moderator => self.audit_log_ui(ui, &bootstrap),
            Section::Reports if moderator => self.reports_ui(ui),
            Section::Expressions if bootstrap.has_permission(PERMISSION_CHANNELS) => {
                self.expressions_settings(ui)
            }
            Section::Members if moderator => self.member_settings_ui(ui, &bootstrap),
            Section::Invites if bootstrap.has_permission(PERMISSION_INVITES) => {
                self.invite_settings_ui(ui)
            }
            Section::Account => self.account_settings_ui(ui),
            Section::Backups if admin => self.backup_settings_ui(ui, &bootstrap),
            Section::Discord if admin => self.discord_settings_ui(ui, &bootstrap),
            _ => {
                self.admin_ui.section = Section::Overview;
            }
        }
        if self
            .admin_requests
            .values()
            .any(|key| key.starts_with("action:"))
        {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(
                    RichText::new("Saving…")
                        .size(font_size(TEXT_CAPTION))
                        .color(c_muted()),
                );
            });
        }
    }

    fn server_presentation_ui(&mut self, ui: &mut egui::Ui, bootstrap: &Bootstrap) {
        use crate::settings_ui as controls;
        if !bootstrap
            .capabilities
            .iter()
            .any(|c| c == "server-settings-v1")
        {
            return;
        }
        self.ensure_admin_data("presentation", "/api/server-settings");
        let admin = bootstrap.has_permission(PERMISSION_ADMIN);
        let pending = self
            .admin_requests
            .values()
            .any(|key| key == "action:presentation");
        self.presentation_images_ui(ui, false);
        let Some((baseline, draft)) = self.admin_ui.presentation.as_mut() else {
            ui.spinner();
            return;
        };
        let mut save = false;
        egui::Frame::none()
            .fill(c_card())
            .rounding(8.0)
            .inner_margin(Margin::same(18.0))
            .show(ui, |ui| {
                let (rect, _) =
                    ui.allocate_exact_size(Vec2::new(ui.available_width(), 72.0), Sense::hover());
                ui.painter()
                    .rect_filled(rect, 8.0, profile_ui::profile_color(draft.accent));
                ui.painter().text(
                    rect.left_center() + Vec2::new(18.0, 0.0),
                    Align2::LEFT_CENTER,
                    "SERVER PROFILE",
                    FontId::proportional(14.0),
                    Color32::WHITE,
                );
                ui.add_space(12.0);
                ui.label(RichText::new(&draft.name).size(font_size(23.0)).strong());
                if !draft.description.is_empty() {
                    ui.add(egui::Label::new(&draft.description).wrap(true));
                }
            });
        ui.add_space(20.0);
        ui.add_enabled_ui(admin && !pending, |ui| {
            controls::nav_group(ui, "SERVER NAME");
            ui.add_sized(
                [ui.available_width(), 38.0],
                egui::TextEdit::singleline(&mut draft.name).char_limit(100),
            );
            controls::nav_group(ui, "DESCRIPTION");
            ui.add_sized(
                [ui.available_width(), 84.0],
                egui::TextEdit::multiline(&mut draft.description)
                    .char_limit(300)
                    .hint_text("What is your server about?"),
            );
            controls::description(
                ui,
                &format!("{}/300 characters", draft.description.chars().count()),
            );
            profile_ui::profile_color_editor(ui, "Server color", &mut draft.accent);
            if bootstrap.capabilities.iter().any(|c| c == "welcome-v1") {
                controls::nav_group(ui, "WELCOME & SYSTEM MESSAGES");
                for (key, label, selected) in [
                    ("welcome-channel", "Welcome channel", &mut draft.welcome_channel_id),
                    ("system-channel", "System messages channel", &mut draft.system_channel_id),
                ] {
                    let name = selected.as_ref().and_then(|id| bootstrap.channels.iter().find(|c| c.id == *id)).map(|c| c.name.as_str()).unwrap_or("None");
                    egui::ComboBox::from_id_source(key).selected_text(format!("{label}: {name}")).show_ui(ui, |ui| {
                        ui.selectable_value(selected, Some(String::new()), "None");
                        for channel in bootstrap.channels.iter().filter(|c| c.kind == "text" && c.visibility == "public" && c.voice_parent_id.is_none() && !bootstrap.conversations.iter().any(|t| t.channel_id == c.id && matches!(t.kind.as_str(), "group" | "thread"))) {
                            ui.selectable_value(selected, Some(channel.id.clone()), format!("#{}", channel.name));
                        }
                    });
                }
                controls::description(ui, "Welcome gives new members a starting place. System messages show community joins and departures.");
            }
        });
        if !admin {
            controls::description(ui, "Only administrators can change this server profile.");
        }
        if draft != baseline {
            ui.add_space(16.0);
            egui::Frame::none()
                .fill(c_rail())
                .rounding(6.0)
                .inner_margin(Margin::same(12.0))
                .show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.label("Careful — you have unsaved changes!");
                        if ui.add_enabled(!pending, secondary("Reset")).clicked() {
                            *draft = baseline.clone();
                        }
                        if ui
                            .add_enabled(
                                admin && !pending && draft.valid(),
                                primary("Save Changes"),
                            )
                            .clicked()
                        {
                            save = true;
                        }
                    });
                });
        }
        let body = save.then(|| json!(draft));
        if let Some(body) = body {
            self.admin_request(
                AdminMethod::Patch,
                "/api/server-settings".into(),
                body,
                "action:presentation".into(),
            );
        }
        controls::divider(ui);
    }

    fn channel_settings_ui(&mut self, ui: &mut egui::Ui, bootstrap: &Bootstrap) {
        self.ensure_admin_data("categories", "/api/categories");
        settings_heading(
            ui,
            "Channels",
            "Choose a channel to edit its name, position, category and access.",
        );
        let old = self
            .admin_ui
            .channel
            .as_ref()
            .map(|channel| channel.id.clone());
        let channels: Vec<_> = bootstrap
            .channels
            .iter()
            .filter(|c| self.community_channel(c))
            .cloned()
            .collect();
        if self.admin_ui.channel.is_none() {
            self.admin_ui.channel = channels.first().cloned();
        }
        manage_channel_combo(ui, &mut self.admin_ui.channel, &channels);
        if self.admin_ui.channel.as_ref().map(|channel| &channel.id) != old.as_ref() {
            self.admin_ui.permissions.clear();
            self.admin_ui.delete_channel = false;
            if let Some(channel) = &self.admin_ui.channel {
                let id = channel.id.clone();
                self.refresh_admin_data(
                    &format!("permissions:{id}"),
                    &format!("/api/channels/{id}/permissions"),
                );
            }
        }
        if let Some(mut channel) = self.admin_ui.channel.clone() {
            field(ui, "NAME", &mut channel.name, false);
            field(ui, "TOPIC", &mut channel.topic, false);
            ui.horizontal(|ui| {
                ui.label("Position");
                ui.add(egui::DragValue::new(&mut channel.position).clamp_range(-1000..=1000));
                ui.label(
                    RichText::new("Smaller numbers appear first.")
                        .size(font_size(TEXT_CAPTION))
                        .color(c_muted()),
                );
            });
            egui::ComboBox::from_id_source("channel-category")
                .selected_text(category_name(
                    &self.admin_data,
                    channel.category_id.as_deref(),
                ))
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut channel.category_id, None, "No category");
                    if let Some(categories) =
                        self.admin_data.get("categories").and_then(Value::as_array)
                    {
                        for category in categories {
                            if let (Some(id), Some(name)) =
                                (category["id"].as_str(), category["name"].as_str())
                            {
                                ui.selectable_value(
                                    &mut channel.category_id,
                                    Some(id.into()),
                                    name,
                                );
                            }
                        }
                    }
                });
            ui.horizontal(|ui| {
                ui.radio_value(&mut channel.visibility, "public".into(), "Public channel");
                ui.radio_value(&mut channel.visibility, "private".into(), "Private channel");
            });
            if channel.kind == "text" && self.has_chat_capability("slowmode-v1") {
                ui.add_space(8.0);
                ui.label(RichText::new("Slowmode").strong());
                ui.add(egui::Label::new(RichText::new("Limit how often each member can send here. Moderators and administrators bypass channel slowmode; everyone still has spam protection.").color(c_muted()).size(font_size(12.0))).wrap(true));
                ui.horizontal_wrapped(|ui| {
                    ui.add(
                        egui::DragValue::new(&mut channel.slowmode_seconds)
                            .clamp_range(0..=21_600)
                            .suffix(" seconds"),
                    );
                    for (seconds, name) in
                        [(0, "Off"), (5, "5s"), (30, "30s"), (60, "1m"), (300, "5m")]
                    {
                        if ui.small_button(name).clicked() {
                            channel.slowmode_seconds = seconds;
                        }
                    }
                });
            }
            let pending = self
                .admin_requests
                .values()
                .any(|key| key == "action:channels");
            let mut reset = false;
            let save = ui
                .horizontal(|ui| {
                    let save = ui.add_enabled(!pending, primary("Save channel")).clicked();
                    reset = ui.add_enabled(!pending, secondary("Reset")).clicked();
                    save
                })
                .inner;
            if save {
                let mut payload = json!({"name":channel.name,"topic":channel.topic,"position":channel.position,"category_id":channel.category_id.clone().unwrap_or_default(),"visibility":channel.visibility});
                if self.has_chat_capability("slowmode-v1") {
                    payload["slowmode_seconds"] = json!(channel.slowmode_seconds);
                }
                self.admin_request(
                    AdminMethod::Patch,
                    format!("/api/channels/{}", channel.id),
                    payload,
                    "action:channels".into(),
                );
            }
            if reset {
                if let Some(original) = bootstrap.channels.iter().find(|c| c.id == channel.id) {
                    channel = original.clone();
                }
            }
            self.admin_ui.channel = Some(channel.clone());
            ui.collapsing("Channel permissions",|ui| {
                ui.label(RichText::new("Public channels allow access by default. Private channels require an explicit role or person; administrators always have access.").size(font_size(TEXT_BODY)).color(c_muted()));
                if self.admin_requests.values().any(|key|key==&format!("permissions:{}",channel.id)){ui.label("Loading permissions…");return;}
                for role in ["member","moderator"] {
                    if !self.admin_ui.permissions.iter().any(|entry|entry.subject_kind=="role"&&entry.subject_id==role)&&ui.small_button(format!("Add {role} permissions")).clicked(){self.admin_ui.permissions.push(ChannelPermission{subject_kind:"role".into(),subject_id:role.into(),read:false,send:false,join:false,stream:false});}
                }
                let mut remove=None;
                for (index,entry) in self.admin_ui.permissions.iter_mut().enumerate(){
                    ui.horizontal_wrapped(|ui| {
                        let label=if entry.subject_kind=="role" {bootstrap.roles.iter().find(|role|role.id==entry.subject_id).map(|role|role.name.clone()).unwrap_or_else(||entry.subject_id.clone())}else{bootstrap.members.iter().find(|member|member.id==entry.subject_id).map(|member|member.username.clone()).unwrap_or_else(||"Member".into())};
                        ui.label(RichText::new(label).strong());
                        ui.checkbox(&mut entry.read,"Read");ui.checkbox(&mut entry.send,"Send");ui.checkbox(&mut entry.join,"Join voice");ui.checkbox(&mut entry.stream,"Share");
                        if ui.small_button("×").clicked(){remove=Some(index);}
                    });
                }
                if let Some(index)=remove{self.admin_ui.permissions.remove(index);}
                ui.horizontal(|ui| {
                    egui::ComboBox::from_id_source("permission-user").selected_text(bootstrap.members.iter().find(|member|member.id==self.admin_ui.permission_user).map(|member|member.username.as_str()).unwrap_or("Choose a person")).show_ui(ui,|ui|{for member in &bootstrap.members{ui.selectable_value(&mut self.admin_ui.permission_user,member.id.clone(),&member.username);}});
                    if ui.add_enabled(!self.admin_ui.permission_user.is_empty(),secondary("Add person")).clicked(){let id=self.admin_ui.permission_user.clone();if !self.admin_ui.permissions.iter().any(|entry|entry.subject_kind=="user"&&entry.subject_id==id){self.admin_ui.permissions.push(ChannelPermission{subject_kind:"user".into(),subject_id:id,read:true,send:true,join:true,stream:true});}}
                });
                if ui.add(primary("Save permissions")).clicked(){self.admin_request(AdminMethod::Put,format!("/api/channels/{}/permissions",channel.id),json!({"entries":self.admin_ui.permissions}),"action:permissions".into());}
                for role in &bootstrap.roles {
                    if !self.admin_ui.permissions.iter().any(|entry|entry.subject_kind=="role"&&entry.subject_id==role.id) && ui.small_button(format!("Add {} permissions",role.name)).clicked() {
                        self.admin_ui.permissions.push(ChannelPermission{subject_kind:"role".into(),subject_id:role.id.clone(),read:false,send:false,join:false,stream:false});
                    }
                }
            });
            ui.collapsing("Delete channel", |ui| {
                ui.label(
                    RichText::new("Deleting a channel also removes its messages.")
                        .size(font_size(12.0))
                        .color(c_bad()),
                );
                ui.checkbox(
                    &mut self.admin_ui.delete_channel,
                    "I want to delete this channel and its messages",
                );
                if ui
                    .add_enabled(self.admin_ui.delete_channel, secondary("Delete channel"))
                    .clicked()
                {
                    self.admin_request(
                        AdminMethod::Delete,
                        format!("/api/channels/{}", channel.id),
                        Value::Null,
                        "action:channels".into(),
                    );
                }
            });
        }
        ui.collapsing("Categories", |ui| {
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.admin_ui.category_name)
                        .hint_text("New category"),
                );
                if ui
                    .add_enabled(
                        !self.admin_ui.category_name.trim().is_empty(),
                        secondary("Create"),
                    )
                    .clicked()
                {
                    self.admin_request(
                        AdminMethod::Post,
                        "/api/categories".into(),
                        json!({"name":self.admin_ui.category_name.trim()}),
                        "action:categories".into(),
                    );
                    self.admin_ui.category_name.clear();
                }
            });
            if let Some(categories) = self
                .admin_data
                .get("categories")
                .and_then(Value::as_array)
                .cloned()
            {
                for category in categories {
                    let Some(id) = category["id"].as_str() else {
                        continue;
                    };
                    let original_name = category["name"].as_str().unwrap_or("Category");
                    let original_position = category["position"].as_i64().unwrap_or_default();
                    let edit = self
                        .admin_ui
                        .category_edits
                        .entry(id.into())
                        .or_insert_with(|| (original_name.into(), original_position));
                    let mut save = None;
                    let mut remove = false;
                    ui.horizontal_wrapped(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut edit.0)
                                .desired_width(210.0)
                                .hint_text("Category name"),
                        );
                        ui.add(egui::DragValue::new(&mut edit.1).clamp_range(-1000..=1000))
                            .on_hover_text("Position · Smaller numbers appear first");
                        if ui
                            .add_enabled(
                                !edit.0.trim().is_empty()
                                    && (edit.0 != original_name || edit.1 != original_position),
                                secondary("Save"),
                            )
                            .clicked()
                        {
                            save = Some(edit.clone());
                        }
                        remove = ui
                            .small_button("Remove category")
                            .on_hover_text("Keeps its channels and moves them out of this category")
                            .clicked();
                    });
                    if let Some((name, position)) = save {
                        self.admin_request(
                            AdminMethod::Patch,
                            format!("/api/categories/{id}"),
                            json!({"name":name,"position":position}),
                            "action:categories".into(),
                        );
                    }
                    if remove {
                        self.admin_request(
                            AdminMethod::Delete,
                            format!("/api/categories/{id}"),
                            Value::Null,
                            "action:categories".into(),
                        );
                    }
                }
            }
        });
    }

    fn member_settings_ui(&mut self, ui: &mut egui::Ui, bootstrap: &Bootstrap) {
        self.ensure_admin_data("members", "/api/members");
        settings_heading(
            ui,
            "Members",
            "Manage roles and community access. Role changes sign the person out.",
        );
        if let Some(members) = self
            .admin_data
            .get("members")
            .and_then(Value::as_array)
            .cloned()
        {
            for member in members {
                let Some(id) = member["id"].as_str() else {
                    continue;
                };
                let name = member["username"].as_str().unwrap_or("Member");
                let role = member["role"].as_str().unwrap_or("member");
                let banned = member["status"].as_str() == Some("banned");
                let removed = member["status"].as_str() == Some("removed");
                ui.horizontal_wrapped(|ui| {
                    ui.add_sized(
                        [140.0, 28.0],
                        egui::Label::new(RichText::new(name).strong()).truncate(true),
                    );
                    if bootstrap.user.role == "admin" {
                        let selected = self
                            .admin_ui
                            .role_edits
                            .entry(id.into())
                            .or_insert_with(|| role.into());
                        egui::ComboBox::from_id_source(("member-role", id))
                            .selected_text(selected.as_str())
                            .show_ui(ui, |ui| {
                                for role in ["member", "moderator", "admin"] {
                                    ui.selectable_value(selected, role.into(), role);
                                }
                            });
                        let value = selected.clone();
                        if ui
                            .add_enabled(value != role, secondary("Save role"))
                            .clicked()
                        {
                            self.admin_request(
                                AdminMethod::Patch,
                                format!("/api/members/{id}"),
                                json!({"role":value}),
                                "action:members".into(),
                            );
                        }
                    } else {
                        ui.label(role);
                    }
                    if ui
                        .add_enabled(
                            id != bootstrap.user.id
                                && (bootstrap.user.role == "admin" || role == "member"),
                            secondary(if banned { "Restore access" } else { "Ban" }),
                        )
                        .clicked()
                    {
                        self.admin_ui.member_confirm = Some((
                            id.into(),
                            name.into(),
                            if banned { "active" } else { "banned" }.into(),
                        ));
                    }
                    if !banned
                        && !removed
                        && ui
                            .add_enabled(
                                id != bootstrap.user.id
                                    && (bootstrap.user.role == "admin" || role == "member"),
                                secondary("Kick"),
                            )
                            .clicked()
                    {
                        self.admin_ui.member_confirm =
                            Some((id.into(), name.into(), "removed".into()));
                    }
                    if banned {
                        ui.label(RichText::new("Banned").color(c_bad()));
                    } else if removed {
                        ui.label(
                            RichText::new("Removed · Can rejoin with an invite").color(c_muted()),
                        );
                    }
                });
                self.member_custom_roles_ui(ui, bootstrap, id, name);
            }
        } else {
            ui.label("Loading members…");
        }
        if let Some((id, name, status)) = self.admin_ui.member_confirm.clone() {
            ui.label(
                RichText::new(format!(
                    "{} {}?",
                    match status.as_str() {
                        "banned" => "Ban",
                        "removed" => "Kick",
                        _ => "Restore access for",
                    },
                    name
                ))
                .color(c_bad()),
            );
            if status == "banned" || status == "removed" {
                ui.label("This signs them out and removes them from voice.");
            }
            if status == "removed" {
                ui.label("They can rejoin using their password and a new invite.");
            }
            ui.horizontal(|ui| {
                if ui.button("Confirm").clicked() {
                    self.admin_request(
                        AdminMethod::Patch,
                        format!("/api/members/{id}"),
                        json!({"status":status}),
                        "action:members".into(),
                    );
                    self.admin_ui.member_confirm = None;
                }
                if ui.small_button("Cancel").clicked() {
                    self.admin_ui.member_confirm = None;
                }
            });
        }
        if ui.small_button("Refresh members").clicked() {
            self.refresh_admin_data("members", "/api/members");
        }
    }

    fn invite_settings_ui(&mut self, ui: &mut egui::Ui) {
        self.ensure_admin_data("invites", "/api/invites");
        settings_heading(ui, "Invites", "Invite your friends to this server.");
        if ui.add(primary("Invite friends")).clicked() {
            if crate::local_host::is_local_url(&self.login.url) {
                self.open_friend_sharing();
            } else if let Some(origin) = self.command_origin() {
                self.send(Command::Invite {
                    origin,
                    request_id: None,
                });
            }
        }
        ui.add_space(12.0);
        ui.collapsing("Advanced invitation limits", |ui| {
        if self.admin_ui.invite_uses == 0 {
            self.admin_ui.invite_uses = 1;
        }
        if self.admin_ui.invite_seconds == 0 {
            self.admin_ui.invite_seconds = 86400;
        }
        field(ui, "LABEL", &mut self.admin_ui.invite_label, false);
        ui.horizontal(|ui| {
            ui.label("Uses");
            ui.add(egui::DragValue::new(&mut self.admin_ui.invite_uses).clamp_range(1..=1000));
            egui::ComboBox::from_id_source("invite-expiry")
                .selected_text(duration_name(self.admin_ui.invite_seconds))
                .show_ui(ui, |ui| {
                    for seconds in [3600, 86400, 604800, 2592000] {
                        ui.selectable_value(
                            &mut self.admin_ui.invite_seconds,
                            seconds,
                            duration_name(seconds),
                        );
                    }
                });
        });
        if ui.add(primary("Create invite")).clicked() {
            self.admin_request(AdminMethod::Post,"/api/invites".into(),json!({"label":self.admin_ui.invite_label,"max_uses":self.admin_ui.invite_uses,"expires_in":self.admin_ui.invite_seconds}),"action:invites".into());
        }
        });
        if let Some(invites) = self
            .admin_data
            .get("invites")
            .and_then(Value::as_array)
            .cloned()
        {
            for invite in invites {
                ui.horizontal_wrapped(|ui| {
                    ui.label(format!(
                        "{} · {} uses left · expires {}",
                        invite["label"]
                            .as_str()
                            .filter(|label| !label.is_empty())
                            .unwrap_or("Invite"),
                        invite["remaining_uses"].as_u64().unwrap_or_default(),
                        date_value(&invite["expires_at"])
                    ));
                    if ui.small_button("Revoke").clicked() {
                        if let Some(id) = invite["id"].as_str() {
                            self.admin_request(
                                AdminMethod::Delete,
                                format!("/api/invites/{id}"),
                                Value::Null,
                                "action:invites".into(),
                            );
                        }
                    }
                });
            }
        }
    }

    pub(super) fn account_settings_ui(&mut self, ui: &mut egui::Ui) {
        self.authenticator_settings(ui);
        self.ensure_admin_data("sessions", "/api/sessions");
        settings_heading(
            ui,
            "Signed-in devices",
            "Revoke sessions you no longer use.",
        );
        if let Some(sessions) = self
            .admin_data
            .get("sessions")
            .and_then(Value::as_array)
            .cloned()
        {
            let (current, others): (Vec<_>, Vec<_>) = sessions
                .into_iter()
                .partition(|session| session["current"].as_bool() == Some(true));
            let mut session_row = |ui: &mut egui::Ui, session: &Value, label: &str| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(format!(
                        "{label} · signed in {}",
                        date_value(&session["created_at"])
                    ));
                    if ui.small_button("Sign out").clicked() {
                        if let Some(id) = session["id"].as_str() {
                            self.admin_request(
                                AdminMethod::Delete,
                                format!("/api/sessions/{id}"),
                                Value::Null,
                                "action:sessions".into(),
                            );
                        }
                    }
                });
            };
            for session in &current {
                session_row(ui, session, "This PC");
            }
            if others.is_empty() {
                crate::settings_ui::description(ui, "No other sessions are signed in.");
            } else {
                ui.collapsing(
                    match others.len() {
                        1 => "1 other session".to_owned(),
                        n => format!("{n} other sessions"),
                    },
                    |ui| {
                        for session in &others {
                            session_row(ui, session, "Other session");
                        }
                    },
                );
            }
        }
        if self
            .bootstrap
            .as_ref()
            .is_some_and(|b| b.account_login_only)
        {
            return;
        }
        ui.collapsing("Change password",|ui|{
            field(ui,"CURRENT PASSWORD",&mut self.admin_ui.current_password,true);field(ui,"NEW PASSWORD",&mut self.admin_ui.new_password,true);
            ui.label(RichText::new("Use at least 10 characters. Changing it signs out every session.").size(font_size(TEXT_CAPTION)).color(c_muted()));
            if ui.add_enabled(!self.admin_ui.current_password.is_empty()&&self.admin_ui.new_password.len()>=10,primary("Change password")).clicked(){self.admin_request(AdminMethod::Patch,"/api/password".into(),json!({"current_password":self.admin_ui.current_password,"new_password":self.admin_ui.new_password}),"action:password".into());self.admin_ui.current_password.clear();self.admin_ui.new_password.clear();}
        });
    }

    fn backup_settings_ui(&mut self, ui: &mut egui::Ui, bootstrap: &Bootstrap) {
        self.ensure_admin_data("backups", "/api/admin/backups");
        settings_heading(
            ui,
            "Community backups",
            "Back up accounts, messages and uploaded files together.",
        );
        ui.horizontal_wrapped(|ui| {
            if ui.add(primary("Save backup to this PC")).clicked() {
                if let Some(path) = rfd::FileDialog::new()
                    .set_file_name("relay-backup.sqlite3")
                    .add_filter("Relay backup", &["sqlite3"])
                    .save_file()
                {
                    self.save_backup(path);
                }
            }
            if ui.add(secondary("Create backup on server")).clicked() {
                self.admin_request(
                    AdminMethod::Post,
                    "/api/admin/backups".into(),
                    Value::Null,
                    "action:backups".into(),
                );
            }
            if ui.small_button("Refresh").clicked() {
                self.refresh_admin_data("backups", "/api/admin/backups");
            }
        });
        let backups = self
            .admin_data
            .get("backups")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for backup in &backups {
            ui.label(format!(
                "{} · {}",
                date_value(&backup["created_at"]),
                size_label(backup["bytes"].as_u64().unwrap_or_default())
            ));
        }
        ui.collapsing("Restore a server backup",|ui|{
            egui::ComboBox::from_id_source("restore-backup").selected_text(backups.iter().find(|backup|backup["id"].as_str()==Some(&self.admin_ui.restore_id)).map(|backup|date_value(&backup["created_at"])).unwrap_or_else(||"Choose a backup".into())).show_ui(ui,|ui|{for backup in &backups{if let Some(id)=backup["id"].as_str(){ui.selectable_value(&mut self.admin_ui.restore_id,id.into(),format!("{} · {}",date_value(&backup["created_at"]),size_label(backup["bytes"].as_u64().unwrap_or_default())));}}});
            ui.label(RichText::new("Restoring replaces this community and signs everyone out. The server saves a recovery backup first.").size(font_size(12.0)).color(c_bad()));
            ui.checkbox(&mut self.admin_ui.restore_confirm,"Replace this community with the selected backup");
            if ui.add_enabled(self.admin_ui.restore_confirm&&!self.admin_ui.restore_id.is_empty(),secondary("Restore community")).clicked(){self.admin_request(AdminMethod::Post,"/api/admin/restore".into(),json!({"backup_id":self.admin_ui.restore_id,"confirm_server_id":bootstrap.server_id}),"action:restore".into());self.admin_ui.restore_confirm=false;}
        });
    }
}

fn manage_channel_combo(
    ui: &mut egui::Ui,
    selected: &mut Option<Channel>,
    channels: &[Channel],
) -> egui::Response {
    let width = ui.available_width().min(360.0);
    let name = selected
        .as_ref()
        .map(|channel| channel.name.clone())
        .unwrap_or_else(|| "Choose a channel".into());
    ui.allocate_ui_with_layout(
        Vec2::new(width, ui.spacing().interact_size.y),
        Layout::top_down(Align::Min),
        |ui| {
            let text_width = (width
                - 2.0 * ui.spacing().button_padding.x
                - ui.spacing().icon_width
                - ui.spacing().icon_spacing)
                .max(1.0);
            let text = crate::settings_ui::single_line(ui, &name, 14.0, c_text(), text_width);
            egui::ComboBox::from_id_source("manage-channel")
                .width(width)
                .wrap(true)
                .selected_text(text)
                .show_ui(ui, |ui| {
                    for channel in channels {
                        let text = crate::settings_ui::single_line(
                            ui,
                            &channel.name,
                            14.0,
                            c_text(),
                            (ui.available_width() - 2.0 * ui.spacing().button_padding.x).max(1.0),
                        );
                        if ui
                            .selectable_label(
                                selected
                                    .as_ref()
                                    .is_some_and(|value| value.id == channel.id),
                                text,
                            )
                            .on_hover_text(&channel.name)
                            .clicked()
                        {
                            *selected = Some(channel.clone());
                        }
                    }
                })
                .response
                .on_hover_text(name)
        },
    )
    .inner
}

fn category_name(data: &HashMap<String, Value>, id: Option<&str>) -> String {
    data.get("categories")
        .and_then(Value::as_array)
        .and_then(|categories| {
            categories
                .iter()
                .find(|category| category["id"].as_str() == id)
        })
        .and_then(|category| category["name"].as_str())
        .unwrap_or("No category")
        .into()
}
fn duration_name(seconds: u64) -> &'static str {
    match seconds {
        3600 => "1 hour",
        604800 => "7 days",
        2592000 => "30 days",
        _ => "1 day",
    }
}
fn date_value(value: &Value) -> String {
    value
        .as_u64()
        .and_then(|seconds| chrono::DateTime::from_timestamp(seconds as i64, 0))
        .map(|date| {
            date.with_timezone(&chrono::Local)
                .format("%d %b %Y %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| "Unknown date".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn longest_channel_names_keep_selector_and_open_menu_inside_settings_column() {
        for (column_width, text_scale) in [(600.0, 1.0), (357.0, 1.4), (260.0, 1.4)] {
            let context = egui::Context::default();
            let mut style = (*context.style()).clone();
            for font in style.text_styles.values_mut() {
                font.size *= text_scale;
            }
            context.set_style(style);
            let channel: Channel = serde_json::from_value(json!({
                "id": "full-channel-id", "name": "W".repeat(64),
                "kind": "text", "topic": ""
            }))
            .unwrap();
            let channels = vec![channel.clone()];
            let mut selected = Some(channel.clone());
            let mut popup_id = None;
            for _ in 0..4 {
                let _ = context.run(
                    egui::RawInput {
                        screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(940.0, 650.0))),
                        ..Default::default()
                    },
                    |context| {
                        egui::CentralPanel::default().show(context, |ui| {
                            let left = ui.next_widget_position().x;
                            let mut column = ui.child_ui(
                                Rect::from_min_size(
                                    ui.next_widget_position(),
                                    Vec2::new(column_width, 600.0),
                                ),
                                Layout::top_down(Align::Min),
                            );
                            let response =
                                manage_channel_combo(&mut column, &mut selected, &channels);
                            assert!(
                                response.rect.right() <= left + column_width.min(360.0) + 0.5,
                                "column={column_width}, left={left}, button={:?}",
                                response.rect
                            );
                            popup_id = Some(response.id.with("popup"));
                            context.memory_mut(|memory| memory.open_popup(popup_id.unwrap()));
                        });
                    },
                );
            }
            let popup_width =
                context.memory(|memory| memory.area_rect(popup_id.unwrap()).unwrap().width());
            assert!(popup_width <= column_width.min(360.0) + 0.5);
            assert_eq!(selected.as_ref().unwrap().id, channel.id);
            assert_eq!(selected.as_ref().unwrap().name, channel.name);
        }
    }
}
