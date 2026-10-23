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
}

impl Controller {
    pub fn new(ctx: &egui::Context) -> Self {
        Self::from_policy(
            relay_updater::bundled_publisher(),
            InstalledRelease {
                version: env!("CARGO_PKG_VERSION").into(),
                sequence: option_env!("RELAY_BUILD_SEQUENCE").unwrap_or("").into(),
            },
            update_directory,
            ctx,
        )
    }

    fn from_policy(
        publisher: Result<Option<Publisher>, String>,
        installed: InstalledRelease,
        directory: impl FnOnce() -> Result<PathBuf, String>,
        ctx: &egui::Context,
    ) -> Self {
        // No root means no path/environment lookup, network request, runtime or worker.
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
        let mut controller = Self {
            state: State::Idle,
            worker: Some(Worker::new(engine)),
            retry_at: None,
            #[cfg(windows)]
            install: None,
            #[cfg(windows)]
            waiting: None,
            install_error: None,
        };
        controller.check(ctx);
        controller
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
        }
    }

    fn check(&mut self, ctx: &egui::Context) {
        if self
            .retry_at
            .is_some_and(|deadline| Instant::now() < deadline)
        {
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
        } else {
            self.failed();
        }
    }

    fn download(&mut self, ctx: &egui::Context) {
        let State::Available(release) = &self.state else {
            return;
        };
        let approved = (**release).clone();
        let Some(worker) = self.worker.as_mut() else {
            return;
        };
        let wake = ctx.clone();
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
        self.state = State::Failed;
        self.retry_at = Some(Instant::now() + Duration::from_secs(10));
    }

    fn apply(&mut self, result: Result<Outcome, String>) {
        if matches!(self.state, State::Cancelling) {
            self.state = State::Idle;
            self.retry_at = None;
            return;
        }
        match result {
            Ok(Outcome::Checked(Check::Current)) => self.state = State::Current,
            Ok(Outcome::Checked(Check::Available(release))) => {
                self.state = State::Available(release)
            }
            Ok(Outcome::Downloaded(ready)) => self.state = State::Ready(ready),
            Ok(Outcome::Cancelled) => self.state = State::Idle,
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

    pub fn tick(
        &mut self,
        ctx: &egui::Context,
        in_call: bool,
        hosting_or_busy: bool,
        #[allow(unused_mut)] mut shutdown: impl FnMut() -> crate::network::ShutdownPoll,
    ) -> bool {
        if in_call && matches!(self.state, State::Downloading) {
            if let Some(worker) = self.worker.as_mut() {
                worker.cancel();
                self.state = State::Cancelling;
            }
        }
        let result = self.worker.as_mut().map(Worker::poll);
        match result {
            Some(Poll::Finished(result)) => self.apply(result),
            Some(Poll::Completing) => ctx.request_repaint_after(Duration::from_millis(5)),
            // Running jobs wake the UI when their output is ready; no status polling timer.
            _ => {}
        }
        #[cfg(windows)]
        if let Some(job) = self.install.as_mut() {
            if !install_allowed(in_call, hosting_or_busy) {
                job.cancel();
            }
            match job.poll() {
                handoff::Poll::Finished(result) => {
                    self.install = None;
                    match result {
                        Ok(waiting) if install_allowed(in_call, hosting_or_busy) => {
                            self.waiting = Some(waiting);
                            ctx.request_repaint();
                        }
                        Ok(_) => self.install_error = Some("Installation stopped because a call, local server or another operation started. Relay was left open.".into()),
                        Err(error) => self.install_error = Some(error),
                    }
                }
                handoff::Poll::Completing => ctx.request_repaint_after(Duration::from_millis(5)),
                handoff::Poll::Running => {}
            }
        }
        #[cfg(windows)]
        if self.waiting.is_some() {
            if !install_allowed(in_call, hosting_or_busy) {
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
                            Err(error) => self.install_error = Some(error),
                        }
                    }
                }
            }
        }
        #[cfg(not(windows))]
        let _ = shutdown;
        false
    }

    pub fn show(&mut self, ui: &mut egui::Ui, in_call: bool, hosting: bool) {
        crate::settings_ui::section_heading(
            ui,
            "Updates",
            "Choose when to download a new version of Relay.",
        );
        let message = match &self.state {
            State::Disabled => "Online updates aren't available in this build. Use a new Relay installer to update.",
            State::Unavailable => "Relay couldn't safely prepare updates. Use a new Relay installer to update.",
            State::Idle => "Ready to check for updates.",
            State::Checking => "Checking for updates…",
            State::Current => "You're up to date.",
            State::Available(_) => "A new version is available.",
            State::Downloading => "Downloading and checking the update…",
            State::Cancelling => "Stopping the update operation…",
            State::Ready(_) => "Download verified. Choose when to close Relay and install it.",
            State::Failed => "Couldn't check or verify the update. Try again in a moment.",
        };
        ui.label(message);
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
        if matches!(self.state, State::Available(_)) {
            if in_call {
                crate::settings_ui::description(ui, "Leave the call before downloading an update.");
            }
            if update_button(ui, !in_call, "Download update").clicked() {
                self.download(ui.ctx());
            }
        }
        if matches!(self.state, State::Checking | State::Downloading)
            && ui.button("Cancel").clicked()
        {
            if let Some(worker) = self.worker.as_mut() {
                worker.cancel();
                self.state = State::Cancelling;
            }
        }
        if let State::Ready(ready) = &self.state {
            #[cfg(windows)]
            if ready.release().handoff.as_deref() == Some("wait-parent-v1") {
                match install_controls(ui, in_call, hosting, self.preparing_install()) {
                    InstallChoice::Start => {
                        self.install_error = None;
                        match handoff::Job::start((**ready).clone(), ui.ctx()) {
                            Ok(job) => self.install = Some(job),
                            Err(error) => self.install_error = Some(error),
                        }
                    }
                    InstallChoice::Cancel => {
                        if let Some(job) = &self.install {
                            job.cancel();
                        }
                        self.waiting = None;
                    }
                    InstallChoice::None => {}
                }
            }
            if in_call {
                crate::settings_ui::description(ui, "Leave the call before installing an update.");
            }
            if hosting {
                crate::settings_ui::description(
                    ui,
                    "Stop your local server normally and wait for other operations before installing.",
                );
            }
            if ui.button("Show download").clicked()
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

pub(crate) fn install_allowed(in_call: bool, hosting_or_busy: bool) -> bool {
    !in_call && !hosting_or_busy
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
    hosting_or_busy: bool,
    preparing: bool,
) -> InstallChoice {
    if preparing {
        ui.label("Preparing installation… Relay will close when the installer is ready.");
        if ui.button("Cancel installation").clicked() {
            return InstallChoice::Cancel;
        }
    } else if update_button(
        ui,
        install_allowed(in_call, hosting_or_busy),
        "Close Relay and install",
    )
    .clicked()
    {
        return InstallChoice::Start;
    }
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
                "Close Relay and install"
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
                        action = install_controls(ui, call, host_or_pending, preparing);
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
                        controller.show(ui, false, false);
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
        controller.tick(&ctx, false, false, || {
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
        controller.tick(&ctx, false, false, || {
            panic!("No installation is authorized")
        });
        assert!(matches!(controller.state, State::Unavailable));
        assert!(controller.worker.is_none());
    }
}
