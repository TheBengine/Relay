use eframe::egui;
use relay_updater::{
    Check, InstalledRelease, Outcome, Poll, PreparedInstaller, Publisher, Release, Worker,
};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

#[cfg(windows)]
mod handoff;
mod release_notice;

const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
const RELEASES_URL: &str = "https://github.com/TheBengine/Relay/releases";

enum State {
    Disabled,
    Unavailable,
    Idle,
    Checking,
    Current,
    Available(Box<Release>),
    Downloading,
    Cancelling,
    Ready(Box<PreparedInstaller>),
    Failed,
}

pub(crate) struct Controller {
    state: State,
    worker: Option<Worker>,
    retry_at: Option<Instant>,
    #[cfg(windows)]
    install: Option<handoff::Job>,
    #[cfg(windows)]
    waiting: Option<relay_updater::handoff::WaitingInstaller>,
    install_error: Option<String>,
    // One explicit click approves only these verified release contents. A
    // cancelled/busy operation never resumes installation on its own.
    install_consent: Option<Box<Release>>,
    download_release: Option<Box<Release>>,
    // GitHub notices never grant download verification or installation permission.
    notices_enabled: bool,
    notice_job: Option<release_notice::Worker>,
    notice: Option<release_notice::Notice>,
    notice_checked: bool,
    notice_failed: bool,
    next_check: Instant,
    banner_hidden: bool,
    last_available: Option<String>,
}

impl Controller {
    pub fn new(ctx: &egui::Context) -> Self {
        let mut controller = Self::from_policy(
            relay_updater::bundled_publisher(),
            InstalledRelease {
                version: env!("CARGO_PKG_VERSION").into(),
                sequence: option_env!("RELAY_BUILD_SEQUENCE").unwrap_or("").into(),
            },
            update_directory,
            ctx,
        );
        // Only the intentionally unconfigured preview gets the informational fallback.
        // A broken configured publisher must not silently fall back to another trust path.
        controller.notices_enabled = cfg!(all(target_os = "windows", target_arch = "x86_64"))
            && matches!(controller.state, State::Disabled);
        controller
    }

    fn from_policy(
        publisher: Result<Option<Publisher>, String>,
        installed: InstalledRelease,
        directory: impl FnOnce() -> Result<PathBuf, String>,
        _ctx: &egui::Context,
    ) -> Self {
        // No root means no verified-download storage, runtime or installer worker.
        // The separate public release notice is enabled only by new(), above.
        let publisher = match publisher {
            Ok(Some(publisher)) => publisher,
            Ok(None) => return Self::inactive(State::Disabled),
            Err(_) => return Self::inactive(State::Unavailable),
        };
        let engine =
            directory().and_then(|path| relay_updater::Engine::new(publisher, installed, path));
        let Ok(engine) = engine else {
            return Self::inactive(State::Unavailable);
        };
        Self {
            state: State::Idle,
            worker: Some(Worker::new(engine)),
            retry_at: None,
            #[cfg(windows)]
            install: None,
            #[cfg(windows)]
            waiting: None,
            install_error: None,
            install_consent: None,
            download_release: None,
            notices_enabled: false,
            notice_job: None,
            notice: None,
            notice_checked: false,
            notice_failed: false,
            next_check: Instant::now() + Duration::from_secs(30),
            banner_hidden: false,
            last_available: None,
        }
    }

    fn inactive(state: State) -> Self {
        Self {
            state,
            worker: None,
            retry_at: None,
            #[cfg(windows)]
            install: None,
            #[cfg(windows)]
            waiting: None,
            install_error: None,
            install_consent: None,
            download_release: None,
            notices_enabled: false,
            notice_job: None,
            notice: None,
            notice_checked: false,
            notice_failed: false,
            next_check: Instant::now() + Duration::from_secs(30),
            banner_hidden: false,
            last_available: None,
        }
    }

    fn check(&mut self, ctx: &egui::Context) {
        if self
            .retry_at
            .is_some_and(|deadline| Instant::now() < deadline)
        {
            return;
        }
        if self.notices_enabled {
            if self.notice_job.is_some() {
                return;
            }
            let wake = ctx.clone();
            self.notice_failed = false;
            self.next_check = Instant::now() + CHECK_INTERVAL;
            match release_notice::Worker::start(
                env!("CARGO_PKG_VERSION").into(),
                env!("RELAY_BUILD_SEQUENCE").into(),
                true, // This distribution currently publishes Windows previews.
                move || wake.request_repaint(),
            ) {
                Ok(job) => self.notice_job = Some(job),
                Err(_) => {
                    self.notice_failed = true;
                    self.retry_at = Some(Instant::now() + Duration::from_secs(30));
                }
            }
            return;
        }
        let Some(worker) = self.worker.as_mut() else {
            return;
        };
        if worker.running() {
            return;
        }
        let wake = ctx.clone();
        if worker.check(move || wake.request_repaint()).is_ok() {
            self.state = State::Checking;
            self.next_check = Instant::now() + CHECK_INTERVAL;
        } else {
            self.failed();
        }
    }

