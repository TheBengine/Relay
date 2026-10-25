use super::*;
use crate::network::{avatar_attachment_id, ProfileAvatarChange};
use crate::settings_ui as controls;

#[path = "dialog_header_ui.rs"]
pub(super) mod dialog_header_ui;

pub(super) fn display_name(user: &User) -> &str {
    if user.profile.display_name.trim().is_empty() {
        &user.username
    } else {
        &user.profile.display_name
    }
}

#[derive(Clone, Default, PartialEq, Eq)]
struct ProfileDraft {
    display_name: String,
    bio: String,
    appearance: ProfileAppearance,
    avatar: ProfileAvatarChange,
}

struct Editor {
    scope: String,
    baseline: User,
    draft: ProfileDraft,
    pending: Option<(u64, ProfileDraft)>,
    preview: Option<egui::TextureHandle>,
    preview_rx: Option<Receiver<Result<egui::ColorImage, String>>>,
}

impl Editor {
    fn new(scope: String, user: User) -> Self {
        Self {
            scope,
            draft: ProfileDraft {
                display_name: user.profile.display_name.clone(),
                bio: user.profile.bio.clone(),
                appearance: user.profile.appearance.clone(),
                avatar: ProfileAvatarChange::Keep,
            },
            baseline: user,
            pending: None,
            preview: None,
            preview_rx: None,
        }
    }
    fn dirty(&self) -> bool {
        self.draft.display_name != self.baseline.profile.display_name
            || self.draft.bio != self.baseline.profile.bio
            || self.draft.appearance != self.baseline.profile.appearance
            || self.draft.avatar != ProfileAvatarChange::Keep
    }
    fn valid(&self) -> bool {
        self.draft.display_name.trim().chars().count() <= 32
            && self.draft.bio.chars().count() <= 190
            && self.draft.appearance.valid()
    }
    fn apply(&mut self, user: User, submitted: Option<ProfileDraft>, complete: bool) {
        if user.profile.version < self.baseline.profile.version {
            return;
        }
        if let Some(submitted) = submitted {
            if self.draft.display_name == submitted.display_name {
                self.draft.display_name = user.profile.display_name.clone();
            }
            if self.draft.bio == submitted.bio {
                self.draft.bio = user.profile.bio.clone();
            }
            if self.draft.appearance == submitted.appearance {
                self.draft.appearance = user.profile.appearance.clone();
            }
            if complete && self.draft.avatar == submitted.avatar {
                self.draft.avatar = ProfileAvatarChange::Keep;
                self.preview = None;
                self.preview_rx = None;
            }
        } else if !self.dirty() {
            self.draft.display_name = user.profile.display_name.clone();
            self.draft.bio = user.profile.bio.clone();
            self.draft.appearance = user.profile.appearance.clone();
        }
        self.baseline = user;
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum ProfileSection {
    #[default]
    Identity,
    Avatar,
    Banner,
    Colors,
    Effects,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum SelfMenuPage {
    #[default]
    Profile,
    Status,
    Accounts,
    CustomStatus,
}

#[derive(Default)]
pub(super) struct ProfileUi {
    editor: Option<Editor>,
    section: ProfileSection,
    pub(super) menu_anchor: Option<Rect>,
    menu_page: SelfMenuPage,
    capture_menu: Option<SelfMenuPage>,
    avatars: HashMap<String, (egui::TextureHandle, Instant)>,
    avatar_pending: HashSet<String>,
    avatar_failed: HashSet<String>,
    pub(super) card: Option<User>,
    pub(super) card_request: Option<u64>,
}

impl ProfileUi {
    pub(super) fn clear_session(&mut self) {
        *self = Self::default();
    }
    pub(super) fn request_failed(&mut self, request_id: u64) -> bool {
        if let Some(editor) = &mut self.editor {
            if editor
                .pending
                .as_ref()
                .is_some_and(|(id, _)| *id == request_id)
            {
                editor.pending = None;
                return true;
            }
        }
        if self.card_request == Some(request_id) {
            self.card_request = None;
            return true;
        }
        false
    }
    pub(super) fn saved(&mut self, url: &str, user: User, request_id: u64, complete: bool) -> bool {
        let Some(editor) = &mut self.editor else {
            return false;
        };
        if editor.scope != format!("{url}|{}", user.id)
            || !editor
                .pending
                .as_ref()
                .is_some_and(|(id, _)| *id == request_id)
        {
            return false;
        }
        let (_, submitted) = editor.pending.take().unwrap();
        editor.apply(user, Some(submitted), complete);
        true
    }
    pub(super) fn fetched_avatar(
        &mut self,
        ctx: &egui::Context,
        key: String,
        media: crate::chat_media::DecodedMedia,
    ) {
        if !self.avatar_pending.remove(&key) {
            return;
        }
        let Some(frame) = media.frames.first() else {
            return;
        };
        if frame.image.size[0] > 256 || frame.image.size[1] > 256 || media.frames.len() != 1 {
            self.avatar_failed.insert(key);
            return;
        }
        while self.avatars.len() >= 64 {
            let oldest = self
                .avatars
                .iter()
                .min_by_key(|(_, (_, at))| at)
                .map(|(key, _)| key.clone())
                .unwrap();
            self.avatars.remove(&oldest);
        }
        self.avatars.insert(
            key.clone(),
            (
                ctx.load_texture(key, frame.image.clone(), egui::TextureOptions::LINEAR),
                Instant::now(),
            ),
        );
    }
    pub(super) fn failed_avatar(&mut self, key: String) {
        self.avatar_pending.remove(&key);
        if self.avatar_failed.len() >= 64 {
            self.avatar_failed.clear();
        }
        self.avatar_failed.insert(key);
    }
}

impl RelayApp {
    pub(super) fn profile_capture_panel(&mut self, panel: &str) {
        if self.capture_path.is_none() {
            return;
        }
        match panel {
            "self-profile" => self.profile_ui.capture_menu = Some(SelfMenuPage::Profile),
            "profile-status" => self.profile_ui.capture_menu = Some(SelfMenuPage::Status),
            "profile-accounts" => self.profile_ui.capture_menu = Some(SelfMenuPage::Accounts),
            "profile-custom-status" => {
                self.profile_ui.capture_menu = Some(SelfMenuPage::CustomStatus)
            }
            "profile-about" | "profile-avatar" | "profile-banner" | "profile-colors"
            | "profile-effects" => {
                self.settings_open = true;
                self.settings_tab = SettingsTab::Profiles;
                self.profile_ui.section = match panel {
                    "profile-avatar" => ProfileSection::Avatar,
                    "profile-banner" => ProfileSection::Banner,
                    "profile-colors" => ProfileSection::Colors,
                    "profile-effects" => ProfileSection::Effects,
                    _ => ProfileSection::Identity,
                };
            }
            _ => {}
        }
    }

    pub(super) fn self_profile_button(&mut self, ui: &mut egui::Ui, user: &User) {
        let width = (ui.available_width() - 122.0).max(38.0);
        let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 36.0), Sense::click());
        response.widget_info(|| {
            egui::WidgetInfo::labeled(
                egui::WidgetType::Button,
                "Your profile, status and accounts",
            )
        });
        if let Some(page) = self.profile_ui.capture_menu.take() {
            self.profile_ui.menu_anchor = Some(rect);
            self.profile_ui.menu_page = page;
        }
        if response.hovered() || self.profile_ui.menu_anchor.is_some() {
            ui.painter().rect_filled(rect, 5.0, c_card());
        }
        let avatar = Rect::from_min_size(rect.min + Vec2::new(2.0, 2.0), Vec2::splat(32.0));
        let texture = self.avatar_texture(user);
        paint_user_avatar(ui, avatar, user, texture.as_ref(), false);
        let own = self.own_presence();
        paint_status(
            ui,
            avatar.right_bottom() - Vec2::splat(3.0),
            &own.status,
            c_panel(),
        );
        let painter = ui.painter().with_clip_rect(rect);
        let name = controls::single_line(
            ui,
            display_name(user),
            13.0,
            c_text(),
            (width - 46.0).max(1.0),
        );
        painter.galley(rect.min + Vec2::new(42.0, 3.0), name, c_text());
        painter.text(
            rect.min + Vec2::new(42.0, 21.0),
            Align2::LEFT_TOP,
            status_name(&own.status),
            egui::FontId::proportional(font_size(10.0)),
            c_muted(),
        );
        if response
            .on_hover_text("Your profile, status and accounts")
            .clicked()
        {
            self.profile_ui.menu_anchor = if self.profile_ui.menu_anchor.is_some() {
                None
            } else {
                Some(rect)
            };
            self.profile_ui.menu_page = SelfMenuPage::Profile;
        }
    }

    fn own_presence(&self) -> UserPresence {
        self.bootstrap
            .as_ref()
            .and_then(|b| b.presence.iter().find(|p| p.user_id == b.user.id))
            .cloned()
            .unwrap_or_else(|| UserPresence {
                user_id: self
                    .session
                    .as_ref()
                    .map(|s| s.user.id.clone())
                    .unwrap_or_default(),
                status: "online".into(),
                ..Default::default()
            })
    }

    fn self_profile_menu(&mut self, ctx: &egui::Context) {
        let Some(anchor) = self.profile_ui.menu_anchor else {
            return;
        };
        let Some(user) = self.session.as_ref().map(|s| s.user.clone()) else {
            self.profile_ui.menu_anchor = None;
            return;
        };
        if self.settings_open || self.add_community_open {
            self.profile_ui.menu_anchor = None;
            return;
        }
        let screen = ctx.screen_rect().shrink(8.0);
        let width = 304.0_f32.min(screen.width());
        let pos = Pos2::new(
            anchor
                .left()
                .clamp(screen.left(), (screen.right() - width).max(screen.left())),
            anchor.top() - 6.0,
        );
        let page = self.profile_ui.menu_page;
        let mut close = false;
        let mut next = None;
        let shown = egui::Window::new("Your Profile")
            .id(egui::Id::new("self-profile-menu"))
            .title_bar(false)
            .collapsible(false)
            .resizable(false)
            .movable(false)
            .pivot(Align2::LEFT_BOTTOM)
            .fixed_pos(pos)
            .constrain(false)
            .default_width(width)
            .min_width(width)
            .max_width(width)
            .default_height((pos.y - screen.top() - 20.0).max(160.0))
            .min_height(0.0)
            .max_height((pos.y - screen.top() - 20.0).max(160.0))
            .vscroll(true)
            .frame(
                egui::Frame::none()
                    .fill(c_panel())
                    .rounding(10.0)
                    .stroke(Stroke::new(1.0_f32, c_card()))
                    .inner_margin(Margin::same(10.0)),
            )
            .show(ctx, |ui| {
                ui.set_width(width - 20.0);
                if page != SelfMenuPage::Profile {
                    if menu_row(ui, "‹  Back to profile", None).clicked() {
                        next = Some(SelfMenuPage::Profile);
                    }
                    controls::divider(ui);
                }
                match page {
                    SelfMenuPage::Profile => {
                        let avatar = self.avatar_texture(&user);
                        let banner = self.asset_texture("profile_banner", &user.id);
                        profile_preview(
                            ui,
                            &user,
                            avatar.as_ref(),
                            banner.as_ref(),
                            self.prefs.reduced_motion,
                            true,
                        );
                        let own = self.own_presence();
                        if !own.text.is_empty() {
                            ui.add_space(8.0);
                            ui.label(&own.text);
                        }
                        ui.add_space(10.0);
                        if menu_row(ui, "Edit Profile", None).clicked() {
                            self.settings_tab = SettingsTab::Profiles;
                            self.settings_open = true;
                            close = true;
                        }
                        let label = format!("{}  ›", status_name(&own.status));
                        let row = ui
                            .add_enabled_ui(self.has_chat_capability("social-v1"), |ui| {
                                status_menu_row(ui, &label, None, Some(&own.status))
                            })
                            .inner;
                        if row.clicked() {
                            next = Some(SelfMenuPage::Status);
                        }
                        if menu_row(ui, "Set Custom Status", None).clicked() {
                            self.admin_ui.presence_draft = None;
                            next = Some(SelfMenuPage::CustomStatus);
                        }
                        ui.separator();
                        if menu_row(ui, "Switch Accounts  ›", None).clicked() {
                            next = Some(SelfMenuPage::Accounts);
                        }
                        if menu_row(ui, "Copy User ID", None).clicked() {
                            ui.output_mut(|o| o.copied_text = user.id.clone());
                            self.notice = Some(("User ID copied".into(), Instant::now()));
                            close = true;
                        }
                    }
                    SelfMenuPage::Status => {
                        controls::nav_group(ui, "SET STATUS");
                        let own = self.own_presence();
                        let pending = self
                            .admin_requests
                            .values()
                            .any(|key| key == "action:presence");
                        for (value, hint) in [
                            ("online", "You're available"),
                            ("away", "You're taking a break"),
                            ("dnd", "Pause desktop notifications"),
                            ("invisible", "Appear offline to other members"),
                        ] {
                            let label = format!(
                                "{}{}",
                                status_name(value),
                                if value == own.status {
                                    "  (current)"
                                } else {
                                    ""
                                }
                            );
                            let row = ui
                                .add_enabled_ui(!pending, |ui| {
                                    status_menu_row(ui, &label, Some(hint), Some(value))
                                })
                                .inner;
                            if row.clicked() {
                                let submitted = UserPresence {
                                    status: value.into(),
                                    ..own.clone()
                                };
                                self.admin_request(
                                    AdminMethod::Put,
                                    "/api/presence".into(),
                                    serde_json::to_value(submitted).unwrap(),
                                    "action:presence".into(),
                                );
                                self.admin_ui.presence_draft = None;
                                close = true;
                            }
                        }
                        controls::description(ui, "Your status applies to this community.");
                    }
                    SelfMenuPage::CustomStatus => {
                        if self.has_chat_capability("social-v1") {
                            self.presence_editor(ui);
                        } else {
                            controls::description(ui, "Update this server to set a custom status.");
                        }
                    }
                    SelfMenuPage::Accounts => {
                        controls::nav_group(ui, "YOUR ACCOUNTS");
                        controls::description(
                            ui,
                            "One account per community. Each community has its own profile.",
                        );
                        let mut accounts = self.prefs.communities.clone();
                        if !accounts.iter().any(|a| a.url == self.login.url) {
                            accounts.push(SavedCommunity {
                                url: self.login.url.clone(),
                                username: user.username.clone(),
                                name: self
                                    .bootstrap
                                    .as_ref()
                                    .map(|b| b.server_name.clone())
                                    .unwrap_or_default(),
                                ..Default::default()
                            });
                        }
                        accounts.sort_by_key(|c| c.url != self.login.url);
                        for account in accounts {
                            let current = account.url == self.login.url;
                            let label = format!(
                                "{}{}",
                                if current {
                                    &user.username
                                } else {
                                    &account.username
                                },
                                if current { "  (active)" } else { "" }
                            );
                            let hint = if account.name.is_empty() {
                                &account.url
                            } else {
                                &account.name
                            };
                            if menu_row(ui, &label, Some(hint)).clicked() && !current {
                                self.switch_community(account.url);
                                close = true;
                            }
                        }
                        controls::divider(ui);
                        if menu_row(ui, "Manage Accounts", None).clicked() {
                            self.settings_tab = SettingsTab::Connections;
                            self.settings_open = true;
                            close = true;
                        }
                        if menu_row(ui, "Add a Community", None).clicked() {
                            self.open_community_dialog();
                            close = true;
                        }
                    }
                }
            });
        if let Some(shown) = shown {
            let outside = ctx.input(|i| {
                i.pointer.any_pressed()
                    && i.pointer
                        .interact_pos()
                        .is_some_and(|p| !shown.response.rect.contains(p) && !anchor.contains(p))
            });
            close |= outside;
        }
        if close {
            self.profile_ui.menu_anchor = None;
        }
        if let Some(next) = next {
            self.profile_ui.menu_page = next;
        }
    }

    pub(super) fn avatar_texture(&mut self, user: &User) -> Option<egui::TextureHandle> {
        user.profile.avatar_url.as_ref()?;
        let key = avatar_attachment_id(user);
        if let Some((texture, at)) = self.profile_ui.avatars.get_mut(&key) {
            *at = Instant::now();
            return Some(texture.clone());
        }
        if self.profile_ui.avatar_pending.len() < 4
            && !self.profile_ui.avatar_pending.contains(&key)
            && !self.profile_ui.avatar_failed.contains(&key)
            && self
                .network
                .send(Command::FetchAvatar { user: user.clone() })
                .is_ok()
        {
            self.profile_ui.avatar_pending.insert(key);
        }
        None
    }
    pub(super) fn user_avatar(
        &mut self,
        ui: &mut egui::Ui,
        user: &User,
        size: f32,
        speaking: bool,
    ) {
        let texture = self.avatar_texture(user);
        let (rect, response) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());
        paint_user_avatar(ui, rect, user, texture.as_ref(), speaking);
        if response
            .on_hover_text(format!("View {}’s profile", display_name(user)))
            .clicked()
        {
            self.open_profile(user.clone());
        }
    }
    pub(super) fn open_profile(&mut self, user: User) {
        self.profile_ui
            .avatar_failed
            .remove(&avatar_attachment_id(&user));
        self.profile_ui.card = Some(user.clone());
        if !self
            .bootstrap
            .as_ref()
            .is_some_and(|b| b.capabilities.iter().any(|cap| cap == "profiles-v1"))
        {
            return;
        }
        let request_id = self.next_request();
        if self
            .network
            .send(Command::FetchUserProfile {
                user_id: user.id,
                request_id,
            })
            .is_ok()
        {
            self.profile_ui.card_request = Some(request_id);
        }
    }
    pub(super) fn apply_profile_user(&mut self, user: &User) {
        let newest_version = self.bootstrap.as_ref().map_or(0, |b| {
            b.members
                .iter()
                .chain(std::iter::once(&b.user))
                .filter(|member| member.id == user.id)
                .map(|member| member.profile.version)
                .max()
                .unwrap_or(0)
        });
        if user.profile.version < newest_version {
            return;
        }
        if let Some(editor) = &mut self.profile_ui.editor {
            if editor.scope == format!("{}|{}", self.login.url, user.id) {
                editor.apply(user.clone(), None, false);
            }
        }
        if let Some(session) = &mut self.session {
            if session.user.id == user.id {
                session.user = user.clone();
            }
        }
        if let Some(b) = self.bootstrap.as_mut().map(Arc::make_mut) {
            if b.user.id == user.id {
                b.user = user.clone();
            }
            for member in &mut b.members {
                if member.id == user.id {
                    *member = user.clone();
                }
            }
            for member in &mut b.voice_members {
                if member.user.id == user.id {
                    member.user = user.clone();
                }
            }
        }
        for message in self
            .messages
            .iter_mut()
            .chain(self.search_messages.iter_mut())
        {
            if message.author.id == user.id {
                message.author = user.clone();
            }
        }
        for pin in &mut self.pins.items {
            if pin.message.author.id == user.id {
                pin.message.author = user.clone();
            }
            if pin.pinned_by.id == user.id {
                pin.pinned_by = user.clone();
            }
        }
        for tile in self.video.values_mut() {
            if tile.metadata.user.id == user.id {
                tile.metadata.user = user.clone();
            }
        }
        if let Some(card) = &mut self.profile_ui.card {
            if card.id == user.id {
                *card = user.clone();
            }
        }
        self.row_heights = Default::default();
    }
    pub(super) fn profiles_page(&mut self, ui: &mut egui::Ui) {
        controls::page_heading(
            ui,
            "Main Profile",
            "Make this profile yours. Changes are shared with this community.",
        );
        let Some(user) = self.bootstrap.as_ref().map(|b| b.user.clone()) else {
            controls::description(ui, "Connect to a server to edit your profile.");
            return;
        };
        if !self
            .bootstrap
            .as_ref()
            .is_some_and(|b| b.capabilities.iter().any(|c| c == "profiles-v1"))
        {
            controls::description(ui, "Update this server to edit profiles.");
            return;
        }
        let extended = self
            .bootstrap
            .as_ref()
            .is_some_and(|b| b.capabilities.iter().any(|c| c == "profiles-v2"));
        let effects = self.has_chat_capability("profiles-v3");
        let reduced_motion = self.prefs.reduced_motion;
        let scope = format!("{}|{}", self.login.url, user.id);
        if !self
            .profile_ui
            .editor
            .as_ref()
            .is_some_and(|e| e.scope == scope)
        {
            self.profile_ui.editor = Some(Editor::new(scope, user.clone()));
        }
        let texture = self.avatar_texture(&user);
        let assets = self.has_chat_capability("assets-v1");
        let mut crop_avatar = false;
        let mut crop_banner = false;
        let mut remove_banner = false;
        let banner_texture = self.asset_texture("profile_banner", &user.id);
        let editor = self.profile_ui.editor.as_mut().unwrap();
        let pending = editor.pending.is_some();
        let mut section = self.profile_ui.section;
        ui.horizontal_wrapped(|ui| {
            for (value, label) in [
                (ProfileSection::Identity, "About Me"),
                (ProfileSection::Avatar, "Avatar"),
                (ProfileSection::Banner, "Banner"),
                (ProfileSection::Colors, "Profile Theme"),
                (ProfileSection::Effects, "Effects"),
            ] {
                if controls::top_tab(ui, ("profile-section", label), label, section == value)
                    .clicked()
                {
                    section = value;
                }
            }
        });
        self.profile_ui.section = section;
        controls::divider(ui);
        profile_editor_layout(ui, |ui, is_preview| {
            if !is_preview {
                ui.add_enabled_ui(!pending, |ui| {
                    if section == ProfileSection::Identity {
                        controls::nav_group(ui, "DISPLAY NAME");
                        ui.add_sized(
                            [ui.available_width(), 38.0],
                            egui::TextEdit::singleline(&mut editor.draft.display_name)
                                .char_limit(32)
                                .hint_text(&user.username),
                        );
                        controls::description(ui, "Leave empty to use your username.");
                    }
                    if section == ProfileSection::Avatar {
                        controls::nav_group(ui, "AVATAR");
                        ui.horizontal_wrapped(|ui| {
                            if ui
                                .add_enabled(
                                    editor.preview_rx.is_none(),
                                    secondary("Change Avatar"),
                                )
                                .clicked()
                            {
                                if let Some(path) = rfd::FileDialog::new()
                                    .add_filter("Avatar image", &["png", "jpg", "jpeg", "webp"])
                                    .pick_file()
                                {
                                    let (tx, rx) = mpsc::sync_channel(1);
                                    let preview_path = path.clone();
                                    if std::thread::Builder::new()
                                        .name("relay-avatar-preview".into())
                                        .spawn(move || {
                                            let _ = tx.send(local_avatar_preview(&preview_path));
                                        })
                                        .is_ok()
                                    {
                                        editor.draft.avatar = ProfileAvatarChange::Upload(path);
                                        editor.preview = None;
                                        editor.preview_rx = Some(rx);
                                    } else {
                                        self.error = Some("Cannot start avatar preview.".into());
                                    }
                                }
                            }
                            if ui.add(secondary("Remove Avatar")).clicked() {
                                editor.draft.avatar = ProfileAvatarChange::Remove;
                                editor.preview = None;
                                editor.preview_rx = None;
                            }
                        });
                        controls::description(ui, "PNG, JPEG or WebP · up to 2 MiB");
                        if assets && ui.add(secondary("Crop Avatar")).clicked() {
                            crop_avatar = true;
                        }
                    }
                    if section == ProfileSection::Banner {
                        if extended {
                            profile_color_editor(
                                ui,
                                "Banner color",
                                &mut editor.draft.appearance.banner,
                            );
                        }
                        if assets {
                            controls::nav_group(ui, "PROFILE BANNER");
                            ui.horizontal_wrapped(|ui| {
                                if ui.add(secondary("Change Banner")).clicked() {
                                    crop_banner = true;
                                }
                                if ui
                                    .add_enabled(
                                        banner_texture.is_some(),
                                        secondary("Remove Banner"),
                                    )
                                    .clicked()
                                {
                                    remove_banner = true;
                                }
                            });
                        }
                    }
                    if extended && section == ProfileSection::Identity {
                        controls::nav_group(ui, "PRONOUNS");
                        ui.add_sized(
                            [ui.available_width(), 38.0],
                            egui::TextEdit::singleline(&mut editor.draft.appearance.pronouns)
                                .char_limit(32)
                                .hint_text("Add your pronouns"),
                        );
                    }
                    if extended && section == ProfileSection::Colors {
                        controls::nav_group(ui, "PROFILE COLORS");
                        ui.horizontal_wrapped(|ui| {
                            for (label, accent, banner) in [
                                ("Relay", 0x5865f2, 0x232428),
                                ("Ocean", 0x4ab5db, 0x20425a),
                                ("Forest", 0x57c59a, 0x193c32),
                                ("Sunset", 0xf7a971, 0x58364e),
                                ("Lavender", 0xb59bff, 0x47336d),
                            ] {
                                let selected = editor.draft.appearance.accent == accent
                                    && editor.draft.appearance.banner == banner;
                                let button = ui.add(
                                    egui::Button::new(RichText::new(label).color(Color32::WHITE))
                                        .fill(profile_color(banner))
                                        .stroke(Stroke::new(
                                            if selected { 2.0_f32 } else { 1.0_f32 },
                                            profile_color(accent),
                                        ))
                                        .min_size(Vec2::new(96.0, 44.0)),
                                );
                                if button.clicked() {
                                    editor.draft.appearance.accent = accent;
                                    editor.draft.appearance.banner = banner;
                                }
                            }
                        });
                        ui.add_space(16.0);
                        profile_color_editor(
                            ui,
                            "Accent color",
                            &mut editor.draft.appearance.accent,
                        );
                        controls::description(
                            ui,
                            "Your accent outlines the profile and highlights your server role.",
                        );
                    }
                    if effects && section == ProfileSection::Effects {
                        controls::nav_group(ui, "PROFILE EFFECT");
                        ui.horizontal_wrapped(|ui| {
                            for effect in ["none", "glow", "sparkle", "wave"] {
                                let selected =
                                    editor.draft.appearance.effect.as_deref().unwrap_or("none")
                                        == effect;
                                let button = ui.add(
                                    egui::Button::new(effect_name(effect))
                                        .min_size(Vec2::new(130.0, 70.0))
                                        .stroke(Stroke::new(
                                            if selected { 2.0_f32 } else { 1.0_f32 },
                                            if selected { c_accent() } else { c_line() },
                                        )),
                                );
                                if button.clicked() {
                                    editor.draft.appearance.effect = Some(effect.into());
                                }
                            }
                        });
                        controls::description(
                            ui,
                            "Free local effects. Reduced Motion keeps the preview still.",
                        );
                    }
                    if section == ProfileSection::Identity {
                        controls::nav_group(ui, "ABOUT ME");
                        ui.add_sized(
                            [ui.available_width(), 110.0],
                            egui::TextEdit::multiline(&mut editor.draft.bio)
                                .char_limit(190)
                                .hint_text("Tell people a little about yourself")
                                .desired_rows(4),
                        );
                        controls::description(
                            ui,
                            &format!("{}/190 characters", editor.draft.bio.chars().count()),
                        );
                    }
                    if section == ProfileSection::Effects && !effects {
                        controls::description(ui, "Update this server to use profile effects.");
                    }
                });
            } else {
                controls::nav_group(ui, "LIVE PREVIEW");
                let mut preview = user.clone();
                preview.profile.display_name = editor.draft.display_name.clone();
                preview.profile.bio = editor.draft.bio.clone();
                preview.profile.appearance = editor.draft.appearance.clone();
                let avatar = if editor.draft.avatar == ProfileAvatarChange::Remove {
                    None
                } else {
                    editor.preview.as_ref().or(texture.as_ref())
                };
                profile_preview(
                    ui,
                    &preview,
                    avatar,
                    banner_texture.as_ref(),
                    reduced_motion,
                    false,
                );
                ui.add_space(12.0);
                controls::description(ui, "This profile is shared with members of this server.");
            }
        });
        ui.add_space(22.0);
        let mut save = false;
        if editor.dirty() || pending {
            egui::Frame::none()
                .fill(c_rail())
                .rounding(6.0)
                .inner_margin(Margin::same(12.0))
                .show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(if pending {
                            "Saving your profile…"
                        } else {
                            "Careful — you have unsaved changes!"
                        });
                        if ui.add_enabled(!pending, secondary("Reset")).clicked() {
                            *editor = Editor::new(editor.scope.clone(), editor.baseline.clone());
                        }
                        if ui
                            .add_enabled(
                                !pending
                                    && editor.dirty()
                                    && editor.valid()
                                    && editor.preview_rx.is_none(),
                                primary("Save Changes"),
                            )
                            .clicked()
                        {
                            save = true;
                        }
                        if pending {
                            ui.spinner();
                        }
                    });
                });
        }
        if !editor.valid() {
            ui.label(
                RichText::new("Check your profile text and colors before saving.").color(c_bad()),
            );
        }
        if save {
            let draft = editor.draft.clone();
            let Some(origin) = self.command_origin() else {
                self.error = Some("Connect to a server before saving your profile.".into());
                return;
            };
            let request_id = self.next_request();
            match self.network.send(Command::SaveProfile {
                origin,
                display_name: draft.display_name.clone(),
                bio: draft.bio.clone(),
                appearance: extended.then(|| draft.appearance.clone()),
                avatar: draft.avatar.clone(),
                request_id,
            }) {
                Ok(()) => {
                    self.profile_ui.editor.as_mut().unwrap().pending = Some((request_id, draft))
                }
                Err(error) => self.error = Some(error),
            }
        }
        if crop_avatar {
            self.choose_crop_image("avatar", &user.id);
        }
        if crop_banner {
            self.choose_crop_image("profile_banner", &user.id);
        }
        if remove_banner {
            self.admin_request(
                AdminMethod::Delete,
                format!("/api/assets/profile_banner/{}", user.id),
                serde_json::Value::Null,
                "action:assets".into(),
            );
        }
    }

    pub(super) fn apply_cropped_avatar(&mut self, bytes: Vec<u8>, image: egui::ColorImage) {
        let Some(user) = self.bootstrap.as_ref().map(|b| b.user.clone()) else {
            return;
        };
        let scope = format!("{}|{}", self.login.url, user.id);
        if self
            .profile_ui
            .editor
            .as_ref()
            .is_none_or(|e| e.scope != scope)
        {
            self.profile_ui.editor = Some(Editor::new(scope, user));
        }
        let editor = self.profile_ui.editor.as_mut().unwrap();
        if editor.pending.is_some() {
            self.error = Some("Wait for the profile save before cropping another avatar.".into());
            return;
        }
        editor.draft.avatar = ProfileAvatarChange::Prepared(bytes);
        editor.preview = Some(self.capture_ctx.load_texture(
            "cropped-avatar-draft",
            image,
            egui::TextureOptions::LINEAR,
        ));
        editor.preview_rx = None;
    }
    pub(super) fn profile_card(&mut self, ctx: &egui::Context) {
        self.self_profile_menu(ctx);
        let Some(user) = self.profile_ui.card.clone() else {
            return;
        };
        let mut open = true;
        let mut edit_profile = false;
        let mut open_after_message = false;
        let mut close = false;
        profile_card_window(ctx).open(&mut open).show(ctx, |ui| {
            if dialog_header_ui::dialog_header(ui, "User Profile") {
                close = true;
            }
            profile_card_layout(ui, |ui, actions| {
                if actions {
                    if self.session.as_ref().is_some_and(|s| s.user.id != user.id)
                        && self.can_message(&user.id)
                        && ui
                            .add(primary("Message").min_size(Vec2::new(ui.available_width(), 36.0)))
                            .clicked()
                    {
                        self.select(Target::Direct(user.id.clone()));
                        open_after_message = true;
                    }
                    if self.session.as_ref().is_some_and(|s| s.user.id == user.id)
                        && ui.add(secondary("Edit Profile")).clicked()
                    {
                        edit_profile = true;
                    }
                    ui.add_space(8.0);
                    self.social_buttons(ui, &user);
                    return;
                }
                let texture = self.avatar_texture(&user);
                let banner = self.asset_texture("profile_banner", &user.id);
                profile_preview(
                    ui,
                    &user,
                    texture.as_ref(),
                    banner.as_ref(),
                    self.prefs.reduced_motion,
                    false,
                );
                ui.add_space(10.0);
                self.presence_label(ui, &user.id);
                self.linked_account_label(ui, &user.id);
                self.role_badges(ui, &user.id);
                if self
                    .session
                    .as_ref()
                    .is_some_and(|session| session.user.id == user.id)
                {
                    ui.collapsing("Set status", |ui| self.presence_editor(ui));
                }
                if self.profile_ui.card_request.is_some() {
                    ui.spinner();
                }
            });
        });
        if edit_profile {
            self.settings_tab = SettingsTab::Profiles;
            self.settings_open = true;
            open = false;
        }
        if open_after_message || close {
            open = false;
        }
        if !open {
            self.profile_ui.card = None;
            self.profile_ui.card_request = None;
        }
    }
}

