use crate::{
    appearance::{AppearanceSettings, Palette, Theme, TEXT_BODY, TEXT_CAPTION},
    audio_settings::{AudioCue, AudioDevices, AudioSettings, TransmissionMode},
    capture::{
        CaptureOptions, CaptureQuality, CaptureSession, CaptureSource, SourceKind, VideoBitrate,
        VideoCodecPreference,
    },
    chat_media::AnimatedImage,
    chat_state::{Draft, DraftStore, ReplyDraft, ScopedDrafts},
    community::{CommunityInfo, SavedCommunity},
    conversation_state::{ConversationMemory, ScrollAnchor},
    gif_library::{GifLibrary, GifPreviews},
    media::{enumerate_audio_devices, MediaEngine},
    network::{AdminMethod, Command, CommandOrigin, ConnectionStatus, Network, Target, Update},
    notifications::{Notification, NotificationClick, NotificationPolicy, Notifications},
    timeline::{self, RowHeights},
};
use eframe::egui::{
    self, Align, Align2, Color32, FontId, Layout, Margin, Pos2, Rect, RichText, Rounding, Sense,
    Stroke, Vec2,
};
use relay_protocol::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{
        mpsc::{self, Receiver},
        Arc,
    },
    time::{Duration, Instant},
};

#[path = "account_feedback_ui.rs"]
mod account_feedback_ui;
#[path = "account_link_ui.rs"]
mod account_link_ui;
#[path = "account_start_ui.rs"]
mod account_start_ui;
#[path = "admin_ui.rs"]
mod admin_ui;
#[path = "app_dialogs.rs"]
mod app_dialogs;
#[path = "app_update.rs"]
mod app_update;
#[path = "audio_device_menu.rs"]
mod audio_device_menu;
#[path = "authenticator_ui.rs"]
mod authenticator_ui;
#[path = "call_switch_ui.rs"]
mod call_switch_ui;
#[path = "central_account_ui.rs"]
mod central_account_ui;
#[path = "channel_sidebar.rs"]
mod channel_sidebar;
#[path = "chat_ui.rs"]
mod chat_ui;
#[path = "composer_ui.rs"]
mod composer_ui;
#[path = "connection_path_ui.rs"]
mod connection_path_ui;
#[path = "conversation_readiness_ui.rs"]
mod conversation_readiness_ui;
#[path = "desktop_ui.rs"]
mod desktop_ui;
#[path = "emoji_ui.rs"]
mod emoji_ui;
#[path = "encryption_ui.rs"]
mod encryption_ui;
#[path = "entry_surface.rs"]
mod entry_surface;
#[path = "help_ui.rs"]
mod help_ui;
#[path = "host_setup_ui.rs"]
mod host_setup_ui;
#[path = "invitation_launch_ui.rs"]
mod invitation_launch_ui;
#[path = "invitation_ui.rs"]
mod invitation_ui;
#[path = "main_view.rs"]
mod main_view;
#[path = "members_ui.rs"]
mod members_ui;
#[path = "message_list.rs"]
mod message_list;
#[path = "message_preview_ui.rs"]
mod message_preview_ui;
#[path = "onboarding_ui.rs"]
mod onboarding_ui;
#[path = "online_gif_ui.rs"]
mod online_gif_ui;
#[path = "people_ui.rs"]
mod people_ui;
#[path = "preference_writer.rs"]
mod preference_writer;
#[path = "private_call_ui.rs"]
mod private_call_ui;
#[path = "profile_ui.rs"]
mod profile_ui;
#[path = "protection_ui.rs"]
mod protection_ui;
#[path = "quick_switch_ui.rs"]
mod quick_switch_ui;
#[path = "routing_ui.rs"]
mod routing_ui;
#[path = "server_menu.rs"]
mod server_menu;
#[path = "settings_pages.rs"]
mod settings_pages;
#[path = "sidebar_tooltip.rs"]
mod sidebar_tooltip;
#[cfg(feature = "native-rtc")]
#[path = "source_preview.rs"]
pub(crate) mod source_preview;
#[path = "voice_room_ui.rs"]
mod voice_room_ui;
#[path = "voice_view.rs"]
mod voice_view;

thread_local! {
    static UI_PALETTE: std::cell::Cell<Palette> = std::cell::Cell::new(crate::appearance::palette(Theme::Dark));
    static UI_FONT_SCALE: std::cell::Cell<f32> = const { std::cell::Cell::new(1.0) };
}
macro_rules! palette_color {
    ($name:ident, $field:ident) => {
        fn $name() -> Color32 {
            UI_PALETTE.with(|palette| palette.get().$field)
        }
    };
}
palette_color!(c_bg, bg);
palette_color!(c_rail, rail);
palette_color!(c_panel, panel);
palette_color!(c_card, card);
palette_color!(c_line, line);
palette_color!(c_text, text);
palette_color!(c_muted, muted);
palette_color!(c_accent, accent);
palette_color!(c_bad, bad);
palette_color!(c_good, good);

fn font_size(base: f32) -> f32 {
    UI_FONT_SCALE.with(|scale| base * scale.get())
}

#[derive(Clone, Serialize, Deserialize)]
struct Preferences {
    server: String,
    username: String,
    /// The name chosen on first start, before any server. It is only a default
    /// for the username on each server; accounts themselves live on the servers.
    #[serde(default)]
    profile_name: String,
    #[serde(default)]
    hosts: Vec<String>,
    #[serde(default = "default_scale")]
    ui_scale: f32,
    #[serde(default)]
    compact: bool,
    #[serde(default = "default_remember_drafts")]
    close_to_tray: bool,
    #[serde(default = "legacy_audio_settings")]
    audio: AudioSettings,
    #[serde(default = "default_ptt_key")]
    ptt_key: i32,
    #[serde(default)]
    screen_options: CaptureOptions,
    #[serde(default)]
    camera_options: CaptureOptions,
    #[serde(default)]
    reduced_motion: bool,
    #[serde(default)]
    welcome_seen: Vec<String>,
    #[serde(default)]
    host_checklist: admin_ui::HostChecklistMemory,
    #[serde(default)]
    conversations: ConversationMemory,
    #[serde(default = "default_remember_drafts")]
    remember_drafts: bool,
    #[serde(default)]
    remember_session: bool,
    #[serde(default)]
    communities: Vec<SavedCommunity>,
    #[serde(default)]
    peer_volumes: HashMap<String, f32>,
    #[serde(default)]
    priority_speakers: HashSet<String>,
    #[serde(default)]
    notifications: bool,
    #[serde(default)]
    notification_previews: bool,
    #[serde(default)]
    notification_policies: HashMap<String, NotificationPolicy>,
    #[serde(default = "default_remember_drafts")]
    mentions_only: bool,
    #[serde(default)]
    gifs: GifLibrary,
    #[serde(default)]
    emoji_recent: Vec<String>,
    #[serde(default)]
    emoji_tone: u8,
    #[serde(default)]
    giphy_api_key: String,
    #[serde(default)]
    giphy_favorites: Vec<String>,
    #[serde(default = "default_remember_drafts")]
    online_gif_previews: bool,
    #[serde(default)]
    appearance: AppearanceSettings,
    #[serde(default)]
    connection: crate::route_choice::ConnectionPrefs,
}

fn default_remember_drafts() -> bool {
    true
}

fn default_scale() -> f32 {
    1.0
}
fn default_ptt_key() -> i32 {
    0xA4
}
fn transmission_label(mode: TransmissionMode) -> &'static str {
    match mode {
        TransmissionMode::VoiceActivation => "Voice activity",
        TransmissionMode::PushToTalk => "Push to talk",
        TransmissionMode::Continuous => "Always transmit",
    }
}
fn ptt_key_name(key: i32) -> &'static str {
    match key {
        0xA2 => "Left Ctrl",
        0x56 => "V",
        0x20 => "Space",
        0x05 => "Mouse button 4",
        0x06 => "Mouse button 5",
        _ => "Left Alt",
    }
}
fn settings_heading(ui: &mut egui::Ui, title: &str, description: &str) {
    ui.add_space(12.0);
    ui.label(
        RichText::new(title)
            .size(font_size(16.0))
            .family(egui::FontFamily::Name("Relay Semibold".into()))
            .strong(),
    );
    ui.label(
        RichText::new(description)
            .size(font_size(12.0))
            .color(c_muted()),
    );
    ui.add_space(6.0);
}
fn source_combo(
    ui: &mut egui::Ui,
    label: &str,
    id: &str,
    selected: &mut Option<String>,
    sources: &[CaptureSource],
) {
    ui.label(RichText::new(label).size(font_size(12.0)).color(c_muted()));
    let name = selected
        .as_ref()
        .and_then(|id| sources.iter().find(|s| s.id == *id))
        .map(|source| source.name.clone())
        .unwrap_or_else(|| "Choose a source".into());
    let width = ui.available_width().min(440.0);
    let text_width = (width
        - 2.0 * ui.spacing().button_padding.x
        - ui.spacing().icon_width
        - ui.spacing().icon_spacing)
        .max(1.0);
    let text = crate::settings_ui::single_line(ui, &name, 14.0, c_text(), text_width);
    egui::ComboBox::from_id_source(id)
        .width(width)
        .selected_text(text)
        .show_ui(ui, |ui| {
            for source in sources {
                let text =
                    crate::settings_ui::single_line(ui, &source.name, 14.0, c_text(), text_width);
                ui.selectable_value(selected, Some(source.id.clone()), text)
                    .on_hover_text(&source.name);
            }
        })
        .response
        .on_hover_text(name);
    ui.add_space(6.0);
}
fn capture_options_ui(
    ui: &mut egui::Ui,
    id: &str,
    options: &mut CaptureOptions,
    high_fps: bool,
) -> bool {
    let mut changed = false;
    ui.horizontal_wrapped(|ui| {
        ui.label(
            RichText::new("Quality")
                .size(font_size(12.0))
                .color(c_muted()),
        );
        egui::ComboBox::from_id_source((id, "quality"))
            .selected_text(match options.quality {
                CaptureQuality::P480 => "480p · Save bandwidth",
                CaptureQuality::P720 => "720p · Balanced",
                CaptureQuality::P1080 => "1080p · More detail",
                CaptureQuality::P1440 => "1440p · Sharp detail",
                CaptureQuality::P2160 => "4K · Highest detail",
                CaptureQuality::Source => "Source resolution · Up to 4K",
            })
            .show_ui(ui, |ui| {
                for (quality, label) in [
                    (CaptureQuality::P480, "480p · Save bandwidth"),
                    (CaptureQuality::P720, "720p · Balanced"),
                    (CaptureQuality::P1080, "1080p · More detail"),
                ] {
                    changed |= ui
                        .selectable_value(&mut options.quality, quality, label)
                        .changed();
                }
                if high_fps {
                    for (quality, label) in [
                        (CaptureQuality::P1440, "1440p · Sharp detail"),
                        (CaptureQuality::P2160, "4K · Highest detail"),
                        (CaptureQuality::Source, "Source resolution · Up to 4K"),
                    ] {
                        changed |= ui
                            .selectable_value(&mut options.quality, quality, label)
                            .changed();
                    }
                }
            });
        egui::ComboBox::from_id_source((id, "fps"))
            .selected_text(if options.fps > 30 && !high_fps {
                format!("{} fps · Requires native video", options.fps)
            } else {
                format!("{} fps", options.fps)
            })
            .show_ui(ui, |ui| {
                changed |= ui
                    .selectable_value(&mut options.fps, 15, "15 fps · Less bandwidth")
                    .changed();
                changed |= ui
                    .selectable_value(&mut options.fps, 24, "24 fps · Film and video")
                    .changed();
                changed |= ui
                    .selectable_value(&mut options.fps, 30, "30 fps · Smoother motion")
                    .changed();
                if high_fps {
                    changed |= ui
                        .selectable_value(&mut options.fps, 60, "60 fps · Native video")
                        .changed();
                }
            });
    });
    if high_fps {
        ui.horizontal_wrapped(|ui| {
            ui.label(
                RichText::new("Video bandwidth limit")
                    .size(font_size(12.0))
                    .color(c_muted()),
            );
            egui::ComboBox::from_id_source((id, "bitrate"))
                .selected_text(match options.bitrate {
                    VideoBitrate::Auto => "Automatic · Match quality",
                    VideoBitrate::Mbps2 => "2 Mbps",
                    VideoBitrate::Mbps4 => "4 Mbps",
                    VideoBitrate::Mbps8 => "8 Mbps",
                    VideoBitrate::Mbps12 => "12 Mbps",
                    VideoBitrate::Mbps20 => "20 Mbps",
                    VideoBitrate::Mbps32 => "32 Mbps",
                })
                .show_ui(ui, |ui| {
                    for (bitrate, label) in [
                        (VideoBitrate::Auto, "Automatic · Match quality"),
                        (VideoBitrate::Mbps2, "2 Mbps · Save bandwidth"),
                        (VideoBitrate::Mbps4, "4 Mbps"),
                        (VideoBitrate::Mbps8, "8 Mbps"),
                        (VideoBitrate::Mbps12, "12 Mbps"),
                        (VideoBitrate::Mbps20, "20 Mbps"),
                        (VideoBitrate::Mbps32, "32 Mbps · Maximum detail"),
                    ] {
                        changed |= ui
                            .selectable_value(&mut options.bitrate, bitrate, label)
                            .changed();
                    }
                });
        });
        ui.label(RichText::new("Higher resolution, frame rate and bandwidth use more upload capacity and processing power. Source resolution keeps the original size up to 4K; smaller sources are never enlarged.").size(font_size(TEXT_BODY)).color(c_muted()));
        changed |= ui
            .checkbox(
                &mut options.adaptive_video,
                "Automatically adjust video quality",
            )
            .changed();
        ui.label(RichText::new("Adjusts output when RTC reports bandwidth or encoder pressure. Your chosen quality and frame rate stay the upper limit.").size(font_size(TEXT_BODY)).color(c_muted()));
    }
    if changed {
        ui.ctx().memory_mut(|memory| memory.close_popup());
    }
    changed
}
impl Default for Preferences {
    fn default() -> Self {
        Self {
            server: String::new(),
            username: String::new(),
            profile_name: String::new(),
            hosts: Vec::new(),
            ui_scale: 1.0,
            compact: false,
            close_to_tray: true,
            audio: AudioSettings::default(),
            ptt_key: default_ptt_key(),
            screen_options: CaptureOptions::default(),
            camera_options: CaptureOptions::default(),
            reduced_motion: false,
            welcome_seen: Vec::new(),
            host_checklist: Default::default(),
            conversations: ConversationMemory::default(),
            remember_drafts: true,
            remember_session: false,
            communities: Vec::new(),
            peer_volumes: HashMap::new(),
            priority_speakers: HashSet::new(),
            notifications: false,
            notification_previews: false,
            notification_policies: HashMap::new(),
            mentions_only: true,
            gifs: GifLibrary::default(),
            emoji_recent: Vec::new(),
            emoji_tone: 0,
            giphy_api_key: String::new(),
            giphy_favorites: Vec::new(),
            online_gif_previews: true,
            appearance: AppearanceSettings::default(),
            connection: Default::default(),
        }
    }
}

#[derive(Clone)]
struct VideoTile {
    metadata: MediaFrame,
    texture: egui::TextureHandle,
    updated: Instant,
}

fn display_video_frame(
    frame: Arc<egui::ColorImage>,
    max_side: usize,
) -> Option<Arc<egui::ColorImage>> {
    let [width, height] = frame.size;
    let (Ok(w), Ok(h)) = (u32::try_from(width), u32::try_from(height)) else {
        return None;
    };
    if max_side == 0
        || !crate::capture::valid_video_dimensions(w, h)
        || frame.pixels.len() != width * height
    {
        return None;
    }
    if width <= max_side && height <= max_side {
        return Some(frame);
    }
    let scale = (max_side as f64 / width as f64).min(max_side as f64 / height as f64);
    let out_width = ((width as f64 * scale).floor() as u32).max(1);
    let out_height = ((height as f64 * scale).floor() as u32).max(1);
    // Keep ColorImage's premultiplied alpha while resizing.
    let bytes = frame
        .pixels
        .iter()
        .flat_map(|pixel| pixel.to_array())
        .collect();
    let image = image::RgbaImage::from_raw(w, h, bytes)?;
    let resized = image::imageops::resize(
        &image,
        out_width,
        out_height,
        image::imageops::FilterType::Triangle,
    );
    Some(Arc::new(egui::ColorImage {
        size: [out_width as usize, out_height as usize],
        pixels: resized
            .pixels()
            .map(|pixel| Color32::from_rgba_premultiplied(pixel[0], pixel[1], pixel[2], pixel[3]))
            .collect(),
    }))
}

