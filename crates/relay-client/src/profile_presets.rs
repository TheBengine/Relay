//! Offline artwork. Avatar glyphs use the already bundled, credited Twemoji atlas.
use eframe::egui;
use image::{Rgba, RgbaImage};

pub const THEMES: &[(&str, u32, u32)] = &[
    ("Relay", 0x5865f2, 0x232428),
    ("Ocean", 0x4ab5db, 0x20425a),
    ("Forest", 0x57c59a, 0x193c32),
    ("Sunset", 0xf7a971, 0x58364e),
    ("Lavender", 0xb59bff, 0x47336d),
    ("Rose", 0xff91b6, 0x542c43),
    ("Midnight", 0x879dff, 0x171e46),
    ("Mint", 0x80e6d0, 0x173f42),
    ("Amber", 0xffcf6f, 0x51351b),
    ("Cherry", 0xff8a8e, 0x501e30),
    ("Glacier", 0xa1deff, 0x284358),
    ("Mocha", 0xdab897, 0x3e302a),
    ("Orchid", 0xe5a1ff, 0x42204f),
    ("Lime", 0xc7e878, 0x303d20),
    ("Coral", 0xffb09a, 0x57362e),
    ("Graphite", 0xc4cad6, 0x2d3038),
];
// Glyph, accessible name, theme. Keep imagery inside a circular avatar's safe area.
pub const AVATARS: &[(&str, &str, usize)] = &[
    ("🤖", "Robot", 0),
    ("🦊", "Fox", 3),
    ("🐱", "Cat", 8),
    ("🐼", "Panda", 15),
    ("🐸", "Frog", 2),
    ("🐧", "Penguin", 10),
    ("👻", "Ghost", 4),
    ("👽", "Alien", 7),
    ("🐙", "Octopus", 5),
    ("🐻", "Bear", 11),
    ("🦉", "Owl", 8),
    ("🐺", "Wolf", 6),
    ("🚀", "Rocket", 6),
    ("🪐", "Saturn", 12),
    ("🌻", "Sunflower", 13),
    ("🍄", "Mushroom", 9),
];
// Name, theme, drawing: mountains / waves / orbital / dusk.
pub const BANNERS: &[(&str, usize, usize)] = &[
    ("Alpine", 10, 0),
    ("Pine Ridge", 2, 0),
    ("Rose Peaks", 5, 0),
    ("Tidal", 1, 1),
    ("Mint Flow", 7, 1),
    ("Violet Silk", 4, 1),
    ("Deep Space", 6, 2),
    ("Orchid Orbit", 12, 2),
    ("Golden Orbit", 8, 2),
    ("Afterglow", 3, 3),
    ("Cherry Dusk", 9, 3),
    ("Blue Hour", 1, 3),
];