pub(super) fn paint_user_avatar(
    ui: &egui::Ui,
    rect: Rect,
    user: &User,
    texture: Option<&egui::TextureHandle>,
    speaking: bool,
) {
    if let Some(texture) = texture {
        egui::Image::new((texture.id(), rect.size()))
            .rounding(rect.width() * 0.5)
            .paint_at(ui, rect);
        if speaking {
            ui.painter().circle_stroke(
                rect.center(),
                rect.width() * 0.5 + 2.0,
                Stroke::new(2.0_f32, c_good()),
            );
        }
    } else {
        draw_avatar(ui.painter(), rect, display_name(user), speaking);
    }
}

fn local_avatar_preview(path: &std::path::Path) -> Result<egui::ColorImage, String> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .map_err(|_| "Cannot read this avatar.")?
        .take(2 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "Cannot read this avatar.")?;
    if bytes.len() > 2 * 1024 * 1024 {
        return Err("Avatars can be up to 2 MiB.".into());
    }
    let format = image::guess_format(&bytes).map_err(|_| "Choose a PNG, JPEG or WebP avatar.")?;
    if !matches!(
        format,
        image::ImageFormat::Png | image::ImageFormat::Jpeg | image::ImageFormat::WebP
    ) {
        return Err("Choose a static PNG, JPEG or WebP avatar.".into());
    }
    let mut reader = image::io::Reader::with_format(std::io::Cursor::new(bytes), format);
    let mut limits = image::io::Limits::default();
    limits.max_image_width = Some(2048);
    limits.max_image_height = Some(2048);
    limits.max_alloc = Some(32 * 1024 * 1024);
    reader.limits(limits);
    let image = reader
        .decode()
        .map_err(|_| "This avatar is damaged or exceeds 2048×2048 pixels.")?
        .thumbnail(256, 256)
        .to_rgba8();
    Ok(egui::ColorImage::from_rgba_unmultiplied(
        [image.width() as usize, image.height() as usize],
        image.as_raw(),
    ))
}

