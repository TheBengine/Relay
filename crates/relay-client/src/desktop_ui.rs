//! Shared desktop navigation, title bar and flat menu controls.
use super::*;

const TITLE_BUTTONS_WIDTH: f32 = 164.0;

pub(super) fn preserve_minimized_layout(input: &mut egui::RawInput) {
    if input.viewport_id != egui::ViewportId::ROOT {
        return;
    }
    let minimized = input
        .viewports
        .get(&egui::ViewportId::ROOT)
        .and_then(|viewport| viewport.minimized)
        .unwrap_or(false);
    let icon_sized = input
        .screen_rect
        .is_some_and(|rect| rect.width() < 64.0 || rect.height() < 64.0);
    if minimized || icon_sized {
        // Windows can report its tiny minimized icon bounds during a transition.
        // None keeps egui's last layout rect, without altering native viewport
        // state or permanently shrinking persisted dialogs and side panels.
        input.screen_rect = None;
    }
}

/// The caption's context: what is open, and in which community. The brand
/// appears only when there is no context (signed out or no community name).
fn desktop_page_caption(
    bootstrap: Option<&Bootstrap>,
    target: Option<&Target>,
    home: Option<SettingsTab>,
    selected_nav: bool,
) -> String {
    let community = bootstrap
        .map(|bootstrap| bootstrap.server_name.trim())
        .filter(|name| !name.is_empty());
    let within = |context: &str| match community {
        Some(name) => format!("{context} \u{2013} {name}"),
        None => context.to_owned(),
    };
    if let Some(page) = home {
        return match page {
            SettingsTab::Friends => within("Friends"),
            SettingsTab::Mentions => within("Inbox"),
            SettingsTab::Events => within("Events"),
            _ => "Relay".into(),
        };
    }
    let Some(bootstrap) = bootstrap else {
        return "Relay".into();
    };
    match target {
        Some(Target::Channel(id)) => {
            if let Some(channel) = bootstrap.channels.iter().find(|channel| channel.id == *id) {
                let group = bootstrap.conversations.iter().any(|conversation| {
                    conversation.channel_id == *id && conversation.kind == "group"
                });
                return if group || channel.kind != "text" {
                    within(&channel.name)
                } else {
                    within(&format!("#{}", channel.name))
                };
            }
        }
        Some(Target::Direct(id)) => {
            if let Some(peer) = bootstrap.members.iter().find(|member| member.id == *id) {
                return within(profile_ui::display_name(peer));
            }
        }
        None => {}
    }
    // An unavailable target falls back to the community itself.
    if selected_nav {
        community.unwrap_or("Relay").to_owned()
    } else {
        within("Direct messages")
    }
}

fn desktop_caption(ui: &mut egui::Ui, context: &str) {
    let ctx = ui.ctx();
    let rect = ui.max_rect();
    let drag_rect = Rect::from_min_max(
        rect.min,
        Pos2::new(rect.right() - TITLE_BUTTONS_WIDTH, rect.bottom()),
    );
    let drag = ui.interact(
        drag_rect,
        ui.id().with("title-drag"),
        Sense::click_and_drag(),
    );
    if drag.double_clicked() {
        ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(
            !ctx.input(|i| i.viewport().maximized.unwrap_or(false)),
        ));
    } else if drag.drag_started_by(egui::PointerButton::Primary) {
        ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
    }
    // The brand is not repeated at the left; the centre carries the context.
    let title_rect = Rect::from_center_size(
        Pos2::new(rect.center().x - 40.0, rect.center().y),
        Vec2::new((rect.width() - 350.0).max(80.0), 24.0),
    );
    ui.allocate_ui_at_rect(title_rect, |ui| {
        ui.add_sized(
            title_rect.size(),
            // Selectable labels take drag ownership away from the caption.
            egui::Label::new(RichText::new(context).size(12.0).color(c_muted()))
                .selectable(false)
                .truncate(true),
        );
    });
}

impl RelayApp {
    /// Only hide if Windows actually has an icon through which to reopen us.
    /// Capture/QA exits and explicit installer/tray exits always close fully.
    fn hide_on_close(&self) -> bool {
        #[cfg(windows)]
        {
            self.prefs.close_to_tray
                && !self.profile_quit_on_close
                && !self.exiting
                && self.capture_path.is_none()
                && self
                    .tray
                    .as_ref()
                    .is_some_and(|tray| tray.available() && !tray.quitting())
        }
        #[cfg(not(windows))]
        {
            false
        }
    }

    pub(super) fn hide_for_exit(&mut self) {
        self.call_alerts.stop();
        self.exiting = true;
        #[cfg(windows)]
        if let Some(tray) = &self.tray {
            tray.begin_exit();
        }
    }

