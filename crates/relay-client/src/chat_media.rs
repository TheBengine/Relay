//! Bounded attachment previews. Decode on a background thread, never in the UI.
use eframe::egui::{self, ColorImage, ImageData, TextureHandle, TextureOptions};
use image::{AnimationDecoder, ImageDecoder, ImageFormat};
use std::{
    io::Cursor,
    sync::Arc,
    time::{Duration, Instant},
};

pub const MAX_COMPRESSED_BYTES: usize = 10 * 1024 * 1024;
const MAX_DIMENSION: u32 = 2048;
const MAX_PIXELS: u64 = 2 * 1024 * 1024;
const MAX_FRAMES: usize = 120;
const MAX_DECODED_BYTES: usize = 32 * 1024 * 1024;

pub struct DecodedFrame {
    pub image: Arc<ColorImage>,
    pub delay: Duration,
}

pub struct DecodedMedia {
    pub source_size: [usize; 2],
    pub frames: Vec<DecodedFrame>,
    pub decoded_bytes: usize,
}

#[derive(Clone, Copy)]
struct PreviewLimits {
    dimension: u32,
    pixels: u64,
    frames: usize,
    decoded_bytes: usize,
}

const LIMITS: PreviewLimits = PreviewLimits {
    dimension: MAX_DIMENSION,
    pixels: MAX_PIXELS,
    frames: MAX_FRAMES,
    decoded_bytes: MAX_DECODED_BYTES,
};

fn check_dimensions(width: u32, height: u32, limits: PreviewLimits) -> Result<usize, String> {
    let pixels = u64::from(width) * u64::from(height);
    if width == 0
        || height == 0
        || width > limits.dimension
        || height > limits.dimension
        || pixels > limits.pixels
    {
        return Err(
            "This image is too large for an inline preview. Download the original file to view it."
                .into(),
        );
    }
    usize::try_from(pixels * 4).map_err(|_| "The image dimensions are invalid.".into())
}

fn decoder_limits(limits: PreviewLimits) -> image::io::Limits {
    let mut result = image::io::Limits::default();
    result.max_image_width = Some(limits.dimension);
    result.max_image_height = Some(limits.dimension);
    result.max_alloc = Some(MAX_DECODED_BYTES as u64);
    result
}

pub fn decode(bytes: &[u8]) -> Result<DecodedMedia, String> {
    decode_with_limits(bytes, LIMITS)
}

pub fn picker_preview(bytes: &[u8]) -> Result<DecodedMedia, String> {
    decode_variant(bytes, LIMITS, Some((144, 100, 8 * 1024 * 1024)))
}
/// Two-times-density inline preview; large previews request the original separately.
pub fn inline_preview(bytes: &[u8]) -> Result<DecodedMedia, String> {
    decode_variant(bytes, LIMITS, Some((800, 520, MAX_DECODED_BYTES)))
}
fn preview_rgba(rgba: image::RgbaImage, preview: Option<(u32, u32, usize)>) -> image::RgbaImage {
    if let Some((width, height, _)) = preview {
        if rgba.width() > width || rgba.height() > height {
            let (w, h) = (u64::from(rgba.width()), u64::from(rgba.height()));
            let (width, height) = if u64::from(width) * h <= u64::from(height) * w {
                (width, (h * u64::from(width) / w).max(1) as u32)
            } else {
                ((w * u64::from(height) / h).max(1) as u32, height)
            };
            return image::imageops::thumbnail(&rgba, width, height);
        }
    }
    rgba
}