    fn download(&mut self, ctx: &egui::Context, restart: bool) {
        let State::Available(release) = &self.state else {
            return;
        };
        let approved = (**release).clone();
        self.install_consent = (restart && in_place_update(&approved))
            .then(|| Box::new(approved.clone()));
        self.install_error = None;
        self.banner_hidden = false;
        let Some(worker) = self.worker.as_mut() else {
            self.install_consent = None;
            return;
        };
        let wake = ctx.clone();
        self.download_release = Some(Box::new(approved.clone()));
        if worker
            .download(approved, move || wake.request_repaint())
            .is_ok()
        {
            self.state = State::Downloading;
        } else {
            self.failed();
        }
    }

    fn failed(&mut self) {
        self.install_consent = None;
        self.download_release = None;
        self.state = State::Failed;
        self.retry_at = Some(Instant::now() + Duration::from_secs(10));
    }

    fn apply(&mut self, result: Result<Outcome, String>) {
        if matches!(self.state, State::Cancelling) {
            self.install_consent = None;
            self.state = self.download_release.take().map_or(State::Idle, State::Available);
            self.retry_at = None;
            return;
        }
        match result {
            Ok(Outcome::Checked(Check::Current)) => self.state = State::Current,
            Ok(Outcome::Checked(Check::Available(release))) => {
                if self.last_available.as_ref() != Some(&release.sequence) {
                    self.banner_hidden = false;
                }
                self.last_available = Some(release.sequence.clone());
                self.state = State::Available(release)
            }
            Ok(Outcome::Downloaded(ready)) => {
                self.download_release = None;
                if !consent_matches(self.install_consent.as_deref(), ready.release()) {
                    self.install_consent = None;
                }
                self.state = State::Ready(ready);
            }
            Ok(Outcome::Cancelled) => {
                self.install_consent = None;
                self.state = State::Idle;
            }
            Err(_) => {
                self.failed();
                return;
            }
        }
        self.retry_at = None;
    }

    pub(crate) fn preparing_install(&self) -> bool {
        #[cfg(windows)]
        {
            self.install.is_some() || self.waiting.is_some()
        }
        #[cfg(not(windows))]
        {
            false
        }
    }

    fn consent_allowed(&self, in_call: bool, hosting: bool, busy: bool) -> bool {
        self.install_consent.as_deref().is_some_and(|release| {
            install_allowed(in_call, hosting, busy, in_place_update(release))
                && match &self.state {
                    State::Ready(ready) => consent_matches(Some(release), ready.release()),
                    State::Downloading => in_place_update(release),
                    _ => false,
                }
        })
    }

    fn cancel_download(&mut self) {
        self.install_consent = None;
        if let Some(worker) = self.worker.as_mut() { worker.cancel(); }
        self.state = State::Cancelling;
    }