pub(super) fn profile_color(value: u32) -> Color32 {
    Color32::from_rgb((value >> 16) as u8, (value >> 8) as u8, value as u8)
}
pub(super) fn profile_color_editor(ui: &mut egui::Ui, label: &str, value: &mut u32) {
    ui.horizontal(|ui| {
        let mut rgb = [(*value >> 16) as u8, (*value >> 8) as u8, *value as u8];
        if ui.color_edit_button_srgb(&mut rgb).changed() {
            *value = (u32::from(rgb[0]) << 16) | (u32::from(rgb[1]) << 8) | u32::from(rgb[2]);
        }
        ui.label(label);
    });
}
// The actions belong to the card, rather than its scrollable profile body.
// A bottom-up layout allocates their real wrapped height before the scroller,
// so no fixed footer estimate can clip larger fonts or translated labels.
fn profile_card_window(ctx: &egui::Context) -> egui::Window<'static> {
    let height = (ctx.screen_rect().height() - 160.0).clamp(80.0, 600.0);
    desktop_ui::dialog(ctx, "User Profile")
        .id(egui::Id::new("user-profile-card"))
        // The shared dialog header draws the title, Close and Escape.
        .title_bar(false)
        .collapsible(false)
        .default_width(360.0)
        .min_height(height)
        .max_height(height)
        .resizable(false)
        .vscroll(false)
}

