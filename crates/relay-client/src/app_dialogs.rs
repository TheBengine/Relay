//! R8.2: the app's dialog driver and its media, sharing, transfer and GIF
//! dialogs, moved unchanged out of `app.rs`; only private methods' visibility
//! became `pub(super)`.
use super::*;

/// Create Channel's types: kind, name, and one line about what it is for.
const CHANNEL_TYPES: [(&str, &str, &str); 5] = [
    ("text", "Text", "Messages, images, GIFs and links"),
    (
        "voice",
        "Voice",
        "Talk together with voice, video and screen sharing",
    ),
    (
        "forum",
        "Forum",
        "Posts with their own replies, kept tidy by topic",
    ),
    (
        "announcement",
        "Announcement",
        "Important updates for the whole community",
    ),
    ("stage", "Stage", "Talks and events with an audience"),
];

/// One channel type, Discord's way: icon, name and a line about it, with a
/// radio mark on the right. The chosen type is filled.
fn channel_type_card(
    ui: &mut egui::Ui,
    kind: &str,
    title: &str,
    about: &str,
    selected: bool,
) -> egui::Response {
    let title_font = semibold(ui, font_size(16.0));
    let about_font = FontId::proportional(font_size(13.0));
    let height =
        (ui.fonts(|f| f.row_height(&title_font) + f.row_height(&about_font)) + 16.0).max(52.0);
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::click());
    response
        .widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::RadioButton, selected, title));
    let palette = UI_PALETTE.with(|palette| palette.get());
    let fill = if selected {
        palette.selection
    } else if response.hovered() {
        palette.hover
    } else {
        c_rail()
    };
    let painter = ui.painter().with_clip_rect(rect.intersect(ui.clip_rect()));
    painter.rect_filled(rect, 6.0, fill);
    if response.has_focus() {
        painter.rect_stroke(rect, 6.0, Stroke::new(2.0_f32, c_accent()));
    }
    let icon_rect = Rect::from_center_size(
        Pos2::new(rect.left() + 26.0, rect.center().y),
        Vec2::splat(22.0),
    );
    let tint = if selected { c_text() } else { c_muted() };
    match kind {
        "text" => {
            painter.text(
                icon_rect.center(),
                Align2::CENTER_CENTER,
                "#",
                FontId::proportional(font_size(24.0)),
                tint,
            );
        }
        "voice" => draw_icon(&painter, icon_rect, Icon::Headphones, tint),
        "forum" => draw_icon(&painter, icon_rect, Icon::Chat, tint),
        "announcement" => draw_icon(&painter, icon_rect, Icon::Megaphone, tint),
        _ => draw_icon(&painter, icon_rect, Icon::Users, tint),
    }
    let text_left = rect.left() + 52.0;
    let radio = Pos2::new(rect.right() - 22.0, rect.center().y);
    let text_clip = Rect::from_min_max(rect.min, Pos2::new(radio.x - 16.0, rect.bottom()));
    let text = painter.with_clip_rect(text_clip.intersect(ui.clip_rect()));
    let title_row = ui.fonts(|f| f.row_height(&title_font));
    let top = rect.center().y - (height - 16.0) / 2.0;
    text.text(
        Pos2::new(text_left, top),
        Align2::LEFT_TOP,
        title,
        title_font,
        c_text(),
    );
    text.text(
        Pos2::new(text_left, top + title_row),
        Align2::LEFT_TOP,
        about,
        about_font,
        c_muted(),
    );
    painter.circle_stroke(
        radio,
        9.0,
        Stroke::new(2.0_f32, if selected { c_text() } else { c_muted() }),
    );
    if selected {
        painter.circle_filled(radio, 5.0, c_text());
    }
    response
}

/// A tab in the Share Screen dialog: bright with an accent underline when chosen.
fn share_tab(ui: &mut egui::Ui, selected: bool, label: &str) -> egui::Response {
    let galley = ui.painter().layout_no_wrap(
        label.to_owned(),
        semibold(ui, font_size(15.0)),
        c_text(),
    );
    let size = galley.size() + Vec2::new(0.0, 12.0);
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    let ink = if selected || response.hovered() {
        c_text()
    } else {
        c_muted()
    };
    ui.painter()
        .galley(rect.left_top() + Vec2::new(0.0, 2.0), galley, ink);
    if selected {
        ui.painter().rect_filled(
            Rect::from_min_max(
                Pos2::new(rect.left(), rect.bottom() - 2.0),
                rect.right_bottom(),
            ),
            1.0,
            c_accent(),
        );
    }
    if response.has_focus() {
        ui.painter()
            .rect_stroke(rect.expand(3.0), 4.0, Stroke::new(1.0_f32, c_accent()));
    }
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, selected, label)
    });
    response
}

/// Discord's small uppercase section label.
fn section_label(ui: &mut egui::Ui, text: &str) {
    ui.label(
        RichText::new(text.to_uppercase())
            .size(font_size(12.0))
            .family(egui::FontFamily::Name("Relay Semibold".into()))
            .color(c_muted()),
    );
    ui.add_space(4.0);
}

/// A source card's height for a card `width` wide: picture, name, padding.
fn source_card_height(width: f32) -> f32 {
    let thumb = (width - 16.0) * 9.0 / 16.0;
    8.0 + thumb + 6.0 + font_size(14.0) * 1.4 + 8.0
}

