//! R4.5: the community's message-protection mode in the interface.
//!
//! The owner can switch end-to-end encryption off for a community. Nobody else
//! is switched with it: each person sees a plain notice and chooses for
//! themselves, and their client sends unprotected text only for the revision
//! they accepted (`crate::protection`). The mode and the person's own standing
//! are shown above the composer for as long as the mode is not the default.
use super::*;
use crate::protection::{consent_for, standing, Standing};
use crate::settings_ui as controls;
use relay_protocol::{CommunityProtection, ProtectionLevel};

#[derive(Default)]
pub(super) struct ProtectionUi {
    /// The owner opened the confirmation for turning encryption off.
    confirm_off: bool,
    /// An owner change is in flight; the bootstrap that follows settles it.
    pending: Option<Instant>,
    /// The consent last handed to the network worker, so it is sent only when it changes.
    last_sent: Option<(String, Option<u64>)>,
    /// The person chose to keep encryption for this revision: hide the banner, not the setting.
    kept: Option<(String, u64)>,
}

impl ProtectionUi {
    pub(super) fn settled(&mut self) {
        self.pending = None;
    }

    pub(super) fn forget_connection(&mut self) {
        self.last_sent = None;
        self.pending = None;
        self.confirm_off = false;
    }
}

impl RelayApp {
    fn announced_protection(&self) -> Option<(String, CommunityProtection, bool)> {
        let b = self.bootstrap.as_ref()?;
        Some((
            b.server_id.clone(),
            b.protection.clone(),
            b.user.role == "admin",
        ))
    }

    fn saved_community_index(&self, server_id: &str) -> Option<usize> {
        self.prefs.communities.iter().position(|c| {
            if c.server_id.is_empty() {
                c.url == self.login.url
            } else {
                c.server_id == server_id
            }
        })
    }

    fn accepted_revision(&self, server_id: &str) -> Option<u64> {
        self.saved_community_index(server_id)
            .and_then(|i| self.prefs.communities[i].open_protection_accepted)
    }

    /// Tells the network worker which revision, if any, this person accepted.
    pub(super) fn sync_open_consent(&mut self) {
        let Some((server_id, protection, _)) = self.announced_protection() else {
            return;
        };
        let value = consent_for(&protection, self.accepted_revision(&server_id));
        let key = (server_id, value);
        if self.protection_ui.last_sent.as_ref() == Some(&key) {
            return;
        }
        let Some(origin) = self.command_origin() else {
            return;
        };
        self.send(Command::OpenConsent {
            origin,
            revision: key.1,
        });
        self.protection_ui.last_sent = Some(key);
    }

    pub(super) fn record_open_consent(&mut self, accepted: Option<u64>) {
        let Some((server_id, protection, _)) = self.announced_protection() else {
            return;
        };
        let Some(index) = self.saved_community_index(&server_id) else {
            return;
        };
        if protection.level == ProtectionLevel::Open
            && consent_for(&protection, accepted).is_none()
            && (self.voice_channel.is_some() || self.joining_voice.is_some())
        {
            // Stop local capture before handing withdrawal to the worker.
            // Cancelling the request also rejects any in-flight join response.
            self.stop_media();
        }
        self.prefs.communities[index].open_protection_accepted = accepted;
        self.defer_preferences();
        self.sync_open_consent();
    }

    fn change_protection(&mut self, level: ProtectionLevel, expected_revision: u64) {
        let Some(origin) = self.command_origin() else {
            return;
        };
        self.protection_ui.pending = Some(Instant::now());
        self.protection_ui.confirm_off = false;
        self.send(Command::SetProtection {
            origin,
            level,
            expected_revision,
        });
    }

    /// Above the composer. Silent in the default, encrypted mode.
    pub(super) fn protection_notice(&mut self, ui: &mut egui::Ui) {
        self.protection_notice_for(ui, false);
    }

    pub(super) fn call_needs_open_consent(&self) -> bool {
        self.announced_protection()
            .is_some_and(|(server, protection, _)| {
                standing(&protection, self.accepted_revision(&server)) == Standing::NeedsDecision
            })
    }

    pub(super) fn call_protection_notice(&mut self, ui: &mut egui::Ui) {
        self.protection_notice_for(ui, true);
    }