fn profile_card_layout(ui: &mut egui::Ui, mut content: impl FnMut(&mut egui::Ui, bool)) {
    ui.with_layout(egui::Layout::bottom_up(Align::Min), |ui| {
        // A horizontal row reserves interact_size before placing its children.
        // Reserve the actual button height so large text cannot grow into the
        // preceding bottom-up action row.
        let button_height =
            ui.text_style_height(&egui::TextStyle::Button) + 2.0 * ui.spacing().button_padding.y;
        ui.spacing_mut().interact_size.y =
            ui.spacing().interact_size.y.max(button_height).max(32.0);
        content(ui, true);
        ui.separator();
        ui.allocate_ui_with_layout(
            ui.available_size(),
            egui::Layout::top_down(Align::Min),
            |ui| {
                egui::ScrollArea::vertical()
                    .id_source("profile-card-body")
                    .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysVisible)
                    .auto_shrink([false, false])
                    .show(ui, |ui| content(ui, false));
            },
        );
    });
}

fn profile_preview(
    ui: &mut egui::Ui,
    user: &User,
    texture: Option<&egui::TextureHandle>,
    banner_texture: Option<&egui::TextureHandle>,
    reduced_motion: bool,
    compact: bool,
) {
    egui::Frame::none()
        .fill(c_card())
        .rounding(8.0)
        .stroke(Stroke::new(
            1.0_f32,
            profile_color(user.profile.appearance.accent),
        ))
        .show(ui, |ui| {
            let (banner, _) =
                ui.allocate_exact_size(Vec2::new(ui.available_width(), 84.0), Sense::hover());
            ui.painter().rect_filled(
                banner,
                Rounding {
                    nw: 8.0,
                    ne: 8.0,
                    sw: 0.0,
                    se: 0.0,
                },
                profile_color(user.profile.appearance.banner),
            );
            if let Some(texture) = banner_texture {
                egui::Image::new((texture.id(), banner.size()))
                    .rounding(8.0)
                    .paint_at(ui, banner);
            }
            paint_profile_effect(ui, banner, &user.profile.appearance, reduced_motion);
            egui::Frame::none()
                .inner_margin(Margin::same(16.0))
                .show(ui, |ui| {
                    ui.add_space(-32.0);
                    let (avatar, _) = ui.allocate_exact_size(Vec2::splat(64.0), Sense::hover());
                    ui.painter().circle_filled(avatar.center(), 36.0, c_card());
                    paint_user_avatar(ui, avatar, user, texture, false);
                    ui.add_space(8.0);
                    ui.add(
                        egui::Label::new(
                            RichText::new(display_name(user))
                                .size(font_size(22.0))
                                .strong(),
                        )
                        .wrap(true),
                    );
                    if compact && !user.profile.appearance.pronouns.is_empty() {
                        controls::description(
                            ui,
                            &format!("{} · {}", user.username, user.profile.appearance.pronouns),
                        );
                    } else {
                        controls::description(ui, &user.username);
                    }
                    if !compact && !user.profile.appearance.pronouns.is_empty() {
                        controls::description(ui, &user.profile.appearance.pronouns);
                    }
                    if !compact {
                        controls::divider(ui);
                        controls::nav_group(ui, "ABOUT ME");
                    } else {
                        ui.add_space(8.0);
                    }
                    if compact {
                        let mut job = egui::text::LayoutJob::simple(
                            user.profile.bio.clone(),
                            FontId::proportional(font_size(13.0)),
                            c_text(),
                            ui.available_width(),
                        );
                        job.wrap.max_rows = 3;
                        ui.add(egui::Label::new(ui.painter().layout_job(job)));
                    } else {
                        ui.add(
                            egui::Label::new(if user.profile.bio.is_empty() {
                                "No bio yet."
                            } else {
                                &user.profile.bio
                            })
                            .wrap(true),
                        );
                        controls::nav_group(ui, "SERVER ROLE");
                        ui.label(
                            RichText::new(&user.role)
                                .color(profile_color(user.profile.appearance.accent)),
                        );
                    }
                });
        });
}

