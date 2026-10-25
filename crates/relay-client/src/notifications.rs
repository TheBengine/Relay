//! Local native notifications. Tokens and notification contents are never persisted.
use crate::network::{CommandOrigin, Target};
use eframe::egui;
use std::{
    collections::{HashMap, VecDeque},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};
type ClickWake = Arc<Mutex<Option<Arc<dyn Fn() + Send + Sync>>>>;

/// RAM-only authority for one authenticated observer generation. A new login
/// must not revive an older queued notification or an older notification click.
#[derive(Clone, Debug)]
pub(crate) struct GenerationGuard(Arc<AtomicBool>);
impl GenerationGuard {
    pub fn new() -> Self {
        Self(Arc::new(AtomicBool::new(true)))
    }
    pub fn valid(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
    pub fn revoke(&self) {
        self.0.store(false, Ordering::Release);
    }
}
impl Default for GenerationGuard {
    fn default() -> Self {
        Self::new()
    }
}
impl PartialEq for GenerationGuard {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for GenerationGuard {}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, Default, PartialEq, Eq)]
pub(crate) struct NotificationPolicy {
    #[serde(default)]
    pub hide_muted_channels: bool,
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub muted_until: u64,
}
impl NotificationPolicy {
    pub fn allows(&self, mentioned: bool, direct: bool, default_mentions: bool, at: u64) -> bool {
        if self.muted_until > at {
            return false;
        }
        match self.mode.as_str() {
            "none" => false,
            "all" => true,
            "mentions" => mentioned || direct,
            _ => !default_mentions || mentioned || direct,
        }
    }
}
impl NotificationPolicy {
    /// A private group of friends notifies for every message unless the member
    /// chose otherwise: "none" and mute silence it, "mentions" needs a mention.
    pub fn allows_group(&self, mentioned: bool, at: u64) -> bool {
        if self.muted_until > at {
            return false;
        }
        match self.mode.as_str() {
            "none" => false,
            "mentions" => mentioned,
            _ => true,
        }
    }
}
/// The group's own choice, else the account's community choice.
pub(crate) fn group_policy_allows(
    policies: &HashMap<String, NotificationPolicy>,
    origin: &CommandOrigin,
    channel: &str,
    mentioned: bool,
    at: u64,
) -> bool {
    let scope = crate::conversation_state::ConversationMemory::scope(&origin.url, &origin.user_id);
    let fallback = NotificationPolicy::default();
    let community = policies
        .get(&format!("{scope}|community"))
        .unwrap_or(&fallback);
    if community.muted_until > at {
        return false;
    }
    policies
        .get(&format!("{scope}|{:?}", Target::Channel(channel.into())))
        .filter(|p| !p.mode.is_empty() || p.muted_until > at)
        .unwrap_or(community)
        .allows_group(mentioned, at)
}
pub(crate) fn direct_policy_allows(
    policies: &HashMap<String, NotificationPolicy>,
    origin: &CommandOrigin,
    peer: &str,
    default_mentions: bool,
    at: u64,
) -> bool {
    let scope = crate::conversation_state::ConversationMemory::scope(&origin.url, &origin.user_id);
    let fallback = NotificationPolicy::default();
    let community = policies
        .get(&format!("{scope}|community"))
        .unwrap_or(&fallback);
    if community.muted_until > at {
        return false;
    }
    let policy = policies
        .get(&format!("{scope}|{:?}", Target::Direct(peer.into())))
        .filter(|p| !p.mode.is_empty() || p.muted_until > at)
        .unwrap_or(community);
    policy.allows(false, true, default_mentions, at)
}