// Bound retained UI textures independently of RTC's decoded-frame cache.
// Updating a stream replaces its previous texture rather than reserving it twice.
fn reserve_video_tile(tiles: &mut HashMap<String, VideoTile>, key: &str, size: [usize; 2]) -> bool {
    const MAX_BYTES: usize = 64 * 1024 * 1024;
    const MAX_TILES: usize = 16;
    let (Ok(width), Ok(height)) = (u32::try_from(size[0]), u32::try_from(size[1])) else {
        return false;
    };
    if !crate::capture::valid_video_dimensions(width, height) {
        return false;
    }
    let bytes = size[0] * size[1] * 4;
    let mut total: usize = tiles
        .iter()
        .filter(|(existing, _)| existing.as_str() != key)
        .map(|(_, tile)| {
            let size = tile.texture.size();
            size[0].saturating_mul(size[1]).saturating_mul(4)
        })
        .fold(0, usize::saturating_add);
    while total.saturating_add(bytes) > MAX_BYTES
        || tiles.len() + usize::from(!tiles.contains_key(key)) > MAX_TILES
    {
        let Some(oldest) = tiles
            .iter()
            .filter(|(existing, _)| existing.as_str() != key)
            .min_by_key(|(_, tile)| tile.updated)
            .map(|(existing, _)| existing.clone())
        else {
            return false;
        };
        if let Some(tile) = tiles.remove(&oldest) {
            let size = tile.texture.size();
            total = total.saturating_sub(size[0].saturating_mul(size[1]).saturating_mul(4));
        }
    }
    true
}

struct UploadView {
    target: Option<Target>,
    kind: TransferKind,
    path: PathBuf,
    transferred: u64,
    total: u64,
    error: Option<String>,
    cancelled: bool,
}
#[derive(Clone)]
enum TransferKind {
    Upload,
    Download(Attachment),
    Backup,
}

#[derive(Clone)]
struct SendAttempt {
    client_id: String,
    content: String,
    attachments: Vec<String>,
    reply_to_id: Option<String>,
    mentions: Vec<String>,
}

type CaptureResult = (bool, u64, Result<CaptureSession, String>);

#[cfg(test)]
mod notification_policy_tests {
    use super::*;
    #[test]
    fn mute_expiry_and_explicit_channel_modes_override_defaults() {
        let mut policy = NotificationPolicy {
            mode: "all".into(),
            muted_until: 100,
            ..Default::default()
        };
        assert!(!policy.allows(true, true, false, 99));
        assert!(policy.allows(false, false, true, 100));
        policy.mode = "mentions".into();
        assert!(!policy.allows(false, false, false, 100));
        assert!(policy.allows(true, false, true, 100));
        assert!(policy.allows(false, true, true, 100));
        policy.mode = "none".into();
        assert!(!policy.allows(true, true, false, 100));
        policy.mode.clear();
        assert!(!policy.allows(false, false, true, 100));
        assert!(policy.allows(false, false, false, 100));
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum SettingsTab {
    #[default]
    Account,
    Profiles,
    Friends,
    Events,
    Mentions,
    Encryption,
    Privacy,
    Connections,
    VoiceVideo,
    Appearance,
    Accessibility,
    Notifications,
    Keybinds,
    Advanced,
    Server,
    Help,
}
#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum VoiceVideoTab {
    #[default]
    Voice,
    Video,
    Debugging,
}
#[derive(Default)]
struct DeviceInventory {
    audio: AudioDevices,
    screens: Vec<CaptureSource>,
    cameras: Vec<CaptureSource>,
    warnings: Vec<String>,
}

pub struct RelayApp {
    #[cfg(windows)]
    tray: Option<crate::tray::Tray>,
    exiting: bool,
    update_ui: crate::update_ui::Controller,
    network: Network,
    notifications: Notifications,
    call_alerts: crate::call_alerts::Alerts,
    call_switch: call_switch_ui::State,
    notification_target: Option<NotificationClick>,
    #[cfg(feature = "native-rtc")]
    rtc: Option<crate::rtc::RtcSession>,
    rtc_request: Option<(u64, u64)>,
    last_source_state: [bool; 2],
    media: Option<MediaEngine>,
    media_error: Option<String>,
    screen: Option<CaptureSession>,
    camera: Option<CaptureSession>,
    capture_rx: Receiver<CaptureResult>,
    capture_ctx: egui::Context,
    capture_generation: [u64; 2],
    #[cfg(feature = "native-rtc")]
    source_preview: Option<source_preview::SourcePreview>,
    preview_release: Option<Receiver<()>>,
    preview_capture: Option<(bool, CaptureSource)>,
    screen_pending: bool,
    camera_pending: bool,
    /// Interrupted sources are only a hint; restarting always needs user action.
    video_restart_needed: [bool; 2],
    prefs: Preferences,
    /// Explicit isolated QA run; never persisted into preferences.
    #[cfg(windows)]
    profile_quit_on_close: bool,
    preference_writer: preference_writer::PreferenceWriter,
    preferences_storage_error: Option<String>,
    preferences_dirty_at: Option<Instant>,
    /// The sign-in form. Its `url` is also the active community's address,
    /// exactly as the former `url` field was.
    login: crate::connection_setup::JoinFlow,
    account_feedback: account_feedback_ui::AccountFeedback,
    account_start: account_start_ui::State,
    /// The "Join friends" form for adding another community.
    add_community: crate::connection_setup::JoinFlow,
    external_invitation: crate::invitation_launch::Pending,
    #[cfg(windows)]
    invitation_inbox: Option<crate::single_instance::Inbox>,
    #[cfg(windows)]
    invitation_window_wake: Option<crate::native_invitation_wake::WindowWake>,
    #[cfg(windows)]
    invitation_registration_pending: bool,
    session: Option<Session>,
    /// Shared, so the panels can hold it while they draw without copying the whole community every frame.
    bootstrap: Option<Arc<Bootstrap>>,
    current: Option<Target>,
    messages: Vec<Message>,
    messages_ready: bool,
    restore_scroll: Option<f32>,
    restore_anchor: Option<ScrollAnchor>,
    jump_message: Option<String>,
    row_heights: RowHeights,
    bottom_follow: timeline::BottomFollow,
    history_before: Option<u64>,
    history_pending: Option<(u64, bool)>,
    unread_navigation: Option<crate::unread_navigation::UnreadNavigation>,
    unread_history_start: Option<u64>,
    history_has_newer: bool,
    search_messages: Vec<Message>,
    search_before: Option<u64>,
    search_pending: Option<u64>,
    search_applied: String,
    search_changed_at: Option<Instant>,
    request_id: u64,
    marked_read: HashMap<Target, u64>,
    uploads: HashMap<String, UploadView>,
    transfers_open: bool,
    send_ids: HashMap<Target, SendAttempt>,
    communities: Vec<CommunityInfo>,
    community_reads: HashMap<String, Vec<ReadState>>,
    community_social: HashMap<String, (String, SocialState, String)>,
    community_friends: crate::community_friends::Directory,
    connection_status: ConnectionStatus,
    community_statuses: HashMap<String, ConnectionStatus>,
    add_community_open: bool,
    people_search: String,
    quick_switch: bool,
    quick_query: String,
    quick_selection: quick_switch_ui::Selection,
    admin_requests: HashMap<u64, String>,
    admin_data: HashMap<String, serde_json::Value>,
    admin_ui: admin_ui::ServerUi,
    profile_ui: profile_ui::ProfileUi,
    encryption_ui: encryption_ui::EncryptionUi,
    message_protection: encryption_ui::ProtectionCache,
    protection_ui: protection_ui::ProtectionUi,
    draft: String,
    reply: Option<ReplyDraft>,
    attachments: Vec<Attachment>,
    drafts: DraftStore,
    scoped_drafts: ScopedDrafts,
    pending_sends: HashSet<Target>,
    readiness: conversation_readiness_ui::ConversationReadiness,
    outgoing_messages: Vec<(Target, Message, bool, bool)>,
    cooldowns: crate::chat_features::Cooldowns,
    typing_users: crate::chat_features::TypingUsers,
    typing_activity: Option<(Target, Instant)>,
    typing_sent: Option<(Target, Instant)>,
    pins: chat_ui::PinsView,
    pending_edits: HashSet<String>,
    editing: Option<(String, String)>,
    revealed_spoiler_message: Option<String>,
    compact: bool,
    server_menu: bool,
    server_actions: server_menu::State,
    server_menu_anchor: Pos2,
    home_page: Option<SettingsTab>,
    message_menu: Option<String>,
    message_hover: Option<String>,
    delete_confirm: Option<Message>,
    initial_search: Option<String>,
    initial_community_tab: Option<String>,
    initial_capture_panel: Option<String>,
    initial_new_community: bool,
    initial_channel: Option<String>,
    initial_direct: Option<String>,
    busy: bool,
    error: Option<String>,
    notice: Option<(String, Instant)>,
    voice_channel: Option<String>,
    /// Original join identity; media/key refreshes have their own generation.
    voice_join_attempt: Option<u64>,
    voice_retry: Option<crate::voice_ui_state::Retry>,
    private_calls: private_call_ui::State,
    authenticator_ui: authenticator_ui::State,
    account_link_ui: account_link_ui::State,
    central_account: crate::central_account::Controller,
    people_ui: people_ui::State,
    #[cfg(windows)]
    account_webview: Option<crate::account_webview::Surface>,
    /// When each other person was first seen in your call (sound check).
    sound_check_since: HashMap<String, Instant>,
    /// Display-only: people a capture shows as heard (no audio runs in captures).
    capture_heard: HashSet<String>,
    audio_connecting: bool,
    voice_encryption_error: Option<String>,
    /// Display state from the worker's bounded recovery, never diagnostic text.
    voice_recovery_pending: bool,
    muted: bool,
    deafened: bool,
    video: HashMap<String, VideoTile>,
    pinned_stream: Option<String>,
    focused_stream: Option<String>,
    stream_preview: Option<String>,
    stream_fullscreen: bool,
    stream_restore_fullscreen: bool,
    settings_open: bool,
    settings_tab: SettingsTab,
    voice_video_tab: VoiceVideoTab,
    settings_search: String,
    applied_appearance: AppearanceSettings,
    applied_system_theme: Option<eframe::Theme>,
    settings_scroll_bottom: bool,
    inventory: DeviceInventory,
    inventory_rx: Option<Receiver<DeviceInventory>>,
    inventory_loaded: bool,
    audio_applied: AudioSettings,
    mic_test: bool,
    joining_voice: Option<String>,
    voice_request: u64,
    share_picker: Option<bool>,
    share_kind: SourceKind,
    screen_source: Option<String>,
    camera_source: Option<String>,
    chat_media: HashMap<String, AnimatedImage>,
    preview_activity_at: Instant,
    chat_media_pending: HashSet<String>,
    chat_media_errors: HashMap<String, String>,
    media_preview: Option<String>,
    gif_picker: bool,
    composer_anchor: Option<Rect>,
    online_gifs: online_gif_ui::GifBrowser,
    gif_upload_sends: HashMap<String, (CommandOrigin, Target)>,
    gif_favorites: bool,
    gif_query: String,
    gif_previews: Option<GifPreviews>,
    gif_textures: HashMap<PathBuf, egui::TextureHandle>,
    gif_pending: HashSet<PathBuf>,
    gif_errors: HashMap<PathBuf, String>,
    gif_hover: Option<(PathBuf, AnimatedImage)>,
    gif_hover_pending: Option<PathBuf>,
    gif_hover_errors: HashSet<PathBuf>,
    create_channel_open: bool,
    /// A masked chat link waiting for "Open link" in the leaving-Relay dialog.
    pending_link: Option<String>,
    channel_name: String,
    channel_topic: String,
    /// Create Channel's chosen type: text, voice, forum, announcement or stage.
    channel_kind: &'static str,
    invite_dialog: Option<crate::invitation::Invitation>,
    invite_address: String,
    hosting_setup: host_setup_ui::HostSetup,
    search: String,
    history_search_open: bool,
    show_members: bool,
    members_compact: bool,
    compact_members_open: bool,
    show_voice_members: bool,
    selected_nav: bool,
    scroll_to_end: bool,
    capture_path: Option<PathBuf>,
    native_render_probe: Option<crate::native_render_probe::Probe>,
    capture_requested: bool,
    capture_icons: bool,
    capture_authenticated: bool,
    /// Display-only: a capture shows the own-server sharing steps for the fixture.
    capture_local_host: bool,
    capture_send: Option<String>,
    initial_pins: bool,
    start: Instant,
    ui_scale: f32,
    own_local_server: bool,
}

// Worker updates currently share FIFO ordering. Keep an independent owner
// check at consumption so a future asynchronous emitter cannot apply a prior
// account's changes, protection labels or sidebar snapshot to its replacement.
fn community_update_allowed(
    update: &Update,
    active_url: &str,
    active: Option<&CommandOrigin>,
    communities: &[CommunityInfo],
) -> bool {
    let (url, server_id, user_id) = match update {
        Update::Changes { origin, .. }
        | Update::MessageProtections { origin, .. }
        | Update::VoiceCallFailed { origin, .. } => {
            (&origin.url, &origin.server_id, &origin.user_id)
        }
        Update::CommunityBootstrap { url, bootstrap } => {
            (url, &bootstrap.server_id, &bootstrap.user.id)
        }
        _ => return true,
    };
    if url.is_empty() || server_id.is_empty() || user_id.is_empty() {
        return false;
    }
    if url == active_url {
        // Connected may precede Communities; logout must not fall back to an
        // old current entry in that list while the active session is absent.
        active.is_some_and(|owner| {
            &owner.url == url && &owner.server_id == server_id && &owner.user_id == user_id
        })
    } else {
        communities
            .iter()
            .find(|community| &community.url == url)
            .is_some_and(|community| {
                &community.server_id == server_id && &community.user.id == user_id
            })
    }
}

// Keep the last accepted account snapshot available when its community is
// parked. Initial active bootstraps may never trigger a background refresh.
fn cache_community_snapshot(
    reads: &mut HashMap<String, Vec<ReadState>>,
    social: &mut HashMap<String, (String, SocialState, String)>,
    url: &str,
    bootstrap: &Bootstrap,
) {
    let status = bootstrap
        .presence
        .iter()
        .find(|presence| presence.user_id == bootstrap.user.id)
        .map(|presence| presence.status.clone())
        .unwrap_or_default();
    reads.insert(url.to_owned(), bootstrap.read_states.clone());
    social.insert(
        url.to_owned(),
        (bootstrap.user.id.clone(), bootstrap.social.clone(), status),
    );
}

fn notification_accounts(
    active_url: &str,
    active: Option<CommandOrigin>,
    communities: &[CommunityInfo],
    statuses: &HashMap<String, ConnectionStatus>,
) -> Vec<CommandOrigin> {
    let mut owners = Vec::new();
    let eligible = |origin: &CommandOrigin| {
        !origin.url.is_empty()
            && !origin.server_id.is_empty()
            && !origin.user_id.is_empty()
            && statuses.get(&origin.url) != Some(&ConnectionStatus::SignInRequired)
    };
    if let Some(active) = active.filter(|origin| origin.url == active_url && eligible(origin)) {
        owners.push(active);
    }
    for community in communities
        .iter()
        .filter(|community| community.url != active_url)
    {
        let origin = CommandOrigin {
            url: community.url.clone(),
            server_id: community.server_id.clone(),
            user_id: community.user.id.clone(),
        };
        if eligible(&origin) && !owners.iter().any(|owner| owner.url == origin.url) {
            owners.push(origin);
        }
    }
    owners
}

fn pending_notification_target(
    click: Option<NotificationClick>,
    url: &str,
    bootstrap: &Bootstrap,
) -> Option<Target> {
    click
        .filter(|click| {
            !click.origin.url.is_empty()
                && !click.origin.server_id.is_empty()
                && !click.origin.user_id.is_empty()
                && click.origin.url == url
                && click.origin.server_id == bootstrap.server_id
                && click.origin.user_id == bootstrap.user.id
                && target_available(&click.target, bootstrap)
        })
        .map(|click| click.target)
}

impl RelayApp {
    pub fn new(cc: &eframe::CreationContext<'_>, launch: crate::invitation_launch::Launch) -> Self {
        configure(&cc.egui_ctx);
        let prefs_path = std::env::var_os("RELAY_CONFIG_DIR")
            .map(PathBuf::from)
            .or_else(|| {
                directories::ProjectDirs::from("org", "Relay", "Relay")
                    .map(|d| d.config_dir().to_path_buf())
            })
            .unwrap_or_else(|| PathBuf::from("data"))
            .join("preferences.json");
        #[cfg(windows)]
        let account_webview = crate::account_webview::Surface::new(
            cc,
            prefs_path.parent().expect("preferences directory"),
        );
        let (mut prefs, preferences_storage_error) =
            match preference_writer::load::<Preferences>(&prefs_path) {
                Ok(prefs) => (prefs, None),
                Err(error) => (Preferences::default(), Some(error)),
            };
        // Communities joined through the account-free route can be opened again.
        for community in &prefs.communities {
            if let Some(route) = &community.iroh {
                crate::iroh_route::register(route);
            }
        }
        let args: Vec<String> = std::env::args().collect();
        if args.iter().any(|argument| argument == "--low-latency") {
            prefs
                .audio
                .set_latency_mode(crate::audio_settings::LatencyMode::LowLatency);
        }
        let value = |name: &str| {
            args.iter()
                .position(|a| a == name)
                .and_then(|i| args.get(i + 1).cloned())
        };
        let url = value("--server").unwrap_or_else(|| {
            if prefs.server.is_empty() {
                "http://127.0.0.1:4580".into()
            } else {
                prefs.server.clone()
            }
        });
        let username = value("--username").unwrap_or_else(|| prefs.username.clone());
        let (mut media, mut media_error) = match MediaEngine::new() {
            Ok(m) => (Some(m), None),
            Err(e) => (None, Some(e)),
        };
        let audio_applied = prefs.audio.clone().normalized();
        if let Some(engine) = &mut media {
            engine.configure(audio_applied.clone());
            engine.set_push_to_talk_key(prefs.ptt_key);
        }
        let capture_path = value("--capture").map(PathBuf::from);
        let ui_scale = prefs.ui_scale.clamp(0.85, 1.4);
        let compact = prefs.compact || args.iter().any(|arg| arg == "--compact");
        prefs.reduced_motion |= args.iter().any(|arg| arg == "--reduced-motion");
        prefs.appearance = prefs.appearance.normalized();
        if let Some(theme) = value("--theme") {
            prefs.appearance.theme = match theme.as_str() {
                "onyx" => Theme::Onyx,
                "ash" => Theme::Ash,
                "light" => Theme::Light,
                "system" => Theme::System,
                _ => Theme::Dark,
            };
        }
        let applied_appearance = prefs.appearance.clone();
        let colors = prefs
            .appearance
            .apply(&cc.egui_ctx, cc.integration_info.system_theme);
        UI_PALETTE.with(|palette| palette.set(colors));
        UI_FONT_SCALE.with(|scale| scale.set(prefs.appearance.font_scale));
        cc.egui_ctx.set_zoom_factor(ui_scale);
        let (_capture_tx, capture_rx) = mpsc::sync_channel(2);
        let notifications = Notifications::new(cc.egui_ctx.clone());
        let call_audio = if capture_path.is_none() {
            media
                .as_mut()
                .and_then(|engine| match engine.call_alert_audio() {
                    Ok(audio) => Some(audio),
                    Err(error) => {
                        media_error = Some(error);
                        None
                    }
                })
        } else {
            None
        };
        let call_alerts = crate::call_alerts::Alerts::new(notifications.clone(), call_audio);
        let mut app = Self {
            #[cfg(windows)]
            tray: if capture_path.is_none() {
                crate::tray::Tray::bind(cc).ok()
            } else {
                None
            },
            exiting: false,
            update_ui: crate::update_ui::Controller::new(&cc.egui_ctx),
            network: Network::with_call_alerts(cc.egui_ctx.clone(), Some(call_alerts.clone())),
            notifications,
            call_alerts,
            call_switch: Default::default(),
            notification_target: None,
            #[cfg(feature = "native-rtc")]
            rtc: None,
            rtc_request: None,
            last_source_state: [false; 2],
            media,
            media_error,
            screen: None,
            camera: None,
            capture_rx,
            capture_ctx: cc.egui_ctx.clone(),
            capture_generation: [0; 2],
            #[cfg(feature = "native-rtc")]
            source_preview: None,
            preview_release: None,
            preview_capture: None,
            screen_pending: false,
            camera_pending: false,
            video_restart_needed: [false; 2],
            prefs,
            #[cfg(windows)]
            profile_quit_on_close: crate::profile_paths::quit_on_close(&args).unwrap_or(false),
            preference_writer: preference_writer::PreferenceWriter::new(
                prefs_path,
                cc.egui_ctx.clone(),
                preferences_storage_error.is_none(),
            ),
            preferences_storage_error: preferences_storage_error.clone(),
            preferences_dirty_at: None,
            login: crate::connection_setup::JoinFlow {
                setup: Default::default(),
                url,
                username,
                password: String::new(),
                second_factor: String::new(),
                factor_binding: None,
                invite: String::new(),
                registering: false,
            },
            account_feedback: Default::default(),
            account_start: Default::default(),
            add_community: crate::connection_setup::JoinFlow::default(),
            external_invitation: crate::invitation_launch::Pending::initial(launch.initial),
            #[cfg(windows)]
            invitation_inbox: launch.inbox,
            #[cfg(windows)]
            invitation_window_wake: None,
            #[cfg(windows)]
            invitation_registration_pending: launch.register_protocol,
            session: None,
            bootstrap: None,
            current: None,
            messages: Vec::new(),
            messages_ready: false,
            restore_scroll: None,
            restore_anchor: None,
            jump_message: None,
            row_heights: RowHeights::default(),
            bottom_follow: timeline::BottomFollow::default(),
            history_before: None,
            history_pending: None,
            unread_navigation: None,
            unread_history_start: None,
            history_has_newer: false,
            search_messages: Vec::new(),
            search_before: None,
            search_pending: None,
            search_applied: String::new(),
            search_changed_at: None,
            request_id: 0,
            marked_read: HashMap::new(),
            uploads: HashMap::new(),
            transfers_open: false,
            send_ids: HashMap::new(),
            communities: Vec::new(),
            community_reads: HashMap::new(),
            community_social: HashMap::new(),
            community_friends: crate::community_friends::Directory::default(),
            connection_status: ConnectionStatus::Offline,
            community_statuses: HashMap::new(),
            add_community_open: false,
            people_search: String::new(),
            quick_switch: false,
            quick_query: String::new(),
            quick_selection: quick_switch_ui::Selection::default(),
            admin_requests: HashMap::new(),
            admin_data: HashMap::new(),
            admin_ui: admin_ui::ServerUi::default(),
            profile_ui: profile_ui::ProfileUi::default(),
            encryption_ui: encryption_ui::EncryptionUi::default(),
            message_protection: encryption_ui::ProtectionCache::default(),
            protection_ui: protection_ui::ProtectionUi::default(),
            draft: String::new(),
            reply: None,
            attachments: Vec::new(),
            drafts: DraftStore::default(),
            scoped_drafts: ScopedDrafts::default(),
            pending_sends: HashSet::new(),
            readiness: Default::default(),
            outgoing_messages: Vec::new(),
            cooldowns: Default::default(),
            typing_users: Default::default(),
            typing_activity: None,
            typing_sent: None,
            pins: Default::default(),
            pending_edits: HashSet::new(),
            editing: None,
            revealed_spoiler_message: None,
            compact,
            server_menu: false,
            server_actions: server_menu::State::default(),
            server_menu_anchor: Pos2::ZERO,
            home_page: None,
            message_menu: None,
            message_hover: None,
            delete_confirm: None,
            initial_search: value("--search"),
            initial_community_tab: value("--community-tab"),
            initial_capture_panel: capture_path.as_ref().and_then(|_| value("--capture-panel")),
            initial_new_community: args.iter().any(|arg| arg == "--new-community"),
            initial_channel: value("--channel"),
            initial_direct: value("--direct"),
            busy: false,
            error: preferences_storage_error,
            notice: None,
            voice_channel: None,
            voice_join_attempt: None,
            voice_retry: None,
            private_calls: private_call_ui::State::default(),
            authenticator_ui: authenticator_ui::State::default(),
            account_link_ui: account_link_ui::State::default(),
            central_account: crate::central_account::Controller::default(),
            people_ui: people_ui::State::default(),
            #[cfg(windows)]
            account_webview,
            sound_check_since: HashMap::new(),
            capture_heard: HashSet::new(),
            audio_connecting: false,
            voice_encryption_error: None,
            voice_recovery_pending: false,
            muted: false,
            deafened: false,
            video: HashMap::new(),
            pinned_stream: None,
            focused_stream: None,
            stream_preview: None,
            stream_fullscreen: false,
            stream_restore_fullscreen: false,
            settings_open: value("--settings").is_some(),
            settings_tab: match value("--settings").as_deref() {
                Some("audio" | "voice" | "video" | "debug") => SettingsTab::VoiceVideo,
                Some("appearance") => SettingsTab::Appearance,
                Some("privacy") => SettingsTab::Privacy,
                Some("connections") => SettingsTab::Connections,
                Some("accessibility") => SettingsTab::Accessibility,
                Some("notifications") => SettingsTab::Notifications,
                Some("keybinds") => SettingsTab::Keybinds,
                Some("help") => SettingsTab::Help,
                Some("advanced") => SettingsTab::Advanced,
                Some("server") => SettingsTab::Server,
                Some("profiles" | "profile") => SettingsTab::Profiles,
                Some("friends" | "status") => SettingsTab::Friends,
                Some("events") => SettingsTab::Events,
                Some("inbox" | "mentions") => SettingsTab::Mentions,
                Some("encryption") => SettingsTab::Encryption,
                _ => SettingsTab::Account,
            },
            voice_video_tab: match value("--settings").as_deref() {
                Some("video") => VoiceVideoTab::Video,
                Some("debug") => VoiceVideoTab::Debugging,
                _ => VoiceVideoTab::Voice,
            },
            settings_search: String::new(),
            applied_appearance,
            applied_system_theme: cc.integration_info.system_theme,
            settings_scroll_bottom: args.iter().any(|arg| arg == "--settings-bottom"),
            inventory: DeviceInventory::default(),
            inventory_rx: None,
            inventory_loaded: false,
            audio_applied,
            mic_test: false,
            joining_voice: None,
            voice_request: 0,
            share_picker: args
                .iter()
                .any(|arg| arg == "--sharing-options")
                .then_some(false),
            share_kind: SourceKind::Monitor,
            screen_source: None,
            camera_source: None,
            chat_media: HashMap::new(),
            preview_activity_at: Instant::now(),
            chat_media_pending: HashSet::new(),
            chat_media_errors: HashMap::new(),
            media_preview: None,
            gif_picker: args.iter().any(|arg| arg == "--gif-picker"),
            composer_anchor: None,
            online_gifs: online_gif_ui::GifBrowser::default(),
            gif_upload_sends: HashMap::new(),
            gif_favorites: false,
            gif_query: String::new(),
            gif_previews: None,
            gif_textures: HashMap::new(),
            gif_pending: HashSet::new(),
            gif_errors: HashMap::new(),
            gif_hover: None,
            gif_hover_pending: None,
            gif_hover_errors: HashSet::new(),
            create_channel_open: false,
            pending_link: None,
            channel_name: String::new(),
            channel_topic: String::new(),
            channel_kind: "text",
            invite_dialog: None,
            invite_address: String::new(),
            hosting_setup: host_setup_ui::HostSetup::from_args(&args),
            search: String::new(),
            history_search_open: false,
            show_members: true,
            members_compact: false,
            compact_members_open: false,
            show_voice_members: false,
            selected_nav: true,
            scroll_to_end: false,
            capture_path: capture_path.clone(),
            native_render_probe: crate::native_render_probe::path_from_args(&args)
                .ok()
                .flatten()
                .and_then(|path| match crate::native_render_probe::Probe::open(path) {
                    Ok(probe) => Some(probe),
                    Err(error) => {
                        eprintln!("Cannot open native render probe: {error}");
                        None
                    }
                }),
            capture_requested: false,
            capture_icons: capture_path.is_some()
                && value("--capture-panel").as_deref() == Some("call-icons"),
            capture_authenticated: args.iter().any(|arg| arg == "--capture-authenticated"),
            capture_local_host: false,
            capture_send: if capture_path.is_some() {
                value("--capture-send")
            } else {
                None
            },
            initial_pins: args.iter().any(|arg| arg == "--pins"),
            start: Instant::now(),
            ui_scale,
            own_local_server: false,
        };
        app.sync_call_alert_preferences();
        #[cfg(windows)]
        if app.capture_path.is_none() {
            app.invitation_window_wake = crate::native_invitation_wake::WindowWake::bind(cc).ok();
            if let Some(wake) = &app.invitation_window_wake {
                app.notifications.set_click_wake(wake.notifier());
            }
        }
        #[cfg(windows)]
        if let Some(inbox) = &app.invitation_inbox {
            let ctx = cc.egui_ctx.clone();
            let notify = app
                .invitation_window_wake
                .as_ref()
                .map(|wake| wake.notifier());
            if notify.is_none() {
                app.invitation_registration_pending = false;
                app.notice = Some(("Open Relay and paste invitations in Join friends if a link cannot open the window.".into(), Instant::now()));
            }
            inbox.attach(move || {
                if let Some(notify) = &notify {
                    notify();
                }
                ctx.request_repaint();
            });
        }
        // Start our saved local host before attempting a remembered login.
        // Merely joining someone else's server never starts a local server.
        if capture_path.is_none() {
            if let (Some(server), Ok(data)) = (
                crate::local_host::server_binary(),
                crate::local_host::data_directory(),
            ) {
                let expected = crate::local_host::AppHost::load(&data)
                    .map(|p| p.server_id)
                    .or_else(|| {
                        let managed =
                            crate::local_host::read_record(&data.join("managed-instance.json"))
                                .ok()?;
                        app.prefs
                            .communities
                            .iter()
                            .find(|c| managed["server_id"].as_str() == Some(c.server_id.as_str()))
                            .map(|c| c.server_id.clone())
                    });
                if let Some(expected) = expected.filter(|id| uuid::Uuid::parse_str(id).is_ok()) {
                    app.send(Command::RestoreLocal {
                        server,
                        data,
                        expected,
                    });
                }
            }
        }
        // Smoke runs can supply credentials through the environment; passwords are never saved.
        if let Ok(password) = std::env::var("RELAY_TEST_PASSWORD") {
            if !app.login.username.is_empty() {
                app.login.password = password;
                app.authenticate();
            }
        } else if app.central_account.provider.is_none()
            && app.prefs.remember_session
            && app
                .prefs
                .communities
                .iter()
                .any(|saved| saved.url == app.login.url)
        {
            app.busy = true;
            app.send(Command::Resume {
                url: app.login.url.clone(),
                expected_server_id: app
                    .prefs
                    .communities
                    .iter()
                    .find(|saved| saved.url == app.login.url)
                    .map(|saved| saved.server_id.clone())
                    .filter(|id| !id.is_empty()),
            });
        }
        app
    }

