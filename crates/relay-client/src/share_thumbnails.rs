//! Small still pictures of each screen and window for the Share Screen
//! picker, as in Discord. Local only: a worker grabs them while the picker is
//! open, they are shown in the picker and never sent anywhere. The live
//! capture itself (`capture.rs`, `graphics_capture.rs`) is not used here.
use crate::capture::{CaptureSource, SourceKind};
use eframe::egui::{self, ColorImage, TextureHandle, TextureOptions};
use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    time::{Duration, Instant},
};

/// The largest thumbnail; each keeps its source's aspect ratio inside it.
pub(crate) const THUMB_WIDTH: u32 = 320;
pub(crate) const THUMB_HEIGHT: u32 = 180;
/// How often the pictures refresh while the picker stays open.
const REFRESH: Duration = Duration::from_millis(1500);

/// One grabbed picture, RGBA.
pub(crate) struct Thumbnail {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// `(source_width, source_height)` scaled to fit the thumbnail box, at least 1x1.
pub(crate) fn fit(width: u32, height: u32) -> (u32, u32) {
    if width == 0 || height == 0 {
        return (THUMB_WIDTH, THUMB_HEIGHT);
    }
    let scale = (THUMB_WIDTH as f32 / width as f32).min(THUMB_HEIGHT as f32 / height as f32);
    (
        ((width as f32 * scale).round() as u32).clamp(1, THUMB_WIDTH),
        ((height as f32 * scale).round() as u32).clamp(1, THUMB_HEIGHT),
    )
}

type Grabbed = (String, Option<Thumbnail>);

/// The picker's thumbnails. Dropping it stops the worker after its current grab.
#[derive(Default)]
pub(crate) struct ShareThumbnails {
    textures: HashMap<String, TextureHandle>,
    /// Sources that could not be pictured (protected, minimized, closed).
    unavailable: HashSet<String>,
    round: Option<mpsc::Receiver<Grabbed>>,
    stop: Arc<AtomicBool>,
    last_round: Option<Instant>,
    /// Capture fixtures hold still pictures; nothing is grabbed.
    fixed: bool,
}

impl ShareThumbnails {
    pub(crate) fn texture(&self, id: &str) -> Option<&TextureHandle> {
        self.textures.get(id)
    }

    pub(crate) fn unavailable(&self, id: &str) -> bool {
        self.unavailable.contains(id)
    }

    /// A still picture for a capture fixture; switches grabbing off.
    pub(crate) fn insert_fixture(&mut self, ctx: &egui::Context, id: &str, image: ColorImage) {
        self.fixed = true;
        let texture = ctx.load_texture(format!("share-thumb-{id}"), image, TextureOptions::LINEAR);
        self.textures.insert(id.to_owned(), texture);
    }

    /// A capture fixture's source that has no picture; switches grabbing off.
    pub(crate) fn mark_unavailable(&mut self, id: &str) {
        self.fixed = true;
        self.unavailable.insert(id.to_owned());
    }