fn decode_with_limits(bytes: &[u8], limits: PreviewLimits) -> Result<DecodedMedia, String> {
    decode_variant(bytes, limits, None)
}
fn decode_variant(
    bytes: &[u8],
    limits: PreviewLimits,
    preview: Option<(u32, u32, usize)>,
) -> Result<DecodedMedia, String> {
    let byte_limit = preview.map_or(limits.decoded_bytes, |(_, _, bytes)| bytes);
    if bytes.len() > MAX_COMPRESSED_BYTES {
        return Err("The image exceeds the 10 MB attachment limit.".into());
    }
    let format = image::guess_format(bytes)
        .map_err(|_| "This attachment is not a supported image.".to_string())?;
    let mut result = DecodedMedia {
        source_size: [0, 0],
        frames: Vec::new(),
        decoded_bytes: 0,
    };
    if format == ImageFormat::Gif {
        let mut decoder = image::codecs::gif::GifDecoder::new(Cursor::new(bytes))
            .map_err(|_| "The GIF could not be decoded.".to_string())?;
        let (width, height) = decoder.dimensions();
        check_dimensions(width, height, limits)?;
        result.source_size = [width as usize, height as usize];
        decoder
            .set_limits(decoder_limits(limits))
            .map_err(|_| "The GIF exceeds preview limits.".to_string())?;
        // The image decoder composites frame offsets/transparency and applies Keep,
        // Background, and Previous disposal before yielding full-canvas RGBA frames.
        for frame in decoder.into_frames() {
            if result.frames.len() >= limits.frames {
                return Err("This GIF needs more preview memory or frames than allowed. Download the original file to view it.".into());
            }
            let frame = frame.map_err(|_| "The GIF contains a damaged frame.".to_string())?;
            let (numerator, denominator) = frame.delay().numer_denom_ms();
            let delay = if numerator == 0 || denominator == 0 {
                Duration::from_millis(100)
            } else {
                Duration::from_secs_f64(f64::from(numerator) / f64::from(denominator) / 1000.0)
            };
            let rgba = frame.into_buffer();
            if rgba.dimensions() != (width, height) {
                return Err("The GIF has inconsistent frame dimensions.".into());
            }
            // Composite one source frame, shrink it and release it before
            // decoding the next. Do not retain the full-size animation first.
            let rgba = preview_rgba(rgba, preview);
            let frame_bytes = rgba.as_raw().len();
            if result.decoded_bytes.saturating_add(frame_bytes) > byte_limit {
                return Err("This GIF exceeds preview memory limits.".into());
            }
            result.frames.push(DecodedFrame {
                image: Arc::new(ColorImage::from_rgba_unmultiplied(
                    [rgba.width() as usize, rgba.height() as usize],
                    rgba.as_raw(),
                )),
                delay,
            });
            result.decoded_bytes += frame_bytes;
        }
    } else if matches!(format, ImageFormat::Png | ImageFormat::Jpeg) {
        let dimensions = image::io::Reader::with_format(Cursor::new(bytes), format)
            .into_dimensions()
            .map_err(|_| "The image dimensions could not be read.".to_string())?;
        let frame_bytes = check_dimensions(dimensions.0, dimensions.1, limits)?;
        result.source_size = [dimensions.0 as usize, dimensions.1 as usize];
        if frame_bytes > limits.decoded_bytes {
            return Err("The image exceeds preview memory limits.".into());
        }
        let mut reader = image::io::Reader::with_format(Cursor::new(bytes), format);
        reader.limits(decoder_limits(limits));
        let rgba = reader
            .decode()
            .map_err(|_| "The image could not be decoded.".to_string())?
            .to_rgba8();
        let rgba = preview_rgba(rgba, preview);
        if rgba.as_raw().len() > byte_limit {
            return Err("The image exceeds preview memory limits.".into());
        }
        result.frames.push(DecodedFrame {
            image: Arc::new(ColorImage::from_rgba_unmultiplied(
                [rgba.width() as usize, rgba.height() as usize],
                rgba.as_raw(),
            )),
            delay: Duration::ZERO,
        });
        result.decoded_bytes = rgba.as_raw().len();
    } else {
        return Err("Inline previews support PNG, JPEG, and GIF files.".into());
    }
    if result.frames.is_empty() {
        return Err("The image has no displayable frames.".into());
    }
    Ok(result)
}

