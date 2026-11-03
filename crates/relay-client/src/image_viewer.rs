//! Attachment lightbox. Originals still use the authenticated, bounded preview worker.
use super::*;

#[derive(Default)]
pub(super) struct ImageCopy {
    pending: Option<Receiver<Result<(), String>>>,
    status: Option<String>,
}

pub(super) fn is_image(attachment: &Attachment) -> bool {
    matches!(attachment.mime.as_str(), "image/png" | "image/jpeg" | "image/gif" | "image/webp")
        || [".png", ".jpg", ".jpeg", ".gif", ".webp"]
            .iter().any(|ext| attachment.name.to_ascii_lowercase().ends_with(ext))
}

fn fitted_size(dimensions: Vec2, available: Vec2) -> Vec2 {
    let scale = (available.x.max(1.0) / dimensions.x.max(1.0))
        .min(available.y.max(1.0) / dimensions.y.max(1.0)).min(2.0);
    dimensions * scale
}

fn bounded_pan(pan: Vec2, image: Vec2, viewport: Vec2) -> Vec2 {
    let limit = ((image - viewport) * 0.5).max(Vec2::ZERO);
    pan.clamp(-limit, limit)
}

impl RelayApp {
    pub(super) fn open_image_preview(&mut self, id: &str) {
        if self.image_copy.pending.is_none() { self.image_copy.status = None; }
        self.media_preview = Some(id.to_owned());
        self.media_preview_zoom = false;
        self.media_preview_pan = Vec2::ZERO;
    }

    fn save_image(&mut self, attachment: &Attachment) {
        if let Some(path) = rfd::FileDialog::new().set_file_name(&attachment.name).save_file() {
            self.start_download(attachment.clone(), path);
        }
    }

    fn copy_preview_image(&mut self, ctx: &egui::Context, id: &str) {
        if self.image_copy.pending.is_some() { return; }
        let Some(image) = self.chat_media.get(id) else { return; };
        let frame = image.displayed_frame();
        let (tx, rx) = mpsc::sync_channel(1);
        let wake = ctx.clone();
        let spawned = std::thread::Builder::new().name("relay-copy-image".into()).spawn(move || {
            // Only one bounded decoded frame is retained. Conversion and the OS's
            // image encoding must never block drawing or the network event loop.
            let pixels: Vec<u8> = frame.pixels.iter()
                .flat_map(Color32::to_srgba_unmultiplied).collect();
            let result = arboard::Clipboard::new().and_then(|mut clipboard| {
                clipboard.set_image(arboard::ImageData {
                    width: frame.size[0], height: frame.size[1],
                    bytes: std::borrow::Cow::Owned(pixels),
                })
            }).map_err(|_| "Could not copy the image. Try Save Image instead.".to_owned());
            let _ = tx.send(result);
            wake.request_repaint();
        });
        if spawned.is_ok() {
            self.image_copy.pending = Some(rx);
            self.image_copy.status = Some("Copying image…".into());
        } else {
            self.image_copy.status = Some("Could not start copying. Try Save Image instead.".into());
        }
    }

    pub(super) fn poll_image_copy(&mut self) {
        let Some(rx) = &self.image_copy.pending else { return; };
        let text = match rx.try_recv() {
            Ok(Ok(())) => "Image copied.".into(),
            Ok(Err(error)) => error,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => "Image copying stopped. Try Save Image instead.".into(),
        };
        self.image_copy.pending = None;
        self.image_copy.status = Some(text.clone());
        self.notice = Some((text, Instant::now()));
    }

    pub(super) fn image_actions(&mut self, ui: &mut egui::Ui, attachment: &Attachment) {
        if ui.add_enabled(self.image_copy.pending.is_none(), egui::Button::new("Copy Image"))
            .on_hover_text("Copy the displayed image; Save Image keeps the original file.").clicked() {
            self.copy_preview_image(ui.ctx(), &attachment.id);
            ui.close_menu();
        }
        if ui.button("Save Image").clicked() {
            self.save_image(attachment);
            ui.close_menu();
        }
    }