#[derive(Clone, Debug)]
pub struct Notification {
    pub title: String,
    pub body: String,
    pub origin: CommandOrigin,
    pub target: Target,
    pub(crate) guard: Option<GenerationGuard>,
    pub(crate) background_only: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotificationClick {
    pub origin: CommandOrigin,
    pub target: Target,
    pub(crate) guard: Option<GenerationGuard>,
}

#[derive(Default)]
struct AccountSources {
    ui: Vec<CommandOrigin>,
    calls: Vec<CommandOrigin>,
}

#[derive(Clone)]
pub struct Notifications {
    ctx: egui::Context,
    clicks: Arc<Mutex<VecDeque<NotificationClick>>>,
    errors: Arc<Mutex<VecDeque<String>>>,
    accounts: Arc<Mutex<Vec<CommandOrigin>>>,
    sources: Arc<Mutex<AccountSources>>,
    #[cfg(target_os = "windows")]
    worker: Arc<Mutex<Option<native::Worker>>>,
    click_wake: ClickWake,
}

impl Notifications {
    pub fn new(ctx: egui::Context) -> Self {
        Self {
            ctx,
            clicks: Arc::new(Mutex::new(VecDeque::new())),
            errors: Arc::new(Mutex::new(VecDeque::new())),
            accounts: Arc::new(Mutex::new(Vec::new())),
            sources: Arc::new(Mutex::new(AccountSources::default())),
            #[cfg(target_os = "windows")]
            worker: Arc::new(Mutex::new(None)),
            click_wake: Arc::new(Mutex::new(None)),
        }
    }
    pub fn set_accounts(&mut self, accounts: Vec<CommandOrigin>) {
        let mut sources = self.sources.lock().unwrap_or_else(|e| e.into_inner());
        sources.ui = accounts
            .into_iter()
            .filter(|origin| {
                !sources
                    .calls
                    .iter()
                    .any(|a| a.url == origin.url && a != origin)
            })
            .collect();
        self.publish_accounts(&sources);
    }
    fn publish_accounts(&self, sources: &AccountSources) {
        // An authenticated subscriber can advance before the UI drains Connected.
        // A stale painted account list must not revoke that exact live origin.
        let mut accounts = sources.calls.clone();
        for origin in &sources.ui {
            if !accounts.iter().any(|a| a.url == origin.url) {
                accounts.push(origin.clone());
            }
        }
        let mut previous = self.accounts.lock().unwrap_or_else(|e| e.into_inner());
        if *previous == accounts {
            return;
        }
        *previous = accounts.clone();
        drop(previous);
        self.clicks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|click| accounts.contains(&click.origin));
        #[cfg(target_os = "windows")]
        if let Some(worker) = &*self.worker.lock().unwrap_or_else(|e| e.into_inner()) {
            worker.wake();
        }
    }
    pub fn allow_account(&self, origin: CommandOrigin) {
        let mut sources = self.sources.lock().unwrap_or_else(|e| e.into_inner());
        sources
            .calls
            .retain(|a| a.url != origin.url || a == &origin);
        sources.ui.retain(|a| a.url != origin.url || a == &origin);
        if !sources.calls.contains(&origin) {
            sources.calls.push(origin);
        }
        self.publish_accounts(&sources);
    }
    pub fn remove_account(&self, origin: &CommandOrigin) {
        let mut sources = self.sources.lock().unwrap_or_else(|e| e.into_inner());
        sources.calls.retain(|a| a != origin);
        sources.ui.retain(|a| a != origin);
        self.publish_accounts(&sources);
    }
    pub fn set_click_wake(&self, wake: impl Fn() + Send + Sync + 'static) {
        *self.click_wake.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(wake));
    }
    pub fn show(&self, notification: Notification) -> Result<(), String> {
        let _ = crate::community::normalize_url(&notification.origin.url)?;
        if !guard_valid(notification.guard.as_ref())
            || !account_allowed(&self.accounts, &notification.origin)
        {
            return Err("The notification's account is no longer connected.".into());
        }
        #[cfg(target_os = "windows")]
        {
            let mut worker = self.worker.lock().unwrap_or_else(|e| e.into_inner());
            if worker.is_none() {
                let native = native::Worker::new(
                    self.ctx.clone(),
                    self.clicks.clone(),
                    self.errors.clone(),
                    self.accounts.clone(),
                )?;
                native.set_click_wake(self.click_wake.clone());
                *worker = Some(native);
            }
            worker
                .as_ref()
                .expect("notification worker initialized")
                .show(notification)
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = notification;
            Err("Native notifications are currently available on Windows.".into())
        }
    }
    pub(crate) fn refresh_guards(&self) {
        #[cfg(windows)]
        if let Some(worker) = &*self.worker.lock().unwrap_or_else(|e| e.into_inner()) {
            worker.wake();
        }
    }
    pub(crate) fn app_focused(&self) -> bool {
        #[cfg(windows)]
        {
            process_focused()
        }
        #[cfg(not(windows))]
        {
            self.ctx.input(|input| input.focused)
        }
    }
    pub fn clicked(&self) -> Vec<NotificationClick> {
        let accounts = self
            .accounts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        self.clicks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
            .filter(|click| guard_valid(click.guard.as_ref()) && accounts.contains(&click.origin))
            .collect()
    }
    pub fn take_error(&self) -> Option<String> {
        self.errors
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
    }

    // Only the ignored, explicitly owned native fixture opts in. Normal unit
    // tests retain their non-desktop sink; production has no such switch.
    #[cfg(all(test, windows))]
    pub(crate) fn enable_owned_qa_shell(&self) -> Result<(), String> {
        let mut worker = self.worker.lock().unwrap_or_else(|e| e.into_inner());
        if worker.is_none() {
            let native = native::Worker::new(
                self.ctx.clone(),
                self.clicks.clone(),
                self.errors.clone(),
                self.accounts.clone(),
            )?;
            native.set_click_wake(self.click_wake.clone());
            *worker = Some(native);
        }
        worker.as_ref().expect("owned worker").enable_owned_shell();
        Ok(())
    }

    #[cfg(all(test, windows))]
    pub(crate) fn owned_qa_shell_accepts(&self) -> usize {
        self.worker
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(native::Worker::owned_shell_accepts)
            .unwrap_or(0)
    }
}