    pub(super) fn window_close(&mut self, ctx: &egui::Context) {
        if self.hide_on_close() {
            #[cfg(windows)]
            if let Some(tray) = &self.tray {
                tray.hide();
            }
        } else {
            self.hide_for_exit();
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    pub(super) fn native_close(&mut self, ctx: &egui::Context, requested: bool) {
        if requested {
            if self.hide_on_close() {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                #[cfg(windows)]
                if let Some(tray) = &self.tray {
                    tray.hide();
                }
            } else {
                self.hide_for_exit();
            }
        }
    }

    pub(super) fn modal_open(&self) -> bool {
        self.add_community_open || self.invitation_overlay_open()
    }

    pub(super) fn invitation_overlay_open(&self) -> bool {
        self.settings_open
            || self.server_actions.removal_open()
            || self.quick_switch
            || self.delete_confirm.is_some()
            || self.create_channel_open
            || self.invite_dialog.is_some()
            || self.hosting_setup.open
            || self.share_picker.is_some()
            || self.media_preview.is_some()
            || self.online_gifs.message_preview_open()
            || self.stream_preview.is_some()
            || self.transfers_open
            || self.profile_ui.card.is_some()
            || self.pins.open
            || self.admin_ui.modal_open()
    }
    pub(super) fn desktop_titlebar(&mut self, ctx: &egui::Context) {
        if self.stream_fullscreen {
            return;
        }
        egui::TopBottomPanel::top("desktop-titlebar")
            .exact_height(32.0)
            .frame(egui::Frame::none().fill(c_rail()))
            .show(ctx, |ui| {
                let rect = ui.max_rect();
                let context = desktop_page_caption(
                    self.bootstrap.as_deref(),
                    self.current.as_ref(),
                    self.home_page,
                    self.selected_nav,
                );
                desktop_caption(ui, &context);
                let controls = Rect::from_min_max(
                    Pos2::new(rect.right() - TITLE_BUTTONS_WIDTH, rect.top()),
                    rect.max,
                );
                let inbox = !self.modal_open() && self.session.is_some();
                match title_controls(ui, controls, inbox, !self.modal_open()) {
                    Some(TitleAction::Inbox) => {
                        self.selected_nav = false;
                        self.home_page = Some(SettingsTab::Mentions);
                    }
                    Some(TitleAction::Help) => {
                        self.settings_tab = SettingsTab::Help;
                        self.settings_open = true;
                    }
                    Some(TitleAction::Minimize) => {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                    }
                    Some(TitleAction::Maximize) => {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(
                            !ctx.input(|i| i.viewport().maximized.unwrap_or(false)),
                        ));
                    }
                    Some(TitleAction::Close) => {
                        self.window_close(ctx);
                    }
                    None => {}
                }
            });
    }

    pub(super) fn desktop_resize(&self, ctx: &egui::Context) {
        resize_edges(ctx);
    }

    pub(super) fn desktop_home(&mut self, ctx: &egui::Context) {
        let Some(page) = self.home_page else { return };
        egui::CentralPanel::default()
            .frame(
                egui::Frame::none()
                    .fill(c_bg())
                    .inner_margin(Margin::same(24.0)),
            )
            .show(ctx, |ui| {
                ui.set_enabled(!self.modal_open());
                if matches!(page, SettingsTab::Mentions) {
                    self.mention_inbox_page(ui);
                    return;
                }
                egui::ScrollArea::vertical()
                    .id_source(("home-page", page as u8))
                    .auto_shrink([false, false])
                    .show(ui, |ui| match page {
                        SettingsTab::Friends => self.friends_page(ui),
                        SettingsTab::Events => self.events_page(ui),
                        _ => {}
                    });
            });
    }
}

fn resize_edges(ctx: &egui::Context) {
    if !ctx.input(|i| {
        i.viewport().maximized.unwrap_or(false) || i.viewport().fullscreen.unwrap_or(false)
    }) {
        let screen = ctx.screen_rect();
        let corner = 12.0;
        let edges = [
            (
                egui::ResizeDirection::West,
                Rect::from_min_max(screen.min, Pos2::new(screen.left() + 4.0, screen.bottom())),
                egui::CursorIcon::ResizeHorizontal,
            ),
            (
                egui::ResizeDirection::East,
                Rect::from_min_max(Pos2::new(screen.right() - 4.0, screen.top()), screen.max),
                egui::CursorIcon::ResizeHorizontal,
            ),
            (
                egui::ResizeDirection::North,
                Rect::from_min_max(screen.min, Pos2::new(screen.right(), screen.top() + 4.0)),
                egui::CursorIcon::ResizeVertical,
            ),
            (
                egui::ResizeDirection::South,
                Rect::from_min_max(Pos2::new(screen.left(), screen.bottom() - 4.0), screen.max),
                egui::CursorIcon::ResizeVertical,
            ),
            (
                egui::ResizeDirection::NorthWest,
                Rect::from_min_size(screen.min, Vec2::splat(corner)),
                egui::CursorIcon::ResizeNwSe,
            ),
            (
                egui::ResizeDirection::NorthEast,
                Rect::from_min_size(
                    Pos2::new(screen.right() - corner, screen.top()),
                    Vec2::splat(corner),
                ),
                egui::CursorIcon::ResizeNeSw,
            ),
            (
                egui::ResizeDirection::SouthWest,
                Rect::from_min_size(
                    Pos2::new(screen.left(), screen.bottom() - corner),
                    Vec2::splat(corner),
                ),
                egui::CursorIcon::ResizeNeSw,
            ),
            (
                egui::ResizeDirection::SouthEast,
                Rect::from_min_size(screen.max - Vec2::splat(corner), Vec2::splat(corner)),
                egui::CursorIcon::ResizeNwSe,
            ),
        ];
        // Title controls drawn this frame take their clicks; resize stays on
        // every edge and corner part outside them.
        let frame = ctx.frame_nr();
        let controls = ctx
            .data(|data| data.get_temp::<(u64, Rect)>(title_controls_id()))
            .filter(|(drawn, _)| *drawn == frame)
            .map(|(_, rect)| rect);
        let parts = edges
            .into_iter()
            .enumerate()
            .flat_map(|(index, (direction, rect, cursor))| {
                outside_title_controls(rect, controls)
                    .into_iter()
                    .enumerate()
                    .map(move |(part, rect)| ((index, part), direction, rect, cursor))
            });
        for (key, direction, rect, cursor) in parts {
            egui::Area::new(egui::Id::new(("window-resize", key)))
                // Tooltip layers deliberately reject all pointer interaction.
                .order(egui::Order::Foreground)
                .constrain(false)
                .movable(false)
                .fixed_pos(rect.min)
                .show(ctx, |ui| {
                    let (_, response) = ui.allocate_exact_size(rect.size(), Sense::drag());
                    let response = response.on_hover_cursor(cursor);
                    if response.drag_started_by(egui::PointerButton::Primary) {
                        ctx.send_viewport_cmd(egui::ViewportCommand::BeginResize(direction));
                    }
                });
        }
    }
}

pub(super) fn dialog<'a>(
    ctx: &egui::Context,
    title: impl Into<egui::WidgetText>,
) -> egui::Window<'a> {
    let screen = ctx.screen_rect();
    let backdrop = Rect::from_min_max(Pos2::new(screen.left(), screen.top() + 32.0), screen.max);
    ctx.layer_painter(egui::LayerId::background()).rect_filled(
        backdrop,
        0.0,
        Color32::from_black_alpha(150),
    );
    let usable = Rect::from_min_max(
        Pos2::new(screen.left() + 16.0, screen.top() + 48.0),
        screen.max - Vec2::splat(16.0),
    );
    egui::Window::new(title)
        .pivot(Align2::CENTER_CENTER)
        .fixed_pos(usable.center())
        .movable(false)
        .constrain_to(usable)
        // Preserve each dialog's default width and clamp persisted sizes to
        // the space left by the desktop caption and window margins.
        .min_width(160.0_f32.min((usable.width() - 24.0).max(1.0)))
        .max_width((usable.width() - 24.0).max(1.0))
        .min_height(0.0)
        .max_height((usable.height() - 64.0).max(80.0))
        .vscroll(true)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TitleAction {
    Inbox,
    Help,
    Minimize,
    Maximize,
    Close,
}