    fn send(&mut self, command: Command) {
        let forget_alerts = match &command {
            Command::Logout => Some(self.login.url.clone()),
            Command::ForgetCommunity { url } => Some(url.clone()),
            _ => None,
        };
        if forget_alerts.is_some() {
            self.cancel_background_people_answer();
        }
        match self.network.send(command) {
            Err(e) => self.error = Some(e),
            Ok(()) => {
                if let Some(url) = forget_alerts {
                    self.call_alerts.forget_url(&url);
                }
            }
        }
    }

    fn authenticate(&mut self) {
        let expected_server_id = match self
            .login
            .setup
            .expected_id_with_saved(&self.login.url, &self.prefs.communities)
        {
            Ok(expected) => expected,
            Err(error) => {
                self.error = Some(error);
                return;
            }
        };
        let attempt = account_feedback_ui::AccountAttempt::new(
            account_feedback_ui::AccountFlow::SignIn,
            &self.login.url,
            self.login.registering,
        );
        self.error = None;
        let request_id = self.next_request();
        if !self.account_feedback.submit(
            attempt,
            request_id,
            &self.login.username,
            &self.login.password,
        ) {
            return;
        }
        self.busy = true;
        self.send(Command::RememberSession {
            enabled: self.prefs.remember_session,
        });
        self.send(Command::Auth {
            url: self.login.url.trim().into(),
            expected_server_id,
            request: AuthRequest {
                second_factor: (!self.login.registering
                    && !self.login.second_factor.trim().is_empty())
                .then(|| self.login.second_factor.trim().to_owned()),
                initial_protection: (self.hosting_setup.new_host
                    && self.login.registering
                    && crate::local_host::is_local_url(&self.login.url))
                .then_some(self.hosting_setup.creation_protection),
                username: self.login.username.trim().into(),
                password: self.login.password.clone(),
                invite: self.login.invite.trim().into(),
            },
            register: self.login.registering,
            request_id,
        });
    }

    fn select(&mut self, target: Target) {
        self.select_at(target, false);
    }

