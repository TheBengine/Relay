//! Settings → Advanced → Data location. Moves happen at the next start;
//! this panel only checks a folder, schedules, cancels and confirms.
use super::{c_bad, c_good, primary, secondary};
use crate::data_location::{self as location, Bootstrap, Place};
use crate::settings_ui as controls;
use eframe::egui::{self, RichText};
use std::{path::PathBuf, sync::mpsc};

enum Done {
    Checked(Result<(Place, u64), String>),
    Deleted(Result<(), String>),
}

#[derive(Default)]
pub(super) struct Panel {
    record: Option<Result<Bootstrap, String>>,
    allow_removable: bool,
    /// A destination that passed every check, with the size to copy.
    ready: Option<(Place, u64)>,
    notice: Option<(bool, String)>,
    confirm_delete: bool,
    job: Option<mpsc::Receiver<Done>>,
}

impl Panel {
    fn reload(&mut self, defaults: &location::DefaultFolders) {
        self.record = Some(location::load(&defaults.bootstrap()));
    }

    fn finish(
        &mut self,
        defaults: &location::DefaultFolders,
        result: Result<(), String>,
        ok: &str,
    ) {
        self.notice = Some(match result {
            Ok(()) => (true, ok.to_string()),
            Err(error) => (false, error),
        });
        self.reload(defaults);
    }