fn rgb(value: u32) -> [f32; 3] {
    [
        ((value >> 16) & 255) as f32,
        ((value >> 8) & 255) as f32,
        (value & 255) as f32,
    ]
}
fn mix(a: [f32; 3], b: [f32; 3], t: f32) -> Rgba<u8> {
    let t = t.clamp(0.0, 1.0);
    Rgba([
        (a[0] + (b[0] - a[0]) * t) as u8,
        (a[1] + (b[1] - a[1]) * t) as u8,
        (a[2] + (b[2] - a[2]) * t) as u8,
        255,
    ])
}
pub fn banner(index: usize, width: u32) -> RgbaImage {
    let (_, theme, drawing) = BANNERS[index];
    let (_, accent, base) = THEMES[theme];
    let a = rgb(accent);
    let b = rgb(base);
    let height = width * 2 / 5;
    RgbaImage::from_fn(width, height, |px, py| {
        let x = px as f32 / width as f32;
        let y = py as f32 / height as f32;
        let mut t = 0.12 + 0.4 * (1.0 - y) + 0.12 * x;
        match drawing {
            0 => {
                for layer in 0..3 {
                    let l = layer as f32;
                    let ridge = 0.42
                        + l * 0.18
                        + 0.14 * (x * 11.0 + l * 2.0).sin()
                        + 0.06 * (x * 27.0 + l).sin();
                    if y > ridge {
                        t = 0.27 - l * 0.1;
                    }
                }
                if ((x - 0.78) * 2.5).powi(2) + (y - 0.24).powi(2) < 0.009 {
                    t = 0.95;
                }
            }
            1 => {
                t = 0.24 + 0.2 * (x * 8.0 + y * 5.0).sin();
                for band in 0..5 {
                    let line =
                        0.15 + band as f32 * 0.19 + 0.12 * (x * 7.0 + band as f32 * 0.6).sin();
                    t += 0.23 * (-((y - line) * 34.0).powi(2)).exp();
                }
            }
            2 => {
                let r = (((x - 0.65) * 2.5).powi(2) + ((y - 0.48) * 1.7).powi(2)).sqrt();
                if r < 0.33 {
                    t = 0.4 + 0.4 * (1.0 - r / 0.33);
                }
                for radius in [0.48, 0.67, 0.87] {
                    if (r - radius).abs() < 0.009 {
                        t = 0.6;
                    }
                }
                // A deterministic sparse star field, independent of render resolution.
                let gx = (x * 90.0).floor() as u32;
                let gy = (y * 36.0).floor() as u32;
                if (gx * 31 + gy * 73).is_multiple_of(211)
                    && (x * 90.0).fract() < 0.13
                    && (y * 36.0).fract() < 0.32
                {
                    t = 0.95;
                }
            }
            _ => {
                t = 0.2 + 0.65 * (1.0 - (y - 0.45).abs());
                if ((x - 0.68) * 2.5).powi(2) + (y - 0.49).powi(2) < 0.04 {
                    t = 1.0;
                }
                if y > 0.65 {
                    t = 0.12 + 0.14 * ((y * 100.0).sin() * 0.5 + 0.5) * (1.0 - (x - 0.68).abs());
                }
            }
        }
        mix(b, a, t)
    })
}
pub fn avatar(index: usize, atlas: &RgbaImage) -> RgbaImage {
    let (glyph, _, theme) = AVATARS[index];
    let (_, accent, base) = THEMES[theme];
    let mut image = RgbaImage::from_fn(256, 256, |x, y| {
        mix(
            rgb(base),
            rgb(accent),
            0.2 + 0.65 * (1.0 - (x + y) as f32 / 512.0),
        )
    });
    let catalog = crate::emoji::catalog();
    let (entry, _) = catalog.at_start(glyph).expect("bundled avatar glyph");
    let uv = catalog.uv(entry);
    let x = (uv.min.x * atlas.width() as f32).round() as u32;
    let y = (uv.min.y * atlas.height() as f32).round() as u32;
    let w = (uv.width() * atlas.width() as f32).round() as u32;
    let h = (uv.height() * atlas.height() as f32).round() as u32;
    let glyph = image::imageops::crop_imm(atlas, x, y, w, h).to_image();
    let glyph = image::imageops::resize(&glyph, 164, 164, image::imageops::FilterType::Lanczos3);
    image::imageops::overlay(&mut image, &glyph, 46, 46);
    // The backdrop is opaque; image 0.24's floating point blend can round alpha to 254.
    for pixel in image.pixels_mut() {
        pixel[3] = 255;
    }
    image
}
pub fn color(image: &RgbaImage) -> egui::ColorImage {
    egui::ColorImage::from_rgba_unmultiplied(
        [image.width() as usize, image.height() as usize],
        image.as_raw(),
    )
}
pub fn png(image: RgbaImage) -> Result<Vec<u8>, String> {
    let mut bytes = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(image)
        .write_to(&mut bytes, image::ImageOutputFormat::Png)
        .map_err(|_| "Cannot prepare this profile image.".to_owned())?;
    Ok(bytes.into_inner())
}

type AvatarImages = Vec<(egui::ColorImage, Vec<u8>)>;
static AVATAR_BUSY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
struct AvatarAdmission;
impl AvatarAdmission {
    fn acquire() -> Option<Self> {
        AVATAR_BUSY
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .ok()
            .map(|_| Self)
    }
}
impl Drop for AvatarAdmission {
    fn drop(&mut self) {
        AVATAR_BUSY.store(false, std::sync::atomic::Ordering::Release);
    }
}