/// The caption's control buttons, laid out left to right in `controls`.
fn title_controls(
    ui: &mut egui::Ui,
    controls: Rect,
    inbox_enabled: bool,
    help_enabled: bool,
) -> Option<TitleAction> {
    let mut action = None;
    ui.allocate_ui_at_rect(controls, |ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        let row = ui.horizontal(|ui| {
            ui.add_enabled_ui(inbox_enabled, |ui| {
                if icon_button(ui, Icon::Inbox, false, "Inbox").clicked() {
                    action = Some(TitleAction::Inbox);
                }
            });
            if ui
                .add_enabled_ui(help_enabled, |ui| title_button(ui, "Help", 0))
                .inner
                .clicked()
            {
                action = Some(TitleAction::Help);
            }
            if title_button(ui, "Minimize", 1).clicked() {
                action = Some(TitleAction::Minimize);
            }
            if title_button(ui, "Maximize / restore", 2).clicked() {
                action = Some(TitleAction::Maximize);
            }
            if title_button(ui, "Close window", 3).clicked() {
                action = Some(TitleAction::Close);
            }
        });
        // `resize_edges` keeps its hotspots off these buttons in this frame.
        let frame = ui.ctx().frame_nr();
        let rect = row.response.rect;
        ui.ctx()
            .data_mut(|data| data.insert_temp(title_controls_id(), (frame, rect)));
    });
    action
}

