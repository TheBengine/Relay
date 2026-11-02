//! Tiny server thumbnails persisted with the existing protected preferences.
//! No URLs or file paths from an image are opened while restoring the cache.
use base64::{engine::general_purpose::STANDARD, Engine};
use eframe::egui::ColorImage;
use serde::{Deserialize, Serialize};

const SIDE: usize = 96;
const MAX_ENCODED: usize = SIDE * SIDE * 4 * 4 / 3;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct SavedIcon {
    server_id: String,
    pub(crate) revision: String,
    size: [usize; 2],
    rgba: String,
}

impl SavedIcon {
    pub(crate) fn new(server_id: &str, revision: &str, image: &ColorImage) -> Option<Self> {
        let [width, height] = image.size;
        if width == 0
            || height == 0
            || width > 256
            || height > 256
            || image.pixels.len() != width * height
        {
            return None;
        }
        let bytes = image
            .pixels
            .iter()
            .flat_map(|p| p.to_srgba_unmultiplied())
            .collect();
        let source = image::RgbaImage::from_raw(width as u32, height as u32, bytes)?;
        let thumbnail = image::imageops::thumbnail(&source, SIDE as u32, SIDE as u32);
        Some(Self {
            server_id: server_id.into(),
            revision: revision.into(),
            size: [thumbnail.width() as usize, thumbnail.height() as usize],
            rgba: STANDARD.encode(thumbnail.as_raw()),
        })
    }

    pub(crate) fn image(&self, server_id: &str) -> Option<ColorImage> {
        if self.server_id != server_id
            || self.rgba.len() > MAX_ENCODED
            || self.size.iter().any(|n| *n == 0 || *n > SIDE)
        {
            return None;
        }
        let bytes = STANDARD.decode(&self.rgba).ok()?;
        (bytes.len() == self.size[0] * self.size[1] * 4)
            .then(|| ColorImage::from_rgba_unmultiplied(self.size, &bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn thumbnail_survives_restart_and_is_bound_to_the_server() {
        let source = ColorImage::new([256, 256], eframe::egui::Color32::from_rgb(12, 34, 56));
        let icon = SavedIcon::new("one", "revision-7", &source).unwrap();
        let restored: SavedIcon =
            serde_json::from_slice(&serde_json::to_vec(&icon).unwrap()).unwrap();
        let image = restored.image("one").unwrap();
        assert_eq!(image.size, [96, 96]);
        assert_eq!(image.pixels[0], source.pixels[0]);
        assert!(restored.image("another-server").is_none());
    }
    #[test]
    fn malformed_or_oversized_cache_does_not_allocate_an_image() {
        let source = ColorImage::new([1, 1], eframe::egui::Color32::WHITE);
        let mut icon = SavedIcon::new("one", "v1", &source).unwrap();
        icon.size = [usize::MAX, 2];
        assert!(icon.image("one").is_none());
        icon.size = [1, 1];
        icon.rgba = "A".repeat(MAX_ENCODED + 1);
        assert!(icon.image("one").is_none());
        icon.rgba = "bad".into();
        assert!(icon.image("one").is_none());
    }
}