#[cfg(windows)]
fn process_focused() -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};
    let mut process = 0;
    unsafe {
        let _ = GetWindowThreadProcessId(GetForegroundWindow(), Some(&mut process));
    }
    process == std::process::id()
}

fn guard_valid(guard: Option<&GenerationGuard>) -> bool {
    guard.is_none_or(GenerationGuard::valid)
}

fn account_allowed(accounts: &Mutex<Vec<CommandOrigin>>, origin: &CommandOrigin) -> bool {
    !origin.url.is_empty()
        && !origin.server_id.is_empty()
        && !origin.user_id.is_empty()
        && accounts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(origin)
}

fn copy_text<const N: usize>(buffer: &mut [u16; N], text: &str) {
    let mut at = 0;
    for character in text.chars().filter(|c| !c.is_control() || *c == '\n') {
        let mut units = [0; 2];
        let units = character.encode_utf16(&mut units);
        if at + units.len() >= N {
            break;
        }
        buffer[at..at + units.len()].copy_from_slice(units);
        at += units.len();
    }
    buffer[at] = 0;
}

#[cfg(target_os = "windows")]
mod native {
    use super::*;
    use std::{
        collections::HashMap,
        sync::{
            atomic::{AtomicBool, AtomicIsize, Ordering},
            mpsc,
        },
    };
    use windows::{
        core::PCWSTR,
        Win32::{
            Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM},
            System::LibraryLoader::GetModuleHandleW,
            UI::{Shell::*, WindowsAndMessaging::*},
        },
    };
    const WAKE: u32 = WM_APP + 31;
    const CLICK: u32 = WM_APP + 32;
    enum Command {
        Show(Notification),
        #[cfg(test)]
        Hold {
            entered: mpsc::Sender<()>,
            release: mpsc::Receiver<()>,
        },
    }
    pub(super) struct Worker {
        tx: mpsc::SyncSender<Command>,
        window: Arc<AtomicIsize>,
        stopping: Arc<AtomicBool>,
        click_wake: ClickWake,
        #[cfg(test)]
        test_output: Arc<Mutex<Vec<NotificationClick>>>,
        #[cfg(test)]
        test_real_shell: Arc<AtomicBool>,
    }
    impl Worker {
        #[cfg(test)]
        pub(super) fn enable_owned_shell(&self) {
            self.test_real_shell.store(true, Ordering::Release);
        }
        #[cfg(test)]
        pub(super) fn owned_shell_accepts(&self) -> usize {
            self.test_output
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .len()
        }
        pub(super) fn new(
            ctx: egui::Context,
            clicks: Arc<Mutex<VecDeque<NotificationClick>>>,
            errors: Arc<Mutex<VecDeque<String>>>,
            accounts: Arc<Mutex<Vec<CommandOrigin>>>,
        ) -> Result<Self, String> {
            let (tx, rx) = mpsc::sync_channel(8);
            let window = Arc::new(AtomicIsize::new(0));
            let worker_window = window.clone();
            let stopping = Arc::new(AtomicBool::new(false));
            let worker_stopping = stopping.clone();
            let click_wake = Arc::new(Mutex::new(None));
            let worker_click_wake = click_wake.clone();
            #[cfg(test)]
            let test_output = Arc::new(Mutex::new(Vec::new()));
            #[cfg(test)]
            let worker_output = test_output.clone();
            #[cfg(test)]
            let test_real_shell = Arc::new(AtomicBool::new(false));
            #[cfg(test)]
            let worker_real_shell = test_real_shell.clone();
            std::thread::Builder::new()
                .name("relay-notifications".into())
                .spawn(move || {
                    let mut state = Box::new(State {
                        rx,
                        ctx,
                        clicks,
                        errors,
                        icons: HashMap::new(),
                        next_id: 1,
                        window: worker_window,
                        accounts,
                        stopping: worker_stopping,
                        click_wake: worker_click_wake,
                        #[cfg(test)]
                        test_output: worker_output,
                        #[cfg(test)]
                        test_real_shell: worker_real_shell,
                    });
                    if let Err(error) = unsafe { run(&mut *state as *mut State) } {
                        state.error(error);
                    }
                })
                .map_err(|e| format!("Cannot start notifications: {e}"))?;
            Ok(Self {
                tx,
                window,
                stopping,
                click_wake,
                #[cfg(test)]
                test_output,
                #[cfg(test)]
                test_real_shell,
            })
        }
        pub(super) fn show(&self, notification: Notification) -> Result<(), String> {
            self.tx
                .try_send(Command::Show(notification))
                .map_err(|_| "Notification queue is busy.".to_string())?;
            self.wake();
            Ok(())
        }
        pub(super) fn set_click_wake(&self, wake: ClickWake) {
            *self.click_wake.lock().unwrap_or_else(|e| e.into_inner()) =
                Some(Arc::new(move || {
                    if let Some(callback) = wake.lock().unwrap_or_else(|e| e.into_inner()).as_ref()
                    {
                        callback();
                    }
                }));
        }
        pub(super) fn wake(&self) {
            let window = self.window.load(Ordering::SeqCst);
            if window != 0 {
                unsafe {
                    let _ = PostMessageW(HWND(window as *mut _), WAKE, WPARAM(0), LPARAM(0));
                }
            }
        }
    }
    impl Drop for Worker {
        fn drop(&mut self) {
            // Shutdown must not depend on free space in the bounded show queue.
            self.stopping.store(true, Ordering::SeqCst);
            self.wake();
        }
    }
    struct State {
        rx: mpsc::Receiver<Command>,
        ctx: egui::Context,
        clicks: Arc<Mutex<VecDeque<NotificationClick>>>,
        errors: Arc<Mutex<VecDeque<String>>>,
        icons: HashMap<u32, NotificationClick>,
        next_id: u32,
        window: Arc<AtomicIsize>,
        accounts: Arc<Mutex<Vec<CommandOrigin>>>,
        stopping: Arc<AtomicBool>,
        click_wake: ClickWake,
        #[cfg(test)]
        test_output: Arc<Mutex<Vec<NotificationClick>>>,
        #[cfg(test)]
        test_real_shell: Arc<AtomicBool>,
    }
    impl State {
        fn error(&self, error: String) {
            let mut errors = self.errors.lock().unwrap_or_else(|e| e.into_inner());
            if errors.len() < 4 {
                errors.push_back(error);
            }
            self.ctx.request_repaint();
        }
        unsafe fn remove(&mut self, hwnd: HWND, id: u32) {
            #[cfg(test)]
            if !self.test_real_shell.load(Ordering::Acquire) {
                self.icons.remove(&id);
                return;
            }
            {
                let data = NOTIFYICONDATAW {
                    cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                    hWnd: hwnd,
                    uID: id,
                    ..Default::default()
                };
                let _ = Shell_NotifyIconW(NIM_DELETE, &data);
            }
            self.icons.remove(&id);
        }
        unsafe fn add_icon(&self, hwnd: HWND, id: u32, notification: &Notification) -> bool {
            #[cfg(test)]
            if !self.test_real_shell.load(Ordering::Acquire) {
                // Real owned message-window/queue tests must never display a
                // desktop popup. This sink is not evidence of Shell UI output.
                let _ = (hwnd, id, &notification.title, &notification.body);
                self.test_output.lock().unwrap().push(NotificationClick {
                    origin: notification.origin.clone(),
                    target: notification.target.clone(),
                    guard: notification.guard.clone(),
                });
                return true;
            }
            {
                let mut data = NOTIFYICONDATAW {
                    cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                    hWnd: hwnd,
                    uID: id,
                    uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_INFO,
                    uCallbackMessage: CLICK,
                    hIcon: LoadIconW(None, IDI_APPLICATION).unwrap_or_default(),
                    dwInfoFlags: NIIF_INFO | NIIF_NOSOUND,
                    ..Default::default()
                };
                copy_text(&mut data.szTip, "Relay");
                copy_text(&mut data.szInfoTitle, &notification.title);
                copy_text(&mut data.szInfo, &notification.body);
                let added = Shell_NotifyIconW(NIM_ADD, &data).as_bool();
                #[cfg(test)]
                if added {
                    self.test_output
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(NotificationClick {
                            origin: notification.origin.clone(),
                            target: notification.target.clone(),
                            guard: notification.guard.clone(),
                        });
                }
                added
            }
        }
        unsafe fn drain(&mut self, hwnd: HWND) -> bool {
            let stale: Vec<_> = self
                .icons
                .iter()
                .filter(|(_, click)| {
                    !guard_valid(click.guard.as_ref())
                        || !account_allowed(&self.accounts, &click.origin)
                })
                .map(|(id, _)| *id)
                .collect();
            for id in stale {
                self.remove(hwnd, id);
            }
            loop {
                if self.stopping.load(Ordering::SeqCst) {
                    return true;
                }
                match self.rx.try_recv() {
                    Ok(Command::Show(notification)) => {
                        if !guard_valid(notification.guard.as_ref())
                            || !account_allowed(&self.accounts, &notification.origin)
                            || notification.background_only && process_focused()
                        {
                            continue;
                        }
                        if self.icons.len() >= 8 {
                            if let Some(id) = self.icons.keys().copied().min() {
                                self.remove(hwnd, id);
                            }
                        }
                        let id = self.next_id;
                        self.next_id = self.next_id.wrapping_add(1).max(1);
                        if self.add_icon(hwnd, id, &notification) {
                            if !guard_valid(notification.guard.as_ref())
                                || !account_allowed(&self.accounts, &notification.origin)
                            {
                                self.remove(hwnd, id);
                                continue;
                            }
                            self.icons.insert(
                                id,
                                NotificationClick {
                                    origin: notification.origin,
                                    target: notification.target,
                                    guard: notification.guard,
                                },
                            );
                        } else {
                            self.error("Windows could not show this notification.".into());
                        }
                    }
                    #[cfg(test)]
                    Ok(Command::Hold { entered, release }) => {
                        let _ = entered.send(());
                        if release
                            .recv_timeout(std::time::Duration::from_secs(3))
                            .is_err()
                        {
                            self.error("Native notification test barrier expired.".into());
                            return true;
                        }
                    }
                    Err(mpsc::TryRecvError::Disconnected) => return true,
                    Err(mpsc::TryRecvError::Empty) => return false,
                }
            }
        }
    }
    unsafe extern "system" fn window_proc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if message == WM_NCCREATE {
            let create = &*(lparam.0 as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
        }
        let pointer = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut State;
        if !pointer.is_null() {
            match message {
                WAKE => {
                    // DestroyWindow synchronously invokes this procedure again.
                    // End the mutable state borrow before entering that call.
                    let stop = (&mut *pointer).drain(hwnd);
                    if stop {
                        let _ = DestroyWindow(hwnd);
                    }
                    return LRESULT(0);
                }
                CLICK => {
                    let state = &mut *pointer;
                    let id = wparam.0 as u32;
                    if lparam.0 as u32 == NIN_BALLOONUSERCLICK {
                        if let Some(click) = state.icons.get(&id).cloned() {
                            if !guard_valid(click.guard.as_ref())
                                || !account_allowed(&state.accounts, &click.origin)
                            {
                                state.remove(hwnd, id);
                                return LRESULT(0);
                            }
                            let mut clicks = state.clicks.lock().unwrap_or_else(|e| e.into_inner());
                            if clicks.len() < 8 {
                                clicks.push_back(click);
                            }
                            drop(clicks);
                            // A click is explicit user intent. Arrival itself
                            // never shows or focuses the hidden Relay window.
                            if let Some(wake) = state
                                .click_wake
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .as_ref()
                            {
                                wake();
                            }
                            state.ctx.request_repaint();
                        }
                        state.remove(hwnd, id);
                    } else if lparam.0 as u32 == NIN_BALLOONTIMEOUT {
                        state.remove(hwnd, id);
                    }
                    return LRESULT(0);
                }
                WM_DESTROY => {
                    let state = &mut *pointer;
                    let ids: Vec<_> = state.icons.keys().copied().collect();
                    for id in ids {
                        state.remove(hwnd, id);
                    }
                    state.window.store(0, Ordering::SeqCst);
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                    PostQuitMessage(0);
                    return LRESULT(0);
                }
                _ => {}
            }
        }
        DefWindowProcW(hwnd, message, wparam, lparam)
    }
    unsafe fn run(state: *mut State) -> Result<(), String> {
        let instance = HINSTANCE(GetModuleHandleW(None).map_err(|e| e.to_string())?.0);
        let name: Vec<_> = format!("RelayNotifications-{}", uuid::Uuid::new_v4())
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            lpszClassName: PCWSTR(name.as_ptr()),
            ..Default::default()
        };
        if RegisterClassW(&class) == 0 {
            return Err("Cannot register the notification window.".into());
        }
        let hwnd = match CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            PCWSTR(name.as_ptr()),
            PCWSTR::null(),
            WINDOW_STYLE::default(),
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            None,
            instance,
            Some(state.cast_const().cast()),
        ) {
            Ok(hwnd) => hwnd,
            Err(error) => {
                let _ = UnregisterClassW(PCWSTR(name.as_ptr()), instance);
                return Err(error.to_string());
            }
        };
        (*state).window.store(hwnd.0 as isize, Ordering::SeqCst);
        // Dispatch all state access through the window procedure, without a
        // Rust reference held across reentrant Windows calls.
        let _ = PostMessageW(hwnd, WAKE, WPARAM(0), LPARAM(0));
        let mut message = MSG::default();
        loop {
            let result = GetMessageW(&mut message, None, 0, 0).0;
            if result <= 0 {
                break;
            }
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        let _ = UnregisterClassW(PCWSTR(name.as_ptr()), instance);
        Ok(())
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        fn owner(user: &str) -> CommandOrigin {
            CommandOrigin {
                url: "http://127.0.0.1:1".into(),
                server_id: "east".into(),
                user_id: user.into(),
            }
        }
        fn notification(origin: CommandOrigin) -> Notification {
            Notification {
                title: "Synthetic test".into(),
                body: "No desktop output".into(),
                origin,
                target: Target::Channel("shared".into()),
                guard: None,
                background_only: false,
            }
        }
        fn hold(worker: &Worker) -> mpsc::Sender<()> {
            let (entered, observed) = mpsc::channel();
            let (release, waiting) = mpsc::channel();
            worker
                .tx
                .send(Command::Hold {
                    entered,
                    release: waiting,
                })
                .unwrap();
            worker.wake();
            observed
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap();
            assert_ne!(worker.window.load(Ordering::SeqCst), 0);
            release
        }
        fn dispatch(worker: &Worker, message: u32, id: u32, event: u32) {
            let hwnd = HWND(worker.window.load(Ordering::SeqCst) as *mut _);
            assert_ne!(hwnd.0, std::ptr::null_mut());
            unsafe {
                SendMessageW(hwnd, message, WPARAM(id as usize), LPARAM(event as isize));
            }
        }
        fn closed(window: &AtomicIsize) {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while window.load(Ordering::SeqCst) != 0 && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            assert_eq!(
                window.load(Ordering::SeqCst),
                0,
                "owned native message window is closed"
            );
        }
        #[test]
        fn native_call_alert_arrival_stays_hidden_and_only_a_current_click_wakes() {
            let origin = owner("caller-test");
            let accounts = Arc::new(Mutex::new(vec![origin.clone()]));
            let clicks = Arc::new(Mutex::new(VecDeque::new()));
            let worker = Worker::new(
                egui::Context::default(),
                clicks.clone(),
                Arc::new(Mutex::new(VecDeque::new())),
                accounts.clone(),
            )
            .unwrap();
            let wakes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let observed = wakes.clone();
            let wake: ClickWake = Arc::new(Mutex::new(Some(Arc::new(move || {
                observed.fetch_add(1, Ordering::SeqCst);
            }))));
            worker.set_click_wake(wake);
            let release = hold(&worker);
            worker.show(notification(origin.clone())).unwrap();
            release.send(()).unwrap();
            dispatch(&worker, WAKE, 0, 0);
            assert_eq!(worker.test_output.lock().unwrap().len(), 1);
            assert_eq!(
                wakes.load(Ordering::SeqCst),
                0,
                "arrival must not restore the main window"
            );
            dispatch(&worker, CLICK, 1, NIN_BALLOONUSERCLICK);
            assert_eq!(wakes.load(Ordering::SeqCst), 1);
            assert_eq!(clicks.lock().unwrap().pop_front().unwrap().origin, origin);
            worker.show(notification(origin)).unwrap();
            dispatch(&worker, WAKE, 0, 0);
            accounts.lock().unwrap().clear();
            dispatch(&worker, CLICK, 2, NIN_BALLOONUSERCLICK);
            assert_eq!(
                wakes.load(Ordering::SeqCst),
                1,
                "revoked identity must not restore the main window"
            );
            assert!(clicks.lock().unwrap().is_empty());
            let window = worker.window.clone();
            drop(worker);
            closed(&window);
        }
        #[test]
        fn native_message_generation_cannot_revive_a_queued_toast_or_click_after_same_origin_login()
        {
            let origin = owner("same-account");
            let accounts = Arc::new(Mutex::new(vec![origin.clone()]));
            let clicks = Arc::new(Mutex::new(VecDeque::new()));
            let worker = Worker::new(
                egui::Context::default(),
                clicks.clone(),
                Arc::new(Mutex::new(VecDeque::new())),
                accounts,
            )
            .unwrap();
            let old = GenerationGuard::new();
            let release = hold(&worker);
            let mut queued = notification(origin.clone());
            queued.guard = Some(old.clone());
            worker.show(queued).unwrap();
            old.revoke(); // Account identity stays equal, its login generation does not.
            let current = GenerationGuard::new();
            release.send(()).unwrap();
            dispatch(&worker, WAKE, 0, 0);
            assert!(worker.test_output.lock().unwrap().is_empty());
            let mut fresh = notification(origin);
            fresh.guard = Some(current.clone());
            worker.show(fresh).unwrap();
            dispatch(&worker, WAKE, 0, 0);
            assert_eq!(worker.test_output.lock().unwrap().len(), 1);
            current.revoke();
            dispatch(&worker, CLICK, 1, NIN_BALLOONUSERCLICK);
            assert!(clicks.lock().unwrap().is_empty());
            let window = worker.window.clone();
            drop(worker);
            closed(&window);
        }
        #[test]
        fn native_queued_old_account_is_cancelled_and_current_click_keeps_identity() {
            let old = owner("alex");
            let new = owner("mira");
            let west = CommandOrigin {
                url: "http://127.0.0.1:2".into(),
                server_id: "west".into(),
                user_id: "jonas".into(),
            };
            let accounts = Arc::new(Mutex::new(vec![old.clone(), west.clone()]));
            let clicks = Arc::new(Mutex::new(VecDeque::new()));
            let errors = Arc::new(Mutex::new(VecDeque::new()));
            let worker = Worker::new(
                egui::Context::default(),
                clicks.clone(),
                errors.clone(),
                accounts.clone(),
            )
            .unwrap();
            let release = hold(&worker);
            worker.show(notification(old)).unwrap();
            *accounts.lock().unwrap() = vec![new.clone(), west.clone()];
            release.send(()).unwrap();
            dispatch(&worker, WAKE, 0, 0);
            assert!(
                worker.test_output.lock().unwrap().is_empty(),
                "cancelled show never reaches the test output sink"
            );
            worker.show(notification(west.clone())).unwrap();
            worker.show(notification(new.clone())).unwrap();
            dispatch(&worker, WAKE, 0, 0);
            assert_eq!(
                worker
                    .test_output
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|c| c.origin.clone())
                    .collect::<Vec<_>>(),
                [west.clone(), new.clone()]
            );
            // IDs are allocated only for permitted shows. Dispatch the actual
            // owned window procedure; no mouse click or Shell popup is used.
            dispatch(&worker, CLICK, 2, NIN_BALLOONUSERCLICK);
            assert_eq!(clicks.lock().unwrap().pop_front().unwrap().origin, new);
            *accounts.lock().unwrap() = vec![owner("mira")];
            dispatch(&worker, CLICK, 1, NIN_BALLOONUSERCLICK);
            assert!(
                clicks.lock().unwrap().is_empty(),
                "revoked background icon cannot enqueue a click"
            );
            let window = worker.window.clone();
            drop(worker);
            closed(&window);
            assert!(errors.lock().unwrap().is_empty());
        }
        #[test]
        fn native_full_show_queue_cannot_prevent_shutdown() {
            let old = owner("alex");
            let errors = Arc::new(Mutex::new(VecDeque::new()));
            let worker = Worker::new(
                egui::Context::default(),
                Arc::new(Mutex::new(VecDeque::new())),
                errors.clone(),
                Arc::new(Mutex::new(vec![old.clone()])),
            )
            .unwrap();
            let release = hold(&worker);
            for _ in 0..8 {
                worker.show(notification(old.clone())).unwrap();
            }
            assert!(
                worker.show(notification(old)).is_err(),
                "the bounded queue really is full"
            );
            let window = worker.window.clone();
            let output = worker.test_output.clone();
            drop(worker);
            release.send(()).unwrap();
            closed(&window);
            assert!(
                output.lock().unwrap().is_empty(),
                "shutdown does not dispatch pending shows"
            );
            assert!(errors.lock().unwrap().is_empty());
        }
        #[test]
        fn native_message_window_closes_without_showing_any_notification() {
            let errors = Arc::new(Mutex::new(VecDeque::new()));
            let worker = Worker::new(
                egui::Context::default(),
                Arc::new(Mutex::new(VecDeque::new())),
                errors.clone(),
                Arc::new(Mutex::new(Vec::new())),
            )
            .unwrap();
            let window = worker.window.clone();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while window.load(Ordering::SeqCst) == 0 && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            assert_ne!(
                window.load(Ordering::SeqCst),
                0,
                "owned message-only window was created"
            );
            drop(worker);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while window.load(Ordering::SeqCst) != 0 && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            assert_eq!(
                window.load(Ordering::SeqCst),
                0,
                "reentrant destruction completed"
            );
            assert!(errors.lock().unwrap().is_empty());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn owner(user: &str, server: &str, url: &str) -> CommandOrigin {
        CommandOrigin {
            url: url.into(),
            server_id: server.into(),
            user_id: user.into(),
        }
    }
    fn click(origin: CommandOrigin) -> NotificationClick {
        NotificationClick {
            origin,
            target: Target::Channel("shared".into()),
            guard: None,
        }
    }
    #[test]
    fn message_click_queued_before_logout_is_rejected_after_same_origin_relogin() {
        let origin = owner("alex", "friends", "https://owned.invalid/people");
        let notifications = Notifications::new(egui::Context::default());
        notifications.allow_account(origin.clone());
        let old = GenerationGuard::new();
        let mut queued = click(origin.clone());
        queued.target = Target::Direct("friend".into());
        queued.guard = Some(old.clone());
        notifications.clicks.lock().unwrap().push_back(queued);
        old.revoke();
        notifications.remove_account(&origin);
        notifications.allow_account(origin.clone());
        assert!(notifications.clicked().is_empty());
        let current = GenerationGuard::new();
        let mut fresh = click(origin);
        fresh.target = Target::Direct("friend".into());
        fresh.guard = Some(current);
        notifications
            .clicks
            .lock()
            .unwrap()
            .push_back(fresh.clone());
        assert_eq!(notifications.clicked(), [fresh]);
    }
    #[test]
    fn live_call_origin_survives_stale_ui_snapshot_and_revocation_removes_it() {
        let old = owner("alex", "east", "http://127.0.0.1:1");
        let current = owner("mira", "east", "http://127.0.0.1:1");
        let mut notifications = Notifications::new(egui::Context::default());
        notifications.set_accounts(vec![old.clone()]);
        notifications.allow_account(current.clone());
        notifications.set_accounts(vec![old.clone()]);
        assert!(account_allowed(&notifications.accounts, &current));
        assert!(!account_allowed(&notifications.accounts, &old));
        notifications
            .clicks
            .lock()
            .unwrap()
            .extend([click(old.clone()), click(current.clone())]);
        assert_eq!(notifications.clicked(), [click(current.clone())]);
        notifications.remove_account(&current);
        assert!(!account_allowed(&notifications.accounts, &current));
        assert!(!account_allowed(&notifications.accounts, &old));
        notifications
            .clicks
            .lock()
            .unwrap()
            .push_back(click(current));
        assert!(notifications.clicked().is_empty());
    }
    #[test]
    fn queued_clicks_are_cancelled_after_same_address_account_replacement() {
        let old = owner("alex", "east", "http://127.0.0.1:1");
        let current = owner("mira", "east", "http://127.0.0.1:1");
        let west = owner("jonas", "west", "http://127.0.0.1:2");
        let mut notifications = Notifications::new(egui::Context::default());
        notifications.set_accounts(vec![old.clone(), west.clone()]);
        notifications
            .clicks
            .lock()
            .unwrap()
            .extend([click(old.clone()), click(west.clone())]);
        notifications.set_accounts(vec![current.clone(), west.clone()]);
        notifications
            .clicks
            .lock()
            .unwrap()
            .extend([click(old), click(current.clone())]);
        assert_eq!(notifications.clicked(), [click(west), click(current)]);
        assert!(notifications.clicked().is_empty());
    }
    #[test]
    fn queued_clicks_reject_server_replacement_unknown_owner_and_logout() {
        let old = owner("alex", "old-server", "http://127.0.0.1:1");
        let current = owner("alex", "new-server", "http://127.0.0.1:1");
        let unknown = owner("alex", "new-server", "http://127.0.0.1:2");
        let mut notifications = Notifications::new(egui::Context::default());
        notifications.set_accounts(vec![old.clone()]);
        notifications
            .clicks
            .lock()
            .unwrap()
            .push_back(click(old.clone()));
        notifications.set_accounts(vec![current.clone()]);
        notifications.clicks.lock().unwrap().extend([
            click(old),
            click(unknown),
            click(current.clone()),
        ]);
        assert_eq!(notifications.clicked(), [click(current.clone())]);
        notifications.set_accounts(Vec::new());
        notifications
            .clicks
            .lock()
            .unwrap()
            .push_back(click(current));
        assert!(notifications.clicked().is_empty());
    }
    #[test]
    fn notification_text_is_bounded_null_terminated_and_keeps_valid_unicode() {
        let mut buffer = [0u16; 6];
        copy_text(&mut buffer, "A😀B😀C\0");
        assert_eq!(String::from_utf16(&buffer[..4]).unwrap(), "A😀B");
        assert_eq!(buffer[4], 0);
        let mut controls = [0; 8];
        copy_text(&mut controls, "x\0\u{7}y");
        assert_eq!(controls[..3], [120, 121, 0]);
    }
    #[test]
    fn notification_clicks_keep_community_and_conversation_scope() {
        let first = NotificationClick {
            origin: CommandOrigin {
                url: "https://one.example".into(),
                server_id: "one".into(),
                user_id: "alex".into(),
            },
            target: Target::Direct("same-id".into()),
            guard: None,
        };
        let second = NotificationClick {
            origin: CommandOrigin {
                url: "https://two.example".into(),
                server_id: "two".into(),
                user_id: "alex".into(),
            },
            target: Target::Direct("same-id".into()),
            guard: None,
        };
        assert_ne!(first, second);
    }
}