    pub fn tick(
        &mut self,
        ctx: &egui::Context,
        in_call: bool,
        hosting: bool,
        busy: bool,
        automatic_checks: bool,
        #[allow(unused_mut)] mut shutdown: impl FnMut() -> crate::network::ShutdownPoll,
    ) -> bool {
        if let Some(job) = self.notice_job.as_mut() {
            match job.poll() {
                release_notice::Poll::Finished(result) => {
                    self.notice_job = None;
                    self.notice_checked = true;
                    self.notice_failed = result.is_err();
                    if let Ok(notice) = result {
                        if self.notice.as_ref().map(|n| &n.tag) != notice.as_ref().map(|n| &n.tag) {
                            self.banner_hidden = false;
                        }
                        self.notice = notice;
                        self.retry_at = None;
                    } else {
                        self.retry_at = Some(Instant::now() + Duration::from_secs(30));
                    }
                }
                release_notice::Poll::Running => {}
            }
        }
        // A sleeping app needs only one future wakeup, not a status polling loop.
        if automatic_checks && (self.worker.is_some() || self.notices_enabled) {
            let now = Instant::now();
            if now >= self.next_check {
                if in_call
                    || self.preparing_install()
                    || matches!(
                        self.state,
                        State::Downloading | State::Cancelling | State::Ready(_)
                    )
                {
                    self.next_check = now + Duration::from_secs(60);
                } else {
                    self.next_check = now + CHECK_INTERVAL;
                    self.check(ctx);
                }
            }
            ctx.request_repaint_after(self.next_check.saturating_duration_since(now));
        }
        if self.install_consent.is_some() && !self.consent_allowed(in_call, hosting, busy) {
            self.install_consent = None;
            self.install_error = Some("Update paused because a call or another operation started. Relay stays open; choose Update and restart when you're ready.".into());
            self.banner_hidden = false;
            #[cfg(windows)]
            self.cancel_install();
            if matches!(self.state, State::Downloading) { self.cancel_download(); }
        }
        if in_call && matches!(self.state, State::Downloading) {
            self.cancel_download();
        }
        let result = self.worker.as_mut().map(Worker::poll);
        match result {
            Some(Poll::Finished(result)) => self.apply(result),
            Some(Poll::Completing) => ctx.request_repaint_after(Duration::from_millis(5)),
            // Running jobs wake the UI when their output is ready; no status polling timer.
            _ => {}
        }
        #[cfg(windows)]
        if matches!(&self.state, State::Ready(ready) if in_place_update(ready.release()))
            && self.consent_allowed(in_call, hosting, busy) && !self.preparing_install()
        {
            self.begin_install(ctx);
        }
        #[cfg(windows)]
        let consent_allowed = self.consent_allowed(in_call, hosting, busy);
        #[cfg(windows)]
        if let Some(job) = self.install.as_mut() {
            if !consent_allowed {
                job.cancel();
            }
            match job.poll() {
                handoff::Poll::Finished(result) => {
                    self.install = None;
                    match result {
                        Ok(waiting) if consent_allowed => {
                            self.waiting = Some(waiting);
                            ctx.request_repaint();
                        }
                        Ok(_) => self.install_error = Some("Installation cancelled. Relay was left open.".into()),
                        Err(error) => {
                            self.install_consent = None;
                            self.install_error = Some(error);
                        }
                    }
                }
                handoff::Poll::Completing => ctx.request_repaint_after(Duration::from_millis(5)),
                handoff::Poll::Running => {}
            }
        }
        #[cfg(windows)]
        if self.waiting.is_some() {
            if !consent_allowed {
                self.waiting = None; // Dropping the approval cancels the installer.
                self.install_error = Some(
                    "Installation stopped because another operation started. Relay was left open."
                        .into(),
                );
            } else {
                match shutdown() {
                    crate::network::ShutdownPoll::Pending(remaining) => {
                        ctx.request_repaint_after(remaining.min(Duration::from_millis(100)))
                    }
                    crate::network::ShutdownPoll::Finished(result) => {
                        let waiting = self.waiting.take().unwrap();
                        match result.and_then(|_| waiting.commit()) {
                            Ok(()) => {
                                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                                return true;
                            }
                            Err(error) => {
                                self.install_consent = None;
                                self.install_error = Some(error);
                            }
                        }
                    }
                }
            }
        }
        #[cfg(not(windows))]
        let _ = shutdown;
        false
    }

    /// Shared by the signed-out screens and the main app. Nothing downloads or
    /// closes Relay until the user explicitly chooses the corresponding action.
    pub fn show_banner(&mut self, ctx: &egui::Context, in_call: bool, hosting: bool, busy: bool) {
        let visible = self.notice.is_some()
            || matches!(
                self.state,
                State::Available(_) | State::Downloading | State::Cancelling | State::Ready(_)
            )
            || self.install_error.is_some()
            || (matches!(self.state, State::Failed) && self.last_available.is_some());
        if !visible || self.banner_hidden {
            return;
        }
        egui::TopBottomPanel::top("relay-update-banner")
            .frame(
                egui::Frame::none()
                    .fill(ctx.style().visuals.faint_bg_color)
                    .inner_margin(egui::Margin::symmetric(12.0, 8.0)),
            )
            .show(ctx, |ui| {
                ui.horizontal_wrapped(|ui| {
                    if let Some(notice) = &self.notice {
                        ui.strong("A Relay update is available");
                        ui.label(&notice.tag);
                        if ui
                            .button("Download update")
                            .on_hover_text("Open this release on GitHub, then run Relay-Setup.exe")
                            .clicked()
                        {
                            ctx.open_url(egui::OpenUrl::new_tab(&notice.url));
                        }
                        ui.weak("Opens GitHub · install manually");
                    } else {
                        match &self.state {
                            State::Available(release) => {
                                ui.strong(format!("Relay {} is available", release.version));
                                let restart = in_place_update(release);
                                let allowed = if restart { install_allowed(in_call, hosting, busy, true) } else { !in_call };
                                if update_button(ui, allowed, if restart { "Update and restart" } else { "Download update" })
                                    .on_hover_text(
                                        if restart { "Download, verify, update this installation and reopen Relay. No setup wizard." }
                                        else { "Download and verify the update. Relay stays open." },
                                    )
                                    .clicked()
                                {
                                    self.download(ctx, restart);
                                }
                                if restart { ui.weak("Temporarily stops servers hosted on this PC"); }
                                if in_call {
                                    ui.label("Available after your call");
                                } else if restart && busy {
                                    ui.label("Finish pending work to update");
                                }
                            }
                            State::Downloading | State::Cancelling => {
                                ui.spinner();
                                ui.label(if matches!(self.state, State::Downloading) {
                                    "Downloading and checking the update…"
                                } else {
                                    "Cancelling download…"
                                });
                                if matches!(self.state, State::Downloading)
                                    && ui.button("Cancel").clicked()
                                {
                                    self.cancel_download();
                                }
                            }
                            State::Ready(ready) => {
                                let ready = ready.clone();
                                ui.strong("Update downloaded");
                                #[cfg(windows)]
                                if supports_handoff(ready.release()) {
                                    match install_controls(
                                        ui,
                                        in_call,
                                        hosting,
                                        busy,
                                        in_place_update(ready.release()),
                                        self.preparing_install(),
                                    ) {
                                        InstallChoice::Start => self.approve_install(ctx),
                                        InstallChoice::Cancel => self.cancel_install(),
                                        InstallChoice::None => {}
                                    }
                                }
                                if !self.preparing_install() && ui.button("Show download").clicked() {
                                    if ready.download_directory().and_then(show_folder).is_err() {
                                        self.failed();
                                    }
                                }
                                if in_call {
                                    ui.label("Leave the call to install");
                                } else if busy {
                                    ui.label("Finish pending work to install");
                                } else if hosting && !in_place_update(ready.release()) {
                                    ui.label("Stop local hosting before running this older installer");
                                }
                            }
                            State::Failed => {
                                ui.label("The update couldn't be verified. Relay is still open.");
                                ui.hyperlink_to("View releases", RELEASES_URL);
                            }
                            _ => {}
                        }
                        if let Some(error) = &self.install_error {
                            ui.label(error);
                        }
                    }
                    if !self.preparing_install() && self.install_consent.is_none()
                        && ui
                            .small_button("Later")
                            .on_hover_text(
                                "Hide this notice until a newer update is found or Relay restarts",
                            )
                            .clicked()
                    {
                        self.banner_hidden = true;
                    }
                });
            });
    }