#[derive(Default)]
pub struct Gallery {
    avatars: Vec<(egui::TextureHandle, Vec<u8>)>,
    avatar_rx: Option<std::sync::mpsc::Receiver<AvatarImages>>,
    avatar_error: bool,
    banners: Vec<egui::TextureHandle>,
}
impl Gallery {
    pub fn avatars(&mut self, ui: &mut egui::Ui) -> Option<(egui::TextureHandle, Vec<u8>)> {
        if self.avatars.is_empty() {
            if let Some(rx) = &self.avatar_rx {
                match rx.try_recv() {
                    Ok(images) => {
                        self.avatars = images
                            .into_iter()
                            .enumerate()
                            .map(|(i, (image, bytes))| {
                                (
                                    ui.ctx().load_texture(
                                        format!("avatar-preset-{i}"),
                                        image,
                                        Default::default(),
                                    ),
                                    bytes,
                                )
                            })
                            .collect();
                        self.avatar_rx = None;
                    }
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        self.avatar_error = true;
                        self.avatar_rx = None;
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => {}
                }
            } else if !self.avatar_error {
                let Some(admission) = AvatarAdmission::acquire() else {
                    ui.spinner();
                    ui.label("Preparing avatars…");
                    ui.ctx()
                        .request_repaint_after(std::time::Duration::from_millis(100));
                    return None;
                };
                let (tx, rx) = std::sync::mpsc::sync_channel(1);
                let ctx = ui.ctx().clone();
                self.avatar_error = std::thread::Builder::new()
                    .name("relay-avatar-gallery".into())
                    .spawn(move || {
                        let _admission = admission;
                        let atlas =
                            image::load_from_memory(include_bytes!("../assets/emoji/atlas.png"))
                                .expect("bundled atlas")
                                .to_rgba8();
                        let images = (0..AVATARS.len())
                            .map(|i| {
                                let image = avatar(i, &atlas);
                                (color(&image), png(image).expect("bundled avatar PNG"))
                            })
                            .collect();
                        let _ = tx.send(images);
                        ctx.request_repaint();
                    })
                    .is_err();
                if !self.avatar_error {
                    self.avatar_rx = Some(rx);
                }
            }
            if self.avatars.is_empty() {
                if self.avatar_error {
                    ui.label("Cannot prepare the avatar gallery.");
                    if ui.button("Try again").clicked() {
                        self.avatar_error = false;
                    }
                } else {
                    ui.spinner();
                    ui.label("Preparing avatars…");
                }
                return None;
            }
        }
        let mut chosen = None;
        ui.horizontal_wrapped(|ui| {
            for (i, (texture, bytes)) in self.avatars.iter().enumerate() {
                let response = ui.add(
                    egui::ImageButton::new((texture.id(), egui::vec2(64.0, 64.0))).rounding(32.0),
                );
                response.widget_info(|| {
                    egui::WidgetInfo::labeled(egui::WidgetType::Button, AVATARS[i].1)
                });
                if response.on_hover_text(AVATARS[i].1).clicked() {
                    chosen = Some((texture.clone(), bytes.clone()));
                }
            }
        });
        chosen
    }
    pub fn banners(&mut self, ui: &mut egui::Ui) -> Option<usize> {
        if self.banners.is_empty() {
            self.banners = BANNERS
                .iter()
                .enumerate()
                .map(|(i, _)| {
                    ui.ctx().load_texture(
                        format!("banner-preset-{i}"),
                        color(&banner(i, 300)),
                        Default::default(),
                    )
                })
                .collect();
        }
        let mut chosen = None;
        // Nested vertical layouts do not participate in egui's horizontal wrapping.
        // Give each tile a bounded column so the gallery cannot push the preview offscreen.
        ui.columns(2, |columns| {
            for (i, texture) in self.banners.iter().enumerate() {
                let ui = &mut columns[i % 2];
                let width =
                    (ui.available_width() - 2.0 * ui.spacing().button_padding.x).clamp(1.0, 180.0);
                let response = ui.add(
                    egui::ImageButton::new((texture.id(), egui::vec2(width, width * 0.4)))
                        .rounding(5.0),
                );
                response.widget_info(|| {
                    egui::WidgetInfo::labeled(egui::WidgetType::Button, BANNERS[i].0)
                });
                if response.clicked() {
                    chosen = Some(i);
                }
                ui.label(BANNERS[i].0);
                ui.add_space(6.0);
            }
        });
        chosen
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn atlas_generation_admits_only_one_worker_even_across_sessions() {
        let guard = AvatarAdmission::acquire().unwrap();
        assert!(AvatarAdmission::acquire().is_none());
        drop(guard);
        assert!(AvatarAdmission::acquire().is_some());
    }
    #[test]
    fn banner_gallery_wraps_inside_narrow_settings_columns() {
        for width in [240.0, 360.0, 460.0, 800.0] {
            let ctx = egui::Context::default();
            let mut gallery = Gallery::default();
            for _ in 0..3 {
                let _ = ctx.run(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(width, 1600.0),
                        )),
                        ..Default::default()
                    },
                    |ctx| {
                        egui::CentralPanel::default().show(ctx, |ui| {
                            gallery.banners(ui);
                            assert!(
                                ui.min_rect().right() <= width,
                                "gallery overflow at {width}: {:?}",
                                ui.min_rect()
                            );
                        });
                    },
                );
            }
        }
    }
    #[test]
    fn built_in_images_roundtrip_within_upload_limits() {
        let atlas = image::load_from_memory(include_bytes!("../assets/emoji/atlas.png"))
            .unwrap()
            .to_rgba8();
        let mut hashes = std::collections::HashSet::new();
        for image in (0..AVATARS.len())
            .map(|i| avatar(i, &atlas))
            .chain((0..BANNERS.len()).map(|i| banner(i, 1200)))
        {
            let dimensions = image.dimensions();
            assert!(image.pixels().all(|p| p[3] == 255));
            let bytes = png(image).unwrap();
            assert!(bytes.len() < 2 * 1024 * 1024);
            assert!(hashes.insert(bytes.clone()), "duplicate artwork");
            assert_eq!(
                image::load_from_memory(&bytes)
                    .unwrap()
                    .to_rgba8()
                    .dimensions(),
                dimensions
            );
        }
    }
}
