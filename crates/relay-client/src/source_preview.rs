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
    pub fn show(&self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("Local preview · Visible only to you").size(font_size(12.0)));
        if let Some(texture) = &self.texture {
            let size = texture.size_vec2();
            let scale = (ui.available_width() / size.x).min(200.0 / size.y).min(1.0);
            ui.add(egui::Image::new((texture.id(), size * scale)).rounding(8.0));
        } else {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Opening the selected source…");
            });
        }
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