fn title_controls_id() -> egui::Id {
    egui::Id::new("relay-title-controls")
}

/// The parts of a resize hotspot that do not cover the title controls.
fn outside_title_controls(rect: Rect, controls: Option<Rect>) -> Vec<Rect> {
    let Some(hole) = controls.filter(|hole| rect.intersects(*hole)) else {
        return vec![rect];
    };
    [
        Rect::from_min_max(rect.min, Pos2::new(hole.left(), rect.bottom())),
        Rect::from_min_max(Pos2::new(hole.right(), rect.top()), rect.max),
        Rect::from_min_max(
            Pos2::new(hole.left().max(rect.left()), rect.top()),
            Pos2::new(hole.right().min(rect.right()), hole.top()),
        ),
        Rect::from_min_max(
            Pos2::new(hole.left().max(rect.left()), hole.bottom()),
            Pos2::new(hole.right().min(rect.right()), rect.bottom()),
        ),
    ]
    .into_iter()
    .filter(|part| part.width() > 0.0 && part.height() > 0.0)
    .collect()
}

fn title_button(ui: &mut egui::Ui, label: &str, kind: u8) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::new(32.0, 30.0), Sense::click());
    if response.hovered() || response.has_focus() {
        ui.painter()
            .rect_filled(rect, 0.0, if kind == 3 { c_bad() } else { c_card() });
    }
    let stroke = Stroke::new(1.2_f32, c_text());
    let center = rect.center();
    match kind {
        0 => {
            ui.painter().circle_stroke(center, 7.0, stroke);
            ui.painter().text(
                center,
                Align2::CENTER_CENTER,
                "?",
                FontId::proportional(12.0),
                c_text(),
            );
        }
        1 => {
            ui.painter().line_segment(
                [center - Vec2::new(5.0, 0.0), center + Vec2::new(5.0, 0.0)],
                stroke,
            );
        }
        2 => {
            ui.painter().rect_stroke(
                Rect::from_center_size(center, Vec2::splat(10.0)),
                0.0,
                stroke,
            );
        }
        _ => {
            draw_icon(ui.painter(), rect.shrink(8.0), Icon::Close, c_text());
        }
    }
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, label));
    response.on_hover_text(label)
}

/// A menu item in the accent colour, for the one action a menu leads with
/// (Discord's "Invite People").
pub(super) fn menu_item_accent(ui: &mut egui::Ui, label: &str) -> egui::Response {
    menu_item_colored(ui, label, false, Some(c_accent()))
}

pub(super) fn menu_item(ui: &mut egui::Ui, label: &str, danger: bool) -> egui::Response {
    menu_item_colored(ui, label, danger, None)
}

fn menu_item_colored(
    ui: &mut egui::Ui,
    label: &str,
    danger: bool,
    rest: Option<Color32>,
) -> egui::Response {
    let width = ui.available_width();
    let galley = ui.painter().layout(
        label.to_owned(),
        FontId::proportional(font_size(14.0)),
        c_text(),
        (width - 20.0).max(1.0),
    );
    let (rect, response) = ui.allocate_exact_size(
        Vec2::new(width, (galley.size().y + 12.0).max(34.0)),
        Sense::click(),
    );
    if response.hovered() || response.has_focus() {
        ui.painter()
            .rect_filled(rect, 4.0, if danger { c_bad() } else { c_accent() });
    }
    let color = if response.hovered() || response.has_focus() {
        Color32::WHITE
    } else if danger {
        c_bad()
    } else {
        rest.unwrap_or_else(c_text)
    };
    ui.painter().galley_with_override_text_color(
        Pos2::new(rect.left() + 10.0, rect.center().y - galley.size().y * 0.5),
        galley,
        color,
    );
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, label));
    response.on_hover_text(label)
}

pub(super) fn voice_chat_action(
    ui: &mut egui::Ui,
    row: &egui::Response,
    visible: bool,
) -> egui::Response {
    let rect = Rect::from_center_size(
        Pos2::new(row.rect.right() - 18.0, row.rect.center().y),
        Vec2::splat(24.0),
    );
    let response = ui.interact(rect, row.id.with("voice-chat"), Sense::click());
    if visible || row.hovered() || response.hovered() || response.has_focus() {
        draw_icon(
            ui.painter(),
            rect.shrink(4.0),
            Icon::Chat,
            if response.hovered() {
                c_text()
            } else {
                c_muted()
            },
        );
    }
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Button, "Open voice channel chat")
    });
    response.on_hover_text("Open chat without joining voice")
}

