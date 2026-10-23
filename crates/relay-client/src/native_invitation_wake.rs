//! Wake a minimized native window before egui's paused render loop is needed.
//! Only the application's own creation handle is subclassed, on its UI thread.
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::{marker::PhantomData, rc::Rc, sync::Arc};
use windows::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, WPARAM},
    System::Threading::{GetCurrentProcessId, GetCurrentThreadId},
    UI::{
        Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
        WindowsAndMessaging::{
            GetWindowThreadProcessId, IsIconic, IsWindowVisible, PostMessageW, ShowWindow,
            SW_SHOWNOACTIVATE, WM_APP, WM_NCDESTROY,
        },
    },
};

const MESSAGE: u32 = WM_APP + 0x269;
const SUBCLASS: usize = 0x524c494e;
struct State {
    window: usize,
    cookie: usize,
}
pub struct WindowWake {
    state: Arc<State>,
    // Installation/removal must stay on the same native UI thread.
    _ui_thread: PhantomData<Rc<()>>,
}

unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    subclass: usize,
    cookie: usize,
) -> LRESULT {
    if message == MESSAGE {
        // A queued message for a destroyed/reused handle cannot wake a later
        // incarnation: the new subclass has a fresh, nonzero cookie.
        if wparam.0 == cookie
            && lparam.0 == 0
            && (IsIconic(window).as_bool() || !IsWindowVisible(window).as_bool())
        {
            let _ = ShowWindow(window, SW_SHOWNOACTIVATE);
        }
        return LRESULT(0);
    }
    if message == WM_NCDESTROY {
        let _ = RemoveWindowSubclass(window, Some(window_proc), subclass);
    }
    DefSubclassProc(window, message, wparam, lparam)
}

impl WindowWake {
    pub fn bind(context: &eframe::CreationContext<'_>) -> Result<Self, &'static str> {
        let RawWindowHandle::Win32(handle) = context
            .window_handle()
            .map_err(|_| "Invitation window handle unavailable")?
            .as_raw()
        else {
            return Err("Invitation window handle unavailable");
        };
        let window = HWND(handle.hwnd.get() as *mut _);
        let mut process = 0;
        let thread = unsafe { GetWindowThreadProcessId(window, Some(&mut process)) };
        if thread != unsafe { GetCurrentThreadId() } || process != unsafe { GetCurrentProcessId() }
        {
            return Err("Invitation window identity unavailable");
        }
        let mut random = [0u8; std::mem::size_of::<usize>()];
        getrandom::getrandom(&mut random).map_err(|_| "Invitation window nonce unavailable")?;
        let cookie = usize::from_ne_bytes(random).max(1);
        if !unsafe { SetWindowSubclass(window, Some(window_proc), SUBCLASS, cookie) }.as_bool() {
            return Err("Invitation window wake unavailable");
        }
        Ok(Self {
            state: Arc::new(State {
                window: handle.hwnd.get() as usize,
                cookie,
            }),
            _ui_thread: PhantomData,
        })
    }

    pub fn notifier(&self) -> impl Fn() + Send + Sync + 'static {
        let state = Arc::downgrade(&self.state);
        move || {
            if let Some(state) = state.upgrade() {
                // Posting is bounded and never waits for the render thread.
                // This callback runs only after authenticated, validated input
                // enters the UI queue. Merely attaching an idle inbox is silent.
                let _ = unsafe {
                    PostMessageW(
                        HWND(state.window as *mut _),
                        MESSAGE,
                        WPARAM(state.cookie),
                        LPARAM(0),
                    )
                };
            }
        }
    }
}
impl Drop for WindowWake {
    fn drop(&mut self) {
        let _ = unsafe {
            RemoveWindowSubclass(
                HWND(self.state.window as *mut _),
                Some(window_proc),
                SUBCLASS,
            )
        };
    }
}