    /// Takes finished pictures and starts a new round for `sources` when the
    /// last one is older than the refresh interval. Cameras are skipped: the
    /// camera dialog has its own explicit preview.
    pub(crate) fn update(&mut self, ctx: &egui::Context, sources: &[CaptureSource]) {
        if self.fixed {
            return;
        }
        if let Some(round) = &self.round {
            loop {
                match round.try_recv() {
                    Ok((id, Some(picture))) => {
                        self.unavailable.remove(&id);
                        let image = ColorImage::from_rgba_unmultiplied(
                            [picture.width as usize, picture.height as usize],
                            &picture.rgba,
                        );
                        match self.textures.get_mut(&id) {
                            Some(texture) => texture.set(image, TextureOptions::LINEAR),
                            None => {
                                let texture = ctx.load_texture(
                                    format!("share-thumb-{id}"),
                                    image,
                                    TextureOptions::LINEAR,
                                );
                                self.textures.insert(id, texture);
                            }
                        }
                    }
                    Ok((id, None)) => {
                        self.textures.remove(&id);
                        self.unavailable.insert(id);
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        self.round = None;
                        self.last_round = Some(Instant::now());
                        break;
                    }
                }
            }
        }
        let wanted: Vec<CaptureSource> = sources
            .iter()
            .filter(|source| source.kind != SourceKind::Camera)
            .cloned()
            .collect();
        self.textures.retain(|id, _| wanted.iter().any(|source| source.id == *id));
        if self.round.is_none() && !wanted.is_empty() {
            let due = self
                .last_round
                .is_none_or(|last| last.elapsed() >= REFRESH);
            if due {
                let (tx, rx) = mpsc::channel();
                let stop = self.stop.clone();
                let repaint = ctx.clone();
                let spawned = std::thread::Builder::new()
                    .name("share-thumbnails".into())
                    .spawn(move || {
                        for source in wanted {
                            if stop.load(Ordering::Relaxed) {
                                return;
                            }
                            let picture = grab(&source);
                            if tx.send((source.id, picture)).is_err() {
                                return;
                            }
                            repaint.request_repaint();
                        }
                    });
                if spawned.is_ok() {
                    self.round = Some(rx);
                } else {
                    self.last_round = Some(Instant::now());
                }
            }
        }
        ctx.request_repaint_after(if self.round.is_some() {
            Duration::from_millis(100)
        } else {
            REFRESH
        });
    }
}

impl Drop for ShareThumbnails {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

#[cfg(windows)]
fn grab(source: &CaptureSource) -> Option<Thumbnail> {
    match source.kind {
        SourceKind::Monitor => gdi::monitor(&source.id),
        SourceKind::Window => gdi::window(&source.id),
        SourceKind::Camera => None,
    }
}

#[cfg(not(windows))]
fn grab(_source: &CaptureSource) -> Option<Thumbnail> {
    None
}

#[cfg(windows)]
mod gdi {
    use super::{fit, Thumbnail};
    use windows::Win32::{
        Foundation::{HWND, RECT},
        Graphics::Gdi::*,
        Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS},
        UI::WindowsAndMessaging::*,
    };

    /// A memory bitmap of `width` x `height` selected into its own DC.
    struct Canvas {
        dc: HDC,
        bitmap: HBITMAP,
        previous: HGDIOBJ,
        width: i32,
        height: i32,
    }

    impl Canvas {
        unsafe fn new(like: HDC, width: i32, height: i32) -> Option<Self> {
            let dc = CreateCompatibleDC(like);
            if dc.is_invalid() {
                return None;
            }
            let bitmap = CreateCompatibleBitmap(like, width, height);
            if bitmap.is_invalid() {
                let _ = DeleteDC(dc);
                return None;
            }
            let previous = SelectObject(dc, bitmap);
            Some(Self {
                dc,
                bitmap,
                previous,
                width,
                height,
            })
        }

        /// The pixels as RGBA; `None` for an all-black picture (a protected
        /// or GPU-only surface that GDI cannot read).
        unsafe fn pixels(&self) -> Option<Thumbnail> {
            let mut rgba = vec![0u8; self.width as usize * self.height as usize * 4];
            let mut info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: self.width,
                    biHeight: -self.height,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            SelectObject(self.dc, self.previous);
            let lines = GetDIBits(
                self.dc,
                self.bitmap,
                0,
                self.height as u32,
                Some(rgba.as_mut_ptr().cast()),
                &mut info,
                DIB_RGB_COLORS,
            );
            SelectObject(self.dc, self.bitmap);
            if lines != self.height || rgba.chunks_exact(4).all(|p| p[..3] == [0, 0, 0]) {
                return None;
            }
            for pixel in rgba.chunks_exact_mut(4) {
                pixel.swap(0, 2);
                pixel[3] = 255;
            }
            Some(Thumbnail {
                width: self.width as u32,
                height: self.height as u32,
                rgba,
            })
        }
    }

    impl Drop for Canvas {
        fn drop(&mut self) {
            unsafe {
                SelectObject(self.dc, self.previous);
                let _ = DeleteObject(self.bitmap);
                let _ = DeleteDC(self.dc);
            }
        }
    }

