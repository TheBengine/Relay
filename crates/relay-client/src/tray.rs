//! The notification-area icon belongs to the native UI thread and its window.
//! Hiding keeps the app alive; quitting hides first, then uses normal shutdown.
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::{cell::Cell, marker::PhantomData, rc::Rc};
use windows::{
    core::w,
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM},
        System::Threading::{GetCurrentProcessId, GetCurrentThreadId},
        UI::{
            Shell::{
                DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass, Shell_NotifyIconW,
                NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_SETFOCUS,
                NIM_SETVERSION, NINF_KEY, NIN_SELECT, NOTIFYICONDATAW, NOTIFYICON_VERSION_4,
            },
            WindowsAndMessaging::{
                AppendMenuW, CreatePopupMenu, DestroyMenu, GetCursorPos, GetWindowThreadProcessId,
                IsIconic, IsWindowVisible, LoadIconW, PostMessageW, RegisterWindowMessageW, SendMessageW,
                SetForegroundWindow, ShowWindow, TrackPopupMenu, HICON, ICON_BIG, ICON_SMALL,
                IDI_APPLICATION, MF_STRING, SW_HIDE, SW_RESTORE, SW_SHOW, TPM_BOTTOMALIGN,
                TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_APP, WM_CLOSE, WM_CONTEXTMENU, WM_GETICON,
                WM_LBUTTONDBLCLK, WM_NCDESTROY, WM_NULL, WM_PAINT,
            },
        },
    },
};

const CALLBACK: u32 = WM_APP + 0x26a;
const SUBCLASS: usize = 0x524c5452;
const ICON_ID: u32 = 1;
const OPEN: usize = 1;
const QUIT: usize = 2;
const NIN_KEYSELECT: u32 = NIN_SELECT | NINF_KEY;

struct State {
    window: HWND,
    icon: HICON, // borrowed from the window (or the shared Windows fallback)
    taskbar_created: u32,
    available: Cell<bool>,
    quitting: Cell<bool>,
    alive: Cell<bool>,
    ctx: eframe::egui::Context,
}

impl State {
    fn data(&self) -> NOTIFYICONDATAW {
        let mut data = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.window,
            uID: ICON_ID,
            uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP,
            uCallbackMessage: CALLBACK,
            hIcon: self.icon,
            ..Default::default()
        };
        for (slot, ch) in data.szTip.iter_mut().zip("Relay".encode_utf16()) {
            *slot = ch;
        }
        data
    }

    fn add(&self) {
        let mut data = self.data();
        let added = unsafe { Shell_NotifyIconW(NIM_ADD, &data) }.as_bool();
        if added {
            data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
            if !unsafe { Shell_NotifyIconW(NIM_SETVERSION, &data) }.as_bool() {
                unsafe {
                    let _ = Shell_NotifyIconW(NIM_DELETE, &data);
                }
                self.available.set(false);
                return;
            }
        }
        self.available.set(added);
    }

    fn remove(&self) {
        if self.available.replace(false) {
            unsafe {
                let _ = Shell_NotifyIconW(NIM_DELETE, &self.data());
            }
        }
    }

    fn restore(&self) {
        if self.alive.get() && !self.quitting.get() {
            unsafe {
                let command = if IsIconic(self.window).as_bool() {
                    SW_RESTORE
                } else {
                    SW_SHOW
                };
                let _ = ShowWindow(self.window, command);
                let _ = SetForegroundWindow(self.window);
            }
            self.ctx.request_repaint();
        }
    }

    fn begin_exit(&self) {
        self.quitting.set(true);
        // Synchronous: the window is gone before any network/server drain.
        if self.alive.get() {
            unsafe {
                let _ = ShowWindow(self.window, SW_HIDE);
            }
        }
        self.remove();
    }

    unsafe fn menu(&self, wparam: WPARAM) {
        let Ok(menu) = CreatePopupMenu() else { return };
        let populated = AppendMenuW(menu, MF_STRING, OPEN, w!("Open Relay"))
            .and_then(|_| AppendMenuW(menu, MF_STRING, QUIT, w!("Quit Relay")));
        if populated.is_ok() {
            // Version 4 packs signed screen coordinates in wParam.
            let mut point = POINT {
                x: (wparam.0 as u16 as i16) as i32,
                y: ((wparam.0 >> 16) as u16 as i16) as i32,
            };
            if point.x == -1 && point.y == -1 {
                let _ = GetCursorPos(&mut point);
            }
            let _ = SetForegroundWindow(self.window);
            let selected = TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN,
                point.x,
                point.y,
                0,
                self.window,
                None,
            )
            .0 as usize;
            let _ = PostMessageW(self.window, WM_NULL, WPARAM(0), LPARAM(0));
            match selected {
                OPEN => self.restore(),
                QUIT => {
                    self.begin_exit();
                    // CloseRequested runs even for a hidden window. Keep the
                    // main thread alive until RelayApp's normal Drop completes.
                    let _ = PostMessageW(self.window, WM_CLOSE, WPARAM(0), LPARAM(0));
                    // Hidden windows do not receive the WM_PAINT normally
                    // scheduled by winit's request_redraw. Dispatch one final
                    // frame explicitly so eframe can accept CloseRequested
                    // without showing the window again.
                    let _ = PostMessageW(self.window, WM_PAINT, WPARAM(0), LPARAM(0));
                    self.ctx.request_repaint();
                }
                _ => {
                    let _ = Shell_NotifyIconW(NIM_SETFOCUS, &self.data());
                }
            }
        }
        let _ = DestroyMenu(menu);
    }
}