    #[cfg(windows)]
    fn approve_install(&mut self, ctx: &egui::Context) {
        let State::Ready(ready) = &self.state else { return; };
        if !supports_handoff(ready.release()) { return; }
        self.install_consent = Some(Box::new(ready.release().clone()));
        self.begin_install(ctx);
    }

    #[cfg(windows)]
    fn begin_install(&mut self, ctx: &egui::Context) {
        if self.preparing_install() { return; }
        let State::Ready(ready) = &self.state else {
            return;
        };
        if !consent_matches(self.install_consent.as_deref(), ready.release()) { return; }
        self.install_error = None;
        match handoff::Job::start((**ready).clone(), ctx) {
            Ok(job) => self.install = Some(job),
            Err(error) => {
                self.install_consent = None;
                self.install_error = Some(error);
            }
        }
    }

    #[cfg(windows)]
    fn cancel_install(&mut self) {
        self.install_consent = None;
        if let Some(job) = &self.install {
            job.cancel();
        }
        self.waiting = None;
    }

    pub fn show(&mut self, ui: &mut egui::Ui, in_call: bool, hosting: bool, busy_work: bool) {
        crate::settings_ui::section_heading(
            ui,
            "Updates",
            "Choose when to download a new version of Relay.",
        );
        if self.notices_enabled {
            let message = if self.notice_job.is_some() {
                "Checking GitHub for a newer release…"
            } else if self.notice_failed {
                "Couldn't check GitHub. You can try again or view the releases."
            } else if self.notice.is_some() {
                "A new Relay preview is available."
            } else if self.notice_checked {
                "No newer published build was found."
            } else {
                "Relay can check GitHub for a newer preview."
            };
            ui.label(message);
            if let Some(notice) = &self.notice {
                ui.label(&notice.tag);
                if ui
                    .button("Download update")
                    .on_hover_text("Open this release on GitHub")
                    .clicked()
                {
                    ui.ctx().open_url(egui::OpenUrl::new_tab(&notice.url));
                }
            }
            crate::settings_ui::description(ui, "For this preview, updates open on GitHub and you run the installer. Automatic installation will be available once signed updates are configured.");
            let can_check =
                self.notice_job.is_none() && self.retry_at.is_none_or(|at| Instant::now() >= at);
            if update_button(ui, can_check, "Check for updates").clicked() {
                self.check(ui.ctx());
            }
            ui.hyperlink_to("View all releases", RELEASES_URL);
            if let Some(at) = self.retry_at.filter(|at| *at > Instant::now()) {
                ui.ctx()
                    .request_repaint_after(at.saturating_duration_since(Instant::now()));
            }
            return;
        }
        let message = match &self.state {
            State::Disabled => "Online updates aren't available in this build. Use a new Relay installer to update.",
            State::Unavailable => "Relay couldn't safely prepare updates. Use a new Relay installer to update.",
            State::Idle => "Ready to check for updates.",
            State::Checking => "Checking for updates…",
            State::Current => "You're up to date.",
            State::Available(_) => "A new version is available.",
            State::Downloading => "Downloading and checking the update…",
            State::Cancelling => "Stopping the update operation…",
            State::Ready(ready) if in_place_update(ready.release()) => "Download verified. Relay can update and reopen without a setup wizard.",
            State::Ready(_) => "Download verified. Run the installer to finish this update.",
            State::Failed => "Couldn't check or verify the update. Try again in a moment.",
        };
        ui.label(message);
        if matches!(self.state, State::Disabled | State::Unavailable) {
            ui.hyperlink_to("View releases", RELEASES_URL);
        }
        if let State::Available(release) = &self.state {
            ui.label(format!("Relay {}", release.version));
            if !release.notes.is_empty() {
                crate::settings_ui::description(ui, &release.notes);
            }
        }
        let busy = matches!(
            self.state,
            State::Checking | State::Downloading | State::Cancelling
        );
        #[cfg(windows)]
        let busy = busy || self.preparing_install();
        let can_check =
            self.worker.is_some() && !busy && self.retry_at.is_none_or(|at| Instant::now() >= at);
        if update_button(ui, can_check, "Check for updates").clicked() {
            self.check(ui.ctx());
        }
        if let State::Available(release) = &self.state {
            let restart = in_place_update(release);
            if in_call {
                crate::settings_ui::description(ui, "Leave the call before downloading an update.");
            }
            if restart {
                if update_button(ui, install_allowed(in_call, hosting, busy_work, true), "Update and restart").clicked() {
                    self.download(ui.ctx(), true);
                }
                crate::settings_ui::description(ui, "Downloads and verifies the update, then reopens Relay. Temporarily stops servers hosted on this PC.");
                if busy_work { ui.label("Finish pending work to update."); }
            }
            if update_button(ui, !in_call, if restart { "Download only" } else { "Download update" }).clicked() {
                self.download(ui.ctx(), false);
            }
        }
        if matches!(self.state, State::Checking | State::Downloading)
            && ui.button("Cancel").clicked()
        {
            self.cancel_download();
        }
        if let State::Ready(ready) = &self.state {
            let ready = ready.clone();
            #[cfg(windows)]
            if supports_handoff(ready.release()) {
                match install_controls(ui, in_call, hosting, busy_work, in_place_update(ready.release()), self.preparing_install()) {
                    InstallChoice::Start => {
                        self.approve_install(ui.ctx());
                    }
                    InstallChoice::Cancel => {
                        self.cancel_install();
                    }
                    InstallChoice::None => {}
                }
            }
            if in_call {
                crate::settings_ui::description(ui, "Leave the call before installing an update.");
            }
            if busy_work {
                crate::settings_ui::description(ui, "Finish pending work before updating.");
            } else if hosting && !in_place_update(ready.release()) {
                crate::settings_ui::description(
                    ui,
                    "Stop your local server normally and wait for other operations before installing.",
                );
            }
            if !self.preparing_install() && ui.button("Show download").clicked()
                && ready.download_directory().and_then(show_folder).is_err()
            {
                self.failed();
            }
        }
        if let Some(error) = &self.install_error {
            ui.label(error);
        }
        if matches!(self.state, State::Failed) {
            if let Some(deadline) = self.retry_at.filter(|deadline| *deadline > Instant::now()) {
                ui.ctx()
                    .request_repaint_after(deadline.saturating_duration_since(Instant::now()));
            }
        }
    }
}