#[cfg(test)]
mod window_tests {
    use super::*;
    use egui::{Event, PointerButton, ViewportCommand, ViewportId};

    /// The real caption, title controls and resize hotspots, in the same order
    /// as `RelayApp::update` draws them. Returns the clicked title action and the
    /// viewport commands of the frame.
    fn titlebar_frame(
        ctx: &egui::Context,
        size: Vec2,
        maximized: bool,
        events: Vec<Event>,
    ) -> (Option<TitleAction>, Vec<ViewportCommand>) {
        let mut input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, size)),
            events,
            ..Default::default()
        };
        input
            .viewports
            .get_mut(&ViewportId::ROOT)
            .unwrap()
            .maximized = Some(maximized);
        let mut action = None;
        let output = ctx.run(input, |ctx| {
            egui::TopBottomPanel::top("desktop-titlebar")
                .exact_height(32.0)
                .frame(egui::Frame::none())
                .show(ctx, |ui| {
                    let rect = ui.max_rect();
                    desktop_caption(ui, "Owned caption fixture");
                    let controls = Rect::from_min_max(
                        Pos2::new(rect.right() - TITLE_BUTTONS_WIDTH, rect.top()),
                        rect.max,
                    );
                    action = title_controls(ui, controls, true, true);
                });
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.label("Owned content fixture");
            });
            resize_edges(ctx);
        });
        let commands = output.viewport_output[&ViewportId::ROOT].commands.clone();
        (action, commands)
    }

    /// A physical press and release at `at` after the layout has settled.
    fn click_titlebar(
        size: Vec2,
        scale: f32,
        maximized: bool,
        at: Pos2,
    ) -> (Option<TitleAction>, Vec<ViewportCommand>) {
        let ctx = egui::Context::default();
        ctx.set_pixels_per_point(scale);
        for _ in 0..4 {
            titlebar_frame(&ctx, size, maximized, Vec::new());
        }
        let press = |pressed| Event::PointerButton {
            pos: at,
            button: PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        let (first, mut commands) = titlebar_frame(
            &ctx,
            size,
            maximized,
            vec![Event::PointerMoved(at), press(true)],
        );
        let (second, more) = titlebar_frame(&ctx, size, maximized, vec![press(false)]);
        commands.extend(more);
        (first.or(second), commands)
    }

    fn resized(commands: &[ViewportCommand]) -> Option<egui::ResizeDirection> {
        commands.iter().find_map(|command| match command {
            ViewportCommand::BeginResize(direction) => Some(*direction),
            _ => None,
        })
    }

    #[test]
    fn title_controls_win_over_resize_hotspots_at_their_edges() {
        for size in [Vec2::new(940.0, 650.0), Vec2::new(1320.0, 850.0)] {
            for scale in [1.0, 1.5] {
                let r = size.x;
                // Points inside each title control, including the top edge and
                // Close's top-right corner where the resize hotspots used to be.
                for (at, expected) in [
                    (Pos2::new(r - 22.0, 15.0), TitleAction::Close),
                    (Pos2::new(r - 8.0, 6.0), TitleAction::Close),
                    (Pos2::new(r - 22.0, 2.0), TitleAction::Close),
                    (Pos2::new(r - 37.0, 2.0), TitleAction::Close),
                    (Pos2::new(r - 54.0, 2.0), TitleAction::Maximize),
                    (Pos2::new(r - 86.0, 2.0), TitleAction::Minimize),
                    (Pos2::new(r - 118.0, 2.0), TitleAction::Help),
                    (Pos2::new(r - 149.0, 2.0), TitleAction::Inbox),
                ] {
                    let (action, commands) = click_titlebar(size, scale, false, at);
                    assert_eq!(
                        (action, resized(&commands)),
                        (Some(expected), None),
                        "{size:?} x{scale} at {at:?}: {commands:?}"
                    );
                }
                // Resize still works everywhere outside the title controls.
                for (at, expected) in [
                    (Pos2::new(r - 300.0, 1.0), egui::ResizeDirection::North),
                    (Pos2::new(2.0, 2.0), egui::ResizeDirection::NorthWest),
                    (Pos2::new(r - 2.0, 300.0), egui::ResizeDirection::East),
                    (Pos2::new(2.0, 300.0), egui::ResizeDirection::West),
                    (Pos2::new(300.0, size.y - 2.0), egui::ResizeDirection::South),
                    (
                        Pos2::new(r - 2.0, size.y - 2.0),
                        egui::ResizeDirection::SouthEast,
                    ),
                    (
                        Pos2::new(2.0, size.y - 2.0),
                        egui::ResizeDirection::SouthWest,
                    ),
                ] {
                    let (action, commands) = click_titlebar(size, scale, false, at);
                    assert_eq!(
                        (action, resized(&commands)),
                        (None, Some(expected)),
                        "{size:?} x{scale} at {at:?}: {commands:?}"
                    );
                }
                // Right of the controls, the top-right corner still resizes.
                let (action, commands) =
                    click_titlebar(size, scale, false, Pos2::new(r - 2.0, 2.0));
                assert!(
                    action.is_none() && resized(&commands).is_some(),
                    "{commands:?}"
                );
                // Maximized: no resize hotspots; Close works at its corner too.
                for at in [Pos2::new(r - 8.0, 6.0), Pos2::new(r - 22.0, 2.0)] {
                    let (action, commands) = click_titlebar(size, scale, true, at);
                    assert_eq!(
                        (action, resized(&commands)),
                        (Some(TitleAction::Close), None)
                    );
                }
            }
        }
    }

    #[test]
    fn caption_follows_inbox_and_active_group_instead_of_stale_friends_navigation() {
        let bootstrap = Bootstrap {
            server_name: "Community".into(),
            channels: vec![Channel {
                id: "group".into(),
                name: "Friday Squad".into(),
                kind: "text".into(),
                ..Default::default()
            }],
            conversations: vec![relay_protocol::ConversationInfo {
                channel_id: "group".into(),
                kind: "group".into(),
                ..Default::default()
            }],
            members: vec![User {
                id: "peer".into(),
                username: "Mira".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let group = Target::Channel("group".into());
        assert_eq!(
            desktop_page_caption(
                Some(&bootstrap),
                Some(&group),
                Some(SettingsTab::Mentions),
                false
            ),
            "Inbox \u{2013} Community"
        );
        assert_eq!(
            desktop_page_caption(Some(&bootstrap), Some(&group), None, false),
            "Friday Squad \u{2013} Community"
        );
        assert_eq!(
            desktop_page_caption(
                Some(&bootstrap),
                Some(&Target::Direct("peer".into())),
                None,
                false
            ),
            "Mira \u{2013} Community"
        );
        assert_eq!(desktop_page_caption(None, None, None, false), "Relay");
    }

    #[test]
    fn caption_names_the_open_place_and_its_community_once() {
        let channel = |id: &str, name: &str, kind: &str| Channel {
            id: id.into(),
            name: name.into(),
            kind: kind.into(),
            ..Default::default()
        };
        let mut bootstrap = Bootstrap {
            server_name: "Community".into(),
            channels: vec![
                channel("general", "general", "text"),
                channel("lounge", "Lounge", "voice"),
            ],
            ..Default::default()
        };
        let caption = |b: &Bootstrap, target: Option<&Target>, home, nav| {
            desktop_page_caption(Some(b), target, home, nav)
        };
        let general = Target::Channel("general".into());
        let lounge = Target::Channel("lounge".into());
        let gone = Target::Channel("deleted".into());
        assert_eq!(
            caption(&bootstrap, Some(&general), None, true),
            "#general \u{2013} Community"
        );
        assert_eq!(
            caption(&bootstrap, Some(&lounge), None, true),
            "Lounge \u{2013} Community"
        );
        assert_eq!(
            caption(&bootstrap, None, Some(SettingsTab::Events), true),
            "Events \u{2013} Community"
        );
        assert_eq!(
            caption(&bootstrap, None, Some(SettingsTab::Friends), false),
            "Friends \u{2013} Community"
        );
        // An unavailable target falls back to the current community, and the
        // direct-messages list names it too.
        assert_eq!(caption(&bootstrap, Some(&gone), None, true), "Community");
        assert_eq!(
            caption(&bootstrap, None, None, false),
            "Direct messages \u{2013} Community"
        );
        // Other home pages and a community without a name show the brand once.
        assert_eq!(
            caption(&bootstrap, None, Some(SettingsTab::Appearance), true),
            "Relay"
        );
        bootstrap.server_name = "  ".into();
        assert_eq!(caption(&bootstrap, Some(&general), None, true), "#general");
        assert_eq!(caption(&bootstrap, None, None, true), "Relay");
    }

    #[test]
    fn caption_paints_its_context_and_no_second_brand() {
        let ctx = egui::Context::default();
        let mut texts = Vec::new();
        for _ in 0..2 {
            let output = ctx.run(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1080.0, 720.0))),
                    ..Default::default()
                },
                |ctx| {
                    egui::TopBottomPanel::top("desktop-titlebar")
                        .exact_height(32.0)
                        .show(ctx, |ui| {
                            desktop_caption(ui, "#general \u{2013} Community");
                        });
                },
            );
            texts = output
                .shapes
                .iter()
                .filter_map(|clipped| match &clipped.shape {
                    egui::Shape::Text(text) => Some(text.galley.job.text.clone()),
                    _ => None,
                })
                .collect();
        }
        assert_eq!(
            texts,
            ["#general \u{2013} Community"],
            "painted caption texts"
        );
    }

    fn frame(ctx: &egui::Context, events: Vec<Event>, maximized: bool) -> Vec<ViewportCommand> {
        let mut input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1080.0, 720.0))),
            events,
            ..Default::default()
        };
        input
            .viewports
            .get_mut(&ViewportId::ROOT)
            .unwrap()
            .maximized = Some(maximized);
        let output = ctx.run(input, |ctx| {
            egui::TopBottomPanel::top("desktop-titlebar")
                .exact_height(32.0)
                .frame(egui::Frame::none())
                .show(ctx, |ui| desktop_caption(ui, "Relay"));
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.label("Channel content");
            });
            // A full-window settings surface must not steal caption or border input.
            egui::Window::new("Settings")
                .title_bar(false)
                .movable(false)
                .resizable(false)
                .constrain(false)
                .fixed_rect(Rect::from_min_max(
                    Pos2::new(0.0, 32.0),
                    Pos2::new(1080.0, 720.0),
                ))
                .frame(egui::Frame::none())
                .show(ctx, |ui| {
                    ui.label("Appearance");
                });
            resize_edges(ctx);
        });
        output.viewport_output[&ViewportId::ROOT].commands.clone()
    }

    fn drag(position: Pos2, maximized: bool, button: PointerButton) -> Vec<ViewportCommand> {
        let ctx = egui::Context::default();
        for _ in 0..3 {
            frame(&ctx, vec![], maximized);
        }
        let mut commands = frame(
            &ctx,
            vec![
                Event::PointerMoved(position),
                Event::PointerButton {
                    pos: position,
                    button,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
            maximized,
        );
        commands.extend(frame(
            &ctx,
            vec![Event::PointerMoved(position + Vec2::new(40.0, 30.0))],
            maximized,
        ));
        commands
    }

    #[test]
    fn dragging_caption_text_and_empty_caption_moves_the_window_with_settings_open() {
        for x in [100.0, 500.0] {
            let commands = drag(Pos2::new(x, 16.0), false, PointerButton::Primary);
            assert!(
                commands.contains(&ViewportCommand::StartDrag),
                "x={x}: {commands:?}"
            );
            assert!(!commands
                .iter()
                .any(|c| matches!(c, ViewportCommand::BeginResize(_))));
        }
        for position in [Pos2::new(960.0, 16.0), Pos2::new(500.0, 100.0)] {
            assert!(!drag(position, false, PointerButton::Primary)
                .contains(&ViewportCommand::StartDrag));
        }
        assert!(
            !drag(Pos2::new(500.0, 16.0), false, PointerButton::Secondary)
                .contains(&ViewportCommand::StartDrag)
        );
    }

    #[test]
    fn dialogs_follow_the_viewport_after_repeated_resize_and_scale_changes() {
        let ctx = egui::Context::default();
        // Reuse the same persisted window state while growing and shrinking.
        // 670 points is the minimum desktop window at 140% UI scale.
        for (width, height) in [
            (1320.0, 850.0),
            (940.0, 650.0),
            (2560.0, 1392.0),
            (670.0, 464.0),
            (360.0, 420.0),
            (1320.0, 850.0),
        ] {
            let screen = Rect::from_min_size(Pos2::ZERO, Vec2::new(width, height));
            let mut bounds = Rect::NOTHING;
            let mut content_width = 0.0;
            for _ in 0..4 {
                let _ = ctx.run(
                    egui::RawInput {
                        screen_rect: Some(screen),
                        ..Default::default()
                    },
                    |ctx| {
                        let result = dialog(ctx, "Responsive dialog")
                            .default_width(400.0)
                            .resizable(false)
                            .show(ctx, |ui| {
                                content_width = ui.available_width();
                                ui.add_sized(
                                    [ui.available_width(), 32.0],
                                    egui::Button::new("Full-width control"),
                                );
                                for _ in 0..60 {
                                    ui.label("Scrollable content");
                                }
                            })
                            .unwrap();
                        bounds = result.response.rect;
                    },
                );
            }
            assert!(content_width <= 400.0, "{width}: {content_width}");
            assert!(content_width > 200.0, "{width}: {content_width}");
            assert!(
                bounds.left() >= 0.0 && bounds.right() <= width,
                "{width}: {bounds:?}"
            );
            assert!(
                bounds.top() >= 32.0 && bounds.bottom() <= height,
                "{height}: {bounds:?}"
            );
        }
    }

    #[test]
    fn minimized_frames_do_not_shrink_dialogs_or_sidebars_after_restore() {
        fn layout(ctx: &egui::Context, size: Vec2, minimized: Option<bool>) -> (Rect, f32) {
            let mut input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, size)),
                ..Default::default()
            };
            input
                .viewports
                .get_mut(&egui::ViewportId::ROOT)
                .unwrap()
                .minimized = minimized;
            preserve_minimized_layout(&mut input);
            let mut result = (Rect::NOTHING, 0.0);
            let _ = ctx.run(input, |ctx| {
                egui::TopBottomPanel::top("desktop-titlebar")
                    .exact_height(32.0)
                    .show(ctx, |_| {});
                result.1 = egui::SidePanel::left("channels")
                    .default_width(240.0)
                    .width_range(180.0..=360.0)
                    .resizable(true)
                    .show(ctx, |ui| {
                        ui.label("Channels");
                    })
                    .response
                    .rect
                    .width();
                result.0 = dialog(ctx, "Join friends")
                    .collapsible(false)
                    .resizable(false)
                    .default_width(460.0)
                    .show(ctx, |ui| {
                        ui.heading("Create your account");
                        for label in ["Username", "Password"] {
                            ui.label(label);
                            ui.add_sized(
                                [ui.available_width(), 32.0],
                                egui::TextEdit::singleline(&mut String::new()),
                            );
                        }
                        for _ in 0..12 {
                            ui.label("Account form content");
                        }
                        ui.button("Create account and join").clicked();
                    })
                    .unwrap()
                    .response
                    .rect;
            });
            result
        }

        for minimized in [Some(true), None, Some(false)] {
            let ctx = egui::Context::default();
            let mut before = (Rect::NOTHING, 0.0);
            for _ in 0..4 {
                before = layout(&ctx, Vec2::new(1000.0, 1000.0), Some(false));
            }
            assert!(
                before.0.width() > 450.0 && before.0.height() > 200.0,
                "Initial account form: {:?}",
                before.0
            );
            for _ in 0..2 {
                layout(&ctx, Vec2::new(160.0, 28.0), minimized);
            }
            for _ in 0..4 {
                let restored = layout(&ctx, Vec2::new(1000.0, 1000.0), Some(false));
                assert!(restored.0.width() >= before.0.width() - 1.0);
                assert!(restored.0.height() >= before.0.height() - 1.0);
                assert!((restored.1 - before.1).abs() < 1.0);
            }
            for _ in 0..4 {
                let resized = layout(&ctx, Vec2::new(940.0, 650.0), Some(false));
                assert!(resized.0.left() >= 0.0 && resized.0.right() <= 940.0);
                assert!(resized.0.top() >= 32.0 && resized.0.bottom() <= 650.0);
            }
        }
    }

    #[test]
    fn all_borders_and_corners_resize_above_the_settings_surface() {
        for (position, direction) in [
            (Pos2::new(2.0, 360.0), egui::ResizeDirection::West),
            (Pos2::new(1078.0, 360.0), egui::ResizeDirection::East),
            (Pos2::new(540.0, 2.0), egui::ResizeDirection::North),
            (Pos2::new(540.0, 718.0), egui::ResizeDirection::South),
            (Pos2::new(6.0, 6.0), egui::ResizeDirection::NorthWest),
            (Pos2::new(1074.0, 6.0), egui::ResizeDirection::NorthEast),
            (Pos2::new(6.0, 714.0), egui::ResizeDirection::SouthWest),
            (Pos2::new(1074.0, 714.0), egui::ResizeDirection::SouthEast),
        ] {
            let commands = drag(position, false, PointerButton::Primary);
            let resizes: Vec<_> = commands
                .iter()
                .filter(|c| matches!(c, ViewportCommand::BeginResize(_)))
                .collect();
            assert_eq!(
                resizes,
                [&ViewportCommand::BeginResize(direction)],
                "{position:?}: {commands:?}"
            );
            assert!(!commands.contains(&ViewportCommand::StartDrag));
            assert!(!drag(position, true, PointerButton::Primary)
                .iter()
                .any(|c| matches!(c, ViewportCommand::BeginResize(_))));
            assert!(!drag(position, false, PointerButton::Secondary)
                .iter()
                .any(|c| matches!(c, ViewportCommand::BeginResize(_))));
        }
    }
}