    /// Scales `source` (a `width` x `height` area at `x`,`y` of `from`) into a thumbnail.
    unsafe fn shrink(from: HDC, x: i32, y: i32, width: i32, height: i32) -> Option<Thumbnail> {
        let (w, h) = fit(width as u32, height as u32);
        let canvas = Canvas::new(from, w as i32, h as i32)?;
        SetStretchBltMode(canvas.dc, HALFTONE);
        let _ = SetBrushOrgEx(canvas.dc, 0, 0, None);
        StretchBlt(canvas.dc, 0, 0, w as i32, h as i32, from, x, y, width, height, SRCCOPY)
            .as_bool()
            .then_some(())?;
        canvas.pixels()
    }

    pub(super) fn monitor(id: &str) -> Option<Thumbnail> {
        let screens = screenshots::Screen::all().ok()?;
        let display = screens
            .iter()
            .map(|screen| screen.display_info)
            .find(|display| format!("monitor:{}", display.id) == id)?;
        unsafe {
            let screen = GetDC(None);
            if screen.is_invalid() {
                return None;
            }
            let picture = shrink(
                screen,
                display.x,
                display.y,
                display.width as i32,
                display.height as i32,
            );
            ReleaseDC(None, screen);
            picture
        }
    }

    pub(super) fn window(id: &str) -> Option<Thumbnail> {
        let (handle, owner) = id.strip_prefix("window:")?.split_once(':')?;
        let hwnd = HWND(usize::from_str_radix(handle, 16).ok()? as *mut std::ffi::c_void);
        let owner = owner.parse::<u32>().ok()?;
        unsafe {
            let mut process = 0u32;
            if !IsWindow(hwnd).as_bool()
                || GetWindowThreadProcessId(hwnd, Some(&mut process)) == 0
                || process != owner
                || !IsWindowVisible(hwnd).as_bool()
                || IsIconic(hwnd).as_bool()
            {
                return None;
            }
            // A window that asks not to be captured gets no picture.
            let mut affinity = 0u32;
            if GetWindowDisplayAffinity(hwnd, &mut affinity).is_ok() && affinity != 0 {
                return None;
            }
            let mut rect = RECT::default();
            GetWindowRect(hwnd, &mut rect).ok()?;
            let (width, height) = (rect.right - rect.left, rect.bottom - rect.top);
            if width <= 0 || height <= 0 || width > 8192 || height > 8192 {
                return None;
            }
            let window_dc = GetWindowDC(hwnd);
            if window_dc.is_invalid() {
                return None;
            }
            let full = Canvas::new(window_dc, width, height);
            ReleaseDC(hwnd, window_dc);
            let full = full?;
            // PW_RENDERFULLCONTENT (2) reads modern, composited windows.
            if !PrintWindow(hwnd, full.dc, PRINT_WINDOW_FLAGS(2)).as_bool() {
                return None;
            }
            shrink(full.dc, 0, 0, width, height)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thumbnails_keep_the_source_shape_inside_the_box() {
        assert_eq!(fit(1920, 1080), (320, 180));
        assert_eq!(fit(2560, 1440), (320, 180));
        assert_eq!(fit(1080, 1920), (101, 180));
        assert_eq!(fit(3440, 1440), (320, 134));
        assert_eq!(fit(0, 0), (THUMB_WIDTH, THUMB_HEIGHT));
        assert_eq!(fit(10_000, 1), (320, 1));
    }

    /// Opt-in check on a real desktop: the primary display gives a picture
    /// that fits the box and is not blank. Pixels stay in memory only.
    #[test]
    #[ignore]
    #[cfg(windows)]
    fn a_real_display_gives_a_fitting_thumbnail() {
        let sources = crate::capture::enumerate_screen_sources().unwrap();
        let display = sources
            .iter()
            .find(|source| source.kind == SourceKind::Monitor)
            .expect("a display");
        let picture = grab(display).expect("a picture of the display");
        assert!(picture.width <= THUMB_WIDTH && picture.height <= THUMB_HEIGHT);
        assert_eq!(picture.rgba.len(), (picture.width * picture.height * 4) as usize);
        let windows = sources.iter().filter(|s| s.kind == SourceKind::Window).count();
        let pictured = sources
            .iter()
            .filter(|s| s.kind == SourceKind::Window)
            .filter(|s| grab(s).is_some())
            .count();
        println!("display {}x{}; windows pictured {pictured}/{windows}", picture.width, picture.height);
    }
}