    fn select_at(&mut self, target: Target, unread: bool) {
        self.home_page = None;
        self.selected_nav = matches!(&target, Target::Channel(_));
        if !unread && self.current.as_ref() == Some(&target) {
            return;
        }
        self.stop_typing();
        self.compact_members_open = false;
        self.typing_users = Default::default();
        self.pins = Default::default();
        self.store_current_draft();
        let draft = self.drafts.take(&target);
        self.draft = draft.content;
        self.reply = draft.reply;
        self.attachments = draft.attachments;
        self.current = Some(target.clone());
        if let Some(scope) = self.conversation_scope() {
            self.restore_scroll = self.prefs.conversations.scroll(&scope, &target);
            self.restore_anchor = self.prefs.conversations.anchor(&scope, &target);
            self.prefs.conversations.remember(&scope, &target);
            if let Some(saved) = self
                .prefs
                .communities
                .iter_mut()
                .find(|saved| saved.url == self.login.url)
            {
                saved.last_target = Some(target.clone());
            }
            self.defer_preferences();
        }
        self.unread_navigation = None;
        self.unread_history_start = None;
        self.jump_message = None;
        self.messages.clear();
        self.messages_ready = false;
        self.history_before = None;
        self.history_pending = None;
        self.history_has_newer = false;
        self.search_messages.clear();
        self.search_before = None;
        self.search_pending = None;
        self.search_applied.clear();
        self.search_changed_at = None;
        self.row_heights = RowHeights::default();
        self.bottom_follow
            .begin(self.restore_scroll.is_some() || self.restore_anchor.is_some());
        self.editing = None;
        self.message_menu = None;
        self.message_hover = None;
        self.search.clear();
        self.history_search_open = false;
        self.scroll_to_end = false;
        self.chat_media.clear();
        self.chat_media_pending.clear();
        self.chat_media_errors.clear();
        self.media_preview = None;
        self.online_gifs.close_message_preview();
        let voice_view = match &target {
            Target::Channel(id) => self
                .bootstrap
                .as_ref()
                .is_some_and(|b| b.channels.iter().any(|c| c.id == *id && c.kind == "voice")),
            Target::Direct(_) => false,
        };
        self.send(if voice_view {
            Command::ClearSelection
        } else {
            Command::SelectConversation(target.clone())
        });
        if !voice_view && unread {
            if let Some(origin) = self.command_origin() {
                let read_sequence = self
                    .read_state(&target)
                    .map(|read| read.last_read_sequence)
                    .unwrap_or_default();
                self.restore_scroll = None;
                self.restore_anchor = None;
                self.bottom_follow.cancel();
                self.unread_navigation = Some(crate::unread_navigation::UnreadNavigation::new(
                    origin,
                    target,
                    read_sequence,
                ));
                self.load_unread_page();
            } else {
                self.load_history(false);
            }
        } else if !voice_view {
            self.load_history(false);
        }
    }

    fn store_current_draft(&mut self) {
        if let Some(target) = &self.current {
            self.drafts.store(
                target.clone(),
                Draft {
                    content: std::mem::take(&mut self.draft),
                    attachments: std::mem::take(&mut self.attachments),
                    reply: self.reply.take(),
                },
            );
        }
    }

    fn restore_current_draft(&mut self) {
        if let Some(target) = &self.current {
            let draft = self.drafts.take(target);
            self.draft = draft.content;
            self.attachments = draft.attachments;
            self.reply = draft.reply;
        }
    }

    fn save_preferences(&mut self) {
        self.sync_call_alert_preferences();
        self.preferences_dirty_at = None;
        if let Some(scope) = self.conversation_scope() {
            let mut drafts = self.drafts.snapshot();
            if let Some(target) = &self.current {
                drafts.retain(|(saved, _)| saved != target);
                drafts.push((
                    target.clone(),
                    Draft {
                        content: self.draft.clone(),
                        attachments: self.attachments.clone(),
                        reply: self.reply.clone(),
                    },
                ));
            }
            save_draft_scope(&mut self.prefs, &mut self.scoped_drafts, &scope, drafts);
        }
        self.prefs.server = self.login.url.clone();
        self.prefs.username = self.login.username.clone();
        self.prefs.ui_scale = self.ui_scale;
        self.prefs.compact = self.compact;
        if !self.prefs.hosts.contains(&self.login.url) {
            self.prefs.hosts.push(self.login.url.clone());
            self.prefs.hosts.truncate(12);
        }
        if let Ok(bytes) = preferences_snapshot(&self.prefs, &self.audio_applied) {
            self.preference_writer.save(bytes);
        }
    }

    fn conversation_scope(&self) -> Option<String> {
        self.session
            .as_ref()
            .map(|session| ConversationMemory::scope(&self.login.url, &session.user.id))
    }

    fn defer_preferences(&mut self) {
        self.preferences_dirty_at = Some(Instant::now());
        self.capture_ctx
            .request_repaint_after(Duration::from_millis(750));
    }

    fn next_request(&mut self) -> u64 {
        self.request_id = self.request_id.wrapping_add(1);
        self.request_id
    }

    fn load_unread_page(&mut self) {
        let Some(navigation) = self.unread_navigation.as_ref() else {
            return;
        };
        if navigation.pending.is_some() {
            return;
        }
        let origin = navigation.origin.clone();
        let target = navigation.target.clone();
        let after = navigation.after;
        if self.command_origin().as_ref() != Some(&origin) || self.current.as_ref() != Some(&target)
        {
            self.unread_navigation = None;
            return;
        }
        let request_id = self.next_request();
        let navigation = self.unread_navigation.as_mut().unwrap();
        navigation.pending = Some(request_id);
        navigation.error = None;
        if let Err(error) = self.network.send(Command::HistoryAfter {
            origin,
            target,
            after,
            request_id,
        }) {
            navigation.pending = None;
            navigation.error = Some(error);
        }
    }

    fn open_latest(&mut self) {
        self.unread_navigation = None;
        self.history_pending = None;
        self.restore_scroll = None;
        self.restore_anchor = None;
        self.jump_message = None;
        self.scroll_to_end = true;
        self.load_history(false);
    }

    fn load_history(&mut self, older: bool) {
        self.unread_navigation = None;
        self.unread_history_start = None;
        let Some(target) = self.current.clone() else {
            return;
        };
        if self.history_pending.is_some() {
            return;
        }
        let before = if older { self.history_before } else { None };
        if older && before.is_none() {
            return;
        }
        if older {
            self.bottom_follow.cancel();
        }
        let request_id = self.next_request();
        self.history_pending = Some((request_id, older));
        let command = if !older && !self.scroll_to_end {
            if let Some(anchor) = &self.restore_anchor {
                Command::AroundMessage {
                    target,
                    message_id: anchor.message_id.clone(),
                    request_id,
                }
            } else {
                Command::History {
                    target,
                    before,
                    request_id,
                }
            }
        } else {
            Command::History {
                target,
                before,
                request_id,
            }
        };
        if let Err(error) = self.network.send(command) {
            self.history_pending = None;
            self.error = Some(error);
        }
    }

    fn search_history(&mut self, more: bool) {
        if self.unread_navigation.is_some() {
            self.open_latest();
        }
        let Some(target) = self.current.clone() else {
            return;
        };
        let query = self.search.trim().to_owned();
        if query.is_empty() {
            self.search_pending = None;
            self.search_messages.clear();
            self.search_applied.clear();
            return;
        }
        let before = if more { self.search_before } else { None };
        if more && before.is_none() {
            return;
        }
        if !more {
            self.search_messages.clear();
            self.search_before = None;
        }
        let request_id = self.next_request();
        self.search_pending = Some(request_id);
        self.search_applied = query.clone();
        if let Err(error) = self.network.send(Command::Search {
            target,
            query,
            before,
            request_id,
        }) {
            self.search_pending = None;
            self.error = Some(error);
        }
    }

    fn start_upload(&mut self, target: Target, path: PathBuf) {
        let _ = self.queue_upload(target, path);
    }

    fn queue_upload(&mut self, target: Target, path: PathBuf) -> Option<String> {
        let origin = self.command_origin()?;
        let operation_id = crate::network::operation_id();
        self.uploads.insert(
            operation_id.clone(),
            UploadView {
                target: Some(target.clone()),
                kind: TransferKind::Upload,
                path: path.clone(),
                transferred: 0,
                total: 0,
                error: None,
                cancelled: false,
            },
        );
        if let Err(error) = self.network.send(Command::StartUpload {
            origin,
            target,
            path,
            operation_id: operation_id.clone(),
        }) {
            if let Some(transfer) = self.uploads.get_mut(&operation_id) {
                transfer.error = Some(error);
            }
            return None;
        }
        Some(operation_id)
    }

    fn command_origin(&self) -> Option<CommandOrigin> {
        Some(CommandOrigin {
            url: self.login.url.clone(),
            server_id: self.bootstrap.as_ref()?.server_id.clone(),
            user_id: self.session.as_ref()?.user.id.clone(),
        })
    }

    fn start_download(&mut self, attachment: Attachment, path: PathBuf) {
        let Some(origin) = self.command_origin() else {
            return;
        };
        let operation_id = crate::network::operation_id();
        self.uploads.insert(
            operation_id.clone(),
            UploadView {
                target: self.current.clone(),
                kind: TransferKind::Download(attachment.clone()),
                path: path.clone(),
                transferred: 0,
                total: attachment.size,
                error: None,
                cancelled: false,
            },
        );
        if let Err(error) = self.network.send(Command::StartDownload {
            origin,
            attachment,
            path,
            operation_id: operation_id.clone(),
        }) {
            if let Some(transfer) = self.uploads.get_mut(&operation_id) {
                transfer.error = Some(error);
            }
        }
        self.transfers_open = true;
    }

    fn save_backup(&mut self, path: PathBuf) {
        let operation_id = crate::network::operation_id();
        self.uploads.insert(
            operation_id.clone(),
            UploadView {
                target: None,
                kind: TransferKind::Backup,
                path: path.clone(),
                transferred: 0,
                total: 0,
                error: None,
                cancelled: false,
            },
        );
        if let Err(error) = self.network.send(Command::SaveBackup {
            path,
            operation_id: operation_id.clone(),
        }) {
            if let Some(transfer) = self.uploads.get_mut(&operation_id) {
                transfer.error = Some(error);
            }
        }
        self.transfers_open = true;
    }

    fn retry_transfer(&mut self, id: &str) {
        if let Some(transfer) = self.uploads.remove(id) {
            match transfer.kind {
                TransferKind::Upload => {
                    if let Some(target) = transfer.target {
                        self.start_upload(target, transfer.path);
                    }
                }
                TransferKind::Download(attachment) => {
                    self.start_download(attachment, transfer.path)
                }
                TransferKind::Backup => self.save_backup(transfer.path),
            }
        }
    }

    fn read_state(&self, target: &Target) -> Option<&ReadState> {
        self.bootstrap
            .as_ref()?
            .read_states
            .iter()
            .find(|read| match target {
                Target::Channel(id) => read.channel_id.as_ref() == Some(id),
                Target::Direct(id) => read.peer_id.as_ref() == Some(id),
            })
    }

    fn reveal_message(&mut self, message_id: String) {
        self.unread_navigation = None;
        self.bottom_follow.cancel();
        self.restore_anchor = None;
        self.restore_scroll = None;
        let Some(target) = self.current.clone() else {
            return;
        };
        self.search.clear();
        self.search_history(false);
        self.jump_message = Some(message_id.clone());
        if self.messages.iter().any(|message| message.id == message_id) {
            return;
        }
        let request_id = self.next_request();
        self.history_pending = Some((request_id, false));
        if let Err(error) = self.network.send(Command::AroundMessage {
            target,
            message_id,
            request_id,
        }) {
            self.history_pending = None;
            self.jump_message = None;
            self.error = Some(error);
        }
    }

    fn remembered_target(&self, direct: bool) -> Option<Target> {
        let bootstrap = self.bootstrap.as_ref()?;
        let saved = self
            .conversation_scope()
            .and_then(|scope| self.prefs.conversations.last(&scope, Some(direct)));
        saved
            .filter(|target| match target {
                Target::Direct(id) => {
                    direct
                        && bootstrap
                            .members
                            .iter()
                            .any(|member| &member.id == id && member.id != bootstrap.user.id)
                }
                Target::Channel(id) => {
                    !direct && bootstrap.channels.iter().any(|channel| &channel.id == id)
                }
            })
            .or_else(|| {
                if direct {
                    bootstrap
                        .members
                        .iter()
                        .find(|member| member.id != bootstrap.user.id)
                        .map(|member| Target::Direct(member.id.clone()))
                } else {
                    bootstrap
                        .channels
                        .iter()
                        .find(|channel| channel.kind == "text")
                        .map(|channel| Target::Channel(channel.id.clone()))
                }
            })
    }

    fn open_saved_community_login(&mut self, url: String) {
        self.cancel_background_people_answer();
        self.add_community.setup = Default::default();
        self.add_community.username = self
            .prefs
            .communities
            .iter()
            .find(|saved| saved.url == url)
            .map(|saved| saved.username.clone())
            .unwrap_or_default();
        self.add_community.url = url;
        self.add_community.password.clear();
        self.add_community.invite.clear();
        self.add_community.registering = false;
        self.add_community_open = true;
    }

    fn community_status(&self, url: &str) -> ConnectionStatus {
        if url == self.login.url && self.session.is_some() {
            return self.connection_status.clone();
        }
        self.community_statuses
            .get(url)
            .cloned()
            .unwrap_or_else(|| {
                if self
                    .communities
                    .iter()
                    .any(|community| community.url == url)
                {
                    ConnectionStatus::Connected
                } else {
                    ConnectionStatus::SignInRequired
                }
            })
    }

    fn switch_community(&mut self, url: String) {
        self.cancel_background_people_answer();
        self.server_actions.cancel_if_other(&url);
        if self.community_statuses.get(&url) == Some(&ConnectionStatus::SignInRequired)
            || (url == self.login.url
                && self.session.is_some()
                && self.connection_status == ConnectionStatus::SignInRequired)
        {
            self.save_preferences();
            self.open_saved_community_login(url);
            return;
        }
        if url == self.login.url {
            self.selected_nav = true;
            if let Some(target) = self.remembered_target(false) {
                self.select(target);
            }
            return;
        }
        self.save_preferences();
        if !self
            .communities
            .iter()
            .any(|community| community.url == url)
        {
            if self.prefs.remember_session {
                let expected_server_id = self
                    .prefs
                    .communities
                    .iter()
                    .find(|saved| saved.url == url)
                    .map(|saved| saved.server_id.clone())
                    .filter(|id| !id.is_empty());
                self.send(Command::Resume {
                    url,
                    expected_server_id,
                });
            } else {
                self.add_community.username = self
                    .prefs
                    .communities
                    .iter()
                    .find(|saved| saved.url == url)
                    .map(|saved| saved.username.clone())
                    .unwrap_or_default();
                self.add_community.url = url;
                self.add_community.password.clear();
                self.add_community_open = true;
            }
            return;
        }
        // Keep the current call until the worker validates the destination.
        // Update::Connected performs the media teardown after a successful switch.
        self.send(Command::SwitchCommunity { url });
    }

    fn open_community_dialog(&mut self) {
        self.add_community.setup = Default::default();
        self.add_community.url.clear();
        self.add_community.username = self.login.username.clone();
        self.add_community.password.clear();
        self.add_community.invite.clear();
        self.add_community.registering = false;
        self.add_community_open = true;
    }

    fn sync_notification_accounts(&mut self) {
        self.sync_call_alert_preferences();
        self.notifications.set_accounts(notification_accounts(
            &self.login.url,
            self.command_origin(),
            &self.communities,
            &self.community_statuses,
        ));
    }

    fn sync_call_alert_preferences(&self) {
        self.call_alerts.configure(crate::call_alerts::Policy {
            notifications: self.prefs.notifications,
            previews: self.prefs.notification_previews,
            deafened: self.deafened,
            disabled: self.exiting
                || self.capture_path.is_some()
                || self.native_render_probe.is_some(),
            audio: self.audio_applied.clone(),
            active: self.command_origin(),
            message_policies: self.prefs.notification_policies.clone(),
            mentions_only: self.prefs.mentions_only,
        });
    }