/// One window or screen in the picker: its live picture, its name under it,
/// an accent ring when chosen.
fn source_card(
    ui: &mut egui::Ui,
    width: f32,
    source: &CaptureSource,
    picture: Option<&egui::TextureHandle>,
    unavailable: bool,
    selected: bool,
) -> egui::Response {
    const PAD: f32 = 8.0;
    let thumb = Vec2::new(width - 2.0 * PAD, (width - 2.0 * PAD) * 9.0 / 16.0);
    let (rect, response) = ui.allocate_exact_size(
        Vec2::new(width, source_card_height(width)),
        Sense::click(),
    );
    let painter = ui.painter_at(rect.expand(3.0));
    let fill = if selected || response.hovered() {
        c_hover()
    } else {
        c_card()
    };
    painter.rect_filled(rect, 8.0, fill);
    let stage = Rect::from_min_size(rect.min + Vec2::splat(PAD), thumb);
    painter.rect_filled(stage, 4.0, Color32::from_rgb(12, 12, 14));
    match picture {
        Some(texture) => {
            let size = texture.size_vec2();
            let scale = (stage.width() / size.x).min(stage.height() / size.y);
            let shown = Rect::from_center_size(stage.center(), size * scale);
            egui::Image::new((texture.id(), size * scale))
                .rounding(4.0)
                .paint_at(ui, shown);
        }
        None => {
            let glyph = Rect::from_center_size(stage.center() - Vec2::new(0.0, 8.0), Vec2::splat(28.0));
            draw_icon(&painter, glyph, Icon::Screen, Color32::from_gray(150));
            painter.text(
                Pos2::new(stage.center().x, glyph.bottom() + 12.0),
                Align2::CENTER_CENTER,
                if unavailable {
                    "No preview for this window"
                } else {
                    "Loading preview…"
                },
                egui::FontId::proportional(font_size(12.0)),
                Color32::from_gray(170),
            );
        }
    }
    let mut job = egui::text::LayoutJob::simple(
        source.name.clone(),
        egui::FontId::proportional(font_size(14.0)),
        if selected { c_text() } else { c_muted() },
        thumb.x,
    );
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    let galley = ui.painter().layout_job(job);
    painter.galley(
        Pos2::new(stage.left(), stage.bottom() + 6.0),
        galley,
        c_text(),
    );
    if selected {
        painter.rect_stroke(rect.expand(1.0), 9.0, Stroke::new(2.0_f32, c_accent()));
    } else if response.has_focus() {
        painter.rect_stroke(rect.expand(1.0), 9.0, Stroke::new(1.0_f32, c_accent()));
    }
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, selected, &source.name)
    });
    response.on_hover_text(&source.name)
}

/// A capture fixture's picture drawn like `SourcePreview::show`.
#[cfg(feature = "native-rtc")]
fn fixture_preview(ui: &mut egui::Ui, picture: &egui::TextureHandle) {
    let width = ui.available_width().max(160.0);
    let (rect, _) = ui.allocate_exact_size(
        Vec2::new(width, (width * 9.0 / 16.0).min(420.0)),
        Sense::hover(),
    );
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 8.0, Color32::from_gray(12));
    let size = picture.size_vec2();
    let scale = (rect.width() / size.x).min(rect.height() / size.y);
    egui::Image::new((picture.id(), size * scale))
        .rounding(8.0)
        .paint_at(ui, Rect::from_center_size(rect.center(), size * scale));
    let galley = painter.layout_no_wrap(
        "Preview · only you can see this".into(),
        egui::FontId::proportional(font_size(12.0)),
        Color32::WHITE,
    );
    let pill = Rect::from_min_size(
        rect.left_bottom() + Vec2::new(10.0, -18.0 - galley.size().y),
        galley.size() + Vec2::new(16.0, 8.0),
    );
    painter.rect_filled(pill, 6.0, Color32::from_black_alpha(170));
    painter.galley(pill.min + Vec2::new(8.0, 4.0), galley, Color32::WHITE);
}

/// A 16:9 stage like the live preview, for when there is none.
fn camera_placeholder(ui: &mut egui::Ui, text: &str) {
    let width = ui.available_width().max(160.0);
    let (rect, _) = ui.allocate_exact_size(
        Vec2::new(width, (width * 9.0 / 16.0).min(420.0)),
        Sense::hover(),
    );
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 8.0, Color32::from_gray(12));
    let glyph = Rect::from_center_size(rect.center() - Vec2::new(0.0, 14.0), Vec2::splat(40.0));
    draw_icon(&painter, glyph, Icon::Camera, Color32::from_gray(150));
    painter.text(
        Pos2::new(rect.center().x, glyph.bottom() + 18.0),
        Align2::CENTER_CENTER,
        text,
        egui::FontId::proportional(font_size(13.0)),
        Color32::from_gray(205),
    );
}

/// A choice pill: accent when chosen. Disabled pills explain why on hover.
fn quality_pill(ui: &mut egui::Ui, selected: bool, enabled: bool, label: &str) -> egui::Response {
    let text = RichText::new(label).size(font_size(14.0)).color(if selected {
        Color32::WHITE
    } else {
        c_text()
    });
    let button = egui::Button::new(text)
        .fill(if selected { c_accent() } else { c_card() })
        .rounding(4.0)
        .min_size(Vec2::new(56.0, 32.0));
    let response = ui.add_enabled(enabled, button);
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, selected, label)
    });
    response.on_disabled_hover_text("Needs the video connection, which starts when you join a call.")
}

fn quality_label(quality: CaptureQuality) -> &'static str {
    match quality {
        CaptureQuality::P480 => "480p",
        CaptureQuality::P720 => "720p",
        CaptureQuality::P1080 => "1080p",
        CaptureQuality::P1440 => "1440p",
        CaptureQuality::P2160 => "4K",
        CaptureQuality::Source => "Source",
    }
}

