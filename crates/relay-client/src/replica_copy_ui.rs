//! The "Copy on this PC" card on the Hosts page, and the glue that ties the
//! copy schedule to the signed-in account.
//!
//! Consent is explicit: only the member's own click starts a copy, and only
//! after the Replica role was accepted on a community that offers copies. The
//! choice and the copy's id are remembered; the pull token never is. After a
//! restart the schedule resumes only for the same account on the same
//! community. Stopping, signing out, switching account or losing the role ends
//! the schedule and its child, and nothing it reports late is shown.
use super::*;
use crate::appearance::{TEXT_BODY, TEXT_CAPTION};
use crate::network::{ReplicaTokenAction, ReplicaTokenOutcome};
use crate::replica_copy::{self as copy, Choice, Controller, Fault, Phase, Tone, View};

enum Pending {
    /// Waiting for the community to issue this copy's token.
    Mint { request_id: u64, choice: Choice },
    /// Waiting for the community and the vault to let go of it.
    Stop { request_id: u64 },
}

pub(in crate::app) struct CopyUi {
    controller: Controller,
    view: Option<View>,
    pending: Option<Pending>,
    /// Revocations nobody waits for: signing out, a lost role, a refused save.
    cleanup: HashSet<u64>,
    error: Option<String>,
    /// After Stop: the copy that stays on this PC, and where.
    left_behind: Option<(Choice, PathBuf)>,
}

impl CopyUi {
    pub(in crate::app) fn new(ctx: egui::Context) -> Self {
        Self {
            controller: Controller::new(ctx),
            view: None,
            pending: None,
            cleanup: HashSet::new(),
            error: None,
            left_behind: None,
        }
    }

    pub(in crate::app) fn shutdown(&mut self) {
        self.controller.shutdown();
    }
}

enum Action {
    Keep,
    Stop,
    Retry,
    CopyPath(String),
}

/// The copy that should be running now. Nothing runs without the member's
/// saved choice for exactly this community and account, a community that
/// offers copies, an unfinished role, and no consent still in flight.
fn wanted<'a>(
    choices: &'a [Choice],
    origin: Option<&CommandOrigin>,
    offered: bool,
    role_lost: bool,
    awaiting: bool,
) -> Option<&'a Choice> {
    if !offered || role_lost || awaiting {
        return None;
    }
    copy::find(choices, origin?)
}

fn tone_color(ui: &egui::Ui, tone: Tone) -> Color32 {
    match tone {
        Tone::Good => c_good(),
        Tone::Info => c_muted(),
        Tone::Warn if ui.visuals().dark_mode => Color32::from_rgb(224, 170, 60),
        Tone::Warn => Color32::from_rgb(138, 90, 0),
        Tone::Bad => c_bad(),
    }
}

impl RelayApp {
    fn replica_offered(&self) -> bool {
        self.bootstrap
            .as_ref()
            .is_some_and(|b| !b.host_paused && b.capabilities.iter().any(|c| c == copy::CAPABILITY))
    }

    /// Whether the community's own host view says this account no longer holds
    /// the Replica role. Unknown until the Hosts page has loaded it.
    fn replica_role_lost(&self, origin: Option<&CommandOrigin>) -> bool {
        self.admin_ui.hosting.view_for(origin).is_some_and(|view| {
            !view.candidates.iter().any(|entry| {
                entry.candidate.user_id == view.you && entry.candidate.holds(HostRole::Replica)
            })
        })
    }

    /// Called every frame. Cheap: it only compares who should be copying with
    /// who is.
    pub(in crate::app) fn replica_copy_tick(&mut self) {
        let now = Instant::now();
        let origin = self.command_origin();
        for event in self.replica_copy.controller.poll() {
            let denied = matches!(
                &event,
                copy::Event::Finished {
                    result: Err(Fault::AccessDenied),
                    ..
                }
            );
            let Some(view) = self.replica_copy.view.as_mut() else {
                continue;
            };
            let accepted = view.apply(event, now);
            let choice = view.choice.clone();
            if accepted && denied {
                // The community ended this access; clear what this PC still holds.
                self.replica_revoke_quietly(choice);
            }
        }
        let awaiting = self.replica_copy.pending.is_some();
        let role_lost = self.replica_role_lost(origin.as_ref());
        let want = wanted(
            &self.prefs.replica_copies,
            origin.as_ref(),
            self.replica_offered(),
            role_lost,
            awaiting,
        )
        .cloned();
        let running = self.replica_copy.view.as_ref().map(|view| &view.choice);
        if want.as_ref() == running {
            return;
        }
        self.replica_copy.controller.stop();
        self.replica_copy.view = None;
        let Some(choice) = want else {
            return;
        };
        self.replica_copy.view = Some(match self.replica_copy.controller.start(choice.clone()) {
            Ok(generation) => View::new(choice, generation),
            Err(_) => {
                // Shown as stopped instead of retried every frame.
                let mut view = View::new(choice, 0);
                view.phase = Phase::Stopped(Fault::Storage);
                view
            }
        });
    }