    fn handle_notification_clicks(&mut self, ctx: &egui::Context) {
        for click in self.notifications.clicked() {
            if click.origin.url == self.login.url
                && !self
                    .bootstrap
                    .as_ref()
                    .is_some_and(|bootstrap| target_available(&click.target, bootstrap))
            {
                continue;
            }
            if self.settings_open {
                self.close_settings();
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            if click.origin.url == self.login.url {
                self.selected_nav = !matches!(click.target, Target::Direct(_));
                self.select(click.target);
            } else {
                let url = click.origin.url.clone();
                self.notification_target = Some(click);
                self.switch_community(url);
            }
        }
    }

    fn sync_voice_state(&mut self) {
        self.sync_call_alert_preferences();
        if self.stage_listener() {
            self.muted = true;
        }
        if let Some(media) = &mut self.media {
            media.set_muted(self.muted);
            media.set_deafened(self.deafened);
        }
        if self.voice_channel.is_some() {
            self.send(Command::VoiceState(VoiceState {
                muted: self.muted,
                deafened: self.deafened,
                sharing: self.source_active(false),
                camera: self.source_active(true),
            }));
        }
    }

    fn peer_volume_key(&self, user_id: &str) -> String {
        format!(
            "{}\n{user_id}",
            self.conversation_scope()
                .unwrap_or_else(|| self.login.url.clone())
        )
    }

    fn gaming_voice_features(&mut self, ctx: &egui::Context) {
        let members: Vec<_> = self
            .bootstrap
            .as_ref()
            .map(|b| b.voice_members.clone())
            .unwrap_or_default();
        let origin = self.command_origin();
        if let Some(media) = &self.media {
            let audio = media.active_settings();
            media.retain_whisper_peers(
                &members
                    .iter()
                    .map(|member| member.user.id.clone())
                    .collect(),
            );
            if audio.whisper_enabled {
                if let (Some(origin), Some(channel_id)) = (&origin, &self.voice_channel) {
                    for member in members
                        .iter()
                        .filter(|member| member.user.id != origin.user_id)
                    {
                        if media.prepare_whisper(&member.user.id) {
                            let _ = self.network.send(Command::PrepareWhisper {
                                origin: origin.clone(),
                                channel_id: channel_id.clone(),
                                peer: member.user.id.clone(),
                            });
                        }
                    }
                }
            }
            let mut speaking = media.speaking_users();
            if media.is_transmitting() {
                if let Some(origin) = &origin {
                    speaking.push(origin.user_id.clone());
                }
            }
            media.set_priority_users(
                members
                    .iter()
                    .filter(|m| {
                        self.prefs
                            .priority_speakers
                            .contains(&self.peer_volume_key(&m.user.id))
                    })
                    .map(|m| m.user.id.clone()),
            );
            media.update_ducking(
                audio.ducking_enabled,
                self.voice_channel.is_some() && (!speaking.is_empty() || media.is_transmitting()),
                audio.ducking_volume,
            );
            if audio.overlay_enabled && self.voice_channel.is_some() {
                let names: Vec<_> = members
                    .iter()
                    .filter(|m| {
                        Some(&m.channel_id) == self.voice_channel.as_ref()
                            && speaking.contains(&m.user.id)
                    })
                    .map(|m| profile_ui::display_name(&m.user).to_owned())
                    .collect();
                ctx.show_viewport_deferred(
                    egui::ViewportId::from_hash_of("voice-speaking-overlay"),
                    egui::ViewportBuilder::default()
                        .with_title("Relay Voice")
                        .with_inner_size([220.0, 36.0 + names.len() as f32 * 22.0])
                        .with_position([audio.overlay_x, audio.overlay_y])
                        .with_decorations(false)
                        .with_transparent(true)
                        .with_always_on_top()
                        .with_mouse_passthrough(true)
                        .with_active(false),
                    move |ctx, _| {
                        egui::CentralPanel::default()
                            .frame(
                                egui::Frame::none()
                                    .fill(Color32::from_black_alpha(190))
                                    .inner_margin(Margin::same(8.0)),
                            )
                            .show(ctx, |ui| {
                                ui.label(
                                    RichText::new("Relay Voice").color(Color32::WHITE).strong(),
                                );
                                for name in &names {
                                    ui.label(RichText::new(name).color(Color32::LIGHT_GREEN));
                                }
                            });
                    },
                );
                ctx.request_repaint_after(Duration::from_millis(50));
            }
        }
    }

    fn member_speaking(&self, member: &VoiceMember) -> bool {
        if member.muted || member.deafened {
            return false;
        }
        self.media.as_ref().is_some_and(|media| {
            if self
                .session
                .as_ref()
                .is_some_and(|session| session.user.id == member.user.id)
            {
                media.is_transmitting()
            } else {
                media.speaking_users().contains(&member.user.id)
            }
        })
    }

    fn source_active(&self, camera: bool) -> bool {
        #[cfg(feature = "native-rtc")]
        if self.rtc.as_ref().is_some_and(|rtc| rtc.is_sharing(camera)) {
            return true;
        }
        if camera {
            self.camera.as_ref()
        } else {
            self.screen.as_ref()
        }
        .is_some_and(CaptureSession::is_active)
    }
    fn source_starting(&self, camera: bool) -> bool {
        if self
            .preview_capture
            .as_ref()
            .is_some_and(|(pending, _)| *pending == camera)
        {
            return true;
        }
        #[cfg(feature = "native-rtc")]
        if self
            .rtc
            .as_ref()
            .is_some_and(|rtc| rtc.is_source_starting(camera))
        {
            return true;
        }
        if camera {
            self.camera_pending
                || self
                    .camera
                    .as_ref()
                    .is_some_and(CaptureSession::is_starting)
        } else {
            self.screen_pending
                || self
                    .screen
                    .as_ref()
                    .is_some_and(CaptureSession::is_starting)
        }
    }
    fn rtc_active(&self) -> bool {
        #[cfg(feature = "native-rtc")]
        {
            self.rtc
                .as_ref()
                .is_some_and(crate::rtc::RtcSession::is_active)
        }
        #[cfg(not(feature = "native-rtc"))]
        {
            false
        }
    }

    fn rtc_starting(&self) -> bool {
        #[cfg(feature = "native-rtc")]
        {
            self.rtc_request.is_some()
                || self
                    .rtc
                    .as_ref()
                    .is_some_and(crate::rtc::RtcSession::is_starting)
        }
        #[cfg(not(feature = "native-rtc"))]
        {
            false
        }
    }

    fn rtc_reconnecting(&self) -> bool {
        #[cfg(feature = "native-rtc")]
        {
            self.rtc
                .as_ref()
                .is_some_and(crate::rtc::RtcSession::is_reconnecting)
        }
        #[cfg(not(feature = "native-rtc"))]
        {
            false
        }
    }

    fn voice_ready(&self) -> bool {
        self.voice_channel.is_some() && self.media.as_ref().is_some_and(MediaEngine::is_active)
    }

    fn voice_reconnecting(&self) -> bool {
        (self.voice_channel.is_some() || self.joining_voice.is_some())
            && (self.voice_recovery_pending
                || (self.voice_encryption_error.is_none()
                    && self.media.as_ref().is_some_and(|media| {
                        media.is_active()
                            && !media.network_stats().tcp_active
                            && media.network_stats().udp_recovery_remaining_ms.is_some()
                    })))
    }

    fn voice_connected(&self) -> bool {
        self.voice_ready()
            && self.media.as_ref().is_some_and(|media| {
                media.network_stats().udp_reachable || media.network_stats().tcp_active
            })
    }

    fn start_protected_voice(
        &mut self,
        url: &str,
        ticket: VoiceTicket,
        keys: std::sync::Arc<crate::e2ee::CallKeys>,
    ) {
        self.voice_encryption_error = None;
        self.voice_recovery_pending = false;
        if self.stage_listener() {
            self.muted = true;
        }
        if let Some(media) = &mut self.media {
            media.stop_microphone_test();
            self.mic_test = false;
            media.set_muted(self.muted);
            media.set_deafened(self.deafened);
            media.set_push_to_talk_key(self.prefs.ptt_key);
            match media.start_voice(url, ticket, keys) {
                Ok(()) => {
                    self.audio_connecting = true;
                    self.media_error = None;
                    self.notice = Some(("Opening audio devices…".into(), Instant::now()));
                    self.sync_voice_state();
                }
                Err(error) => {
                    self.media_error = Some(error.clone());
                    let channel = self.voice_channel.clone();
                    self.stop_media();
                    self.voice_failure(channel, error);
                    self.send(Command::LeaveVoice);
                }
            }
        } else {
            self.error = self.media_error.clone();
            self.stop_media();
            self.send(Command::LeaveVoice);
        }
    }

    fn request_video_connection(&mut self) {
        #[cfg(feature = "native-rtc")]
        if self.voice_channel.is_some()
            && self.rtc.is_none()
            && self.rtc_request.is_none()
            && self.voice_encryption_error.is_none()
            && self.bootstrap.as_ref().is_some_and(|bootstrap| {
                bootstrap
                    .capabilities
                    .iter()
                    .any(|cap| cap == "native-rtc-v1")
            })
        {
            let request_id = self.next_request();
            self.rtc_request = Some((request_id, self.voice_request));
            self.send(Command::RtcToken {
                channel_id: self.voice_channel.clone().unwrap(),
                request_id,
            });
        }
    }

    fn voice_failure(&mut self, channel: Option<String>, error: String) {
        self.finish_private_call();
        self.voice_retry = channel
            .zip(self.command_origin())
            .map(|(channel_id, origin)| crate::voice_ui_state::Retry {
                origin,
                channel_id,
                error: error.clone(),
            });
        self.error = Some(error);
    }

    fn voice_retry_target(&self) -> Option<String> {
        self.voice_retry
            .as_ref()?
            .target(
                self.command_origin().as_ref(),
                self.bootstrap.as_deref(),
                self.connection_status == ConnectionStatus::Connected
                    && self.voice_channel.is_none()
                    && self.joining_voice.is_none()
                    && !self.busy,
                self.error.as_deref(),
            )
            .map(str::to_owned)
    }

    fn join_voice(&mut self, channel: String) {
        self.cancel_background_people_answer();
        // An explicit room choice supersedes any earlier outgoing call intent.
        self.private_calls.intent = None;
        if self
            .bootstrap
            .as_ref()
            .and_then(|bootstrap| bootstrap.channels.iter().find(|entry| entry.id == channel))
            .is_some_and(|channel| !channel.can_join)
        {
            self.notice = Some((
                "You can view this voice room, but do not have permission to join.".into(),
                Instant::now(),
            ));
            return;
        }
        // Every entry point reaches the same consent notice before requesting
        // an Open call or stopping an existing call.
        if self.call_needs_open_consent() {
            self.select(Target::Channel(channel));
            return;
        }
        if !crate::voice_ui_state::should_join(
            self.voice_channel.as_deref(),
            self.joining_voice.as_deref(),
            &channel,
        ) {
            return;
        }
        self.finish_private_call();
        self.voice_retry = None;
        self.stop_media();
        self.sound_check_since.clear();
        self.error = None;
        match self.network.send(Command::JoinVoiceRequest {
            channel_id: channel.clone(),
            request_id: self.voice_request,
        }) {
            Ok(()) => self.joining_voice = Some(channel),
            Err(error) => self.voice_failure(Some(channel), error),
        }
    }

    fn leave_voice(&mut self) {
        self.finish_private_call();
        self.private_calls.intent = None;
        self.voice_retry = None;
        let connected = self.voice_connected();
        self.joining_voice = None;
        self.stop_media();
        self.sound_check_since.clear();
        if connected {
            if let Some(media) = &mut self.media {
                let _ = media.play_tone(AudioCue::CallEnd);
            }
        }
        self.send(Command::LeaveVoice);
    }

    fn toggle_mute(&mut self) {
        if self.stage_listener() {
            self.error = Some("Request to speak before unmuting in this stage.".into());
            return;
        }
        self.muted = !self.muted;
        if let Some(media) = &mut self.media {
            let _ = media.play_tone(if self.muted {
                AudioCue::Mute
            } else {
                AudioCue::Unmute
            });
        }
        self.sync_voice_state();
    }

    fn toggle_deafen(&mut self) {
        self.deafened = !self.deafened;
        if let Some(media) = &mut self.media {
            let _ = media.play_tone(if self.deafened {
                AudioCue::Deafen
            } else {
                AudioCue::Undeafen
            });
        }
        self.sync_voice_state();
    }

    fn refresh_devices(&mut self) {
        if self.inventory_rx.is_some() {
            return;
        }
        let (tx, rx) = mpsc::sync_channel(1);
        self.inventory_rx = Some(rx);
        let ctx = self.capture_ctx.clone();
        std::thread::spawn(move || {
            let mut inventory = DeviceInventory::default();
            match enumerate_audio_devices() {
                Ok(audio) => inventory.audio = audio,
                Err(error) => inventory.warnings.push(error),
            }
            match crate::capture::enumerate_screen_sources() {
                Ok(screens) => inventory.screens = screens,
                Err(error) => inventory.warnings.push(error),
            }
            match crate::capture::enumerate_cameras() {
                Ok(cameras) => inventory.cameras = cameras,
                Err(error) => inventory.warnings.push(error),
            }
            let _ = tx.send(inventory);
            ctx.request_repaint();
        });
    }

    fn apply_audio_settings(&mut self) {
        if self.mic_test {
            if let Some(media) = &mut self.media {
                media.stop_microphone_test();
            }
            self.mic_test = false;
        }
        self.prefs.audio = self.prefs.audio.clone().normalized();
        self.configure_audio(self.prefs.audio.clone());
    }

    fn apply_quick_audio_change(&mut self, change: impl Fn(&mut AudioSettings)) {
        let next = crate::audio_settings::quick_audio_change(
            &self.audio_applied,
            &mut self.prefs.audio,
            change,
        );
        self.configure_audio(next);
    }

    fn configure_audio(&mut self, settings: AudioSettings) {
        let reopen = self
            .media
            .as_mut()
            .is_some_and(|media| media.configure(settings.clone()));
        self.audio_applied = settings;
        self.save_preferences();
        if reopen {
            if self.voice_channel.is_some() {
                if let Some(media) = &mut self.media {
                    if let Err(error) = media.reopen_voice_devices() {
                        self.error = Some(error);
                    }
                }
            } else if self.mic_test {
                if let Some(media) = &mut self.media {
                    media.stop_microphone_test();
                    if let Err(error) = media.start_microphone_test() {
                        self.mic_test = false;
                        self.error = Some(error);
                    }
                }
            }
        }
    }

    fn stop_media_for_call_change(&mut self) {
        let attempt = self.voice_join_attempt;
        let interrupted = [false, true].map(|camera| {
            self.video_restart_needed[usize::from(camera)]
                || self.source_active(camera)
                || self.source_starting(camera)
        });
        self.stop_media();
        self.voice_join_attempt = attempt;
        self.video_restart_needed = interrupted;
    }

    fn stop_media(&mut self) {
        self.cancel_background_people_answer();
        self.voice_join_attempt = None;
        self.video_restart_needed = [false; 2];
        self.preview_capture = None;
        self.stop_source_preview();
        self.voice_request = self.voice_request.wrapping_add(1);
        self.network.invalidate_live_media();
        self.joining_voice = None;
        self.screen_pending = false;
        self.camera_pending = false;
        self.share_picker = None;
        self.voice_channel = None;
        self.audio_connecting = false;
        self.voice_encryption_error = None;
        self.voice_recovery_pending = false;
        self.video.clear();
        for generation in &mut self.capture_generation {
            *generation = generation.wrapping_add(1);
        }
        self.pinned_stream = None;
        self.focused_stream = None;
        self.stream_preview = None;
        self.rtc_request = None;
        self.last_source_state = [false; 2];
        #[cfg(feature = "native-rtc")]
        if let Some(rtc) = self.rtc.take() {
            rtc.cancel();
            drop(rtc);
        }
        self.network.next_capture_generation(false);
        self.network.next_capture_generation(true);
        release_capture(self.screen.take());
        release_capture(self.camera.take());
        if let Some(media) = &mut self.media {
            media.stop_voice();
            media.stop_microphone_test();
        }
        self.mic_test = false;
    }

    fn screen_toggle(&mut self) {
        if self.source_active(false) || self.source_starting(false) {
            self.stop_capture(false);
        } else {
            self.share_picker = Some(false);
            self.request_video_connection();
            self.refresh_devices();
        }
        self.sync_voice_state();
    }

    fn camera_toggle(&mut self) {
        if self.source_active(true) || self.source_starting(true) {
            self.stop_capture(true);
        } else {
            self.share_picker = Some(true);
            self.request_video_connection();
            self.refresh_devices();
        }
        self.sync_voice_state();
    }

    fn stop_source_preview(&mut self) {
        #[cfg(feature = "native-rtc")]
        if let Some(preview) = self.source_preview.take() {
            self.preview_release = Some(preview.stop(self.capture_ctx.clone()));
        }
    }

    fn stop_capture(&mut self, camera: bool) {
        if self
            .preview_capture
            .as_ref()
            .is_some_and(|(pending, _)| *pending == camera)
        {
            self.preview_capture = None;
        }
        if let Some(session) = &self.session {
            let key = format!(
                "{}:{}",
                session.user.id,
                if camera { "camera" } else { "screen" }
            );
            self.video.remove(&key);
            if self.stream_preview.as_ref() == Some(&key) {
                self.stream_preview = None;
            }
        }
        let index = usize::from(camera);
        self.capture_generation[index] = self.capture_generation[index].wrapping_add(1);
        self.network.next_capture_generation(camera);
        #[cfg(feature = "native-rtc")]
        if let Some(rtc) = &self.rtc {
            if camera {
                rtc.stop_camera();
            } else {
                rtc.stop_screen();
            }
        }
        if camera {
            self.camera_pending = false;
            release_capture(self.camera.take());
        } else {
            self.screen_pending = false;
            release_capture(self.screen.take());
        }
        self.send(Command::StopCapture(
            if camera { "camera" } else { "screen" }.into(),
        ));
        self.sync_voice_state();
    }

    fn start_capture(&mut self, camera: bool, source: CaptureSource) {
        #[cfg(not(feature = "native-rtc"))]
        let _ = &source;
        if self
            .voice_channel
            .as_ref()
            .and_then(|id| {
                self.bootstrap
                    .as_ref()?
                    .channels
                    .iter()
                    .find(|channel| &channel.id == id)
            })
            .is_none_or(|channel| !channel.can_stream)
        {
            self.error = Some("You do not have permission to share in this room.".into());
            return;
        }
        if !self.voice_ready() || self.source_active(camera) || self.source_starting(camera) {
            return;
        }
        #[cfg(feature = "native-rtc")]
        if self.source_preview.is_some() || self.preview_release.is_some() {
            self.stop_source_preview();
            self.preview_capture = Some((camera, source));
            return;
        }
        #[cfg(feature = "native-rtc")]
        if let Some(rtc) = self.rtc.as_ref().filter(|rtc| rtc.is_active()) {
            let mut options = if camera {
                self.prefs.camera_options
            } else {
                self.prefs.screen_options
            };
            options.application_audio &= !camera && source.kind == SourceKind::Window;
            let result = if camera {
                rtc.start_camera(source, options)
            } else {
                rtc.start_screen(source, options)
            };
            if let Err(error) = result {
                self.error = Some(error);
            } else if camera {
                self.camera_pending = true;
            } else {
                self.screen_pending = true;
            }
            return;
        }
        self.error = Some(if self.rtc_starting() {
            "Protected video is still connecting. Wait for the connection before sharing.".into()
        } else {
            "Protected video is unavailable. Reconnect video or install the native RTC build; Relay will not publish an unprotected preview.".into()
        });
    }

    fn host_local(&mut self) {
        let Some(server) = crate::local_host::server_binary() else {
            self.error =
                Some("The bundled server is missing. Reinstall Relay to restore it.".into());
            return;
        };
        let data = match crate::local_host::data_directory() {
            Ok(data) => data,
            Err(error) => {
                self.error = Some(error);
                return;
            }
        };
        // Starting a process does not mean creating a new community.
        self.login.registering = !data.join("relay.sqlite3").is_file();
        self.busy = true;
        self.send(Command::HostLocal { server, data });
    }

    fn login_guide(&mut self, ui: &mut egui::Ui) {
        egui::Frame::none().fill(c_panel()).rounding(8.0)
            .inner_margin(Margin::same(20.0)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new("New to Relay?").size(font_size(18.0)).strong());
                ui.add_space(10.0);
                ui.label(RichText::new("Joining someone else's community").strong());
                ui.add(egui::Label::new("Paste the invitation or server address they sent you. Choose Create account the first time; choose Sign in if you already have an account on that server.").wrap(true));
                ui.add_space(16.0);
                ui.label(RichText::new("Starting your own community").strong());
                ui.add(egui::Label::new("Choose Start a server on this PC below. When it is ready, choose Create account. The first account becomes the administrator and does not need an invite code.").wrap(true));
                ui.add_space(16.0);
                ui.label(
                    RichText::new("Click a field to see what it is for.").color(c_muted()),
                );
            });
    }

