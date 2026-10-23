use eframe::egui::{ColorImage, Context};
use image::{AnimationDecoder, ImageDecoder};
use serde::{Deserialize, Serialize};
use std::{
    path::PathBuf,
    sync::{mpsc, Arc},
};

#[derive(Clone, Serialize, Deserialize)]
pub struct GifEntry {
    pub path: PathBuf,
    pub favorite: bool,
    used: u64,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct GifLibrary {
    entries: Vec<GifEntry>,
    clock: u64,
}

impl GifLibrary {
    pub fn remember(&mut self, path: PathBuf) {
        self.clock = self.clock.saturating_add(1);
        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.path == path) {
            entry.used = self.clock;
        } else {
            self.entries.push(GifEntry {
                path,
                favorite: false,
                used: self.clock,
            });
        }
        self.entries.sort_by_key(|entry| {
            (
                std::cmp::Reverse(entry.favorite),
                std::cmp::Reverse(entry.used),
            )
        });
        self.entries.truncate(64);
    }
    pub fn entries(&self, favorites: bool, query: &str) -> Vec<GifEntry> {
        let query = query.to_lowercase();
        let mut entries: Vec<_> = self
            .entries
            .iter()
            .filter(|entry| {
                (!favorites || entry.favorite)
                    && entry
                        .path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_lowercase()
                        .contains(&query)
            })
            .cloned()
            .collect();
        entries.sort_by_key(|entry| std::cmp::Reverse(entry.used));
        if !favorites {
            entries.truncate(24);
        }
        entries
    }
    pub fn toggle_favorite(&mut self, path: &PathBuf) {
        if let Some(entry) = self.entries.iter_mut().find(|entry| &entry.path == path) {
            entry.favorite = !entry.favorite;
        }
    }
    pub fn remove(&mut self, path: &PathBuf) {
        self.entries.retain(|entry| &entry.path != path);
    }
}

pub type PreviewResult = (PathBuf, Result<Arc<ColorImage>, String>);
pub type AnimationPreviewResult = (PathBuf, Result<crate::chat_media::DecodedMedia, String>);
pub struct GifPreviews {
    tx: mpsc::SyncSender<PathBuf>,
    pub rx: mpsc::Receiver<PreviewResult>,
    animation_tx: mpsc::SyncSender<PathBuf>,
    pub animation_rx: mpsc::Receiver<AnimationPreviewResult>,
}
impl GifPreviews {
    pub fn new(ctx: Context) -> Self {
        let (tx, commands) = mpsc::sync_channel::<PathBuf>(4);
        let (results, rx) = mpsc::sync_channel(4);
        let still_ctx = ctx.clone();
        std::thread::spawn(move || {
            while let Ok(path) = commands.recv() {
                let image = thumbnail(&path);
                if results.send((path, image)).is_err() {
                    break;
                }
                still_ctx.request_repaint();
            }
        });
        let (animation_tx, commands) = mpsc::sync_channel::<PathBuf>(1);
        let (results, animation_rx) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            while let Ok(path) = commands.recv() {
                let result = animation_preview(&path);
                if results.send((path, result)).is_err() {
                    break;
                }
                ctx.request_repaint();
            }
        });
        Self {
            tx,
            rx,
            animation_tx,
            animation_rx,
        }
    }
    pub fn request(&self, path: PathBuf) -> bool {
        self.tx.try_send(path).is_ok()
    }
    pub fn request_animation(&self, path: PathBuf) -> bool {
        self.animation_tx.try_send(path).is_ok()
    }
}

fn animation_preview(path: &PathBuf) -> Result<crate::chat_media::DecodedMedia, String> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .map_err(|_| "This GIF is no longer available.")?
        .take(crate::chat_media::MAX_COMPRESSED_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "Could not read this GIF.")?;
    crate::chat_media::picker_preview(&bytes)
}

fn thumbnail(path: &PathBuf) -> Result<Arc<ColorImage>, String> {
    let metadata = std::fs::metadata(path)
        .map_err(|_| "This file moved or is no longer available.".to_string())?;
    if metadata.len() > crate::chat_media::MAX_COMPRESSED_BYTES as u64 {
        return Err("GIF exceeds the 10 MB limit.".into());
    }
    let bytes = std::fs::read(path).map_err(|error| format!("Read GIF: {error}"))?;
    if bytes.len() > crate::chat_media::MAX_COMPRESSED_BYTES {
        return Err("GIF exceeds the 10 MB limit.".into());
    }
    let mut decoder = image::codecs::gif::GifDecoder::new(std::io::Cursor::new(bytes))
        .map_err(|_| "Choose a valid GIF file.".to_string())?;
    let (width, height) = decoder.dimensions();
    if width == 0
        || height == 0
        || width > 2048
        || height > 2048
        || u64::from(width) * u64::from(height) > 2 * 1024 * 1024
    {
        return Err("GIF is too large for a preview.".into());
    }
    let mut limits = image::io::Limits::default();
    limits.max_image_width = Some(2048);
    limits.max_image_height = Some(2048);
    limits.max_alloc = Some(32 * 1024 * 1024);
    decoder
        .set_limits(limits)
        .map_err(|_| "GIF exceeds preview limits.".to_string())?;
    let frame = decoder
        .into_frames()
        .next()
        .ok_or_else(|| "GIF has no frames.".to_string())?
        .map_err(|_| "GIF preview is damaged.".to_string())?;
    let image = image::imageops::thumbnail(&frame.into_buffer(), 144, 100);
    Ok(Arc::new(ColorImage::from_rgba_unmultiplied(
        [image.width() as usize, image.height() as usize],
        image.as_raw(),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn favorites_recent_order_and_missing_file_removal_survive_restart() {
        let a = PathBuf::from("a.gif");
        let b = PathBuf::from("b.gif");
        let mut library = GifLibrary::default();
        library.remember(a.clone());
        library.toggle_favorite(&a);
        library.remember(b.clone());
        library.remember(a.clone());
        let restored: GifLibrary =
            serde_json::from_slice(&serde_json::to_vec(&library).unwrap()).unwrap();
        assert_eq!(restored.entries(false, "")[0].path, a);
        assert_eq!(restored.entries(true, "").len(), 1);
        library.remove(&a);
        assert!(library.entries(true, "").is_empty());
        assert_eq!(library.entries(false, "B")[0].path, b);
    }
    #[test]
    fn library_bounds_keep_favorites_and_cap_recent_list() {
        let mut library = GifLibrary::default();
        let favorite = PathBuf::from("favorite.gif");
        library.remember(favorite.clone());
        library.toggle_favorite(&favorite);
        for index in 0..100 {
            library.remember(PathBuf::from(format!("{index}.gif")));
        }
        assert_eq!(library.entries.len(), 64);
        assert_eq!(library.entries(false, "").len(), 24);
        assert_eq!(library.entries(true, "")[0].path, favorite);
    }
}