    /// Sends a revocation whose answer nobody waits for.
    fn replica_revoke_quietly(&mut self, choice: Choice) {
        let Some(origin) = self.command_origin().filter(|o| choice.belongs_to(o)) else {
            return;
        };
        let request_id = self.next_request();
        if self
            .network
            .send(Command::ReplicaToken {
                origin,
                action: ReplicaTokenAction::Revoke,
                replica_id: choice.replica_id,
                request_id,
            })
            .is_ok()
        {
            self.replica_copy.cleanup.insert(request_id);
        }
    }

    /// Signing out ends the copy's access first. The choice stays, so signing
    /// back in as the same account offers to start it again.
    pub(in crate::app) fn replica_before_logout(&mut self) {
        self.replica_copy.controller.stop();
        let running = self.replica_copy.view.take().map(|view| view.choice);
        // A token still being issued is revoked right behind its own request,
        // in the same queue and for the same account, and its answer is ignored.
        let issuing = match self.replica_copy.pending.take() {
            Some(Pending::Mint { request_id, choice }) => {
                self.replica_copy.cleanup.insert(request_id);
                Some(choice)
            }
            other => {
                self.replica_copy.pending = other;
                None
            }
        };
        for choice in running.into_iter().chain(issuing) {
            self.replica_revoke_quietly(choice);
        }
    }

    fn replica_start_mint(&mut self, choice: Choice) {
        let Some(origin) = self.command_origin().filter(|o| choice.belongs_to(o)) else {
            self.replica_copy.error = Some("Connect to this community first.".into());
            return;
        };
        let request_id = self.next_request();
        match self.network.send(Command::ReplicaToken {
            origin,
            action: ReplicaTokenAction::Mint,
            replica_id: choice.replica_id.clone(),
            request_id,
        }) {
            Ok(()) => {
                self.replica_copy.error = None;
                self.replica_copy.view = None;
                self.replica_copy.pending = Some(Pending::Mint { request_id, choice });
            }
            Err(error) => self.replica_copy.error = Some(error),
        }
    }

    fn replica_stop(&mut self, choice: Choice) {
        self.replica_copy.controller.stop();
        self.replica_copy.view = None;
        let Some(origin) = self.command_origin().filter(|o| choice.belongs_to(o)) else {
            return;
        };
        copy::forget(&mut self.prefs.replica_copies, &origin);
        self.save_preferences();
        if let Ok(folder) = copy::copy_directory(&choice) {
            self.replica_copy.left_behind = Some((choice.clone(), folder));
        }
        let request_id = self.next_request();
        match self.network.send(Command::ReplicaToken {
            origin,
            action: ReplicaTokenAction::Revoke,
            replica_id: choice.replica_id,
            request_id,
        }) {
            Ok(()) => self.replica_copy.pending = Some(Pending::Stop { request_id }),
            Err(error) => {
                self.replica_copy.error = Some(format!(
                    "Copying stopped on this PC, but the access could not be removed yet: {error}"
                ))
            }
        }
    }

    /// The answer to a mint or a revoke. It carries no token.
    pub(in crate::app) fn replica_token_result(
        &mut self,
        origin: CommandOrigin,
        request_id: u64,
        replica_id: String,
        outcome: ReplicaTokenOutcome,
    ) {
        if self.replica_copy.cleanup.remove(&request_id) {
            return;
        }
        match (self.replica_copy.pending.take(), outcome) {
            (
                Some(Pending::Mint {
                    request_id: id,
                    choice,
                }),
                ReplicaTokenOutcome::Minted { .. },
            ) if id == request_id
                && choice.replica_id == replica_id
                && choice.belongs_to(&origin) =>
            {
                match copy::remember(&mut self.prefs.replica_copies, choice.clone()) {
                    Ok(()) => {
                        self.save_preferences();
                        self.replica_copy.left_behind = None;
                        self.replica_copy.error = None;
                    }
                    Err(error) => {
                        self.replica_copy.error = Some(error);
                        self.replica_revoke_quietly(choice);
                    }
                }
            }
            (Some(Pending::Stop { request_id: id }), ReplicaTokenOutcome::Revoked { remote })
                if id == request_id =>
            {
                if !remote {
                    self.notice = Some((
                        "Copying stopped on this PC. The community could not be reached, so its side of the access ends when it expires or you step down.".into(),
                        Instant::now(),
                    ));
                }
            }
            (other, outcome) => {
                self.replica_copy.pending = other;
                if matches!(outcome, ReplicaTokenOutcome::Minted { .. }) {
                    // A token nobody is waiting for is not left behind.
                    self.replica_revoke_quietly(Choice {
                        source_url: crate::community::normalize_url(&origin.url)
                            .unwrap_or_default(),
                        server_id: origin.server_id.clone(),
                        user_id: origin.user_id.clone(),
                        replica_id,
                    });
                }
            }
        }
    }