fn profile_editor_layout(ui: &mut egui::Ui, mut content: impl FnMut(&mut egui::Ui, bool)) {
    if ui.available_width() >= 660.0 {
        ui.columns(2, |columns| {
            content(&mut columns[0], false);
            content(&mut columns[1], true);
        });
    } else {
        content(ui, false);
        ui.add_space(20.0);
        content(ui, true);
    }
}

fn menu_row(ui: &mut egui::Ui, label: &str, hint: Option<&str>) -> egui::Response {
    status_menu_row(ui, label, hint, None)
}

fn status_menu_row(
    ui: &mut egui::Ui,
    label: &str,
    hint: Option<&str>,
    status: Option<&str>,
) -> egui::Response {
    let scale = font_size(14.0) / 14.0;
    let (rect, response) = ui.allocate_exact_size(
        Vec2::new(
            ui.available_width(),
            if hint.is_some() {
                52.0 * scale
            } else {
                36.0 * scale
            },
        ),
        Sense::click(),
    );
    let accessible = hint.map_or_else(|| label.to_owned(), |hint| format!("{label}\n{hint}"));
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, &accessible));
    if response.hovered() || response.has_focus() {
        ui.painter()
            .rect_filled(rect, 5.0, ui.visuals().widgets.hovered.weak_bg_fill);
    }
    let left = if status.is_some() { 36.0 } else { 12.0 };
    let color = if ui.is_enabled() { c_text() } else { c_muted() };
    let title = controls::single_line(ui, label, 14.0, color, (rect.width() - left - 8.0).max(1.0));
    let title_y = if hint.is_some() {
        rect.top() + 7.0 * scale
    } else {
        rect.center().y - title.size().y / 2.0
    };
    ui.painter()
        .galley(Pos2::new(rect.left() + left, title_y), title, color);
    if let Some(hint) = hint {
        let text = controls::single_line(
            ui,
            hint,
            11.0,
            c_muted(),
            (rect.width() - left - 8.0).max(1.0),
        );
        ui.painter().galley(
            Pos2::new(rect.left() + left, rect.top() + 28.0 * scale),
            text,
            c_muted(),
        );
    }
    if let Some(status) = status {
        paint_status(
            ui,
            Pos2::new(rect.left() + 16.0, rect.center().y),
            status,
            c_panel(),
        );
    }
    response.on_hover_text(accessible)
}