fn in_place_update(release: &Release) -> bool {
    cfg!(windows) && release.handoff.as_deref() == Some("in-place-v2")
}

#[cfg(windows)]
fn supports_handoff(release: &Release) -> bool {
    matches!(release.handoff.as_deref(), Some("wait-parent-v1" | "in-place-v2"))
}

fn consent_matches(consent: Option<&Release>, ready: &Release) -> bool {
    consent.is_some_and(|consent| consent == ready)
}

pub(crate) fn install_allowed(in_call: bool, hosting: bool, busy: bool, in_place: bool) -> bool {
    !in_call && !busy && (!hosting || in_place)
}

#[cfg(windows)]
#[derive(Debug, PartialEq, Eq)]
enum InstallChoice {
    None,
    Start,
    Cancel,
}

#[cfg(windows)]
fn install_controls(
    ui: &mut egui::Ui,
    in_call: bool,
    hosting: bool,
    busy: bool,
    in_place: bool,
    preparing: bool,
) -> InstallChoice {
    if preparing {
        ui.label("Preparing update… Relay will close when the installer is ready.");
        if ui.button("Cancel installation").clicked() {
            return InstallChoice::Cancel;
        }
    } else if update_button(
        ui,
        install_allowed(in_call, hosting, busy, in_place),
        if in_place { "Update and restart" } else { "Install and reopen Relay" },
    )
    .clicked()
    {
        return InstallChoice::Start;
    }
    if in_place { ui.weak("Temporarily stops servers hosted on this PC"); }
    InstallChoice::None
}