    fn protection_notice_for(&mut self, ui: &mut egui::Ui, for_call: bool) {
        // Runs every frame: nothing is copied in the usual, encrypted mode.
        if self
            .bootstrap
            .as_ref()
            .is_none_or(|b| b.protection.level != ProtectionLevel::Open)
        {
            return;
        }
        let Some((server_id, protection, _)) = self.announced_protection() else {
            return;
        };
        let accepted = self.accepted_revision(&server_id);
        let mut accept = false;
        let mut keep = false;
        let mut review = false;
        match standing(&protection, accepted) {
            Standing::Protected => return,
            Standing::Accepted => {
                ui.horizontal_wrapped(|ui| {
                    ui.add(
                        egui::Label::new(
                            RichText::new("Not end-to-end encrypted")
                                .strong()
                                .color(c_bad()),
                        )
                        .wrap(true),
                    );
                    ui.add(
                        egui::Label::new(
                            RichText::new(
                                "Messages, files and calls here can be read by whoever runs the server.",
                            )
                            .color(c_muted()),
                        )
                        .wrap(true),
                    );
                    review = ui.add(secondary("Review")).clicked();
                });
                ui.add_space(8.0);
            }
            Standing::NeedsDecision => {
                if !for_call
                    && self.protection_ui.kept.as_ref()
                        == Some(&(server_id.clone(), protection.revision))
                {
                    return;
                }
                egui::Frame::none()
                    .fill(c_panel())
                    .stroke(Stroke::new(1.0_f32, c_line()))
                    .rounding(8.0)
                    .inner_margin(Margin::symmetric(12.0, 10.0))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.add(egui::Label::new(RichText::new(if for_call {
                            "Before you join voice"
                        } else { "The owner turned off end-to-end encryption" }).strong()).wrap(true));
                        ui.add(egui::Label::new(RichText::new(if for_call {
                            "Calls require accepting this server's choice. If you accept, the host can read your messages, files and calls. iroh still encrypts the connection."
                        } else { "Your messages stay encrypted until you accept. If you accept, messages, files and calls in this community can be read by whoever runs the server." }).color(c_muted())).wrap(true));
                        if for_call {
                            ui.add(egui::Label::new(RichText::new("You can keep using encrypted text. Accepting does not join a call or open your microphone.").color(c_muted())).wrap(true));
                        }
                        ui.add_space(6.0);
                        ui.horizontal_wrapped(|ui| {
                            if !for_call {
                                keep = ui.add(primary("Keep encryption")).clicked();
                            }
                            accept = ui.add(secondary("Accept and continue")).clicked();
                            review = ui.add(secondary("Details")).clicked();
                        });
                    });
                ui.add_space(8.0);
            }
        }
        if accept {
            self.record_open_consent(Some(protection.revision));
        }
        if keep {
            self.protection_ui.kept = Some((server_id, protection.revision));
        }
        if review {
            self.settings_tab = SettingsTab::Encryption;
            self.settings_open = true;
        }
    }

    /// A section on the Encryption page: the mode, this person's choice, and the owner's switch.
    pub(super) fn protection_section(&mut self, ui: &mut egui::Ui) {
        let Some((server_id, protection, owner)) = self.announced_protection() else {
            return;
        };
        if self
            .protection_ui
            .pending
            .is_some_and(|at| at.elapsed() >= Duration::from_secs(10))
        {
            self.protection_ui.pending = None;
        }
        let accepted = self.accepted_revision(&server_id);
        let state = standing(&protection, accepted);
        let busy = self.protection_ui.pending.is_some();
        let mut accept = false;
        let mut withdraw = false;
        let mut ask_off = false;
        let mut cancel = false;
        let mut switch_off = false;
        let mut switch_on = false;
        controls::divider(ui);
        controls::section_heading(
            ui,
            "Message protection",
            "Whether messages, files and calls in this community use end-to-end encryption.",
        );
        match protection.level {
            ProtectionLevel::Protected => {
                ui.label(
                    RichText::new("End-to-end encrypted")
                        .strong()
                        .color(c_good()),
                );
                controls::description(
                    ui,
                    "Messages are encrypted on each device. The server host cannot read them.",
                );
            }
            ProtectionLevel::Open => {
                ui.label(
                    RichText::new("Not end-to-end encrypted")
                        .strong()
                        .color(c_bad()),
                );
                controls::description(ui, "The server manages protection for messages, files and calls in this mode and can read them. Calls do not need device approval. iroh still encrypts the connection. Earlier encrypted history stays protected.");
                match state {
                    Standing::Accepted => {
                        controls::description(
                            ui,
                            "You accepted this server without end-to-end encryption.",
                        );
                        withdraw = ui.add(secondary("Keep my messages encrypted")).clicked();
                        controls::description(ui, "This keeps your new messages encrypted and ends any current call here. To join again, you must accept the server's choice.");
                    }
                    _ => {
                        controls::description(ui, "Your messages stay encrypted until you agree.");
                        accept = ui.add(secondary("Accept and continue")).clicked();
                    }
                }
            }
        }
        if owner {
            ui.add_space(8.0);
            match protection.level {
                ProtectionLevel::Protected => {
                    if !self.protection_ui.confirm_off {
                        ask_off = ui
                            .add_enabled(!busy, secondary("Turn off end-to-end encryption…"))
                            .clicked();
                    } else {
                        egui::Frame::none().fill(c_panel()).stroke(Stroke::new(1.0_f32, c_bad())).rounding(8.0).inner_margin(Margin::same(16.0)).show(ui, |ui| {
                            controls::section_heading(ui, "Turn off end-to-end encryption?", "This is for the whole community.");
                            controls::description(ui, "Nobody is switched automatically. Each person is asked, and until they agree their messages stay encrypted.");
                            controls::description(ui, "Messages, files and calls in this mode can be read by whoever runs the server. Earlier encrypted history stays encrypted. You can turn encryption back on at any time.");
                            ui.horizontal_wrapped(|ui| {
                                switch_off = ui.add_enabled(!busy, egui::Button::new(RichText::new("Turn off for this community").color(c_bad()))).clicked();
                                cancel = ui.add(secondary("Cancel")).clicked();
                            });
                        });
                    }
                }
                ProtectionLevel::Open => {
                    switch_on = ui
                        .add_enabled(!busy, secondary("Turn end-to-end encryption back on"))
                        .clicked();
                }
            }
            if busy {
                ui.label("Waiting for the server…");
            }
        }
        if ask_off {
            self.protection_ui.confirm_off = true;
        }
        if cancel {
            self.protection_ui.confirm_off = false;
        }
        if accept {
            self.record_open_consent(Some(protection.revision));
        }
        if withdraw {
            self.record_open_consent(None);
        }
        if switch_off {
            self.change_protection(ProtectionLevel::Open, protection.revision);
        }
        if switch_on {
            self.change_protection(ProtectionLevel::Protected, protection.revision);
        }
    }
}