fn status_name(value: &str) -> &str {
    match value {
        "online" => "Online",
        "away" => "Idle",
        "dnd" => "Do Not Disturb",
        "invisible" => "Invisible",
        _ => "Offline",
    }
}

fn paint_status(ui: &egui::Ui, center: Pos2, status: &str, background: Color32) {
    let color = match status {
        "online" => c_good(),
        "away" => Color32::from_rgb(240, 178, 50),
        "dnd" => c_bad(),
        _ => c_muted(),
    };
    ui.painter().circle_filled(center, 7.0, background);
    if matches!(status, "invisible" | "offline") {
        ui.painter()
            .circle_stroke(center, 4.0, Stroke::new(2.0_f32, color));
    } else {
        ui.painter().circle_filled(center, 5.0, color);
        if status == "dnd" {
            ui.painter().line_segment(
                [center - Vec2::new(3.0, 0.0), center + Vec2::new(3.0, 0.0)],
                Stroke::new(2.0_f32, background),
            );
        } else if status == "away" {
            ui.painter()
                .circle_filled(center + Vec2::new(-2.0, -2.0), 4.0, background);
        }
    }
}

fn effect_name(effect: &str) -> &str {
    match effect {
        "glow" => "Soft Glow",
        "sparkle" => "Sparkles",
        "wave" => "Waves",
        _ => "None",
    }
}