    fn advanced_login_form(&mut self, ui: &mut egui::Ui) {
        egui::Frame::none().fill(c_panel()).rounding(8.0)
            .inner_margin(Margin::same(20.0)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new(if self.login.registering { "Create your account" } else { "Sign in" })
                    .size(font_size(20.0)).strong());
                ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            if tab(ui, "Sign in", !self.login.registering) {
                                self.login.registering = false;
                                self.error = None;
                            }
                            if tab(ui, "Create account", self.login.registering) {
                                self.login.registering = true;
                                self.error = None;
                            }
                        });
                        ui.add(egui::Label::new(RichText::new(if self.login.registering {
                            "Choose a username and password for this community."
                        } else {
                            "Use the username and password you created on this server. Running your own server? Sign in as admin with the password the installer showed."
                        }).color(c_muted())).wrap(true));
                        self.setup_controls(ui, crate::connection_setup::FlowKind::Login);
                        field(ui, "USERNAME", &mut self.login.username, false);
                        let response = field(ui, "PASSWORD", &mut self.login.password, true);
                        if ui
                            .checkbox(
                                &mut self.prefs.remember_session,
                                "Keep me signed in on this PC",
                            )
                            .changed()
                        {
                            self.save_preferences();
                        }
                        if self.login.registering {
                            field(ui, "INVITE CODE (IF REQUIRED)", &mut self.login.invite, false);
                        }
                        ui.add_space(18.0);
                        let text = if self.busy {
                            "Connecting…"
                        } else if self.login.registering {
                            "Create account"
                        } else {
                            "Sign in"
                        };
                        if ui
                            .add_enabled(
                                !self.busy,
                                primary(text).min_size(Vec2::new(ui.available_width(), 40.0)),
                            )
                            .clicked()
                            || (response.lost_focus()
                                && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                        {
                            self.authenticate();
                        }
                        ui.add_space(10.0);
                        if ui
                            .add_enabled(
                                !self.busy,
                                secondary("Start a server on this PC")
                                    .min_size(Vec2::new(ui.available_width(), 36.0)),
                            )
                            .on_hover_text("Start your own community on this computer. After it is ready, create the first account. Keep this PC running while others use the community.")
                            .clicked()
                        {
                            self.host_local();
                        }
                        ui.add(egui::Label::new(RichText::new(
                            "Only needed when you want to host your own community. Joining a friend's server does not require this.")
                            .size(font_size(TEXT_BODY)).color(c_muted())).wrap(true));
                        ui.label(RichText::new("Local start does not make this PC reachable from another home.").color(c_muted()));
                        if ui.add(secondary("Invite friends")).clicked() {
                            self.open_friend_sharing();
                        }
                        if let Some(error) = &self.error {
                            ui.add_space(12.0);
                            ui.add(egui::Label::new(RichText::new(error).size(font_size(13.0)).color(c_bad())).wrap(true));
                            if error.contains("Cannot reach") {
                                ui.add(egui::Label::new("For your own community, start the server using the button above. For a friend's community, ask the host to check that it is running and send you its address or invitation.").wrap(true));
                            }
                        } else if let Some((notice, _)) = &self.notice {
                            ui.add_space(12.0);
                            ui.label(RichText::new(notice).size(font_size(13.0)).color(c_muted()));
                        }
                        ui.add_space(16.0);
                        ui.label(
                            RichText::new("Free and self-hosted · Early alpha")
                                .size(font_size(TEXT_CAPTION))
                                .color(c_muted()),
                        );
            });
    }

    fn current_name(&self) -> (String, String, bool) {
        match (&self.current, &self.bootstrap) {
            (Some(Target::Channel(id)), Some(b)) => b
                .channels
                .iter()
                .find(|c| &c.id == id)
                .map(|c| (c.name.clone(), c.topic.clone(), c.kind == "voice"))
                .unwrap_or(("Channels".into(), String::new(), false)),
            (Some(Target::Direct(id)), Some(b)) => (
                b.members
                    .iter()
                    .find(|u| &u.id == id)
                    .map(|u| profile_ui::display_name(u).to_owned())
                    .unwrap_or("Direct message".into()),
                b.members
                    .iter()
                    .find(|u| &u.id == id)
                    .map(|u| format!("@{}", u.username))
                    .unwrap_or_default(),
                false,
            ),
            _ => ("Welcome".into(), String::new(), false),
        }
    }

    fn current_voice_text_parent(&self) -> Option<Channel> {
        let Some(Target::Channel(id)) = &self.current else {
            return None;
        };
        let bootstrap = self.bootstrap.as_ref()?;
        let channel = bootstrap
            .channels
            .iter()
            .find(|channel| channel.id == *id)?;
        let parent = channel.voice_parent_id.as_ref()?;
        bootstrap
            .channels
            .iter()
            .find(|channel| channel.id == *parent && channel.kind == "voice")
            .cloned()
    }

    fn begin_reply(&mut self, ui: &mut egui::Ui, message: &Message) {
        self.reply = Some(ReplyDraft {
            message_id: message.id.clone(),
            author: profile_ui::display_name(&message.author).to_owned(),
            excerpt: if message.content.trim().is_empty() {
                message
                    .attachments
                    .first()
                    .map(|attachment| attachment.name.clone())
                    .unwrap_or_else(|| "Attachment".into())
            } else {
                message.content.clone()
            },
            author_id: Some(message.author.id.clone()),
            notify_author: false,
        });
        ui.memory_mut(|memory| {
            memory.request_focus(egui::Id::new(("composer-editor", self.current.clone())))
        });
        self.defer_preferences();
    }

    fn react_to_message(&mut self, message: &Message, emoji: &str) {
        let Some(target) = self.current.clone() else {
            return;
        };
        let Some(session) = &self.session else { return };
        let target_client_id = match self.message_protection.get(&message.id) {
            Some(crate::e2ee::MessageProtection::Protected) => message.client_id.clone(),
            Some(crate::e2ee::MessageProtection::Legacy) => None,
            _ => {
                self.error =
                    Some("Wait for this message's encryption status before reacting.".into());
                return;
            }
        };
        let active = !message.reactions.iter().any(|reaction| {
            reaction.emoji == emoji && reaction.user_ids.contains(&session.user.id)
        });
        let Some(origin) = self.command_origin() else {
            return;
        };
        self.send(Command::React {
            origin,
            target,
            message_id: message.id.clone(),
            target_client_id,
            emoji: emoji.into(),
            active,
        });
    }

    fn attachment_preview(
        &mut self,
        ui: &mut egui::Ui,
        attachment: &Attachment,
        message: &Message,
    ) {
        let name = attachment.name.to_ascii_lowercase();
        if ![".gif", ".png", ".jpg", ".jpeg"]
            .iter()
            .any(|ext| name.ends_with(ext))
        {
            return;
        }
        if let Some(image) = self.chat_media.get_mut(&attachment.id) {
            ui.add_space(6.0);
            let dimensions = image.dimensions();
            let scale = (ui.available_width().min(400.0) / dimensions.x)
                .min(260.0 / dimensions.y)
                .min(1.0);
            let size = dimensions * scale;
            let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
            let texture = image.texture_with_autoplay(
                ui.ctx(),
                ui.is_rect_visible(rect) || self.media_preview.as_ref() == Some(&attachment.id),
                self.prefs.reduced_motion,
                self.prefs.appearance.gif_autoplay,
            );
            let response = ui.put(
                rect,
                egui::Image::new((texture, size))
                    .rounding(8.0)
                    .sense(Sense::click()),
            );
            if response.clicked() {
                self.media_preview = Some(attachment.id.clone());
            }
            response.context_menu(|ui| {
                if ui.button("View image").clicked() {
                    self.media_preview = Some(attachment.id.clone());
                    ui.close_menu();
                }
                self.message_actions(ui, message);
            });
        } else if let Some(error) = self.chat_media_errors.get(&attachment.id) {
            ui.label(
                RichText::new("Preview unavailable · download to view")
                    .size(font_size(TEXT_CAPTION))
                    .color(c_muted()),
            )
            .on_hover_text(error);
        } else if !ui.ctx().input(|i| i.viewport().minimized.unwrap_or(false))
            && ui.is_rect_visible(Rect::from_min_size(ui.cursor().min, Vec2::new(80.0, 20.0)))
        {
            if !self.chat_media_pending.contains(&attachment.id) {
                let Some(origin) = self.command_origin() else {
                    return;
                };
                match self.network.send(Command::FetchChatMedia {
                    original: false,
                    origin,
                    attachment: attachment.clone(),
                }) {
                    Ok(()) => {
                        self.chat_media_pending.insert(attachment.id.clone());
                    }
                    Err(error) => {
                        self.chat_media_errors.insert(attachment.id.clone(), error);
                    }
                }
            }
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(
                    RichText::new("Loading image…")
                        .size(font_size(TEXT_CAPTION))
                        .color(c_muted()),
                );
            });
        }
    }

    pub(super) fn finish_gif_picker(&mut self, ctx: &egui::Context) {
        if !self.gif_picker {
            self.online_gifs.close_picker();
            self.gif_previews = None;
            self.gif_textures.clear();
            self.gif_pending.clear();
            self.gif_errors.clear();
            self.gif_hover = None;
            self.gif_hover_pending = None;
            self.gif_hover_errors.clear();
            if !self.modal_open() && !self.expressions_open() {
                ctx.memory_mut(|memory| {
                    memory.request_focus(egui::Id::new(("composer-editor", self.current.clone())))
                });
            }
        }
    }

    fn community_dialog(&mut self, ctx: &egui::Context) {
        self.friend_community_dialog(ctx);
    }
}

fn release_capture(capture: Option<CaptureSession>) {
    if let Some(capture) = capture {
        capture.cancel();
        std::thread::spawn(move || drop(capture));
    }
}

impl Drop for RelayApp {
    fn drop(&mut self) {
        self.call_alerts.stop();
        self.hide_for_exit();
        self.save_preferences();
        self.stop_media();
        let (_, empty) = mpsc::sync_channel(1);
        let pending = std::mem::replace(&mut self.capture_rx, empty);
        std::thread::spawn(move || drop(pending));
        // Closing the window is not the account's explicit Log Out action.
        // Network's bounded shutdown preserves remembered logins and key files.
        self.network.finish_drop();
        crate::embedded_network::shutdown();
    }
}

#[cfg(test)]
fn decode_frame(bytes: &[u8]) -> Result<egui::ColorImage, image::ImageError> {
    let mut reader =
        image::io::Reader::with_format(std::io::Cursor::new(bytes), image::ImageFormat::Jpeg);
    let mut limits = image::io::Limits::default();
    limits.max_image_width = Some(1920);
    limits.max_image_height = Some(1080);
    limits.max_alloc = Some(16 * 1024 * 1024);
    reader.limits(limits);
    let img = reader.decode()?.to_rgba8();
    Ok(egui::ColorImage::from_rgba_unmultiplied(
        [img.width() as usize, img.height() as usize],
        img.as_raw(),
    ))
}

fn configure(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    style.visuals = egui::Visuals::dark();
    style.visuals.panel_fill = c_bg();
    style.visuals.window_fill = c_panel();
    style.visuals.extreme_bg_color = c_rail();
    style.visuals.override_text_color = Some(c_text());
    style.visuals.selection.bg_fill = Color32::from_rgb(70, 75, 113);
    style.visuals.selection.stroke = Stroke::new(1.0_f32, c_accent());
    style.visuals.widgets.inactive.bg_fill = c_card();
    style.visuals.widgets.inactive.weak_bg_fill = c_card();
    style.visuals.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, c_line());
    style.visuals.widgets.inactive.rounding = Rounding::same(7.0);
    style.visuals.widgets.hovered.bg_fill = Color32::from_rgb(65, 67, 74);
    style.visuals.widgets.hovered.rounding = Rounding::same(7.0);
    style.visuals.widgets.active.bg_fill = Color32::from_rgb(70, 75, 113);
    style.visuals.widgets.active.rounding = Rounding::same(7.0);
    style.visuals.window_rounding = Rounding::same(15.0);
    style.visuals.window_stroke = Stroke::new(1.0_f32, c_line());
    style.spacing.item_spacing = Vec2::new(8.0, 4.0);
    style.spacing.button_padding = Vec2::new(12.0, 8.0);
    style
        .text_styles
        .insert(egui::TextStyle::Body, FontId::proportional(font_size(14.0)));
    style.text_styles.insert(
        egui::TextStyle::Button,
        FontId::proportional(font_size(12.0)),
    );
    ctx.set_style(style);
    let mut fonts = egui::FontDefinitions::default();
    if let Ok(bytes) = std::fs::read("C:\\Windows\\Fonts\\segoeui.ttf") {
        fonts
            .font_data
            .insert("Segoe UI".into(), egui::FontData::from_owned(bytes));
        fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .insert(0, "Segoe UI".into());
    }
    let mut semibold = fonts.families[&egui::FontFamily::Proportional].clone();
    if let Ok(bytes) = std::fs::read("C:\\Windows\\Fonts\\seguisb.ttf") {
        fonts.font_data.insert(
            "Segoe UI Semibold".into(),
            egui::FontData::from_owned(bytes),
        );
        semibold.insert(0, "Segoe UI Semibold".into());
    }
    fonts
        .families
        .insert(egui::FontFamily::Name("Relay Semibold".into()), semibold);
    crate::emoji::install_fonts(&mut fonts);
    ctx.set_fonts(fonts);
}