    pub(super) fn media_preview_dialog(&mut self, ctx: &egui::Context) {
        let Some(id) = self.media_preview.clone() else { return; };
        self.request_original_preview(ctx, &id);
        let message = self.messages.iter().chain(&self.search_messages)
            .find(|m| m.attachments.iter().any(|a| a.id == id));
        let attachment = message.and_then(|m| m.attachments.iter().find(|a| a.id == id)).cloned();
        let author = message.map(|m| profile_ui::display_name(&m.author)).unwrap_or_default();
        let Some(image) = self.chat_media.get_mut(&id) else {
            self.media_preview = None; return;
        };
        let dimensions = image.dimensions();
        let texture = image.texture_with_autoplay(ctx, true, self.prefs.reduced_motion,
            self.prefs.appearance.gif_autoplay);
        let animated = image.is_animated();
        let paused = image.is_paused_for(self.prefs.appearance.gif_autoplay);
        let screen = ctx.screen_rect();
        let bounds = Rect::from_min_max(Pos2::new(screen.left(), screen.top() + 32.0), screen.max);
        let mut close = false;
        let mut copy = false;
        let mut save = false;
        let mut toggle_play = false;
        egui::Area::new(egui::Id::new("attachment-lightbox"))
            .order(egui::Order::Foreground).fixed_pos(bounds.min).movable(false)
            .show(ctx, |ui| {
                ui.set_min_size(bounds.size());
                ui.set_max_size(bounds.size());
                ui.painter().rect_filled(bounds, 0.0, Color32::from_black_alpha(235));
                let background = ui.interact(bounds, ui.id().with("backdrop"), Sense::click());
                let toolbar = Rect::from_min_max(bounds.min + Vec2::splat(16.0),
                    Pos2::new(bounds.right() - 16.0, bounds.top() + 60.0));
                ui.allocate_ui_at_rect(toolbar, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(author).strong());
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            close = ui.add(secondary("Close ×")).on_hover_text("Close image · Esc").clicked();
                            if let Some(a) = &attachment {
                                save = ui.add(secondary("Save")).on_hover_text(&a.name).clicked();
                                copy = ui.add_enabled(self.image_copy.pending.is_none(), secondary("Copy"))
                                    .on_hover_text("Copy the displayed image; Save keeps the original file.").clicked();
                            }
                            if ui.add(secondary(if self.media_preview_zoom { "Fit" } else { "Zoom +" })).clicked() {
                                self.media_preview_zoom = !self.media_preview_zoom;
                                self.media_preview_pan = Vec2::ZERO;
                            }
                            if animated && !self.prefs.reduced_motion {
                                toggle_play = ui.add(secondary(if paused { "Play" } else { "Pause" })).clicked();
                            }
                        });
                    });
                });
                let view = Rect::from_min_max(bounds.min + Vec2::new(24.0, 76.0), bounds.max - Vec2::new(24.0, 42.0));
                let size = fitted_size(dimensions, view.size()) * if self.media_preview_zoom { 2.0 } else { 1.0 };
                self.media_preview_pan = bounded_pan(self.media_preview_pan, size, view.size());
                let rect = Rect::from_center_size(view.center() + self.media_preview_pan, size);
                ui.scope(|ui| {
                    ui.set_clip_rect(view);
                    let response = ui.interact(rect.intersect(view), ui.id().with("image"), Sense::click_and_drag());
                    ui.painter().image(texture, rect, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
                    if self.media_preview_zoom {
                        if response.dragged() { self.media_preview_pan += ctx.input(|i| i.pointer.delta()); }
                        response.clone().on_hover_cursor(egui::CursorIcon::Grab);
                    } else { response.clone().on_hover_cursor(egui::CursorIcon::ZoomIn); }
                    if response.clicked() {
                        self.media_preview_zoom = !self.media_preview_zoom;
                        self.media_preview_pan = Vec2::ZERO;
                    }
                    response.context_menu(|ui| {
                        if attachment.is_some() {
                            copy = ui.add_enabled(self.image_copy.pending.is_none(), egui::Button::new("Copy Image")).clicked();
                            save = ui.button("Save Image").clicked();
                            if copy || save { ui.close_menu(); }
                        }
                    });
                });
                let footer = Rect::from_min_max(Pos2::new(bounds.left()+24.0, bounds.bottom()-34.0), bounds.max-Vec2::splat(8.0));
                ui.allocate_ui_at_rect(footer, |ui| {
                    let label = if let Some(status) = self.image_copy.status.as_deref() { status }
                        else if self.chat_media_pending.contains(&id) { "Loading full image…" }
                        else if self.chat_media_errors.contains_key(&id) { "Preview quality · Save to view the original" }
                        else { "Click to zoom · Drag when zoomed · Esc to close" };
                    ui.label(RichText::new(label).small().color(c_muted()));
                });
                if background.clicked() && !ctx.is_context_menu_open() { close = true; }
            });
        if toggle_play {
            if let Some(image) = self.chat_media.get_mut(&id) { image.set_paused(!paused); }
        }
        if copy { self.copy_preview_image(ctx, &id); }
        if save { if let Some(attachment) = attachment { self.save_image(&attachment); } }
        // Consume Escape so the underlying chat does not also react.
        if close || ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
            self.media_preview = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn portrait_landscape_and_small_windows_fit_without_cropping() {
        for dimensions in [Vec2::new(4000.0, 1000.0), Vec2::new(1000.0, 4000.0), Vec2::splat(350.0)] {
            for available in [Vec2::new(1300.0, 700.0), Vec2::new(280.0, 150.0)] {
                let size = fitted_size(dimensions, available);
                assert!(size.x <= available.x + 0.01 && size.y <= available.y + 0.01);
                assert!((size.x / size.y - dimensions.x / dimensions.y).abs() < 0.001);
            }
        }
    }
    #[test]
    fn zoom_pan_cannot_lose_image_outside_viewport() {
        assert_eq!(bounded_pan(Vec2::new(900.0,-900.0),Vec2::new(800.0,300.0),Vec2::new(400.0,400.0)),Vec2::new(200.0,0.0));
    }
}
