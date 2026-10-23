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

impl RelayApp {
    pub(super) fn media_preview_dialog(&mut self, ctx: &egui::Context) {
        let Some(id) = self.media_preview.clone() else {
            return;
        };
        self.request_original_preview(ctx, &id);
        let Some(image) = self.chat_media.get_mut(&id) else {
            self.media_preview = None;
            return;
        };
        let mut open = true;
        let window = desktop_ui::dialog(ctx, "Image preview")
            .open(&mut open)
            .collapsible(false)
            .resizable(false);
        // The shared header (title, Close, Escape) stays above the scrolling body.
        if host_setup_ui::show_with_header(ctx, window, "Image preview", |ui| {
            let dimensions = image.dimensions();
            let scale = ((ctx.screen_rect().width() - 120.0).min(800.0) / dimensions.x)
                .min((ctx.screen_rect().height() - 160.0) / dimensions.y)
                .min(2.0);
            let size = dimensions * scale;
            let texture = image.texture_with_autoplay(
                ctx,
                true,
                self.prefs.reduced_motion,
                self.prefs.appearance.gif_autoplay,
            );
            let response = ui.add(
                egui::Image::new((texture, size))
                    .rounding(8.0)
                    .sense(Sense::click()),
            );
            response.widget_info(|| {
                egui::WidgetInfo::labeled(egui::WidgetType::Button, "GIF animation")
            });
            if image.is_animated() && !self.prefs.reduced_motion && response.clicked() {
                image.set_paused(!image.is_paused_for(self.prefs.appearance.gif_autoplay));
            }
        }) {
            open = false;
        }
        if !open {
            self.media_preview = None;
        }
    }

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
            if ui.add(secondary("Full Screen")).clicked() {
                self.stream_restore_fullscreen =
                    ctx.input(|i| i.viewport().fullscreen.unwrap_or(false));
                self.stream_fullscreen = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(true));
            }
            let pinned = self.pinned_stream.as_ref() == Some(&key);
            if ui
                .small_button(if pinned { "Unpin stream" } else { "Pin stream" })
                .clicked()
            {
                self.pinned_stream = if pinned { None } else { Some(key.clone()) };
            }
        }) {
            open = false;
        }
        if !open {
            self.stream_preview = None;
        }
    }

    pub(super) fn share_dialog(&mut self, ctx: &egui::Context) {
        let native_video = self.rtc_active();
        if !self.inventory_loaded && self.inventory_rx.is_none() {
            self.refresh_devices();
        }
        let camera = self.share_picker.unwrap_or(false);
        let mut open = true;
        desktop_ui::dialog(ctx, if camera { "Turn on your camera" } else { "Share your screen" })
            .open(&mut open).collapsible(false).resizable(false).default_width(480.0)
            .title_bar(false)
            .frame(egui::Frame::none().fill(c_panel()).rounding(12.0).inner_margin(Margin::same(20.0)))
            .show(ctx, |ui| {
                ui.set_width(480.0);
                // The shared header (title, Close, Escape) stays above the chooser's own
                // scrolling area. Closing cancels like the old X: nothing starts.
                let title = if camera { "Turn on your camera" } else { "Share your screen" };
                if profile_ui::dialog_header_ui::dialog_header(ui, title) {
                    self.share_picker = None;
                }
                egui::ScrollArea::vertical().max_height((ctx.screen_rect().height()-160.0).max(220.0)).show(ui, |ui| {
                    if !camera {
                        ui.horizontal(|ui| { ui.selectable_value(&mut self.share_kind, SourceKind::Monitor, "Entire screen"); ui.selectable_value(&mut self.share_kind, SourceKind::Window, "Application window"); });
                        ui.add_space(8.0);
                    }
                    let sources: Vec<_> = if camera { self.inventory.cameras.clone() } else { self.inventory.screens.iter().filter(|s|s.kind==self.share_kind).cloned().collect() };
                    let selected = if camera { &mut self.camera_source } else { &mut self.screen_source };
                    if selected.as_ref().is_none_or(|id| !sources.iter().any(|s|s.id==*id)) { *selected = sources.first().map(|s|s.id.clone()); }
                    source_combo(ui, if camera {"Camera"} else {"What would you like to share?"}, "sharing-source", selected, &sources);
                    if self.inventory_rx.is_some() { ui.horizontal(|ui| { ui.spinner(); ui.label("Finding available sources…"); }); }
                    else if sources.is_empty() { ui.label(RichText::new(if camera {"No camera found. Connect one and refresh."} else {"No available source. Open the window you want to share and refresh."}).color(c_muted())); }
                    if ui.small_button("Refresh sources").clicked() { self.refresh_devices(); }
                    ui.add_space(12.0);
                    let options = if camera { &mut self.prefs.camera_options } else { &mut self.prefs.screen_options };
                    let mut changed=capture_options_ui(ui,"sharing-profile",options,native_video);
                    if !camera {
                        let can_audio=native_video&&self.share_kind==SourceKind::Window&&!sources.is_empty();
                        let mut include_audio=options.application_audio&&can_audio;
                        if ui.add_enabled(can_audio,egui::Checkbox::new(&mut include_audio,"Include selected app audio")).changed(){options.application_audio=include_audio;changed=true;}
                        ui.label(RichText::new(if can_audio {"Includes sound from this application and its child processes. Audio starts only when you click Start sharing."}else{"Application audio requires native video and a selected application window."}).size(font_size(TEXT_BODY)).color(c_muted()));
                    }
                    if changed {self.save_preferences();}
                    ui.add_space(8.0);
                    ui.label(RichText::new(if camera {"The camera opens when you click Start camera."} else {"Only the selected display or window will be shared."}).size(font_size(12.0)).color(c_muted()));
                    ui.add_space(14.0);
                    let id = if camera {self.camera_source.as_ref()} else {self.screen_source.as_ref()};
                    let source = id.and_then(|id|sources.iter().find(|s|s.id==*id)).cloned();
                    #[cfg(feature="native-rtc")]
                    {
                        if self.source_preview.as_ref().is_some_and(|preview|preview.camera!=camera||source.as_ref().is_none_or(|source|source.id!=preview.source_id)){self.stop_source_preview();}
                        ui.horizontal(|ui| {
                            if self.source_preview.is_some() {if ui.add(secondary("Stop local preview")).clicked(){self.stop_source_preview();}}
                            else if ui.add_enabled(source.is_some()&&self.inventory_rx.is_none()&&self.preview_release.is_none()&&!self.source_active(camera)&&!self.source_starting(camera),secondary("Preview source")).clicked() {
                                match source_preview::SourcePreview::new(source.clone().unwrap(),camera){Ok(preview)=>self.source_preview=Some(preview),Err(error)=>self.error=Some(error)}
                            }
                        });
                        if let Some(preview)=&self.source_preview{preview.show(ui);}
                        else if self.preview_release.is_some(){ui.horizontal(|ui|{ui.spinner();ui.label("Closing local preview…");});}
                        ui.add_space(8.0);
                    }
                    if self.rtc_starting(){ui.horizontal(|ui|{ui.spinner();ui.label(if self.rtc_reconnecting(){"Reconnecting video…"}else{"Connecting native video…"});});}
                    let ready = source.is_some() && self.voice_ready() && !self.rtc_starting() && self.inventory_rx.is_none();
                    ui.horizontal(|ui| {
                        if ui.add_enabled(ready,primary(if camera {"Start camera"} else {"Start sharing"})).clicked() { self.start_capture(camera,source.unwrap()); self.share_picker=None; }
                        if ui.add(secondary("Cancel")).clicked() { self.share_picker=None; }
                    });
                    for warning in &self.inventory.warnings { ui.label(RichText::new(warning).size(font_size(TEXT_BODY)).color(c_bad())); }
                });
            });
        if !open {
            self.share_picker = None;
        }
        if self.share_picker.is_none() {
            self.stop_source_preview();
        }
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
            if self.uploads.is_empty() {
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
        }
        if self.media_preview.is_some() {
            self.media_preview_dialog(ctx);
        }
        self.online_gif_preview_dialog(ctx);
        if self.stream_preview.is_some() {
            self.stream_preview_dialog(ctx);
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