/// The gold edge Discord gives a message which mentions you.
const MENTION_BAR: Color32 = Color32::from_rgb(240, 178, 50);

fn primary(text: &str) -> egui::Button<'_> {
    egui::Button::new(
        RichText::new(text)
            .color(UI_PALETTE.with(|palette| palette.get().accent_text))
            .family(egui::FontFamily::Name("Relay Semibold".into()))
            .strong(),
    )
    .fill(c_accent())
    .rounding(4.0)
    .min_size(Vec2::new(90.0, 34.0))
}
fn secondary(text: &str) -> egui::Button<'_> {
    egui::Button::new(RichText::new(text).color(c_text()))
        .fill(c_card())
        .stroke(Stroke::NONE)
        .rounding(4.0)
        .min_size(Vec2::new(70.0, 32.0))
}
fn tab(ui: &mut egui::Ui, label: &str, selected: bool) -> bool {
    ui.add(
        egui::Button::new(
            RichText::new(label)
                .color(if selected { c_accent() } else { c_muted() })
                .size(font_size(12.0)),
        )
        .fill(if selected {
            c_card()
        } else {
            Color32::TRANSPARENT
        })
        .stroke(Stroke::NONE)
        .rounding(6.0),
    )
    .clicked()
}
fn help_tip(ui: &mut egui::Ui, label: &str, explanation: &str) {
    let id = ui.make_persistent_id(("field-help", label));
    let response = ui
        .add(
            egui::Button::new(RichText::new("?").color(c_muted()))
                .small()
                .rounding(10.0)
                .min_size(Vec2::splat(20.0)),
        )
        .on_hover_text(explanation);
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Button, format!("About {label}"))
    });
    if response.clicked() {
        ui.memory_mut(|memory| memory.toggle_popup(id));
    }
    egui::popup_below_widget(ui, id, &response, |ui| {
        ui.set_max_width(ui.ctx().screen_rect().width().min(360.0) - 32.0);
        ui.add(egui::Label::new(explanation).wrap(true));
    });
}

/// Relay's semibold face, or the regular one where it is not loaded (tests).
fn semibold(ui: &egui::Ui, size: f32) -> FontId {
    let family = egui::FontFamily::Name("Relay Semibold".into());
    if ui.fonts(|fonts| fonts.families().contains(&family)) {
        FontId::new(size, family)
    } else {
        FontId::proportional(size)
    }
}

/// A labelled text field. Like Relay's browser sign-in, it has no question marks:
/// the label explains itself on hover, and the explanation is shown under the
/// field while it is being filled in.
fn field(ui: &mut egui::Ui, label: &str, value: &mut String, password: bool) -> egui::Response {
    ui.add_space(12.0);
    let help = match label {
            "SERVER ADDRESS OR INVITATION" => Some("Paste the invitation or server address from the community's host. An invitation can also fill in your invite code. The default 127.0.0.1 address points to this PC and works only after you start a local server."),
            "USERNAME" => Some("Your account belongs to this community. To sign in, enter the username you created on this server. To create an account, choose a new username; an account on another server does not sign you in here."),
            "PASSWORD" => Some("To sign in, use your password for this community. To create an account, choose a new password. Relay does not use your Windows or Discord login."),
            "INVITE CODE" | "INVITE CODE (IF REQUIRED)" => Some("The host supplies this code when the community requires an invitation. Using a Relay invitation fills it in for you. Leave it blank for the first account on a new server."),
            _ => None,
    };
    let caption = ui.label(
        RichText::new(label)
            .size(font_size(TEXT_CAPTION))
            .color(c_muted())
            .family(egui::FontFamily::Name("Relay Semibold".into()))
            .strong(),
    );
    if let Some(help) = help {
        caption.on_hover_text(help);
    }
    ui.add_space(2.0);
    let height = ui.spacing().interact_size.y.max(33.0);
    let padding = if height >= 44.0 {
        Vec2::new(12.0, 10.0)
    } else {
        Vec2::new(10.0, 8.0)
    };
    let mut response = ui.add_sized(
        [ui.available_width(), height],
        egui::TextEdit::singleline(value)
            .password(password)
            .margin(padding),
    );
    crate::accessible_input::apply(ui, &mut response, value, 16 * 1024);
    if let Some(help) = help.filter(|_| response.has_focus()) {
        ui.add_space(4.0);
        ui.add(
            egui::Label::new(
                RichText::new(help)
                    .size(font_size(TEXT_CAPTION))
                    .color(c_muted()),
            )
            .wrap(true),
        );
    }
    response
}
fn dot(ui: &mut egui::Ui, color: Color32, radius: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(radius * 2.0), Sense::hover());
    ui.painter().circle_filled(rect.center(), radius, color);
}
fn mark(ui: &mut egui::Ui, size: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    let c = rect.center();
    ui.painter()
        .circle_stroke(c, size * 0.32, Stroke::new(size * 0.055, c_accent()));
    ui.painter().circle_filled(
        c + Vec2::new(size * 0.23, -size * 0.23),
        size * 0.08,
        c_accent(),
    );
    ui.painter().circle_filled(
        c - Vec2::new(size * 0.23, -size * 0.23),
        size * 0.08,
        c_accent(),
    );
}
fn identity_color(name: &str) -> Color32 {
    let colors = [
        c_accent(),
        Color32::from_rgb(61, 221, 255),
        Color32::from_rgb(255, 188, 77),
        Color32::from_rgb(246, 95, 213),
    ];
    colors[name.bytes().fold(0usize, |a, b| a.wrapping_add(b as usize)) % colors.len()]
}
fn avatar(ui: &mut egui::Ui, name: &str, size: f32, active: bool) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    draw_avatar(ui.painter(), rect, name, active);
}
fn draw_avatar(painter: &egui::Painter, rect: Rect, name: &str, active: bool) {
    let size = rect.width();
    let color = identity_color(name);
    painter.circle_filled(rect.center(), size / 2.0, color.gamma_multiply(0.22));
    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        name.chars()
            .next()
            .unwrap_or('?')
            .to_uppercase()
            .to_string(),
        FontId::proportional(size * 0.42),
        c_text(),
    );
    if active {
        painter.circle_stroke(rect.center(), size / 2.0, Stroke::new(2.0_f32, c_good()));
        painter.circle_filled(
            rect.right_bottom() - Vec2::splat(size * 0.13),
            size * 0.12,
            c_good(),
        );
    }
}
fn channel_row(
    ui: &mut egui::Ui,
    name: &str,
    tooltip_identity: egui::Id,
    voice: bool,
    selected: bool,
    joined: bool,
    (unread, mentions): (u64, u64),
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), 34.0),
        Sense::click_and_drag(),
    );
    let r = rect.shrink2(Vec2::new(10.0, 1.0));
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, selected, name)
    });
    if selected || response.hovered() || response.has_focus() {
        ui.painter().rect_filled(
            r,
            6.0,
            if selected {
                UI_PALETTE.with(|palette| palette.get().selection)
            } else {
                c_card()
            },
        );
    }
    if voice {
        draw_icon(
            ui.painter(),
            Rect::from_center_size(Pos2::new(r.left() + 21.0, r.center().y), Vec2::splat(14.0)),
            Icon::Headphones,
            if joined { c_accent() } else { c_muted() },
        );
    } else {
        ui.painter().text(
            Pos2::new(r.left() + 14.0, r.center().y),
            Align2::LEFT_CENTER,
            "#",
            FontId::proportional(font_size(18.0)),
            c_muted(),
        );
    }
    row_name(
        ui,
        name,
        Rect::from_min_max(
            Pos2::new(r.left() + 40.0, r.top()),
            Pos2::new(r.right() - if unread > 0 { 42.0 } else { 8.0 }, r.bottom()),
        ),
        14.0,
        if selected || unread > 0 {
            c_text()
        } else {
            c_muted()
        },
    );
    if unread > 0 {
        unread_badge(
            ui,
            Pos2::new(r.right() - 20.0, r.center().y),
            if mentions > 0 { mentions } else { unread },
            mentions > 0,
        );
    }
    // After 500 ms of hover, beside the row (R6.1); not egui's global delay.
    sidebar_tooltip::delayed_row_tooltip(ui, &response, tooltip_identity, name);
    response
}
/// The hover-timer identity of a channel row: its community and channel id,
/// never its display name or position in the list.
fn channel_tooltip_identity(server_id: &str, channel_id: &str) -> egui::Id {
    egui::Id::new(("channel-row-tooltip", server_id, channel_id))
}
fn user_row(
    ui: &mut egui::Ui,
    user: &User,
    selected: bool,
    unread: u64,
    mentions: u64,
    texture: Option<&egui::TextureHandle>,
) -> egui::Response {
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 40.0), Sense::click());
    response.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::WidgetType::SelectableLabel,
            selected,
            profile_ui::display_name(user),
        )
    });
    if selected || response.hovered() || response.has_focus() {
        ui.painter().rect_filled(rect, 5.0, c_card());
    }
    profile_ui::paint_user_avatar(
        ui,
        Rect::from_center_size(
            Pos2::new(rect.left() + 21.0, rect.center().y),
            Vec2::splat(28.0),
        ),
        user,
        texture,
        false,
    );
    row_name(
        ui,
        profile_ui::display_name(user),
        Rect::from_min_max(
            Pos2::new(rect.left() + 44.0, rect.top()),
            Pos2::new(
                rect.right() - if unread > 0 { 42.0 } else { 8.0 },
                rect.bottom(),
            ),
        ),
        14.0,
        if selected || unread > 0 {
            c_text()
        } else {
            c_muted()
        },
    );
    if unread > 0 {
        unread_badge(
            ui,
            Pos2::new(rect.right() - 20.0, rect.center().y),
            if mentions > 0 { mentions } else { unread },
            mentions > 0,
        );
    }
    response.on_hover_text(&user.username)
}
fn unread_badge(ui: &egui::Ui, center: Pos2, count: u64, mentioned: bool) {
    let rect = Rect::from_center_size(
        center,
        Vec2::new(if count > 99 { 30.0 } else { 24.0 }, 18.0),
    );
    ui.painter()
        .rect_filled(rect, 9.0, if mentioned { c_bad() } else { c_accent() });
    ui.painter().text(
        center,
        Align2::CENTER_CENTER,
        if count > 99 {
            "99+".into()
        } else {
            count.to_string()
        },
        FontId::proportional(font_size(10.0)),
        Color32::WHITE,
    );
}
/// A sidebar navigation row with an icon, laid out like `channel_row`. The
/// persistent `id` keeps its response and focus stable when rows above change.
fn icon_nav_row(
    ui: &mut egui::Ui,
    id: impl std::hash::Hash,
    shape: Icon,
    label: &str,
    selected: bool,
) -> egui::Response {
    let (_, rect) = ui.allocate_space(Vec2::new(ui.available_width(), 34.0));
    let response = ui
        .interact(rect, ui.make_persistent_id(id), Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    let r = rect.shrink2(Vec2::new(10.0, 1.0));
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, selected, label)
    });
    if selected || response.hovered() || response.has_focus() {
        ui.painter().rect_filled(
            r,
            6.0,
            if selected {
                UI_PALETTE.with(|palette| palette.get().selection)
            } else {
                c_card()
            },
        );
    }
    draw_icon(
        ui.painter(),
        Rect::from_center_size(Pos2::new(r.left() + 21.0, r.center().y), Vec2::splat(14.0)),
        shape,
        if selected { c_text() } else { c_muted() },
    );
    row_name(
        ui,
        label,
        Rect::from_min_max(
            Pos2::new(r.left() + 40.0, r.top()),
            Pos2::new(r.right() - 8.0, r.bottom()),
        ),
        14.0,
        if selected { c_text() } else { c_muted() },
    );
    response
}
#[cfg(test)]
mod icon_nav_row_tests {
    use super::*;

    /// One frame of the Events row in a sidebar-width panel, below `above`
    /// thread rows (as `threads_navigation` draws them first).
    fn frame_below(
        ctx: &egui::Context,
        above: usize,
        events: Vec<egui::Event>,
    ) -> (Rect, bool, egui::Id) {
        let mut result = (Rect::NOTHING, false, egui::Id::NULL);
        let _ = ctx.run(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(940.0, 650.0))),
                events,
                ..Default::default()
            },
            |ctx| {
                egui::SidePanel::left("nav-test")
                    .exact_width(240.0)
                    .show(ctx, |ui| {
                        for index in 0..above {
                            let _ = ui.selectable_label(false, format!("Thread {index}"));
                        }
                        let response =
                            icon_nav_row(ui, "server-events", Icon::Calendar, "Events", false);
                        result = (response.rect, response.clicked(), response.id);
                    });
            },
        );
        result
    }

    fn frame(ctx: &egui::Context, events: Vec<egui::Event>) -> (Rect, bool) {
        let (rect, clicked, _) = frame_below(ctx, 0, events);
        (rect, clicked)
    }

    #[test]
    fn events_keeps_its_id_and_focus_when_rows_above_change() {
        let ctx = egui::Context::default();
        configure(&ctx);
        let (_, _, id) = frame_below(&ctx, 0, Vec::new());
        ctx.memory_mut(|memory| memory.request_focus(id));
        frame_below(&ctx, 0, Vec::new());
        for above in [1, 3, 0] {
            let (_, _, current) = frame_below(&ctx, above, Vec::new());
            assert_eq!(current, id, "{above} rows above changed the Events id");
            assert!(
                ctx.memory(|memory| memory.has_focus(id)),
                "{above} rows above"
            );
        }
    }

    #[test]
    fn events_row_opens_from_its_icon_and_its_label() {
        let ctx = egui::Context::default();
        configure(&ctx);
        let (row, _) = frame(&ctx, Vec::new());
        assert!(row.height() >= 34.0 && row.width() > 200.0, "{row:?}");
        // The calendar icon sits at the row's left like channel icons; the
        // label follows. Both are part of the one clickable row.
        for at in [
            Pos2::new(row.left() + 31.0, row.center().y),
            Pos2::new(row.left() + 70.0, row.center().y),
        ] {
            let press = |pressed| egui::Event::PointerButton {
                pos: at,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            };
            frame(&ctx, vec![egui::Event::PointerMoved(at), press(true)]);
            let (_, clicked) = frame(&ctx, vec![press(false)]);
            assert!(clicked, "click at {at:?}");
        }
    }
}

fn row_name(ui: &egui::Ui, name: &str, rect: Rect, size: f32, color: Color32) {
    let mut text = egui::text::LayoutJob::simple(
        name.to_owned(),
        FontId::proportional(font_size(size)),
        color,
        rect.width().max(0.0),
    );
    text.wrap.max_rows = 1;
    text.wrap.break_anywhere = true;
    let galley = ui.painter().layout_job(text);
    let pos = Pos2::new(rect.left(), rect.center().y - galley.size().y / 2.0);
    ui.painter().with_clip_rect(rect).galley(pos, galley, color);
}
fn display_time(time: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(time)
        .map(|value| {
            value
                .with_timezone(&chrono::Local)
                .format("%H:%M")
                .to_string()
        })
        .unwrap_or_else(|_| time.to_string())
}

fn message_day(time: &str) -> Option<chrono::NaiveDate> {
    chrono::DateTime::parse_from_rfc3339(time)
        .ok()
        .map(|t| t.with_timezone(&chrono::Local).date_naive())
}
fn day_label(time: &str) -> String {
    let Some(day) = message_day(time) else {
        return "Messages".into();
    };
    let today = chrono::Local::now().date_naive();
    if day == today {
        "Today".into()
    } else if today.signed_duration_since(day).num_days() == 1 {
        "Yesterday".into()
    } else {
        day.format("%d %B %Y").to_string()
    }
}
fn group_messages(previous: &Message, current: &Message) -> bool {
    if previous.author.id != current.author.id
        || message_day(&previous.created_at) != message_day(&current.created_at)
    {
        return false;
    }
    match (
        chrono::DateTime::parse_from_rfc3339(&previous.created_at),
        chrono::DateTime::parse_from_rfc3339(&current.created_at),
    ) {
        (Ok(previous), Ok(current)) => (chrono::Duration::zero()..=chrono::Duration::seconds(300))
            .contains(&current.signed_duration_since(previous)),
        _ => false,
    }
}
fn size_label(size: u64) -> String {
    if size >= 1_048_576 {
        format!("{:.1} MB", size as f64 / 1_048_576.0)
    } else {
        format!("{} KB", size.div_ceil(1024))
    }
}

fn capture_only_input(input: &mut egui::RawInput) {
    input
        .events
        .retain(|event| matches!(event, egui::Event::Screenshot { .. }));
    input.modifiers = egui::Modifiers::NONE;
    input.focused = false;
    input.hovered_files.clear();
    input.dropped_files.clear();
}

