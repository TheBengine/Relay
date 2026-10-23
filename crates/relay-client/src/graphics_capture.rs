//! Explicit Windows.Graphics.Capture of the selected item; never switches capture source.
use crate::capture::{CaptureSource, SourceKind};
use image::{DynamicImage, RgbaImage};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use windows::{
    core::{factory, Interface},
    Graphics::{
        Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession},
        DirectX::{Direct3D11::IDirect3DDevice, DirectXPixelFormat},
        SizeInt32,
    },
    Win32::{
        Foundation::{HMODULE, HWND, POINT},
        Graphics::{
            Direct3D::D3D_DRIVER_TYPE_HARDWARE,
            Direct3D11::*,
            Dxgi::{Common::DXGI_SAMPLE_DESC, IDXGIDevice},
            Gdi::{MonitorFromPoint, MONITOR_DEFAULTTONULL},
        },
        System::WinRT::{
            Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess},
            Graphics::Capture::IGraphicsCaptureItemInterop,
            RoInitialize, RoUninitialize, RO_INIT_MULTITHREADED,
        },
        UI::WindowsAndMessaging::{GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible},
    },
};

struct Apartment;
impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe {
            RoUninitialize();
        }
    }
}
pub struct GraphicsCapture {
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    _item: GraphicsCaptureItem,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    winrt_device: IDirect3DDevice,
    staging: Option<ID3D11Texture2D>,
    size: SizeInt32,
    window: Option<(HWND, u32)>,
    stop: Arc<AtomicBool>,
    /// QPC-based capture timestamp supplied by Windows, in 100 ns units.
    pub last_system_relative_time: i64,
    _apartment: Apartment,
}
impl GraphicsCapture {
    pub fn open(source: &CaptureSource, stop: Arc<AtomicBool>) -> Result<Self, String> {
        if stop.load(Ordering::Acquire) {
            return Err("Capture cancelled".into());
        }
        // SAFETY: all COM interfaces are created, used, and released on this owner thread.
        unsafe {
            RoInitialize(RO_INIT_MULTITHREADED)
                .map_err(|e| format!("Initialize Windows capture: {e}"))?;
        }
        let apartment = Apartment;
        if !GraphicsCaptureSession::IsSupported()
            .map_err(|e| format!("Check Windows capture: {e}"))?
        {
            return Err("Windows.Graphics.Capture is unavailable on this system".into());
        }
        let interop: IGraphicsCaptureItemInterop =
            factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
                .map_err(|e| format!("Create capture item: {e}"))?;
        let (item, window) = match source.kind {
            SourceKind::Window => {
                let fields: Vec<_> = source.id.split(':').collect();
                if fields.len() != 3 || fields[0] != "window" {
                    return Err("Invalid selected window".into());
                }
                let handle =
                    usize::from_str_radix(fields[1], 16).map_err(|_| "Invalid window handle")?;
                let pid = fields[2]
                    .parse::<u32>()
                    .map_err(|_| "Invalid window owner")?;
                let hwnd = HWND(handle as *mut std::ffi::c_void);
                validate_window(hwnd, pid)?;
                // SAFETY: handle and process identity were validated; only this item is captured.
                let item = unsafe { interop.CreateForWindow::<_, GraphicsCaptureItem>(hwnd) }
                    .map_err(|e| format!("Select Windows capture window: {e}"))?;
                (item, Some((hwnd, pid)))
            }
            SourceKind::Monitor => {
                let screen = screenshots::Screen::all()
                    .map_err(|e| format!("Find selected display: {e}"))?
                    .into_iter()
                    .find(|s| format!("monitor:{}", s.display_info.id) == source.id)
                    .ok_or("Selected display is no longer available")?;
                let info = screen.display_info;
                // SAFETY: this point lies inside the currently selected display's rectangle.
                let monitor = unsafe {
                    MonitorFromPoint(
                        POINT {
                            x: info.x + info.width as i32 / 2,
                            y: info.y + info.height as i32 / 2,
                        },
                        MONITOR_DEFAULTTONULL,
                    )
                };
                if monitor.is_invalid() {
                    return Err("Selected display is no longer available".into());
                }
                let item = unsafe { interop.CreateForMonitor::<_, GraphicsCaptureItem>(monitor) }
                    .map_err(|e| format!("Select Windows capture display: {e}"))?;
                (item, None)
            }
            SourceKind::Camera => return Err("Select a window or display".into()),
        };
        let size = item
            .Size()
            .map_err(|e| format!("Capture source size: {e}"))?;
        validate_size(size)?;
        let (mut device, mut context) = (None, None);
        // SAFETY: initialized output slots and valid Windows SDK constants; no raw buffer input.
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
        }
        .map_err(|e| format!("Create capture GPU device: {e}"))?;
        let device = device.ok_or("Windows returned no capture GPU device")?;
        let context = context.ok_or("Windows returned no capture GPU context")?;
        let dxgi: IDXGIDevice = device
            .cast()
            .map_err(|e| format!("Capture DXGI device: {e}"))?;
        let winrt_device: IDirect3DDevice = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) }
            .map_err(|e| format!("Capture WinRT device: {e}"))?
            .cast()
            .map_err(|e| format!("Capture D3D device interface: {e}"))?;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &winrt_device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            2,
            size,
        )
        .map_err(|e| format!("Create bounded capture frame pool: {e}"))?;
        let session = pool
            .CreateCaptureSession(&item)
            .map_err(|e| format!("Create capture session: {e}"))?;
        if stop.load(Ordering::Acquire) {
            let _ = session.Close();
            let _ = pool.Close();
            return Err("Capture cancelled".into());
        }
        session
            .StartCapture()
            .map_err(|e| format!("Start selected Windows capture: {e}"))?;
        Ok(Self {
            pool,
            session,
            _item: item,
            device,
            context,
            winrt_device,
            staging: None,
            size,
            window,
            stop,
            last_system_relative_time: 0,
            _apartment: apartment,
        })
    }
    pub fn capture(&mut self) -> Result<DynamicImage, String> {
        let started = Instant::now();
        loop {
            if self.stop.load(Ordering::Acquire) {
                return Err("Capture cancelled".into());
            }
            if let Some((window, pid)) = self.window {
                validate_window(window, pid)?;
            }
            let frame = match self.pool.TryGetNextFrame() {
                Ok(frame) => frame,
                Err(_) => {
                    if started.elapsed() > Duration::from_secs(2) {
                        return Err("Selected item did not produce a Windows capture frame".into());
                    }
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
            };
            let result = (|| {
                let size = frame
                    .ContentSize()
                    .map_err(|e| format!("Capture frame size: {e}"))?;
                validate_size(size)?;
                if size.Width > self.size.Width || size.Height > self.size.Height {
                    return Ok(None);
                }
                let access: IDirect3DDxgiInterfaceAccess = frame
                    .Surface()
                    .map_err(|e| format!("Capture surface: {e}"))?
                    .cast()
                    .map_err(|e| format!("Capture surface access: {e}"))?;
                let texture: ID3D11Texture2D = unsafe { access.GetInterface() }
                    .map_err(|e| format!("Capture GPU texture: {e}"))?;
                let mut desc = D3D11_TEXTURE2D_DESC::default();
                unsafe {
                    texture.GetDesc(&mut desc);
                }
                let needs_staging = self.staging.as_ref().is_none_or(|texture| {
                    let mut previous = D3D11_TEXTURE2D_DESC::default();
                    unsafe {
                        texture.GetDesc(&mut previous);
                    }
                    previous.Width != desc.Width || previous.Height != desc.Height
                });
                if needs_staging {
                    desc.Usage = D3D11_USAGE_STAGING;
                    desc.BindFlags = 0;
                    desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
                    desc.MiscFlags = 0;
                    desc.MipLevels = 1;
                    desc.ArraySize = 1;
                    desc.SampleDesc = DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    };
                    let mut staging = None;
                    unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut staging)) }
                        .map_err(|e| format!("Create reusable capture staging buffer: {e}"))?;
                    self.staging = staging;
                }
                let staging = self
                    .staging
                    .as_ref()
                    .ok_or("Capture staging buffer missing")?;
                unsafe {
                    self.context.CopyResource(staging, &texture);
                }
                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                unsafe {
                    self.context
                        .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                }
                .map_err(|e| format!("Read capture texture: {e}"))?;
                let copied = (|| {
                    let width = size.Width as usize;
                    let height = size.Height as usize;
                    let pitch = mapped.RowPitch as usize;
                    if mapped.pData.is_null() || pitch < width * 4 {
                        return Err("Invalid capture GPU row layout".to_string());
                    }
                    let mut rgba = vec![0u8; width * height * 4];
                    // SAFETY: Map returned a valid read-only texture mapping, and bounded ContentSize
                    // fits within the pool texture. Each read stays in its mapped row pitch.
                    for row in 0..height {
                        let bgra = unsafe {
                            std::slice::from_raw_parts(
                                (mapped.pData as *const u8).add(row * pitch),
                                width * 4,
                            )
                        };
                        for (to, from) in rgba[row * width * 4..(row + 1) * width * 4]
                            .chunks_exact_mut(4)
                            .zip(bgra.chunks_exact(4))
                        {
                            to.copy_from_slice(&[from[2], from[1], from[0], 255]);
                        }
                    }
                    RgbaImage::from_raw(width as u32, height as u32, rgba)
                        .map(DynamicImage::ImageRgba8)
                        .ok_or("Invalid capture pixel dimensions".into())
                })();
                unsafe {
                    self.context.Unmap(staging, 0);
                }
                self.last_system_relative_time = frame
                    .SystemRelativeTime()
                    .map_err(|e| format!("Capture timestamp: {e}"))?
                    .Duration;
                copied.map(Some)
            })();
            let next_size = frame.ContentSize().ok();
            let _ = frame.Close();
            if let Some(size) = next_size {
                if size != self.size {
                    validate_size(size)?;
                    self.pool
                        .Recreate(
                            &self.winrt_device,
                            DirectXPixelFormat::B8G8R8A8UIntNormalized,
                            2,
                            size,
                        )
                        .map_err(|e| format!("Resize capture frame pool: {e}"))?;
                    self.size = size;
                    self.staging = None;
                }
            }
            match result? {
                Some(image) => return Ok(image),
                None => continue,
            }
        }
    }
}
impl Drop for GraphicsCapture {
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}
fn validate_size(size: SizeInt32) -> Result<(), String> {
    if size.Width <= 0 || size.Height <= 0 || size.Width > 4096 || size.Height > 4096 {
        return Err("Capture source must fit within 4096×4096 pixels".into());
    }
    Ok(())
}
fn validate_window(window: HWND, pid: u32) -> Result<(), String> {
    // SAFETY: these APIs validate/query the handle without dereferencing application memory.
    unsafe {
        let mut actual = 0;
        if !IsWindow(window).as_bool()
            || !IsWindowVisible(window).as_bool()
            || IsIconic(window).as_bool()
            || GetWindowThreadProcessId(window, Some(&mut actual)) == 0
            || actual != pid
        {
            return Err(
                "Selected window is closed, minimized, or belongs to another process".into(),
            );
        }
    }
    Ok(())
}