fn paint_profile_effect(
    ui: &mut egui::Ui,
    rect: Rect,
    appearance: &ProfileAppearance,
    reduced_motion: bool,
) {
    let effect = appearance.effect.as_deref().unwrap_or("none");
    if effect == "none" {
        return;
    }
    let time = if reduced_motion {
        0.0
    } else {
        ui.input(|i| i.time) as f32
    };
    let color = profile_color(appearance.accent);
    let painter = ui.painter().with_clip_rect(rect);
    match effect {
        "glow" => {
            let pulse = 0.5 + 0.5 * (time * 1.6).sin();
            for ring in 0..5 {
                painter.rect_stroke(
                    rect.shrink(2.0 + ring as f32 * 2.0),
                    8.0,
                    Stroke::new(
                        2.0_f32,
                        color.linear_multiply((0.16 + pulse * 0.12) * (1.0 - ring as f32 / 6.0)),
                    ),
                );
            }
        }
        "sparkle" => {
            for index in 0..12 {
                let x = ((index as f32 * 0.618 + time * 0.025).fract() + 1.0).fract();
                let y = ((index as f32 * 0.383 + time * 0.015).fract() + 1.0).fract();
                let pulse = 0.5 + 0.5 * (time * 1.4 + index as f32).sin();
                painter.circle_filled(
                    egui::pos2(
                        rect.left() + x * rect.width(),
                        rect.top() + y * rect.height(),
                    ),
                    1.5 + pulse,
                    Color32::WHITE.linear_multiply(0.35 + pulse * 0.5),
                );
            }
        }
        "wave" => {
            for line in 0..4 {
                let mut points = Vec::with_capacity(25);
                for step in 0..25 {
                    let x = step as f32 / 24.0;
                    let y = 0.28
                        + line as f32 * 0.14
                        + 0.08 * (x * 9.0 + time * 0.7 + line as f32).sin();
                    points.push(egui::pos2(
                        rect.left() + x * rect.width(),
                        rect.top() + y * rect.height(),
                    ));
                }
                painter.add(egui::Shape::line(
                    points,
                    Stroke::new(2.0_f32, Color32::WHITE.linear_multiply(0.24)),
                ));
            }
        }
        _ => return,
    }
    if !reduced_motion {
        ui.ctx().request_repaint_after(Duration::from_millis(50));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn menu_frame(
        ctx: &egui::Context,
        events: Vec<egui::Event>,
        enabled: bool,
    ) -> (egui::Id, Rect, bool) {
        let mut result = None;
        let _ = ctx.run(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(240.0, 400.0))),
                focused: true,
                events,
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.add_enabled_ui(enabled, |ui| {
                        let response = status_menu_row(
                            ui,
                            "Do Not Disturb",
                            Some("Pause desktop notifications"),
                            Some("dnd"),
                        );
                        result = Some((response.id, response.rect, response.clicked()));
                    });
                });
            },
        );
        result.unwrap()
    }
    #[test]
    fn status_menu_supports_keyboard_and_cannot_activate_while_disabled() {
        let ctx = egui::Context::default();
        let (id, rect, _) = menu_frame(&ctx, vec![], true);
        assert!(rect.width() <= 240.0);
        ctx.memory_mut(|m| m.request_focus(id));
        let key = |key| egui::Event::Key {
            key,
            physical_key: Some(key),
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        };
        assert!(menu_frame(&ctx, vec![key(egui::Key::Enter)], true).2);
        assert!(!menu_frame(&ctx, vec![key(egui::Key::Space)], false).2);
        menu_frame(&ctx, vec![], true);
        let events = vec![
            egui::Event::PointerMoved(rect.center()),
            egui::Event::PointerButton {
                pos: rect.center(),
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            },
            egui::Event::PointerButton {
                pos: rect.center(),
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            },
        ];
        assert!(menu_frame(&ctx, events, true).2);
    }
    fn user() -> User {
        User {
            id: "me".into(),
            username: "login-name".into(),
            profile: UserProfile {
                display_name: "Visible name".into(),
                bio: "Original".into(),
                ..Default::default()
            },
            ..Default::default()
        }
    }
    struct CardGeometry {
        window: Rect,
        clip: Rect,
        body: Rect,
        block: Rect,
        block_id: Option<egui::Id>,
        message: Rect,
        body_top: f32,
        last_metadata: Rect,
        clicked: bool,
        header: Rect,
        closed: bool,
    }

    impl Default for CardGeometry {
        fn default() -> Self {
            Self {
                window: Rect::NOTHING,
                clip: Rect::NOTHING,
                body: Rect::NOTHING,
                block: Rect::NOTHING,
                block_id: None,
                message: Rect::NOTHING,
                body_top: 0.0,
                last_metadata: Rect::NOTHING,
                clicked: false,
                header: Rect::NOTHING,
                closed: false,
            }
        }
    }

    fn card_frame(
        ctx: &egui::Context,
        screen: Rect,
        profile: &User,
        events: Vec<egui::Event>,
    ) -> CardGeometry {
        let mut result = CardGeometry::default();
        let _ = ctx.run(
            egui::RawInput {
                screen_rect: Some(screen),
                events,
                ..Default::default()
            },
            |ctx| {
                let shown = profile_card_window(ctx)
                    .show(ctx, |ui| {
                        let top = ui.cursor().top();
                        result.closed = dialog_header_ui::dialog_header(ui, "User Profile");
                        result.header =
                            Rect::from_x_y_ranges(ui.max_rect().x_range(), top..=ui.cursor().top());
                        profile_card_layout(ui, |ui, actions| {
                            if actions {
                                result.message = ui.add(secondary("Message")).rect;
                                ui.add_space(8.0);
                                ui.horizontal_wrapped(|ui| {
                                    let _ = ui.button("Add Friend");
                                    let button = ui.button("Block");
                                    result.block = button.rect;
                                    result.block_id = Some(button.id);
                                    result.clicked = button.clicked();
                                    let _ = ui.button("Report");
                                });
                                result.clip = ui.clip_rect();
                            } else {
                                result.body = ui.clip_rect();
                                result.body_top = ui.cursor().top();
                                profile_preview(ui, profile, None, None, true, false);
                                for _ in 0..8 {
                                    result.last_metadata = ui.label("More profile metadata").rect;
                                }
                            }
                        });
                    })
                    .unwrap();
                result.window = shown.response.rect;
            },
        );
        result
    }

    #[test]
    fn profile_card_closes_with_close_or_escape_at_every_size() {
        let profile = user();
        for (size, font_scale) in [
            (Vec2::new(940.0, 650.0), 1.0),
            (Vec2::new(360.0, 420.0), 1.0),
            (Vec2::new(360.0, 420.0), 1.4),
        ] {
            let ctx = egui::Context::default();
            crate::appearance::AppearanceSettings {
                font_scale,
                ..Default::default()
            }
            .apply(&ctx, None);
            let screen = Rect::from_min_size(Pos2::ZERO, size);
            let mut card = CardGeometry::default();
            for _ in 0..4 {
                card = card_frame(&ctx, screen, &profile, vec![]);
            }
            // Close is the right end of the header, fully on screen.
            let close = Pos2::new(card.header.right() - 15.0, card.header.top() + 15.0);
            assert!(
                screen.contains(close) && card.window.contains(close),
                "{size:?}"
            );
            let press = |pressed| egui::Event::PointerButton {
                pos: close,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            };
            let clicked = card_frame(
                &ctx,
                screen,
                &profile,
                vec![egui::Event::PointerMoved(close), press(true), press(false)],
            );
            assert!(clicked.closed, "Close at {size:?} x{font_scale}");
            assert!(!clicked.clicked, "Close does not press an action behind it");
            let escaped = card_frame(
                &ctx,
                screen,
                &profile,
                vec![egui::Event::Key {
                    key: egui::Key::Escape,
                    physical_key: Some(egui::Key::Escape),
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }],
            );
            assert!(escaped.closed && !escaped.clicked, "Escape at {size:?}");
            assert!(
                escaped.clip.contains_rect(escaped.block),
                "actions stay visible"
            );
        }
    }

    #[test]
    fn profile_actions_stay_visible_clickable_and_stationary_while_body_scrolls() {
        let ctx = egui::Context::default();
        let mut profile = user();
        profile.profile.bio = "Long profile line.\n".repeat(10);
        for (size, font_scale) in [
            (Vec2::new(1320.0, 850.0), 1.0),
            (Vec2::new(940.0, 650.0), 1.0),
            (Vec2::new(670.0, 464.0), 1.0),
            (Vec2::new(360.0, 420.0), 1.0),
            (Vec2::new(670.0, 464.0), 1.4),
            (Vec2::new(360.0, 420.0), 1.4),
            (Vec2::new(1320.0, 850.0), 1.0),
        ] {
            crate::appearance::AppearanceSettings {
                font_scale,
                ..Default::default()
            }
            .apply(&ctx, None);
            let screen = Rect::from_min_size(Pos2::ZERO, size);
            let mut before = CardGeometry::default();
            for _ in 0..5 {
                before = card_frame(&ctx, screen, &profile, vec![]);
            }
            // The shared header sits inside the window, above the body.
            assert!(
                before.window.contains_rect(before.header),
                "header at {size:?}"
            );
            // The body's clip rect is its viewport widened by egui's clip margin.
            let margin = ctx.style().visuals.clip_rect_margin;
            assert!(
                before.header.bottom() <= before.body.top() + margin + 0.5,
                "{size:?}: header {:?}, body viewport {:?}",
                before.header,
                before.body
            );
            assert!(!before.closed);
            // Preserve the window's persisted geometry through each resize;
            // move its body back to the top using actual egui scroll input.
            card_frame(
                &ctx,
                screen,
                &profile,
                vec![
                    egui::Event::PointerMoved(before.body.center()),
                    egui::Event::Scroll(Vec2::new(0.0, 10000.0)),
                ],
            );
            for _ in 0..8 {
                before = card_frame(&ctx, screen, &profile, vec![]);
            }
            assert!(
                screen.contains_rect(before.window),
                "card outside {size:?}: {:?}",
                before.window
            );
            assert!(
                before.clip.contains_rect(before.block)
                    && before.clip.contains_rect(before.message),
                "actions clipped at {size:?}"
            );
            assert!(
                before.block.bottom() < before.message.top(),
                "Profile action rows overlap at {size:?} / font {font_scale}: {:?} / {:?}",
                before.block,
                before.message
            );
            assert!(
                before.body.bottom() < before.block.top(),
                "Body overlaps profile actions"
            );
            let position = before.block.center();
            let pointer = |pressed| egui::Event::PointerButton {
                pos: position,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            };
            card_frame(
                &ctx,
                screen,
                &profile,
                vec![egui::Event::PointerMoved(position), pointer(true)],
            );
            assert!(
                card_frame(&ctx, screen, &profile, vec![pointer(false)]).clicked,
                "Block click lost at {size:?}"
            );
            ctx.memory_mut(|memory| memory.request_focus(before.block_id.unwrap()));
            assert!(
                card_frame(
                    &ctx,
                    screen,
                    &profile,
                    vec![egui::Event::Key {
                        key: egui::Key::Enter,
                        physical_key: Some(egui::Key::Enter),
                        pressed: true,
                        repeat: false,
                        modifiers: egui::Modifiers::NONE
                    }]
                )
                .clicked,
                "Block keyboard activation lost at {size:?}"
            );
            card_frame(
                &ctx,
                screen,
                &profile,
                vec![egui::Event::Key {
                    key: egui::Key::Enter,
                    physical_key: Some(egui::Key::Enter),
                    pressed: false,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }],
            );
            card_frame(
                &ctx,
                screen,
                &profile,
                vec![
                    egui::Event::PointerMoved(before.body.center()),
                    egui::Event::Scroll(Vec2::new(0.0, -180.0)),
                ],
            );
            let mut after = CardGeometry::default();
            for _ in 0..8 {
                after = card_frame(&ctx, screen, &profile, vec![]);
            }
            assert!(
                after.body_top < before.body_top - 1.0,
                "profile body did not scroll at {size:?}"
            );
            assert_eq!(after.block, before.block, "scroll moved Block");
            assert_eq!(after.message, before.message, "scroll moved Message");
            assert!(
                after.clip.contains_rect(after.block) && after.clip.contains_rect(after.message)
            );
            card_frame(
                &ctx,
                screen,
                &profile,
                vec![
                    egui::Event::PointerMoved(after.body.center()),
                    egui::Event::Scroll(Vec2::new(0.0, -10000.0)),
                ],
            );
            for _ in 0..16 {
                after = card_frame(&ctx, screen, &profile, vec![]);
            }
            assert!(after.body.contains_rect(after.last_metadata), "Cannot reach the last profile row at {size:?} / font {font_scale}: {:?} outside {:?}", after.last_metadata, after.body);
            assert_eq!(after.block, before.block, "bottom scroll moved Block");
            assert_eq!(after.message, before.message, "bottom scroll moved Message");
        }
    }

    #[test]
    fn profile_ack_preserves_later_edits_and_partial_failure_keeps_avatar_retry() {
        let mut editor = Editor::new("server|me".into(), user());
        editor.draft.display_name = "Submitted".into();
        editor.draft.avatar = ProfileAvatarChange::Remove;
        let submitted = editor.draft.clone();
        editor.draft.bio = "Later edit".into();
        editor.draft.appearance.pronouns = "Later pronouns".into();
        let mut ack = user();
        ack.profile.display_name = "Submitted".into();
        editor.apply(ack, Some(submitted), false);
        assert_eq!(editor.draft.bio, "Later edit");
        assert_eq!(editor.draft.appearance.pronouns, "Later pronouns");
        assert_eq!(editor.draft.avatar, ProfileAvatarChange::Remove);
        assert!(editor.dirty());
    }
    #[test]
    fn profile_limits_count_unicode_characters_and_ignore_stale_other_account_acks() {
        let mut ui = ProfileUi {
            editor: Some(Editor::new("server|me".into(), user())),
            ..Default::default()
        };
        let editor = ui.editor.as_mut().unwrap();
        editor.draft.display_name = "🦊".repeat(32);
        editor.draft.bio = "ø".repeat(190);
        assert!(editor.valid());
        editor.draft.display_name.push('x');
        assert!(!editor.valid());
        let submitted = editor.draft.clone();
        editor.pending = Some((7, submitted));
        assert!(!ui.saved("other-server", user(), 7, true));
        assert!(!ui.saved("server", user(), 8, true));
        assert!(ui.editor.as_ref().unwrap().pending.is_some());
    }

    #[test]
    fn an_older_save_response_cannot_rollback_a_newer_profile_or_discard_a_local_edit() {
        let mut original = user();
        original.profile.version = 1;
        let mut editor = Editor::new("server|me".into(), original);
        editor.draft.display_name = "My pending edit".into();
        let submitted = editor.draft.clone();
        let mut newer = user();
        newer.profile.version = 3;
        newer.profile.display_name = "Newer other-device update".into();
        newer.profile.avatar_url = Some("/api/profiles/me/avatar?v=3".into());
        editor.apply(newer.clone(), None, false);
        let mut old_ack = user();
        old_ack.profile.version = 2;
        old_ack.profile.display_name = "My pending edit".into();
        editor.apply(old_ack, Some(submitted), true);
        assert_eq!(editor.baseline.profile, newer.profile);
        assert_eq!(editor.draft.display_name, "My pending edit");
        assert!(editor.dirty());
    }

    #[test]
    fn local_avatar_preview_is_bounded_and_preserves_aspect_before_upload() {
        use std::io::Cursor;
        let folder = std::env::var_os("RELAY_TEST_WORK_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("relay-tests"))
            .join(format!("avatar-preview-{}", crate::network::operation_id()));
        std::fs::create_dir_all(&folder).unwrap();
        let path = folder.join("synthetic.png");
        for (width, height, expected) in [(512, 256, Some([256, 128])), (2049, 1, None)] {
            let image = image::DynamicImage::new_rgba8(width, height);
            let mut bytes = Cursor::new(Vec::new());
            image
                .write_to(&mut bytes, image::ImageOutputFormat::Png)
                .unwrap();
            std::fs::write(&path, bytes.into_inner()).unwrap();
            match expected {
                Some(size) => assert_eq!(local_avatar_preview(&path).unwrap().size, size),
                None => assert!(local_avatar_preview(&path).is_err()),
            }
        }
        std::fs::write(&path, vec![0; 2 * 1024 * 1024 + 1]).unwrap();
        assert!(local_avatar_preview(&path).unwrap_err().contains("2 MiB"));
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(folder).unwrap();
    }
}