    fn spawn(&mut self, ctx: &egui::Context, work: impl FnOnce() -> Done + Send + 'static) {
        let (sender, receiver) = mpsc::channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = sender.send(work());
            ctx.request_repaint();
        });
        self.job = Some(receiver);
    }

    fn check(
        &mut self,
        ctx: &egui::Context,
        defaults: &location::DefaultFolders,
        boot: &Bootstrap,
        folder: Option<PathBuf>,
    ) {
        self.ready = None;
        self.notice = None;
        let (defaults, boot, allow) = (defaults.clone(), boot.clone(), self.allow_removable);
        self.spawn(ctx, move || {
            Done::Checked((|| {
                let needed = location::size_of(&boot.active, &defaults)?;
                let probe = location::SystemProbe;
                let place = match folder {
                    Some(folder) => {
                        location::validate_folder(
                            &folder, allow, needed, &defaults, &boot, &probe, false,
                        )?;
                        location::new_folder_place(folder, allow)
                    }
                    None => {
                        location::validate_default(needed, &defaults, &boot, &probe)?;
                        Place::Default
                    }
                };
                Ok((place, needed))
            })())
        });
    }

    pub(super) fn show(&mut self, ui: &mut egui::Ui) {
        controls::section_heading(
            ui,
            "Data location",
            "Where Relay keeps messages, encryption keys, servers you host and settings on this PC.",
        );
        let Some(defaults) = location::managed() else {
            controls::description(ui, "This window uses a separate profile (--profile-dir or a storage override), so its data location is fixed. Change the location from your normal Relay.");
            return;
        };
        if let Some(job) = &self.job {
            match job.try_recv() {
                Ok(Done::Checked(result)) => {
                    self.job = None;
                    match result {
                        Ok(ready) => self.ready = Some(ready),
                        Err(error) => self.notice = Some((false, error)),
                    }
                }
                Ok(Done::Deleted(result)) => {
                    self.job = None;
                    self.confirm_delete = false;
                    self.finish(defaults, result, "The old copy was deleted.");
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => self.job = None,
            }
        }
        if self.record.is_none() {
            self.reload(defaults);
        }
        let boot = match self.record.clone() {
            Some(Ok(boot)) => boot,
            Some(Err(error)) => {
                ui.label(RichText::new(error).color(c_bad()));
                if ui.add(secondary("Try again")).clicked() {
                    self.reload(defaults);
                }
                return;
            }
            None => return,
        };
        ui.label(format!("Current location: {}", boot.active.describe()));
        if let Some(outcome) = &boot.last_result {
            ui.add(
                egui::Label::new(RichText::new(&outcome.message).color(if outcome.ok {
                    c_good()
                } else {
                    c_bad()
                }))
                .wrap(true),
            );
            if ui.add(secondary("Dismiss")).clicked() {
                let result = location::dismiss_result(defaults);
                self.finish(defaults, result, "");
                self.notice = None;
            }
        }
        if let Some((ok, text)) = &self.notice {
            ui.add(
                egui::Label::new(RichText::new(text).color(if *ok { c_good() } else { c_bad() }))
                    .wrap(true),
            );
        }
        let busy = self.job.is_some();
        if busy {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Working…");
            });
        }
        if let Some(pending) = &boot.pending {
            controls::description(ui, &format!(
                "Relay will copy your data to {} the next time it starts, check every file against the original, then switch. Your current data stays where it is. Quit Relay (including servers hosted on this PC) and start it again to move now.",
                pending.target.describe()
            ));
            if ui.add_enabled(!busy, secondary("Cancel move")).clicked() {
                let result = location::cancel(defaults);
                self.finish(defaults, result, "The move was cancelled.");
            }
            return;
        }
        if let Some(previous) = boot.previous.clone() {
            controls::description(ui, &format!(
                "The old copy is still in {}. Delete it when Relay works as expected. Until then you can move back; Relay then copies your current data there.",
                previous.describe()
            ));
            if self.confirm_delete {
                ui.label(
                    RichText::new("Delete the old copy permanently? This cannot be undone.")
                        .color(c_bad()),
                );
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(!busy, primary("Delete permanently"))
                        .clicked()
                    {
                        let defaults = defaults.clone();
                        self.spawn(ui.ctx(), move || {
                            Done::Deleted(location::delete_previous(&defaults))
                        });
                    }
                    if ui.add_enabled(!busy, secondary("Keep it")).clicked() {
                        self.confirm_delete = false;
                    }
                });
                return;
            }
            ui.horizontal_wrapped(|ui| {
                if ui
                    .add_enabled(!busy, secondary("Delete old copy…"))
                    .clicked()
                {
                    self.confirm_delete = true;
                }
                if ui.add_enabled(!busy, secondary("Move back")).clicked() {
                    let result = location::schedule(defaults, previous.clone());
                    self.finish(
                        defaults,
                        result,
                        "Relay will move your data back at the next start.",
                    );
                }
                if ui
                    .add_enabled(!busy, secondary("Forget old copy (keep files)"))
                    .clicked()
                {
                    let result = location::forget_previous(defaults);
                    self.finish(
                        defaults,
                        result,
                        "Relay no longer tracks the old copy. Its files were not deleted.",
                    );
                }
            });
            return;
        }
        ui.checkbox(
            &mut self.allow_removable,
            "Allow a removable or network drive",
        );
        controls::description(ui, "Relay will not start while such a drive is disconnected. It never starts with an empty profile instead.");
        ui.horizontal_wrapped(|ui| {
            if ui.add_enabled(!busy, secondary("Choose folder…")).clicked() {
                if let Some(folder) = rfd::FileDialog::new()
                    .set_title("Choose an empty folder for your Relay data")
                    .pick_folder()
                {
                    self.check(ui.ctx(), defaults, &boot, Some(folder));
                }
            }
            if boot.active != Place::Default
                && ui
                    .add_enabled(!busy, secondary("Use Windows default folders"))
                    .clicked()
            {
                self.check(ui.ctx(), defaults, &boot, None);
            }
        });
        if let Some((place, needed)) = self.ready.clone() {
            controls::description(ui, &format!(
                "{} is ready. Relay will copy {} there at the next start, verify every file, then switch. The current copy is kept until you delete it.",
                place.describe(),
                location::human_size(needed)
            ));
            if ui
                .add_enabled(!busy, primary("Move at next start"))
                .clicked()
            {
                let result = location::schedule(defaults, place);
                self.ready = None;
                self.finish(
                    defaults,
                    result,
                    "Move scheduled. Quit Relay and start it again to move your data.",
                );
            }
        }
    }
}
