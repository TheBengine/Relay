//! Explicit local previews use an isolated, single-frame mailbox and publish nothing.
use super::font_size;
use crate::capture::{
    CaptureOptions, CaptureQuality, CaptureSession, CaptureSource, RawCaptureFrame, SourceKind,
};
use eframe::egui::{self, ColorImage, TextureHandle, TextureOptions};
use std::{
    sync::mpsc::{self, Receiver},
    time::Duration,
};

pub(crate) struct SourcePreview {
    pub source_id: String,
    pub camera: bool,
    capture: Option<CaptureSession>,
    frames: Receiver<RawCaptureFrame>,
    texture: Option<TextureHandle>,
}

fn preview_options() -> CaptureOptions {
    CaptureOptions {
        quality: CaptureQuality::P480,
        fps: 15,
        application_audio: false,
        ..Default::default()
    }
}

impl SourcePreview {
    pub fn new(source: CaptureSource, camera: bool) -> Result<Self, String> {
        if (source.kind == SourceKind::Camera) != camera {
            return Err("Select the matching camera or screen source.".into());
        }
        let (tx, frames) = mpsc::sync_channel(1);
        let source_id = source.id.clone();
        let capture = if camera {
            CaptureSession::start_camera_raw(tx, 0, source, preview_options())?
        } else {
            CaptureSession::start_screen_raw(tx, 0, source, preview_options())?
        };
        Ok(Self {
            source_id,
            camera,
            capture: Some(capture),
            frames,
            texture: None,
        })
    }
    pub fn update(&mut self, ctx: &egui::Context) -> Option<String> {
        if let Some(error) = self.capture.as_ref().and_then(CaptureSession::take_error) {
            return Some(error);
        }
        if let Ok(frame) = self.frames.try_recv() {
            let width = frame.width as usize;
            let height = frame.height as usize;
            if width == 0
                || height == 0
                || width > 854
                || height > 480
                || frame.rgba.len() != width.saturating_mul(height).saturating_mul(4)
            {
                return Some("The source returned an invalid preview frame.".into());
            }
            let image = ColorImage::from_rgba_unmultiplied([width, height], &frame.rgba);
            if let Some(texture) = &mut self.texture {
                texture.set(image, TextureOptions::LINEAR);
            } else {
                self.texture =
                    Some(ctx.load_texture("local-source-preview", image, TextureOptions::LINEAR));
            }
        }
        if self.capture.is_some() {
            ctx.request_repaint_after(Duration::from_millis(67));
        }
        None
    }
    /// A 16:9 stage across the available width, as in Discord's video
    /// settings; the picture is letterboxed, never cropped or stretched.
    pub fn show(&self, ui: &mut egui::Ui) {
        let width = ui.available_width().max(160.0);
        let height = (width * 9.0 / 16.0).min(420.0);
        let (rect, _) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 8.0, egui::Color32::from_gray(12));
        if let Some(texture) = &self.texture {
            let size = texture.size_vec2();
            let scale = (rect.width() / size.x).min(rect.height() / size.y);
            let picture = egui::Rect::from_center_size(rect.center(), size * scale);
            egui::Image::new((texture.id(), size * scale))
                .rounding(8.0)
                .paint_at(ui, picture);
        } else {
            let spinner = egui::Rect::from_center_size(rect.center() - egui::vec2(0.0, 14.0), egui::vec2(24.0, 24.0));
            ui.put(spinner, egui::Spinner::new());
            painter.text(
                rect.center() + egui::vec2(0.0, 18.0),
                egui::Align2::CENTER_CENTER,
                if self.camera { "Starting your camera…" } else { "Opening the selected source…" },
                egui::FontId::proportional(font_size(13.0)),
                egui::Color32::from_gray(210),
            );
        }
        // Who can see this, on a dark pill so it reads over any picture.
        let galley = painter.layout_no_wrap(
            "Preview · only you can see this".into(),
            egui::FontId::proportional(font_size(12.0)),
            egui::Color32::WHITE,
        );
        let pill = egui::Rect::from_min_size(
            rect.left_bottom() + egui::vec2(10.0, -10.0 - galley.size().y - 8.0),
            galley.size() + egui::vec2(16.0, 8.0),
        );
        painter.rect_filled(pill, 6.0, egui::Color32::from_black_alpha(170));
        painter.galley(pill.min + egui::vec2(8.0, 4.0), galley, egui::Color32::WHITE);
    }
    /// Cancel first, then release native handles off the UI thread before reuse.
    pub fn stop(mut self, ctx: egui::Context) -> Receiver<()> {
        let (tx, done) = mpsc::channel();
        let capture = self.capture.take();
        if let Some(capture) = &capture {
            capture.cancel();
        }
        std::thread::spawn(move || {
            drop(capture);
            let _ = tx.send(());
            ctx.request_repaint();
        });
        done
    }
}
impl Drop for SourcePreview {
    fn drop(&mut self) {
        if let Some(capture) = self.capture.take() {
            capture.cancel();
            std::thread::spawn(move || drop(capture));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn generated_local_preview_is_bounded_has_no_audio_and_releases_before_reuse() {
        let options = preview_options();
        assert_eq!(options.fps, 15);
        assert!(!options.application_audio);
        let (tx, frames) = mpsc::sync_channel(1);
        let capture = CaptureSession::synthetic_raw(tx, 0, options).unwrap();
        let mut preview = SourcePreview {
            source_id: "owned-generated".into(),
            camera: false,
            capture: Some(capture),
            frames,
            texture: None,
        };
        let ctx = egui::Context::default();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while preview.texture.is_none() && std::time::Instant::now() < deadline {
            assert!(preview.update(&ctx).is_none());
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(preview.texture.as_ref().unwrap().size(), [854, 480]);
        preview
            .stop(ctx)
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
    }
}