/// The resolution and frame-rate choices Discord shows for a stream or a
/// camera, written into the stored capture options. A stored value that is
/// not among the pills (chosen under More options) shows as its own pill.
fn quality_pills(
    ui: &mut egui::Ui,
    options: &mut CaptureOptions,
    camera: bool,
    native_video: bool,
) -> bool {
    let mut changed = false;
    let mut resolutions: Vec<(CaptureQuality, bool)> = if camera {
        vec![
            (CaptureQuality::P480, true),
            (CaptureQuality::P720, true),
            (CaptureQuality::P1080, true),
        ]
    } else {
        vec![
            (CaptureQuality::P720, true),
            (CaptureQuality::P1080, true),
            (CaptureQuality::P1440, native_video),
            (CaptureQuality::Source, native_video),
        ]
    };
    if !resolutions.iter().any(|(q, _)| *q == options.quality) {
        resolutions.push((options.quality, true));
    }
    let mut rates: Vec<(u32, bool)> = if camera {
        vec![(15, true), (30, true)]
    } else {
        vec![(15, true), (30, true), (60, native_video)]
    };
    if !rates.iter().any(|(fps, _)| *fps == options.fps) {
        rates.push((options.fps, true));
        rates.sort_by_key(|(fps, _)| *fps);
    }
    ui.horizontal_wrapped(|ui| {
        ui.vertical(|ui| {
            ui.label(
                RichText::new("Resolution")
                    .size(font_size(12.0))
                    .color(c_muted()),
            );
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                for (quality, enabled) in &resolutions {
                    if quality_pill(ui, options.quality == *quality, *enabled, quality_label(*quality))
                        .clicked()
                    {
                        options.quality = *quality;
                        changed = true;
                    }
                }
            });
        });
        ui.add_space(16.0);
        ui.vertical(|ui| {
            ui.label(
                RichText::new("Frame Rate")
                    .size(font_size(12.0))
                    .color(c_muted()),
            );
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                for (fps, enabled) in &rates {
                    if quality_pill(ui, options.fps == *fps, *enabled, &format!("{fps} FPS"))
                        .clicked()
                    {
                        options.fps = *fps;
                        changed = true;
                    }
                }
            });
        });
    });
    changed
}