// Appearance, drafts and community changes must not commit unapplied voice edits.
fn preferences_snapshot(
    prefs: &Preferences,
    applied_audio: &AudioSettings,
) -> serde_json::Result<Vec<u8>> {
    let mut stored = prefs.clone();
    stored.audio = applied_audio.clone();
    serde_json::to_vec(&stored)
}

fn chat_scroll_intent(input: &egui::InputState, rect: Rect) -> bool {
    let hovered = input
        .pointer
        .hover_pos()
        .filter(|position| rect.contains(*position));
    let wheel = hovered.is_some()
        && (input.raw_scroll_delta != Vec2::ZERO || input.smooth_scroll_delta != Vec2::ZERO);
    let scrollbar_press = hovered.is_some_and(|position| position.x >= rect.right() - 16.0)
        && input.pointer.any_pressed();
    let dragging = input
        .pointer
        .press_origin()
        .is_some_and(|position| rect.contains(position))
        && input.pointer.is_decidedly_dragging()
        && input.pointer.delta().y.abs() > 0.1;
    wheel || scrollbar_press || dragging
}

fn save_draft_scope(
    prefs: &mut Preferences,
    ram: &mut ScopedDrafts,
    scope: &str,
    drafts: Vec<(Target, Draft)>,
) {
    ram.store(scope, drafts.clone());
    if prefs.remember_drafts {
        prefs.conversations.save_drafts(scope, drafts);
    } else {
        prefs.conversations.clear_drafts();
    }
}

fn codec_options_ui(
    ui: &mut egui::Ui,
    id: &str,
    label: &str,
    codec: &mut VideoCodecPreference,
) -> bool {
    let mut changed = false;
    ui.label(RichText::new(label).size(font_size(12.0)).color(c_muted()));
    egui::ComboBox::from_id_source(id)
        .selected_text(match codec {
            VideoCodecPreference::Auto => "Automatic",
            VideoCodecPreference::Software => "Software VP8",
            VideoCodecPreference::Hardware => "Hardware H264 (required)",
        })
        .show_ui(ui, |ui| {
            for (value, label) in [
                (VideoCodecPreference::Auto, "Automatic"),
                (VideoCodecPreference::Software, "Software VP8"),
                (VideoCodecPreference::Hardware, "Hardware H264 (required)"),
            ] {
                changed |= ui.selectable_value(codec, value, label).changed();
            }
        });
    changed
}

fn target_available(target: &Target, bootstrap: &Bootstrap) -> bool {
    match target {
        Target::Channel(id) => bootstrap.channels.iter().any(|channel| &channel.id == id),
        Target::Direct(id) => bootstrap
            .members
            .iter()
            .any(|member| &member.id == id && member.id != bootstrap.user.id),
    }
}

fn mentioned_users(content: &str, members: &[User]) -> Vec<String> {
    composer_ui::mentioned_users(content, members)
}

#[derive(Clone, Copy)]
enum Icon {
    Filter,
    Plus,
    Pin,
    Mic,
    Headphones,
    Settings,
    Users,
    Send,
    HangUp,
    Chevron,
    Chat,
    More,
    Close,
    Smile,
    Sticker,
    Search,
    Edit,
    Reply,
    Inbox,
    Camera,
    Screen,
    Calendar,
    Megaphone,
}
fn message_toolbar_position(row: Rect, clip: Rect, width: f32) -> Pos2 {
    let left = clip.left() + 2.0;
    let top = clip.top() + 2.0;
    Pos2::new(
        (row.right() - width - 12.0).clamp(left, (clip.right() - width - 6.0).max(left)),
        (row.top() - 12.0).clamp(top, (clip.bottom() - 42.0).max(top)),
    )
}

fn icon_button(ui: &mut egui::Ui, shape: Icon, selected: bool, tooltip: &str) -> egui::Response {
    icon_button_sized(ui, 30.0, shape, selected, tooltip)
}

fn icon_button_sized(
    ui: &mut egui::Ui,
    size: f32,
    shape: Icon,
    selected: bool,
    tooltip: &str,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());
    if response.hovered() || selected || response.has_focus() {
        ui.painter().rect_filled(rect, 7.0, c_card());
    }
    if response.has_focus() {
        ui.painter()
            .rect_stroke(rect, 7.0, Stroke::new(1.0_f32, c_accent()));
    }
    draw_icon(
        ui.painter(),
        rect.shrink((size - 16.0) * 0.5),
        shape,
        if selected && matches!(shape, Icon::Mic | Icon::Headphones) {
            c_bad()
        } else if selected {
            c_accent()
        } else {
            c_muted()
        },
    );
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, tooltip));
    response.on_hover_text(tooltip)
}
fn icon(ui: &mut egui::Ui, shape: Icon, size: f32, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    draw_icon(ui.painter(), rect.shrink(size * 0.15), shape, color);
}
fn draw_icon(p: &egui::Painter, r: Rect, shape: Icon, color: Color32) {
    let s = Stroke::new(1.6_f32, color);
    let c = r.center();
    let w = r.width();
    let point = |x: f32, y: f32| Pos2::new(r.left() + x * w, r.top() + y * w);
    match shape {
        Icon::Filter => {
            for (y, x) in [(0.2, 0.3), (0.5, 0.7), (0.8, 0.4)] {
                p.line_segment([point(0.1, y), point(0.9, y)], s);
                p.circle_filled(point(x, y), w * 0.1, color);
            }
        }
        Icon::Search => {
            p.circle_stroke(point(0.42, 0.42), w * 0.28, s);
            p.line_segment([point(0.62, 0.62), point(0.9, 0.9)], s);
        }
        Icon::Sticker => {
            p.add(egui::Shape::closed_line(
                vec![
                    point(0.18, 0.12),
                    point(0.82, 0.12),
                    point(0.82, 0.58),
                    point(0.54, 0.88),
                    point(0.18, 0.88),
                ],
                s,
            ));
            p.line_segment([point(0.54, 0.88), point(0.54, 0.58)], s);
            p.line_segment([point(0.54, 0.58), point(0.82, 0.58)], s);
            p.circle_filled(point(0.37, 0.36), 1.0, color);
            p.circle_filled(point(0.61, 0.36), 1.0, color);
        }
        Icon::Pin => {
            p.add(egui::Shape::line(
                vec![
                    point(0.5, 0.08),
                    point(0.76, 0.27),
                    point(0.65, 0.55),
                    point(0.76, 0.67),
                    point(0.25, 0.67),
                    point(0.36, 0.55),
                    point(0.24, 0.27),
                    point(0.5, 0.08),
                    point(0.5, 0.92),
                ],
                s,
            ));
        }
        Icon::Close => {
            p.line_segment([point(0.2, 0.2), point(0.8, 0.8)], s);
            p.line_segment([point(0.8, 0.2), point(0.2, 0.8)], s);
        }
        Icon::Chevron => {
            p.line_segment([point(0.18, 0.36), point(0.5, 0.68)], s);
            p.line_segment([point(0.5, 0.68), point(0.82, 0.36)], s);
        }
        Icon::Reply => {
            p.add(egui::Shape::line(
                vec![point(0.4, 0.14), point(0.1, 0.42), point(0.4, 0.7)],
                s,
            ));
            p.add(egui::Shape::line(
                vec![
                    point(0.12, 0.42),
                    point(0.58, 0.42),
                    point(0.83, 0.62),
                    point(0.88, 0.9),
                ],
                s,
            ));
        }
        Icon::More => {
            for x in [0.2, 0.5, 0.8] {
                p.circle_filled(point(x, 0.5), w * 0.075, color);
            }
        }
        Icon::Chat => {
            p.add(egui::Shape::closed_line(
                vec![
                    point(0.08, 0.08),
                    point(0.92, 0.08),
                    point(0.92, 0.72),
                    point(0.44, 0.72),
                    point(0.2, 0.92),
                    point(0.2, 0.72),
                    point(0.08, 0.72),
                ],
                s,
            ));
            p.line_segment([point(0.28, 0.3), point(0.72, 0.3)], s);
            p.line_segment([point(0.28, 0.5), point(0.62, 0.5)], s);
        }
        Icon::Smile => {
            p.circle_stroke(c, w * 0.44, s);
            p.circle_filled(point(0.35, 0.38), w * 0.045, color);
            p.circle_filled(point(0.65, 0.38), w * 0.045, color);
            p.add(egui::Shape::line(
                vec![
                    point(0.28, 0.61),
                    point(0.38, 0.72),
                    point(0.62, 0.72),
                    point(0.72, 0.61),
                ],
                s,
            ));
        }
        Icon::Edit => {
            p.add(egui::Shape::closed_line(
                vec![
                    point(0.13, 0.7),
                    point(0.13, 0.91),
                    point(0.34, 0.91),
                    point(0.91, 0.34),
                    point(0.7, 0.13),
                ],
                s,
            ));
            p.line_segment([point(0.6, 0.24), point(0.8, 0.44)], s);
        }
        Icon::Plus => {
            p.line_segment([point(0.1, 0.5), point(0.9, 0.5)], s);
            p.line_segment([point(0.5, 0.1), point(0.5, 0.9)], s);
        }
        Icon::Mic => {
            p.rect_stroke(
                Rect::from_min_max(point(0.34, 0.05), point(0.66, 0.61)),
                w * 0.16,
                s,
            );
            p.line_segment([point(0.18, 0.44), point(0.18, 0.66)], s);
            p.line_segment([point(0.18, 0.66), point(0.5, 0.77)], s);
            p.line_segment([point(0.5, 0.77), point(0.82, 0.66)], s);
            p.line_segment([point(0.82, 0.66), point(0.82, 0.44)], s);
            p.line_segment([point(0.5, 0.77), point(0.5, 0.97)], s);
        }
        Icon::Headphones => {
            p.add(egui::Shape::line(
                vec![
                    point(0.1, 0.7),
                    point(0.1, 0.36),
                    point(0.23, 0.16),
                    point(0.5, 0.08),
                    point(0.77, 0.16),
                    point(0.9, 0.36),
                    point(0.9, 0.7),
                ],
                s,
            ));
            p.rect_stroke(
                Rect::from_min_max(point(0.08, 0.5), point(0.29, 0.92)),
                2.0,
                s,
            );
            p.rect_stroke(
                Rect::from_min_max(point(0.71, 0.5), point(0.92, 0.92)),
                2.0,
                s,
            );
        }
        Icon::Settings => {
            p.circle_stroke(c, w * 0.2, s);
            p.circle_stroke(c, w * 0.42, s);
            for i in 0..8 {
                let angle = i as f32 * std::f32::consts::TAU / 8.0;
                let v = Vec2::angled(angle);
                p.line_segment([c + v * w * 0.4, c + v * w * 0.51], s);
            }
        }
        Icon::Users => {
            p.circle_stroke(point(0.38, 0.27), w * 0.18, s);
            p.add(egui::Shape::line(
                vec![
                    point(0.03, 0.93),
                    point(0.12, 0.64),
                    point(0.38, 0.55),
                    point(0.64, 0.64),
                    point(0.72, 0.93),
                ],
                s,
            ));
            p.circle_stroke(point(0.8, 0.33), w * 0.12, s);
            p.line_segment([point(0.82, 0.58), point(0.98, 0.89)], s);
        }
        Icon::Send => {
            p.add(egui::Shape::closed_line(
                vec![
                    point(0.05, 0.05),
                    point(0.95, 0.5),
                    point(0.05, 0.95),
                    point(0.22, 0.5),
                ],
                s,
            ));
            p.line_segment([point(0.22, 0.5), point(0.7, 0.5)], s);
        }
        Icon::Megaphone => {
            p.add(egui::Shape::closed_line(
                vec![
                    point(0.08, 0.38),
                    point(0.3, 0.38),
                    point(0.86, 0.1),
                    point(0.86, 0.9),
                    point(0.3, 0.62),
                    point(0.08, 0.62),
                ],
                s,
            ));
            p.line_segment([point(0.2, 0.62), point(0.3, 0.92)], s);
        }
        Icon::Inbox => {
            p.add(egui::Shape::closed_line(
                vec![
                    point(0.08, 0.1),
                    point(0.92, 0.1),
                    point(0.92, 0.85),
                    point(0.08, 0.85),
                ],
                s,
            ));
            p.add(egui::Shape::line(
                vec![
                    point(0.08, 0.58),
                    point(0.32, 0.58),
                    point(0.38, 0.71),
                    point(0.62, 0.71),
                    point(0.68, 0.58),
                    point(0.92, 0.58),
                ],
                s,
            ));
        }
        Icon::Camera => {
            p.rect_stroke(
                Rect::from_min_max(point(0.05, 0.2), point(0.67, 0.8)),
                2.0,
                s,
            );
            p.add(egui::Shape::closed_line(
                vec![
                    point(0.67, 0.38),
                    point(0.95, 0.22),
                    point(0.95, 0.78),
                    point(0.67, 0.62),
                ],
                s,
            ));
        }
        Icon::Calendar => {
            p.rect_stroke(
                Rect::from_min_max(point(0.1, 0.18), point(0.9, 0.9)),
                2.0,
                s,
            );
            p.line_segment([point(0.1, 0.4), point(0.9, 0.4)], s);
            for x in [0.32, 0.68] {
                p.line_segment([point(x, 0.06), point(x, 0.28)], s);
            }
        }
        Icon::Screen => {
            p.rect_stroke(
                Rect::from_min_max(point(0.06, 0.08), point(0.94, 0.72)),
                2.0,
                s,
            );
            p.line_segment([point(0.5, 0.72), point(0.5, 0.94)], s);
            p.line_segment([point(0.27, 0.94), point(0.73, 0.94)], s);
            p.line_segment([point(0.5, 0.54), point(0.5, 0.26)], s);
            p.add(egui::Shape::line(
                vec![point(0.36, 0.4), point(0.5, 0.26), point(0.64, 0.4)],
                s,
            ));
        }
        Icon::HangUp => {
            // A horizontal handset with downward earpieces, shared by both call controls.
            p.add(egui::Shape::line(
                vec![
                    point(0.08, 0.55),
                    point(0.18, 0.43),
                    point(0.33, 0.35),
                    point(0.5, 0.32),
                    point(0.67, 0.35),
                    point(0.82, 0.43),
                    point(0.92, 0.55),
                ],
                Stroke::new(w * 0.13, color),
            ));
            for flip in [false, true] {
                let ear = |x: f32, y: f32| point(if flip { 1.0 - x } else { x }, y);
                p.add(egui::Shape::convex_polygon(
                    vec![
                        ear(0.04, 0.5),
                        ear(0.18, 0.41),
                        ear(0.26, 0.48),
                        ear(0.27, 0.65),
                        ear(0.08, 0.73),
                        ear(0.03, 0.67),
                    ],
                    color,
                    Stroke::NONE,
                ));
            }
        }
    }
}

fn consume_send_key(input: &mut egui::InputState) -> bool {
    let mut consumed = false;
    input.events.retain(|event| {
        let send = matches!(
            event,
            egui::Event::Key {
                key: egui::Key::Enter,
                pressed: true,
                modifiers,
                ..
            } if *modifiers == egui::Modifiers::NONE
        );
        consumed |= send && matches!(event, egui::Event::Key { repeat: false, .. });
        !send
    });
    consumed
}

fn call_control(
    ui: &mut egui::Ui,
    shape: Icon,
    selected: bool,
    disconnect: bool,
    label: &str,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(46.0), Sense::click());
    let active_media = selected && matches!(shape, Icon::Camera | Icon::Screen);
    let fill = if disconnect || (selected && !active_media) {
        c_bad()
    } else if active_media {
        c_good()
    } else {
        c_card()
    };
    ui.painter().circle_filled(
        rect.center(),
        23.0,
        if response.hovered() {
            fill.gamma_multiply(0.8)
        } else {
            fill
        },
    );
    if response.has_focus() {
        ui.painter()
            .circle_stroke(rect.center(), 22.0, Stroke::new(2.0_f32, c_accent()));
    }
    draw_icon(
        ui.painter(),
        rect.shrink(13.0),
        shape,
        if disconnect || selected {
            Color32::WHITE
        } else {
            c_text()
        },
    );
    if selected && matches!(shape, Icon::Mic | Icon::Headphones) {
        let r = rect.shrink(12.0);
        ui.painter().line_segment(
            [r.left_top(), r.right_bottom()],
            Stroke::new(2.0_f32, Color32::WHITE),
        );
    }
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, label));
    response.on_hover_text(label)
}

#[cfg(test)]
#[path = "app_tests.rs"]
mod tests;

#[cfg(all(test, windows))]
#[path = "background_alert_native_fixture.rs"]
mod background_alert_native_fixture;

#[path = "preview_cache_ui.rs"]
mod preview_cache_ui;

fn legacy_audio_settings() -> AudioSettings {
    AudioSettings {
        adaptive_jitter: false,
        ..Default::default()
    }
}