pub(crate) struct Tray {
    state: Box<State>, // stable address while the native subclass can call it
    _ui_thread: PhantomData<Rc<()>>,
}

unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    subclass: usize,
    pointer: usize,
) -> LRESULT {
    let state = &*(pointer as *const State);
    if message == state.taskbar_created && message != 0 && !state.quitting.get() {
        state.available.set(false);
        state.add();
        // Do not leave an unreachable hidden app if Explorer cannot restore it.
        if !state.available.get() {
            state.restore();
        }
    } else if message == CALLBACK {
        if ((lparam.0 as u32) >> 16) == ICON_ID {
            match lparam.0 as u32 & 0xffff {
                NIN_SELECT | NIN_KEYSELECT | WM_LBUTTONDBLCLK => state.restore(),
                WM_CONTEXTMENU => state.menu(wparam),
                _ => {}
            }
        }
        return LRESULT(0);
    } else if message == WM_NCDESTROY {
        state.remove();
        state.alive.set(false);
        let _ = RemoveWindowSubclass(window, Some(window_proc), subclass);
    }
    DefSubclassProc(window, message, wparam, lparam)
}

impl Tray {
    pub(crate) fn bind(cc: &eframe::CreationContext<'_>) -> Result<Self, &'static str> {
        let RawWindowHandle::Win32(handle) = cc
            .window_handle()
            .map_err(|_| "Tray window unavailable")?
            .as_raw()
        else {
            return Err("Tray window unavailable");
        };
        let window = HWND(handle.hwnd.get() as *mut _);
        let mut process = 0;
        let thread = unsafe { GetWindowThreadProcessId(window, Some(&mut process)) };
        if thread != unsafe { GetCurrentThreadId() } || process != unsafe { GetCurrentProcessId() }
        {
            return Err("Tray window identity unavailable");
        }
        let mut icon =
            unsafe { SendMessageW(window, WM_GETICON, WPARAM(ICON_SMALL as usize), LPARAM(0)) }.0;
        if icon == 0 {
            icon =
                unsafe { SendMessageW(window, WM_GETICON, WPARAM(ICON_BIG as usize), LPARAM(0)) }.0;
        }
        let icon = if icon == 0 {
            unsafe { LoadIconW(None, IDI_APPLICATION) }.map_err(|_| "Tray icon unavailable")?
        } else {
            HICON(icon as *mut _)
        };
        let state = Box::new(State {
            window,
            icon,
            taskbar_created: unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) },
            available: Cell::new(false),
            quitting: Cell::new(false),
            alive: Cell::new(true),
            ctx: cc.egui_ctx.clone(),
        });
        if !unsafe {
            SetWindowSubclass(
                window,
                Some(window_proc),
                SUBCLASS,
                (&*state as *const State) as usize,
            )
        }
        .as_bool()
        {
            return Err("Tray window callback unavailable");
        }
        state.add();
        Ok(Self {
            state,
            _ui_thread: PhantomData,
        })
    }

    pub(crate) fn available(&self) -> bool {
        self.state.available.get()
    }
    pub(crate) fn quitting(&self) -> bool {
        self.state.quitting.get()
    }
    /// A tray-hidden window is not necessarily minimized. Query the actual
    /// native visibility so background notifications cannot resume video work.
    pub(crate) fn window_visible(&self) -> bool {
        self.state.alive.get()
            && unsafe { IsWindowVisible(self.state.window).as_bool() }
            && !unsafe { IsIconic(self.state.window).as_bool() }
    }
    pub(crate) fn hide(&self) {
        if self.state.alive.get() {
            unsafe {
                let _ = ShowWindow(self.state.window, SW_HIDE);
            }
        }
    }
    pub(crate) fn begin_exit(&self) {
        let first = !self.state.quitting.get();
        self.state.begin_exit();
        if first && self.state.alive.get() {
            // The custom close button and installer also hide before queuing
            // egui's Close command. That command needs a final hidden frame.
            unsafe {
                let _ = PostMessageW(self.state.window, WM_PAINT, WPARAM(0), LPARAM(0));
            }
        }
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        self.state.remove();
        if self.state.alive.get() {
            unsafe {
                let _ = RemoveWindowSubclass(self.state.window, Some(window_proc), SUBCLASS);
            }
        }
    }
}