impl RelayApp {
    pub(super) fn stream_preview_dialog(&mut self, ctx: &egui::Context) {
        let Some(key) = self.stream_preview.clone() else {
            return;
        };
        let Some(tile) = self.video.get(&key) else {
            self.stream_preview = None;
            return;
        };
        if self.stream_fullscreen {
            let mut close = false;
            let mut back = false;
            egui::Area::new(egui::Id::new("fullscreen-stream"))
                .order(egui::Order::Foreground)
                .fixed_pos(ctx.screen_rect().min)
                .show(ctx, |ui| {
                    egui::Frame::none()
                        .fill(c_rail())
                        .inner_margin(Margin::same(14.0))
                        .show(ui, |ui| {
                            ui.set_min_size(ctx.screen_rect().size() - Vec2::splat(28.0));
                            ui.horizontal_wrapped(|ui| {
                                ui.label(
                                    RichText::new(format!(
                                        "{} · {}",
                                        profile_ui::display_name(&tile.metadata.user),
                                        tile.metadata.kind
                                    ))
                                    .strong(),
                                );
                                back = ui.add(secondary("Exit Full Screen")).clicked();
                                close = ui.add(secondary("Close Stream")).clicked();
                            });
                            let size = tile.texture.size_vec2();
                            let scale = (ui.available_width() / size.x)
                                .min((ctx.screen_rect().height() - 80.0).max(50.0) / size.y);
                            ui.vertical_centered(|ui| {
                                ui.add(egui::Image::new((tile.texture.id(), size * scale)));
                            });
                        });
                });
            if close || back || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                self.stream_fullscreen = false;
                ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(
                    self.stream_restore_fullscreen,
                ));
                if close {
                    self.stream_preview = None;
                }
            }
            return;
        }
        let mut open = true;
        let title = format!(
            "{} · {}",
            profile_ui::display_name(&tile.metadata.user),
            if tile.metadata.kind == "screen" {
                "Screen share"
            } else {
                "Camera"
            }
        );
        let window = desktop_ui::dialog(ctx, title.as_str())
            .id(egui::Id::new("stream-preview"))
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_width((ctx.screen_rect().width() - 120.0).clamp(320.0, 1120.0));
        // The shared header (title, Close, Escape) stays above the scrolling body;
        // full screen keeps its own controls and Escape path above.
        if host_setup_ui::show_with_header(ctx, window, &title, |ui| {
            let dimensions = tile.texture.size_vec2();
            let scale = (ui.available_width() / dimensions.x)
                .min((ctx.screen_rect().height() - 150.0).max(100.0) / dimensions.y);
            ui.add(egui::Image::new((tile.texture.id(), dimensions * scale)).rounding(8.0));
            ui.horizontal(|ui| {
                if ui.add(secondary("Full Screen")).clicked() {
                    self.stream_restore_fullscreen =
                        ctx.input(|i| i.viewport().fullscreen.unwrap_or(false));
                    self.stream_fullscreen = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(true));
                }
                if ui
                    .add(secondary("Pop Out"))
                    .on_hover_text("Open this stream in its own window you can move and resize")
                    .clicked()
                {
                    self.stream_popout = Some(key.clone());
                    self.stream_preview = None;
                }
                let pinned = self.pinned_stream.as_ref() == Some(&key);
                if ui
                    .add(secondary(if pinned { "Unpin Stream" } else { "Pin Stream" }))
                    .clicked()
                {
                    self.pinned_stream = if pinned { None } else { Some(key.clone()) };
                }
            });
        }) {
            open = false;
        }
        if !open {
            self.stream_preview = None;
        }
    }

    /// Discord's Screen Share and camera preview dialogs. Screens: tabs for
    /// applications and screens, a grid of live thumbnails, stream quality as
    /// pills, then Go Live. Camera: a live preview first, the device and its
    /// quality, then Turn On Camera. Nothing is sent before the final button.
    pub(super) fn share_dialog(&mut self, ctx: &egui::Context) {
        let native_video = self.rtc_active() || self.call_state().video_connected;
        if !self.inventory_loaded && self.inventory_rx.is_none() {
            self.refresh_devices();
        }
        let camera = self.share_picker.unwrap_or(false);
        let title = if camera { "Turn on your camera" } else { "Share your screen" };
        let width = (if camera { 520.0_f32 } else { 680.0 }).min(ctx.screen_rect().width() - 48.0);
        let mut open = true;
        desktop_ui::dialog(ctx, title)
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(width)
            // Tall enough for the footer on first show; the dialog scrolls
            // (desktop_ui::dialog) when the window is shorter than this.
            .min_height((if camera { 640.0_f32 } else { 600.0 }).min(ctx.screen_rect().height() - 150.0))
            .title_bar(false)
            .frame(
                egui::Frame::none()
                    .fill(c_panel())
                    .rounding(12.0)
                    .inner_margin(Margin::same(20.0)),
            )
            .show(ctx, |ui| {
                ui.set_width(width);
                // The shared header (title, Close, Escape) stays above the
                // scrolling body. Closing cancels: nothing starts.
                if profile_ui::dialog_header_ui::dialog_header(ui, title) {
                    self.share_picker = None;
                }
                if camera {
                    self.camera_picker_body(ui, native_video);
                } else {
                    // The grid scrolls on its own; quality and Go Live stay in view.
                    self.screen_picker_body(ui, native_video);
                }
                for warning in &self.inventory.warnings {
                    ui.label(
                        RichText::new(warning)
                            .size(font_size(TEXT_BODY))
                            .color(c_bad()),
                    );
                }
                ui.add_space(12.0);
                self.share_footer(ui, camera);
            });
        if !open {
            self.share_picker = None;
        }
        if self.share_picker.is_none() {
            self.stop_source_preview();
            self.share_thumbnails = None;
        }
    }

    /// The source the dialog would start, if it is still listed.
    fn picked_source(&self, camera: bool) -> Option<CaptureSource> {
        let id = if camera {
            self.camera_source.as_ref()
        } else {
            self.screen_source.as_ref()
        }?;
        let list = if camera {
            &self.inventory.cameras
        } else {
            &self.inventory.screens
        };
        list.iter().find(|source| &source.id == id).cloned()
    }

    /// Cancel and the start button, with the reason when it cannot start yet.
    fn share_footer(&mut self, ui: &mut egui::Ui, camera: bool) {
        let source = self.picked_source(camera);
        let call = self.call_state();
        let video_wait = self.rtc_starting() && self.capture_path.is_none();
        let ready = source.is_some() && call.ready && !video_wait && self.inventory_rx.is_none();
        let reason = if self.inventory_rx.is_some() {
            Some("Finding sources…")
        } else if source.is_none() {
            Some(if camera {
                "Choose a camera."
            } else {
                "Choose a window or screen."
            })
        } else if !call.ready {
            Some("Join a voice room first.")
        } else if video_wait {
            Some(if self.rtc_reconnecting() {
                "Reconnecting video…"
            } else {
                "Connecting video…"
            })
        } else {
            None
        };
        ui.horizontal(|ui| {
            if let Some(reason) = reason {
                if video_wait || self.inventory_rx.is_some() {
                    ui.spinner();
                }
                ui.label(
                    RichText::new(reason)
                        .size(font_size(TEXT_BODY))
                        .color(c_muted()),
                );
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let start = primary(if camera { "Turn On Camera" } else { "Go Live" })
                    .min_size(Vec2::new(112.0, 38.0));
                let hint = if camera {
                    "Others see you after this. The preview above is only on this PC."
                } else {
                    "Shares only the window or screen you picked. The pictures in this dialog stay on this PC."
                };
                if ui.add_enabled(ready, start).on_hover_text(hint).clicked() {
                    if let Some(source) = source {
                        self.start_capture(camera, source);
                    }
                    self.share_picker = None;
                }
                if ui
                    .add(secondary("Cancel").min_size(Vec2::new(0.0, 38.0)))
                    .clicked()
                {
                    self.share_picker = None;
                }
            });
        });
    }

    fn screen_picker_body(&mut self, ui: &mut egui::Ui, native_video: bool) {
        // Discord's tabs: Applications (windows) and Screens (displays).
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 20.0;
            for (kind, label) in [
                (SourceKind::Window, "Applications"),
                (SourceKind::Monitor, "Screens"),
            ] {
                if share_tab(ui, self.share_kind == kind, label).clicked() {
                    self.share_kind = kind;
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui
                    .add_enabled(
                        self.inventory_rx.is_none(),
                        egui::Button::new(
                            RichText::new("Refresh")
                                .size(font_size(TEXT_BODY))
                                .color(c_muted()),
                        )
                        .frame(false),
                    )
                    .on_hover_text("Look for windows and screens again")
                    .clicked()
                {
                    self.refresh_devices();
                }
            });
        });
        let line = ui.cursor().min.y;
        ui.painter().hline(
            ui.max_rect().x_range(),
            line,
            Stroke::new(1.0_f32, c_line()),
        );
        ui.add_space(12.0);
        let all = self.inventory.screens.clone();
        let sources: Vec<_> = all
            .iter()
            .filter(|source| source.kind == self.share_kind)
            .cloned()
            .collect();
        if self
            .screen_source
            .as_ref()
            .is_none_or(|id| !sources.iter().any(|source| source.id == *id))
        {
            self.screen_source = sources.first().map(|source| source.id.clone());
        }
        if self.capture_path.is_none() {
            self.share_thumbnails
                .get_or_insert_with(Default::default)
                .update(ui.ctx(), &all);
        }
        if self.inventory_rx.is_some() && sources.is_empty() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Finding windows and screens…");
            });
        } else if sources.is_empty() {
            ui.add_space(16.0);
            ui.vertical_centered(|ui| {
                icon(ui, Icon::Screen, 40.0, c_muted());
                ui.add_space(8.0);
                ui.label(
                    RichText::new(if self.share_kind == SourceKind::Window {
                        "No windows to share. Open the app you want to share, then Refresh."
                    } else {
                        "No screens found. Refresh to look again."
                    })
                    .color(c_muted()),
                );
            });
            ui.add_space(16.0);
        } else {
            let mut picked = None;
            let mut go = false;
            let columns = if ui.available_width() < 420.0 { 2 } else { 3 };
            let gap = 12.0;
            // Leave room for the scrollbar so cards never sit under it.
            let card = (ui.available_width() - 12.0 - gap * (columns as f32 - 1.0))
                / columns as f32;
            let row = source_card_height(card) + gap;
            // One or two whole rows (the rest scrolls), so quality and Go Live
            // stay in view and no card is cut through its name.
            let room = ui.ctx().screen_rect().height() - 520.0;
            let max_height = if room >= 2.0 * row { 2.0 * row } else { row };
            egui::ScrollArea::vertical()
                .id_source("share-sources")
                .max_height(max_height)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    for row in sources.chunks(columns) {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = gap;
                            for source in row {
                                let selected = self.screen_source.as_ref() == Some(&source.id);
                                let thumbnails = self.share_thumbnails.as_ref();
                                let response = source_card(
                                    ui,
                                    card,
                                    source,
                                    thumbnails.and_then(|t| t.texture(&source.id)),
                                    thumbnails.is_some_and(|t| t.unavailable(&source.id)),
                                    selected,
                                );
                                if response.clicked() {
                                    picked = Some(source.id.clone());
                                }
                                if response.double_clicked() {
                                    picked = Some(source.id.clone());
                                    go = true;
                                }
                            }
                        });
                        ui.add_space(gap);
                    }
                });
            if let Some(id) = picked {
                self.screen_source = Some(id);
            }
            if go && self.call_state().ready && !self.rtc_starting() {
                if let Some(source) = self.picked_source(false) {
                    self.start_capture(false, source);
                    self.share_picker = None;
                    return;
                }
            }
        }
        ui.add_space(8.0);
        section_label(ui, "Stream Quality");
        let options = &mut self.prefs.screen_options;
        let mut changed = quality_pills(ui, options, false, native_video);
        let can_audio =
            native_video && self.share_kind == SourceKind::Window && !sources.is_empty();
        let mut include_audio = options.application_audio && can_audio;
        ui.add_space(4.0);
        if ui
            .add_enabled(
                can_audio,
                egui::Checkbox::new(&mut include_audio, "Share the application's sound"),
            )
            .on_disabled_hover_text(if self.share_kind == SourceKind::Window {
                "Needs the video connection, which starts when you join a call."
            } else {
                "Sound can be shared from an application window."
            })
            .changed()
        {
            options.application_audio = include_audio;
            changed = true;
        }
        ui.add_space(4.0);
        egui::CollapsingHeader::new(
            RichText::new("More options")
                .size(font_size(TEXT_BODY))
                .color(c_muted()),
        )
        .id_source("share-more-options")
        .show(ui, |ui| {
            changed |= capture_options_ui(ui, "sharing-profile", options, native_video);
        });
        if changed {
            self.save_preferences();
        }
    }

    fn camera_picker_body(&mut self, ui: &mut egui::Ui, native_video: bool) {
        let sources = self.inventory.cameras.clone();
        if self
            .camera_source
            .as_ref()
            .is_none_or(|id| !sources.iter().any(|source| source.id == *id))
        {
            self.camera_source = sources.first().map(|source| source.id.clone());
        }
        let source = self.picked_source(true);
        #[cfg(feature = "native-rtc")]
        {
            // Discord opens the preview as soon as the dialog opens: you see
            // yourself before anyone else can. One try per camera, so a busy
            // or broken camera does not retry every frame.
            if self.source_preview.as_ref().is_some_and(|preview| {
                !preview.camera || source.as_ref().is_none_or(|s| s.id != preview.source_id)
            }) {
                self.stop_source_preview();
            }
            let tried = egui::Id::new("camera-preview-tried");
            let tried_for = ui.ctx().data(|d| d.get_temp::<String>(tried));
            if let Some(source) = source.as_ref().filter(|source| {
                self.capture_path.is_none()
                    && self.source_preview.is_none()
                    && self.preview_release.is_none()
                    && self.inventory_rx.is_none()
                    && !self.source_active(true)
                    && !self.source_starting(true)
                    && tried_for.as_deref() != Some(source.id.as_str())
            }) {
                ui.ctx()
                    .data_mut(|d| d.insert_temp(tried, source.id.clone()));
                match source_preview::SourcePreview::new(source.clone(), true) {
                    Ok(preview) => self.source_preview = Some(preview),
                    Err(error) => self.error = Some(crate::capture::camera_error_message(&error)),
                }
            }
            let fixture = self
                .share_thumbnails
                .as_ref()
                .filter(|_| self.capture_path.is_some())
                .zip(source.as_ref())
                .and_then(|(thumbnails, source)| thumbnails.texture(&source.id).cloned());
            if let Some(preview) = &self.source_preview {
                preview.show(ui);
            } else if let Some(picture) = fixture {
                // Capture fixtures: a generated picture in the same stage.
                fixture_preview(ui, &picture);
            } else {
                camera_placeholder(
                    ui,
                    if self.inventory_rx.is_some() {
                        "Looking for cameras…"
                    } else if sources.is_empty() {
                        "No camera found. Connect one, then choose Refresh."
                    } else if self.preview_release.is_some() {
                        "Closing the preview…"
                    } else if self.source_active(true) {
                        "Your camera is already on."
                    } else {
                        "The preview could not start. Pick another camera or try again."
                    },
                );
            }
        }
        #[cfg(not(feature = "native-rtc"))]
        camera_placeholder(ui, "Camera preview needs the native video build.");
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.set_width((ui.available_width() - 90.0).max(160.0));
                source_combo(ui, "Camera", "sharing-source", &mut self.camera_source, &sources);
            });
            ui.vertical(|ui| {
                ui.add_space(font_size(12.0) * 1.4);
                if ui
                    .add_enabled(self.inventory_rx.is_none(), secondary("Refresh"))
                    .clicked()
                {
                    ui.ctx()
                        .data_mut(|d| d.remove::<String>(egui::Id::new("camera-preview-tried")));
                    self.refresh_devices();
                }
            });
        });
        ui.add_space(12.0);
        section_label(ui, "Video Quality");
        let options = &mut self.prefs.camera_options;
        let mut changed = quality_pills(ui, options, true, native_video);
        egui::CollapsingHeader::new(
            RichText::new("More options")
                .size(font_size(TEXT_BODY))
                .color(c_muted()),
        )
        .id_source("camera-more-options")
        .show(ui, |ui| {
            changed |= capture_options_ui(ui, "camera-profile", options, native_video);
        });
        if changed {
            self.save_preferences();
        }
        ui.label(
            RichText::new("The preview is only on this PC. Others see you after Turn On Camera.")
                .size(font_size(12.0))
                .color(c_muted()),
        );
    }

    /// A stream in its own window that the viewer can move and resize, as
    /// Discord's Pop Out. The picture fits the window; Escape or the window's
    /// close button ends it. Where Windows cannot open a second window, egui
    /// shows it as a movable window inside Relay instead.
    pub(super) fn stream_popout_window(&mut self, ctx: &egui::Context) {
        let Some(key) = self.stream_popout.clone() else {
            return;
        };
        let Some(tile) = self.video.get(&key).cloned() else {
            self.stream_popout = None;
            return;
        };
        let name = profile_ui::display_name(&tile.metadata.user).to_owned();
        let kind = if tile.metadata.kind == "screen" { "Screen" } else { "Camera" };
        let title = format!("{name} · {kind} — Relay");
        let size = tile.texture.size_vec2();
        let start = if size.x > 0.0 && size.y > 0.0 {
            let scale = (960.0 / size.x).min(540.0 / size.y);
            size * scale
        } else {
            Vec2::new(960.0, 540.0)
        };
        let mut close = false;
        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of(("stream-popout", &key)),
            egui::ViewportBuilder::default()
                .with_title(title)
                .with_inner_size(start)
                .with_min_inner_size([320.0, 180.0])
                .with_resizable(true),
            |ctx, _class| {
                egui::CentralPanel::default()
                    .frame(egui::Frame::none().fill(Color32::BLACK))
                    .show(ctx, |ui| {
                        let area = ui.max_rect();
                        if size.x > 0.0 && size.y > 0.0 {
                            let scale = (area.width() / size.x).min(area.height() / size.y);
                            let shown = Rect::from_center_size(area.center(), size * scale);
                            egui::Image::new((tile.texture.id(), size * scale)).paint_at(ui, shown);
                        }
                        let painter = ui.painter();
                        let galley = painter.layout_no_wrap(
                            format!("{name} · {kind}"),
                            egui::FontId::proportional(font_size(13.0)),
                            Color32::WHITE,
                        );
                        let pill = Rect::from_min_size(
                            area.left_bottom() + Vec2::new(10.0, -18.0 - galley.size().y),
                            galley.size() + Vec2::new(16.0, 8.0),
                        );
                        painter.rect_filled(pill, 6.0, Color32::from_black_alpha(153));
                        painter.galley(pill.min + Vec2::new(8.0, 4.0), galley, Color32::WHITE);
                    });
                if ctx.input(|i| i.viewport().close_requested() || i.key_pressed(egui::Key::Escape)) {
                    close = true;
                }
            },
        );
        if close {
            self.stream_popout = None;
        }
        ctx.request_repaint_after(Duration::from_millis(33));
    }

    pub(super) fn transfers_dialog(&mut self, ctx: &egui::Context) {
        let mut open = true;
        let window = desktop_ui::dialog(ctx, "Transfers")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(500.0);
        // The shared header (title, Close, Escape) stays above the scrolling body.
        if host_setup_ui::show_with_header(ctx, window, "Transfers", |ui| {
            let update_visible = self.update_ui.show_transfer(ui);
            if self.uploads.is_empty() && !update_visible {
                ui.label("All transfers have finished.");
            }
            let ids: Vec<_> = self.uploads.keys().cloned().collect();
            egui::ScrollArea::vertical()
                .max_height((ctx.screen_rect().height() - 160.0).clamp(180.0, 500.0))
                .show(ui, |ui| {
                    for id in ids {
                        let Some(transfer) = self.uploads.get(&id) else {
                            continue;
                        };
                        let name = transfer
                            .path
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned();
                        let error = transfer.error.clone();
                        let cancelled = transfer.cancelled;
                        let transferred = transfer.transferred;
                        let total = transfer.total;
                        let kind = match transfer.kind {
                            TransferKind::Upload => "Uploading",
                            TransferKind::Download(_) => "Downloading",
                            TransferKind::Backup => "Saving backup",
                        };
                        ui.label(RichText::new(format!("{kind} · {name}")).strong());
                        if total > 0 {
                            ui.add(
                                egui::ProgressBar::new(
                                    (transferred as f32 / total as f32).clamp(0.0, 1.0),
                                )
                                .text(format!(
                                    "{} / {}",
                                    size_label(transferred),
                                    size_label(total)
                                )),
                            );
                        } else if error.is_none() {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label(if transferred > 0 {
                                    format!("{} transferred", size_label(transferred))
                                } else {
                                    "Preparing transfer…".into()
                                });
                            });
                        }
                        if let Some(error) = error {
                            ui.label(
                                RichText::new(if cancelled {
                                    "Transfer cancelled".into()
                                } else {
                                    error
                                })
                                .size(font_size(12.0))
                                .color(if cancelled {
                                    c_muted()
                                } else {
                                    c_bad()
                                }),
                            );
                            ui.horizontal(|ui| {
                                if ui.add(secondary("Retry")).clicked() {
                                    self.retry_transfer(&id);
                                }
                                if ui.small_button("Dismiss").clicked() {
                                    self.uploads.remove(&id);
                                }
                            });
                        } else if ui.small_button("Cancel").clicked() {
                            self.send(Command::CancelUpload {
                                operation_id: id.clone(),
                            });
                        }
                        ui.add_space(10.0);
                        ui.separator();
                    }
                });
        }) {
            open = false;
        }
        self.transfers_open &= open;
    }

    pub(super) fn gif_picker_content(&mut self, ui: &mut egui::Ui) -> Option<String> {
        let ctx = ui.ctx().clone();
        if self.gif_previews.is_none() {
            self.gif_previews = Some(GifPreviews::new(ctx.clone()));
        }
        if self.online_gif_tabs(ui) {
            return self.online_gif_picker(ui);
        }
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.gif_favorites, false, "Recent");
            ui.selectable_value(&mut self.gif_favorites, true, "Favorites");
            if ui.add(secondary("Add GIF")).clicked() {
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter("GIF images", &["gif"])
                    .pick_file()
                {
                    self.prefs.gifs.remember(path);
                    self.save_preferences();
                }
            }
        });
        ui.add(
            egui::TextEdit::singleline(&mut self.gif_query)
                .hint_text("Search your files")
                .desired_width(f32::INFINITY),
        );
        ui.label(
            RichText::new("GIFs stay on this PC until you choose Send.")
                .size(font_size(TEXT_CAPTION))
                .color(c_muted()),
        );
        let entries = self.prefs.gifs.entries(self.gif_favorites, &self.gif_query);
        if entries.is_empty() {
            ui.add_space(18.0);
            ui.label(
                RichText::new(if self.gif_favorites {
                    "Favorite a GIF to keep it here."
                } else {
                    "Add a GIF from your computer to start your collection."
                })
                .color(c_muted()),
            );
        }
        egui::ScrollArea::vertical()
            .max_height(ui.available_height().max(1.0))
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    for entry in entries {
                        egui::Frame::none()
                            .fill(c_card())
                            .rounding(8.0)
                            .inner_margin(Margin::same(8.0))
                            .show(ui, |ui| {
                                ui.vertical(|ui| {
                                    ui.set_width(154.0);
                                    let tile = self.local_gif_tile(ui, &entry.path);
                                    tile.context_menu(|ui| {
                                        if ui.button("Remove from this collection").clicked() {
                                            self.prefs.gifs.remove(&entry.path);
                                            self.save_preferences();
                                            ui.close_menu();
                                        }
                                    });
                                    if tile.clicked() && !self.gif_errors.contains_key(&entry.path)
                                    {
                                        if let Some(target) = self.current.clone() {
                                            self.prefs.gifs.remember(entry.path.clone());
                                            self.save_preferences();
                                            self.start_gif_upload(target, entry.path.clone());
                                            self.gif_picker = false;
                                        }
                                    }
                                    let name = entry
                                        .path
                                        .file_name()
                                        .unwrap_or_default()
                                        .to_string_lossy();
                                    ui.add(
                                        egui::Label::new(RichText::new(name).size(font_size(12.0)))
                                            .truncate(true),
                                    )
                                    .on_hover_text(entry.path.display().to_string());
                                    if let Some(error) = self.gif_errors.get(&entry.path) {
                                        ui.label(
                                            RichText::new(error)
                                                .size(font_size(TEXT_CAPTION))
                                                .color(c_bad()),
                                        );
                                    }
                                    ui.horizontal(|ui| {
                                        if ui
                                            .add_enabled(
                                                !self.gif_errors.contains_key(&entry.path),
                                                secondary("Send"),
                                            )
                                            .clicked()
                                        {
                                            if let Some(target) = self.current.clone() {
                                                self.prefs.gifs.remember(entry.path.clone());
                                                self.save_preferences();
                                                self.start_gif_upload(target, entry.path.clone());
                                                self.gif_picker = false;
                                            }
                                        }
                                        if ui
                                            .small_button(if entry.favorite {
                                                "Unfavorite"
                                            } else {
                                                "Favorite"
                                            })
                                            .clicked()
                                        {
                                            self.prefs.gifs.toggle_favorite(&entry.path);
                                            self.save_preferences();
                                        }
                                    });
                                });
                            });
                    }
                });
            });
        None
    }

    pub(super) fn dialogs(&mut self, ctx: &egui::Context) {
        self.audio_setup_dialog(ctx);
        self.squads_window(ctx);
        self.compact_members_dialog(ctx);
        self.profile_card(ctx);
        self.social_dialogs(ctx);
        self.conversation_dialogs(ctx);
        self.poll_dialog(ctx);
        self.crop_image_dialog(ctx);
        self.safety_number_dialog(ctx);
        self.expressions_dialog(ctx);
        let mut checklist = std::mem::take(&mut self.prefs.host_checklist);
        if self.observe_host_progress(&mut checklist) {
            self.defer_preferences();
        }
        self.prefs.host_checklist = checklist;
        self.community_tools_dialogs(ctx);
        self.pins_dialog(ctx);
        if self.add_community_open {
            self.community_dialog(ctx);
        }
        if self.quick_switch {
            self.quick_switch_dialog(ctx);
        }
        if self.transfers_open {
            self.transfers_dialog(ctx);
        }
        self.server_options_menu(ctx);
        if let Some(message) = self.delete_confirm.clone() {
            let mut open = true;
            let window = desktop_ui::dialog(ctx, "Delete message?")
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .default_width(350.0);
            // The shared header (title, Close, Escape) stays above the scrolling body.
            // Closing cancels; only the Delete button deletes.
            if host_setup_ui::show_with_header(ctx, window, "Delete message?", |ui| {
                ui.label("This will remove the message and its attachments.");
                ui.add_space(8.0);
                ui.add(
                    egui::Label::new(RichText::new(&message.content).color(c_muted())).wrap(true),
                );
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if ui
                        .add(
                            egui::Button::new(RichText::new("Delete").color(Color32::WHITE))
                                .fill(Color32::from_rgb(177, 53, 65)),
                        )
                        .clicked()
                    {
                        self.send(Command::DeleteMessage(message.id.clone()));
                        self.delete_confirm = None;
                    }
                    if ui.add(secondary("Cancel")).clicked() {
                        self.delete_confirm = None;
                    }
                });
            }) {
                open = false;
            }
            if !open {
                self.delete_confirm = None;
            }
        }
        if self.share_picker.is_some() {
            self.share_dialog(ctx);
        } else {
            self.share_thumbnails = None;
        }
        self.poll_image_copy();
        if self.media_preview.is_some() {
            self.media_preview_dialog(ctx);
        }
        self.online_gif_preview_dialog(ctx);
        if self.stream_preview.is_some() {
            self.stream_preview_dialog(ctx);
        }
        if self.stream_popout.is_some() {
            self.stream_popout_window(ctx);
        }
        if let Some(url) = self.pending_link.clone() {
            let mut open = true;
            let mut go = false;
            let mut cancel = false;
            let window = desktop_ui::dialog(ctx, "Leaving Relay")
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .default_width(420.0);
            if host_setup_ui::show_with_header(ctx, window, "Leaving Relay", |ui| {
                ui.label("This link goes to:");
                ui.label(
                    RichText::new(crate::message_format::link_host(&url))
                        .strong()
                        .size(font_size(16.0)),
                );
                ui.add(egui::Label::new(RichText::new(&url).color(c_muted())).wrap(true));
                ui.add_space(4.0);
                ui.label(
                    RichText::new("Only open it if you trust where it goes.").color(c_muted()),
                );
                ui.add_space(14.0);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    go = ui.add(primary("Open link")).clicked();
                    cancel = ui.add(secondary("Cancel")).clicked();
                });
            }) {
                open = false;
                go = false;
            }
            if go {
                ctx.open_url(egui::OpenUrl::new_tab(&url));
            }
            if go || cancel || !open {
                self.pending_link = None;
            }
        }
        if self.create_channel_open {
            let mut open = true;
            let mut create = false;
            let mut cancel = false;
            let window = desktop_ui::dialog(ctx, "Create Channel")
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .default_width(460.0)
                // Room for every type, the name and the buttons without scrolling.
                .default_height(640.0);
            // The shared header (title, Close, Escape) stays above the scrolling body.
            if host_setup_ui::show_with_header(ctx, window, "Create Channel", |ui| {
                ui.label(
                    RichText::new("CHANNEL TYPE")
                        .size(font_size(crate::appearance::TEXT_CAPTION))
                        .color(c_muted())
                        .family(egui::FontFamily::Name("Relay Semibold".into()))
                        .strong(),
                );
                ui.add_space(6.0);
                for (kind, title, about) in CHANNEL_TYPES {
                    if channel_type_card(ui, kind, title, about, self.channel_kind == kind)
                        .clicked()
                    {
                        self.channel_kind = kind;
                    }
                    ui.add_space(6.0);
                }
                field(ui, "CHANNEL NAME", &mut self.channel_name, false);
                if self.channel_kind == "text" {
                    field(ui, "TOPIC (OPTIONAL)", &mut self.channel_topic, false);
                }
                ui.add_space(18.0);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    create = ui
                        .add_enabled(
                            !self.channel_name.trim().is_empty(),
                            primary("Create Channel"),
                        )
                        .clicked();
                    cancel = ui.add(secondary("Cancel")).clicked();
                });
            }) {
                open = false;
                // Closing never creates the channel.
                create = false;
            }
            if create && !matches!(self.channel_kind, "text" | "voice") {
                // Forum, announcement and stage channels are conversations on the server.
                self.admin_request(
                    AdminMethod::Post,
                    "/api/conversations".into(),
                    serde_json::json!({
                        "name": self.channel_name.trim(),
                        "kind": self.channel_kind,
                        "parent_id": null,
                        "member_ids": [],
                    }),
                    "action:conversation:create".into(),
                );
                self.channel_name.clear();
                self.channel_topic.clear();
                self.create_channel_open = false;
            } else if create {
                if let Some(origin) = self.command_origin() {
                    self.send(Command::CreateChannel {
                        origin,
                        request: CreateChannel {
                            name: self.channel_name.trim().into(),
                            kind: self.channel_kind.into(),
                            topic: self.channel_topic.trim().into(),
                            visibility: "public".into(),
                            category_id: None,
                            position: 0,
                            slowmode_seconds: 0,
                        },
                    });
                    self.channel_name.clear();
                    self.channel_topic.clear();
                    self.create_channel_open = false;
                } else {
                    self.error = Some("Connect to a community before creating a channel.".into());
                }
            }
            self.create_channel_open &= open && !cancel;
        }
        self.invite_dialog(ctx);
    }
}