fn frame_ends(media: &DecodedMedia) -> Box<[u128]> {
    if media.frames.len() <= 1 {
        return Box::default();
    }
    let mut end = 0u128;
    media
        .frames
        .iter()
        .map(|frame| {
            end = end.saturating_add(frame.delay.as_nanos());
            end
        })
        .collect()
}

pub struct AnimatedImage {
    media: DecodedMedia,
    frame_ends: Box<[u128]>,
    texture: Option<TextureHandle>,
    texture_frame: usize,
    paused: bool,
    manual_playback: bool,
    position: Duration,
    last_draw: Option<Instant>,
    was_running: bool,
    last_visible: Option<Instant>,
}

impl AnimatedImage {
    pub fn new(media: DecodedMedia) -> Self {
        let frame_ends = frame_ends(&media);
        Self {
            media,
            frame_ends,
            texture: None,
            texture_frame: usize::MAX,
            paused: false,
            manual_playback: false,
            position: Duration::ZERO,
            last_draw: None,
            was_running: false,
            last_visible: None,
        }
    }

    pub fn dimensions(&self) -> egui::Vec2 {
        let size = self.media.frames[0].image.size;
        egui::vec2(size[0] as f32, size[1] as f32)
    }

    pub fn is_thumbnail(&self) -> bool {
        self.media.source_size != self.media.frames[0].image.size
    }
    pub fn replace_media(&mut self, media: DecodedMedia) {
        self.frame_ends = frame_ends(&media);
        self.media = media;
        self.texture_frame = usize::MAX;
        self.last_draw = None;
        self.was_running = false;
    }
    pub fn suspend(&mut self) {
        self.last_draw = None;
        self.was_running = false;
    }
    pub fn last_visible(&self) -> Option<Instant> {
        self.last_visible
    }
    pub fn decoded_bytes(&self) -> usize {
        self.media.decoded_bytes
    }
    pub fn is_animated(&self) -> bool {
        self.media.frames.len() > 1
    }
    pub fn is_paused(&self) -> bool {
        self.paused
    }
    /// Manual Play overrides autoplay-off for this image, but not reduced motion.
    pub fn is_paused_for(&self, autoplay: bool) -> bool {
        self.is_paused() || (!autoplay && !self.manual_playback)
    }
    pub fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
        self.manual_playback = !paused;
        self.last_draw = None;
        self.was_running = false;
    }

    fn advance(
        &mut self,
        elapsed: Duration,
        visible: bool,
        reduced_motion: bool,
    ) -> (usize, Duration) {
        if !self.is_animated() || reduced_motion {
            return (0, Duration::ZERO);
        }
        let total = self.frame_ends.last().copied().unwrap_or_default();
        if visible && !self.paused {
            self.position = self.position.saturating_add(elapsed);
        }
        let nanos = self.position.as_nanos() % total.max(1);
        let index = self.frame_ends.partition_point(|end| *end <= nanos);
        if let Some(end) = self.frame_ends.get(index) {
            return (
                index,
                Duration::from_nanos((end - nanos).min(u128::from(u64::MAX)) as u64),
            );
        }
        (0, self.media.frames[0].delay)
    }

    /// Call only for a loaded preview. Pass the UI's visibility result so hidden
    /// animations freeze and request no timer repaints. Reduced motion uses frame 0.
    pub fn last_used(&self) -> Option<Instant> {
        self.last_draw
    }

    pub fn texture(
        &mut self,
        ctx: &egui::Context,
        visible: bool,
        reduced_motion: bool,
    ) -> egui::TextureId {
        self.texture_internal(ctx, visible, reduced_motion, true, Instant::now())
    }

    pub fn texture_with_autoplay(
        &mut self,
        ctx: &egui::Context,
        visible: bool,
        reduced_motion: bool,
        autoplay: bool,
    ) -> egui::TextureId {
        if autoplay {
            self.texture(ctx, visible, reduced_motion)
        } else {
            self.texture_internal(ctx, visible, reduced_motion, false, Instant::now())
        }
    }

    fn texture_internal(
        &mut self,
        ctx: &egui::Context,
        visible: bool,
        reduced_motion: bool,
        autoplay: bool,
        now: Instant,
    ) -> egui::TextureId {
        let visible = visible && ctx.input(|input| !input.viewport().minimized.unwrap_or(false));
        if visible {
            self.last_visible = Some(now);
        }
        let playback_visible = visible && !self.is_paused_for(autoplay);
        let running = playback_visible && !reduced_motion;
        let elapsed = if running && self.was_running {
            self.last_draw
                .map(|last| now.saturating_duration_since(last))
                .unwrap_or_default()
        } else {
            Duration::ZERO
        };
        self.last_draw = Some(now);
        self.was_running = running;
        // Autoplay-off and manual Pause hold the current frame. Actual reduced
        // motion alone forces frame zero, even after an explicit manual Play.
        let (index, remaining) = self.advance(elapsed, playback_visible, reduced_motion);
        if self.texture_frame != index {
            let pixels = ImageData::Color(self.media.frames[index].image.clone());
            if let Some(texture) = &mut self.texture {
                texture.set(pixels, TextureOptions::LINEAR);
            } else {
                self.texture = Some(ctx.load_texture(
                    "chat-attachment-preview",
                    pixels,
                    TextureOptions::LINEAR,
                ));
            }
            self.texture_frame = index;
        }
        if self.is_animated() && running {
            ctx.request_repaint_after(remaining);
        }
        self.texture
            .as_ref()
            .expect("decoded previews contain a frame")
            .id()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_thumbnail_reduces_retained_pixels_and_original_stays_available() {
        let mut bytes = Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(1920, 1080)
            .write_to(&mut bytes, ImageFormat::Png)
            .unwrap();
        let preview = inline_preview(bytes.get_ref()).unwrap();
        assert_eq!(preview.source_size, [1920, 1080]);
        assert_eq!(preview.frames[0].image.size, [800, 450]);
        assert_eq!(preview.decoded_bytes, 800 * 450 * 4);
        assert!(AnimatedImage::new(preview).is_thumbnail());
        let original = decode(bytes.get_ref()).unwrap();
        assert_eq!(original.decoded_bytes, 1920 * 1080 * 4);
        assert!(!AnimatedImage::new(original).is_thumbnail());
    }

    #[test]
    fn minimized_animation_freezes_and_resume_does_not_catch_up_hidden_time() {
        let ctx = egui::Context::default();
        let mut image = AnimatedImage::new(decode(&disposal_gif()).unwrap());
        let start = Instant::now();
        image.texture_internal(&ctx, true, false, true, start);
        image.texture_internal(&ctx, true, false, true, start + Duration::from_millis(80));
        let before = image.position;
        let mut input = egui::RawInput::default();
        input
            .viewports
            .get_mut(&egui::ViewportId::ROOT)
            .unwrap()
            .minimized = Some(true);
        ctx.begin_frame(input);
        image.texture_internal(&ctx, true, false, true, start + Duration::from_secs(60));
        assert_eq!(image.position, before);
        assert!(!image.was_running);
        let _ = ctx.end_frame();
        let mut input = egui::RawInput::default();
        input
            .viewports
            .get_mut(&egui::ViewportId::ROOT)
            .unwrap()
            .minimized = Some(false);
        ctx.begin_frame(input);
        image.texture_internal(&ctx, true, false, true, start + Duration::from_secs(61));
        assert_eq!(image.position, before);
        let _ = ctx.end_frame();
    }

    // Actual GIF89a, four composited frames, explicit delays and disposal methods.
    // Small literal LZW streams encode palette indices; no device API is involved.
    pub(super) fn disposal_gif() -> Vec<u8> {
        let mut gif = b"GIF89a\x02\x00\x01\x00\x81\x00\x00".to_vec();
        gif.extend_from_slice(&[255, 0, 0, 0, 0, 255, 0, 255, 0, 0, 0, 0]);
        let frames = [
            (1u8, 5u16, 0u16, 2u16, vec![4, 10]),
            (3, 12, 0, 1, vec![76, 1]),
            (2, 7, 1, 1, vec![84, 1]),
            (1, 9, 0, 1, vec![76, 1]),
        ];
        for (disposal, delay, left, width, data) in frames {
            gif.extend_from_slice(&[0x21, 0xf9, 4, disposal << 2]);
            gif.extend_from_slice(&delay.to_le_bytes());
            gif.extend_from_slice(&[0, 0, 0x2c]);
            gif.extend_from_slice(&left.to_le_bytes());
            gif.extend_from_slice(&0u16.to_le_bytes());
            gif.extend_from_slice(&width.to_le_bytes());
            gif.extend_from_slice(&1u16.to_le_bytes());
            gif.extend_from_slice(&[0, 2, data.len() as u8]);
            gif.extend_from_slice(&data);
            gif.push(0);
        }
        gif.push(0x3b);
        gif
    }

    #[test]
    fn real_gif_composites_previous_background_and_offsets_with_actual_delays() {
        let media = decode(&disposal_gif()).unwrap();
        assert_eq!(media.frames.len(), 4);
        assert_eq!(media.decoded_bytes, 32);
        let red = egui::Color32::RED;
        let blue = egui::Color32::BLUE;
        let green = egui::Color32::GREEN;
        assert_eq!(media.frames[0].image.pixels, vec![red, red]);
        assert_eq!(media.frames[1].image.pixels, vec![blue, red]);
        assert_eq!(media.frames[2].image.pixels, vec![red, green]);
        assert_eq!(
            media.frames[3].image.pixels,
            vec![blue, egui::Color32::TRANSPARENT]
        );
        assert_eq!(
            media
                .frames
                .iter()
                .map(|f| f.delay.as_millis())
                .collect::<Vec<_>>(),
            vec![50, 120, 70, 90]
        );
    }

    #[test]
    fn picker_animation_keeps_composited_frames_delays_and_bounded_thumbnail_size() {
        let original = decode(&disposal_gif()).unwrap();
        let preview = picker_preview(&disposal_gif()).unwrap();
        assert_eq!(original.frames.len(), preview.frames.len());
        for (original, preview) in original.frames.iter().zip(&preview.frames) {
            assert_eq!(original.delay, preview.delay);
            assert_eq!(original.image.pixels, preview.image.pixels);
            assert!(preview.image.size[0] <= 144 && preview.image.size[1] <= 100);
        }
        assert!(preview.decoded_bytes <= 8 * 1024 * 1024);
        assert!(picker_preview(b"not a GIF").is_err());
    }

    #[test]
    fn resized_picker_preview_preserves_transparent_image_color() {
        let image = image::RgbaImage::from_pixel(200, 100, image::Rgba([240, 40, 20, 128]));
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(image)
            .write_to(
                &mut std::io::Cursor::new(&mut png),
                image::ImageOutputFormat::Png,
            )
            .unwrap();
        let preview = picker_preview(&png).unwrap();
        assert!(preview.frames[0].image.size[0] <= 144);
        let color = preview.frames[0].image.pixels[0].to_srgba_unmultiplied();
        assert!((i16::from(color[0]) - 240).abs() <= 2);
        assert!((i16::from(color[3]) - 128).abs() <= 2);
    }

    #[test]
    fn playback_respects_delays_pause_visibility_reduced_motion_and_looping() {
        let mut image = AnimatedImage::new(decode(&disposal_gif()).unwrap());
        assert_eq!(image.advance(Duration::from_millis(49), true, false).0, 0);
        assert_eq!(image.advance(Duration::from_millis(1), true, false).0, 1);
        image.set_paused(true);
        assert_eq!(image.advance(Duration::from_secs(5), true, false).0, 1);
        image.set_paused(false);
        assert_eq!(image.advance(Duration::from_secs(5), false, false).0, 1);
        assert_eq!(image.advance(Duration::from_secs(5), true, true).0, 0);
        assert_eq!(image.advance(Duration::from_millis(120), true, false).0, 2);
        assert_eq!(image.advance(Duration::from_millis(160), true, false).0, 0);
    }

    #[test]
    fn manual_play_overrides_autoplay_off_pause_holds_frame_and_reduced_motion_wins() {
        let ctx = egui::Context::default();
        let mut image = AnimatedImage::new(decode(&disposal_gif()).unwrap());
        let start = Instant::now();
        let at = |millis| start + Duration::from_millis(millis);
        assert!(image.is_paused_for(false));
        image.texture_internal(&ctx, true, false, false, at(0));
        image.texture_internal(&ctx, true, false, false, at(1000));
        assert_eq!(image.texture_frame, 0);
        assert_eq!(image.position, Duration::ZERO);

        image.set_paused(false);
        assert!(!image.is_paused_for(false));
        image.texture_internal(&ctx, true, false, false, at(1000));
        image.texture_internal(&ctx, true, false, false, at(1080));
        assert_eq!(image.texture_frame, 1);
        assert_eq!(image.position, Duration::from_millis(80));

        image.set_paused(true);
        assert!(image.is_paused_for(true));
        assert!(image.is_paused_for(false));
        image.texture_internal(&ctx, true, false, true, at(5000));
        assert_eq!(image.texture_frame, 1, "Manual Pause must retain its frame");
        image.texture_internal(&ctx, true, false, false, at(6000));
        assert_eq!(
            image.texture_frame, 1,
            "Autoplay-off must not rewind a paused image"
        );
        assert_eq!(image.position, Duration::from_millis(80));

        image.set_paused(false);
        image.texture_internal(&ctx, true, false, false, at(7000));
        image.texture_internal(&ctx, true, true, false, at(7100));
        assert_eq!(
            image.texture_frame, 0,
            "Reduced Motion overrides manual Play"
        );
        assert_eq!(image.position, Duration::from_millis(80));
        image.texture_internal(&ctx, false, false, false, at(9000));
        image.texture_internal(&ctx, true, false, false, at(10000));
        assert_eq!(
            image.position,
            Duration::from_millis(80),
            "Hidden time must not accumulate"
        );
        image.texture_internal(&ctx, true, false, false, at(10100));
        assert_eq!(image.texture_frame, 2);
        assert_eq!(image.position, Duration::from_millis(180));
    }

    #[test]
    fn rejects_frame_pixel_memory_encoded_and_malformed_limits() {
        let gif = disposal_gif();
        assert!(decode_with_limits(
            &gif,
            PreviewLimits {
                frames: 2,
                ..LIMITS
            }
        )
        .is_err());
        assert!(decode_with_limits(
            &gif,
            PreviewLimits {
                decoded_bytes: 8,
                ..LIMITS
            }
        )
        .is_err());
        assert!(decode_with_limits(
            &gif,
            PreviewLimits {
                pixels: 1,
                ..LIMITS
            }
        )
        .is_err());
        assert!(decode_with_limits(
            &gif,
            PreviewLimits {
                dimension: 1,
                ..LIMITS
            }
        )
        .is_err());
        assert!(decode(&vec![0; MAX_COMPRESSED_BYTES + 1]).is_err());
        assert!(decode(&gif[..gif.len() / 2]).is_err());
        assert!(decode(b"not an image").is_err());
    }

    #[test]
    fn png_and_jpeg_decode_to_one_bounded_still_frame() {
        for format in [ImageFormat::Png, ImageFormat::Jpeg] {
            let mut bytes = Cursor::new(Vec::new());
            image::DynamicImage::new_rgb8(8, 12)
                .write_to(&mut bytes, format)
                .unwrap();
            let decoded = decode(bytes.get_ref()).unwrap();
            assert_eq!(decoded.frames.len(), 1);
            assert_eq!(decoded.frames[0].image.size, [8, 12]);
            assert_eq!(decoded.decoded_bytes, 8 * 12 * 4);
            assert!(!AnimatedImage::new(decoded).is_animated());
        }
    }
}

#[cfg(test)]
#[path = "chat_media_resource_tests.rs"]
mod resource_tests;