fn update_button(ui: &mut egui::Ui, enabled: bool, label: &str) -> egui::Response {
    let response = ui.add_enabled(enabled, egui::Button::new(label));
    // egui 0.27 does not publish a disabled Button's state to AccessKit.
    ui.ctx().accesskit_node_builder(response.id, |builder| {
        if response.enabled() {
            builder.clear_disabled();
        } else {
            builder.set_disabled();
        }
    });
    response
}

fn update_directory() -> Result<PathBuf, String> {
    if let Some(root) = std::env::var_os("RELAY_PROFILE_DIR") {
        return Ok(PathBuf::from(root).join("updates"));
    }
    directories::ProjectDirs::from("org", "Relay", "Relay")
        .map(|dirs| dirs.cache_dir().join("verified-updates"))
        .ok_or_else(|| "Your update storage folder is unavailable.".into())
}

#[cfg(windows)]
fn show_folder(path: &std::path::Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL};
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let verb: Vec<u16> = "open".encode_utf16().chain(Some(0)).collect();
    let result = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(verb.as_ptr()),
            PCWSTR(wide.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    if result.0 as isize <= 32 {
        Err("Couldn't open your download folder.".into())
    } else {
        Ok(())
    }
}
#[cfg(not(windows))]
fn show_folder(_path: &std::path::Path) -> Result<(), String> {
    Err("This update is for Windows.".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v2_release() -> Release {
        Release {
            schema: 1, app_id: "relay".into(), platform: "windows-x64".into(),
            channel: "beta".into(), version: "0.10.1".into(),
            sequence: "20261026080000".into(), installer_target: "fixture.exe".into(),
            installer_sha256: "0".repeat(64), installer_bytes: 123,
            notes: String::new(), handoff: Some("in-place-v2".into()),
        }
    }

    #[cfg(windows)]
    #[test]
    fn automatic_install_consent_is_exact_and_call_or_work_cancellation_is_final() {
        let ctx = egui::Context::default();
        let release = v2_release();
        assert!(!consent_matches(None, &release));
        assert!(consent_matches(Some(&release), &release));
        let changed = Release { installer_sha256: "1".repeat(64), ..release.clone() };
        assert!(!consent_matches(Some(&release), &changed));
        let legacy = Release { handoff: Some("wait-parent-v1".into()), ..release.clone() };
        assert!(!in_place_update(&legacy));

        for (in_call, busy) in [(true, false), (false, true)] {
            let mut controller = Controller::inactive(State::Downloading);
            controller.install_consent = Some(Box::new(release.clone()));
            controller.download_release = Some(Box::new(release.clone()));
            // Hosting alone is covered by the user's v2 approval, but neither
            // call intent nor unsent work may be ignored during the download.
            assert!(controller.consent_allowed(false, true, false));
            assert!(!controller.tick(&ctx, in_call, true, busy, false, || {
                panic!("A call or pending work must not shut down Relay")
            }));
            assert!(controller.install_consent.is_none());
            assert!(matches!(controller.state, State::Cancelling));
            controller.apply(Ok(Outcome::Cancelled));
            assert!(matches!(controller.state, State::Available(_)));
            assert!(!controller.tick(&ctx, false, true, false, false, || {
                panic!("Ending a call must not revive cancelled consent")
            }));
            assert!(controller.install_consent.is_none());
        }
    }

    #[cfg(windows)]
    #[test]
    fn update_and_restart_control_allows_local_hosting_but_blocks_calls_and_work() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut target = None;
        for (call, work, invoke, expected) in [
            (false, false, false, InstallChoice::None),
            (true, false, true, InstallChoice::None),
            (false, true, true, InstallChoice::None),
            (false, false, true, InstallChoice::Start),
        ] {
            let events = if invoke { vec![egui::Event::AccessKitActionRequest(
                egui::accesskit::ActionRequest { target: target.unwrap(),
                    action: egui::accesskit::Action::Default, data: None }
            )] } else { Vec::new() };
            let mut action = InstallChoice::None;
            let output = ctx.run(egui::RawInput { events, ..Default::default() }, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    action = install_controls(ui, call, true, work, true, false);
                });
            });
            let nodes = output.platform_output.accesskit_update.unwrap().nodes;
            let (id, button) = nodes.iter().find(|(_, node)| node.name() == Some("Update and restart")).unwrap();
            target = Some(*id);
            assert_eq!(button.is_disabled(), call || work);
            assert_eq!(action, expected);
        }
    }

    #[test]
    fn automatic_check_opt_out_and_call_deferral_start_no_request() {
        let ctx = egui::Context::default();
        let mut controller = Controller::inactive(State::Disabled);
        controller.notices_enabled = true;
        controller.next_check = Instant::now() - Duration::from_secs(1);
        controller.tick(&ctx, false, false, false, false, || {
            panic!("No installation was approved")
        });
        assert!(controller.notice_job.is_none());
        controller.tick(&ctx, true, false, false, true, || {
            panic!("A call must never trigger shutdown")
        });
        assert!(controller.notice_job.is_none());
        assert!(controller.next_check > Instant::now());
    }

    #[test]
    fn preview_banner_opens_only_the_release_page_and_later_hides_it() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut controller = Controller::inactive(State::Disabled);
        controller.notices_enabled = true;
        controller.notice = Some(release_notice::Notice {
            tag: "v0.10.0-preview.20261026.1".into(),
            url: "https://github.com/TheBengine/Relay/releases/tag/v0.10.0-preview.20261026.1"
                .into(),
            version: "0.10.0".into(),
            sequence: "20261026080000".into(),
            prerelease: true,
        });
        let render = |controller: &mut Controller, action: Option<egui::accesskit::NodeId>| {
            let events = action
                .map(|target| {
                    egui::Event::AccessKitActionRequest(egui::accesskit::ActionRequest {
                        target,
                        action: egui::accesskit::Action::Default,
                        data: None,
                    })
                })
                .into_iter()
                .collect();
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(360.0, 800.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ctx| controller.show_banner(ctx, false, false, false),
            )
        };
        let first = render(&mut controller, None);
        let nodes = first.platform_output.accesskit_update.unwrap().nodes;
        let download = nodes
            .iter()
            .find(|(_, n)| n.name() == Some("Download update"))
            .unwrap()
            .0;
        let later = nodes
            .iter()
            .find(|(_, n)| n.name() == Some("Later"))
            .unwrap()
            .0;
        let clicked = render(&mut controller, Some(download));
        assert_eq!(
            clicked.platform_output.open_url.unwrap().url,
            controller.notice.as_ref().unwrap().url
        );
        assert!(controller.worker.is_none());
        assert!(!controller.preparing_install());
        assert!(matches!(controller.state, State::Disabled));
        render(&mut controller, Some(later));
        assert!(controller.banner_hidden);
        let hidden = render(&mut controller, None);
        assert!(hidden.platform_output.open_url.is_none());
        assert!(!hidden
            .platform_output
            .accesskit_update
            .unwrap()
            .nodes
            .iter()
            .any(|(_, n)| n.name() == Some("Download update")));
    }

    #[test]
    fn periodic_recheck_does_not_redisplay_a_dismissed_signed_release() {
        let mut controller = Controller::inactive(State::Disabled);
        let release = Release {
            schema: 1,
            app_id: "relay".into(),
            platform: "windows-x64".into(),
            channel: "beta".into(),
            version: "0.10.1".into(),
            sequence: "20261026080000".into(),
            installer_target: "fixture.exe".into(),
            installer_sha256: "0".repeat(64),
            installer_bytes: 123,
            notes: String::new(),
            handoff: None,
        };
        controller.apply(Ok(Outcome::Checked(Check::Available(Box::new(
            release.clone(),
        )))));
        controller.banner_hidden = true;
        controller.state = State::Checking;
        controller.apply(Ok(Outcome::Checked(Check::Available(Box::new(
            release.clone(),
        )))));
        assert!(controller.banner_hidden);
        let next = Release {
            sequence: "20261027080000".into(),
            ..release
        };
        controller.apply(Ok(Outcome::Checked(Check::Available(Box::new(next)))));
        assert!(!controller.banner_hidden);
    }

    #[cfg(windows)]
    #[test]
    fn real_install_controls_block_calls_host_operations_and_require_explicit_action() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut target = None;
        // Invocations from a former enabled frame must also be blocked when
        // a call/host/other operation becomes active before the next frame.
        for (call, host_or_pending, preparing, invoke, expected) in [
            (false, false, false, false, InstallChoice::None),
            (true, false, false, true, InstallChoice::None),
            (false, true, false, true, InstallChoice::None),
            (true, true, false, true, InstallChoice::None),
            (false, false, false, true, InstallChoice::Start),
            (false, false, true, false, InstallChoice::None),
            (true, true, true, true, InstallChoice::Cancel),
        ] {
            let label = if preparing {
                "Cancel installation"
            } else {
                "Install and reopen Relay"
            };
            let mut events = vec![];
            // Refresh the cancellation target when changing controls.
            if preparing && !invoke {
                target = None;
            }
            if invoke {
                events.push(egui::Event::AccessKitActionRequest(
                    egui::accesskit::ActionRequest {
                        target: target.unwrap(),
                        action: egui::accesskit::Action::Default,
                        data: None,
                    },
                ));
            }
            let mut action = InstallChoice::None;
            let output = ctx.run(
                egui::RawInput {
                    events,
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        action = install_controls(ui, call, host_or_pending, false, false, preparing);
                    });
                },
            );
            let update = output.platform_output.accesskit_update.unwrap();
            let (id, button) = update
                .nodes
                .iter()
                .find(|(_, node)| node.name() == Some(label))
                .unwrap();
            target = Some(*id);
            assert_eq!(
                button.is_disabled(),
                !preparing && (call || host_or_pending)
            );
            assert_eq!(action, expected);
        }
    }

    #[test]
    fn disabled_update_buttons_publish_their_state_and_ignore_accessibility_invocation() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut target = None;
        let mut clicked = false;
        for (enabled, scope_enabled, invoke, expected) in [
            (false, true, false, false),
            (false, true, true, false),
            (true, true, true, true),
            (true, false, true, false),
        ] {
            let events = if invoke {
                vec![egui::Event::AccessKitActionRequest(
                    egui::accesskit::ActionRequest {
                        target: egui::accesskit::NodeId(target.unwrap()),
                        action: egui::accesskit::Action::Default,
                        data: None,
                    },
                )]
            } else {
                vec![]
            };
            let output = ctx.run(
                egui::RawInput {
                    events,
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        ui.add_enabled_ui(scope_enabled, |ui| {
                            let response = update_button(ui, enabled, "Download update");
                            target = Some(response.id.value());
                            clicked = response.clicked();
                        });
                    });
                },
            );
            let node = &output
                .platform_output
                .accesskit_update
                .unwrap()
                .nodes
                .into_iter()
                .find(|(id, _)| id.0 == target.unwrap())
                .unwrap()
                .1;
            assert_eq!(node.name(), Some("Download update"));
            assert_eq!(node.is_disabled(), !(enabled && scope_enabled));
            assert_eq!(clicked, expected);
        }
    }

    #[test]
    fn unconfigured_check_is_disabled_in_the_actual_settings_page() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut controller = Controller::inactive(State::Disabled);
        let mut target = None;
        for invoke in [false, true] {
            let events = if invoke {
                vec![egui::Event::AccessKitActionRequest(
                    egui::accesskit::ActionRequest {
                        target: target.unwrap(),
                        action: egui::accesskit::Action::Default,
                        data: None,
                    },
                )]
            } else {
                vec![]
            };
            let output = ctx.run(
                egui::RawInput {
                    events,
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        controller.show(ui, false, false, false);
                    });
                },
            );
            let update = output.platform_output.accesskit_update.unwrap();
            let (id, button) = update
                .nodes
                .iter()
                .find(|(_, node)| node.name() == Some("Check for updates"))
                .unwrap();
            target = Some(*id);
            assert!(button.is_disabled());
            assert!(matches!(controller.state, State::Disabled));
            assert!(controller.worker.is_none());
        }
    }

    #[test]
    fn missing_publisher_never_resolves_personal_paths_or_starts_a_worker() {
        let ctx = egui::Context::default();
        let mut controller = Controller::from_policy(
            Ok(None),
            InstalledRelease {
                version: "0.10.0".into(),
                sequence: String::new(),
            },
            || panic!("Unconfigured updates may not access storage"),
            &ctx,
        );
        controller.check(&ctx);
        controller.tick(&ctx, false, false, false, false, || {
            panic!("disabled updates must not close the connection")
        });
        assert!(matches!(controller.state, State::Disabled));
        assert!(controller.worker.is_none());
    }
    #[test]
    fn cancellation_intent_discards_late_results_and_failure_has_a_retry_deadline() {
        let mut controller = Controller::inactive(State::Disabled);
        controller.state = State::Cancelling;
        controller.apply(Ok(Outcome::Checked(Check::Current)));
        assert!(matches!(controller.state, State::Idle));
        controller.apply(Err("invalid publisher signature".into()));
        assert!(matches!(controller.state, State::Failed));
        assert!(controller.retry_at.is_some_and(|at| at > Instant::now()));
    }

    #[test]
    fn invalid_bundled_publisher_cannot_resolve_paths_or_create_an_update_worker() {
        let ctx = egui::Context::default();
        let mut controller = Controller::from_policy(
            Err("Invalid bundled root".into()),
            InstalledRelease {
                version: "0.10.0".into(),
                sequence: String::new(),
            },
            || panic!("An invalid publisher may not access personal storage"),
            &ctx,
        );
        controller.check(&ctx);
        controller.tick(&ctx, false, false, false, false, || {
            panic!("No installation is authorized")
        });
        assert!(matches!(controller.state, State::Unavailable));
        assert!(controller.worker.is_none());
    }
}