    /// A failed mint or revoke. Returns whether this page owned the request.
    pub(in crate::app) fn replica_request_failed(&mut self, request_id: u64, error: &str) -> bool {
        if self.replica_copy.cleanup.remove(&request_id) {
            return true;
        }
        match self.replica_copy.pending.take() {
            Some(Pending::Mint { request_id: id, .. }) if id == request_id => {
                self.replica_copy.error = Some(format!("The copy was not started: {error}"));
                true
            }
            Some(Pending::Stop { request_id: id }) if id == request_id => {
                self.replica_copy.error = Some(format!(
                    "Copying stopped on this PC, but its saved access could not be fully removed: {error}"
                ));
                true
            }
            other => {
                self.replica_copy.pending = other;
                false
            }
        }
    }

    /// The card. Shown only to a member whose Replica role is accepted, on a
    /// community that offers copies.
    pub(in crate::app) fn copy_card(&mut self, ui: &mut egui::Ui, stale: bool) {
        let Some(origin) = self.command_origin() else {
            return;
        };
        if !self.replica_offered() {
            return;
        }
        let choice = copy::find(&self.prefs.replica_copies, &origin).cloned();
        let view = self
            .replica_copy
            .view
            .clone()
            .filter(|view| Some(&view.choice) == choice.as_ref());
        let waiting = self.replica_copy.pending.is_some();
        let folder = choice
            .as_ref()
            .and_then(|choice| copy::copy_directory(choice).ok());
        let left = self
            .replica_copy
            .left_behind
            .clone()
            .filter(|(old, _)| choice.is_none() && old.belongs_to(&origin));
        let supported = copy::address_supported(&origin.url);
        let error = self.replica_copy.error.clone();
        let mut action: Option<Action> = None;

        card(ui, |ui| {
            ui.label(RichText::new("Copy on this PC").strong());
            match &choice {
                None => {
                    ui.label(
                        RichText::new("Save the messages and files your account can read on this PC. Relay checks for changes every 30 seconds while you are signed in to this community. Switching communities pauses the copy.")
                            .size(font_size(TEXT_BODY))
                            .color(c_muted()),
                    );
                    ui.label(
                        RichText::new("Stored in your Windows profile. End-to-end encrypted content stays encrypted; other content has no extra file encryption. Login secrets and private keys are not copied. Stop keeps files already saved. This copy cannot host the community.")
                            .size(font_size(TEXT_CAPTION))
                            .color(c_muted()),
                    );
                    if !supported {
                        ui.label(
                            RichText::new(copy::Fault::Unsupported.explain())
                                .size(font_size(TEXT_BODY))
                                .color(c_bad()),
                        );
                    }
                    ui.horizontal_wrapped(|ui| {
                        if ui
                            .add_enabled(
                                supported && !waiting && !stale,
                                primary("Keep a copy on this PC"),
                            )
                            .clicked()
                        {
                            action = Some(Action::Keep);
                        }
                        if waiting {
                            ui.spinner();
                            ui.label(
                                RichText::new("Allowing this PC to copy…")
                                    .size(font_size(TEXT_BODY))
                                    .color(c_muted()),
                            );
                        }
                    });
                    if let Some((_, path)) = &left {
                        ui.label(
                            RichText::new("Copying has stopped. The copy already on this PC stays there until you delete this folder:")
                                .size(font_size(TEXT_CAPTION))
                                .color(c_muted()),
                        );
                        ui.label(
                            RichText::new(path.display().to_string())
                                .size(font_size(TEXT_CAPTION))
                                .color(c_muted()),
                        );
                        if ui.add(secondary("Copy folder path")).clicked() {
                            action = Some(Action::CopyPath(path.display().to_string()));
                        }
                    }
                }
                Some(_) => {
                    let retry;
                    match &view {
                        Some(view) => {
                            let (tone, headline, lines) = copy::describe(view, Instant::now());
                            ui.label(RichText::new(headline).color(tone_color(ui, tone)));
                            for line in lines {
                                ui.label(
                                    RichText::new(line)
                                        .size(font_size(TEXT_BODY))
                                        .color(c_muted()),
                                );
                            }
                            retry = matches!(
                                view.phase,
                                Phase::Stopped(_)
                                    | Phase::Waiting {
                                        failed: Some(_),
                                        ..
                                    }
                            );
                            ui.ctx().request_repaint_after(Duration::from_secs(1));
                        }
                        None => {
                            let text = if waiting {
                                "Allowing this PC to copy…"
                            } else {
                                "Copying is paused. It continues when you are signed in to this community again with an active Replica role. Switching to another community pauses it."
                            };
                            ui.label(
                                RichText::new(text)
                                    .size(font_size(TEXT_BODY))
                                    .color(c_muted()),
                            );
                            retry = !waiting;
                        }
                    }
                    ui.label(
                        RichText::new("This is a copy for safekeeping. It is not a claim that this PC could take over the community.")
                            .size(font_size(TEXT_CAPTION))
                            .color(c_muted()),
                    );
                    ui.horizontal_wrapped(|ui| {
                        if retry && ui.add_enabled(!waiting, primary("Retry")).clicked() {
                            action = Some(Action::Retry);
                        }
                        if ui.add_enabled(!waiting, secondary("Stop")).clicked() {
                            action = Some(Action::Stop);
                        }
                        if waiting {
                            ui.spinner();
                        }
                    });
                    if let Some(path) = &folder {
                        ui.label(
                            RichText::new(format!(
                                "Stored in {}. End-to-end encrypted content stays encrypted; other content has no extra file encryption. Stop keeps the saved copy.",
                                path.display()
                            ))
                            .size(font_size(TEXT_CAPTION))
                            .color(c_muted()),
                        );
                    }
                }
            }
            if let Some(error) = &error {
                ui.label(
                    RichText::new(error)
                        .size(font_size(TEXT_BODY))
                        .color(c_bad()),
                );
            }
        });

        match action {
            Some(Action::Keep) => match Choice::new(&origin) {
                Ok(choice) => self.replica_start_mint(choice),
                Err(error) => self.replica_copy.error = Some(error),
            },
            Some(Action::Stop) => {
                if let Some(choice) = choice {
                    self.replica_stop(choice);
                }
            }
            Some(Action::Retry) => {
                if let Some(choice) = choice {
                    match view.as_ref().map(|view| &view.phase) {
                        // Access that is gone or unreadable needs a fresh token.
                        Some(Phase::Stopped(
                            Fault::AccessDenied | Fault::NoAccess | Fault::Vault,
                        ))
                        | None => self.replica_start_mint(choice),
                        Some(Phase::Stopped(_)) => {
                            self.replica_copy.view = None;
                        }
                        Some(_) => self.replica_copy.controller.check_now(),
                    }
                }
            }
            Some(Action::CopyPath(path)) => ui.output_mut(|output| output.copied_text = path),
            None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SERVER: &str = "6f6087b7-a4a8-4dba-b4c5-58bf76b59d6b";
    const COPY: &str = "f79a7de8-05d0-4297-b56d-7f34cc03a56d";

    fn choice(user: &str) -> Choice {
        Choice {
            source_url: "https://one.example".into(),
            server_id: SERVER.into(),
            user_id: user.into(),
            replica_id: COPY.into(),
        }
    }

    fn origin(url: &str, server: &str, user: &str) -> CommandOrigin {
        CommandOrigin {
            url: url.into(),
            server_id: server.into(),
            user_id: user.into(),
        }
    }

    #[test]
    fn a_saved_choice_resumes_only_for_the_same_account_and_community() {
        let saved = vec![choice("alice")];
        let alice = origin("https://one.example", SERVER, "alice");
        assert_eq!(
            wanted(&saved, Some(&alice), true, false, false),
            Some(&saved[0])
        );
        // Signed out, another account, another community or another address.
        assert_eq!(wanted(&saved, None, true, false, false), None);
        for other in [
            origin("https://one.example", SERVER, "bob"),
            origin("https://one.example", COPY, "alice"),
            origin("https://two.example", SERVER, "alice"),
        ] {
            assert_eq!(wanted(&saved, Some(&other), true, false, false), None);
        }
    }

    #[test]
    fn nothing_runs_without_the_capability_the_role_and_a_finished_consent() {
        let saved = vec![choice("alice")];
        let alice = origin("https://one.example", SERVER, "alice");
        assert_eq!(wanted(&saved, Some(&alice), false, false, false), None);
        assert_eq!(wanted(&saved, Some(&alice), true, true, false), None);
        assert_eq!(wanted(&saved, Some(&alice), true, false, true), None);
        // No saved choice: an accepted role and the capability alone start nothing.
        assert_eq!(wanted(&[], Some(&alice), true, false, false), None);
    }
}
