use super::*;
use crate::recovery::{BackupResult, HistoryArchive};
use zeroize::Zeroizing;

#[derive(Default)]
pub(in crate::app) struct HistoryUi {
    password: Zeroizing<String>,
    repeat: Zeroizing<String>,
    pending: Option<u64>,
    result: Option<Result<String, String>>,
    archive: Option<HistoryArchive>,
    query: String,
    indexed_query: Option<String>,
    matches: Vec<usize>,
    page: usize,
}

impl HistoryUi {
    pub(in crate::app) fn finish(&mut self, request_id: u64, result: Result<BackupResult, String>) {
        if self.pending != Some(request_id) {
            return;
        }
        self.pending = None;
        self.result = Some(match result {
            Ok(BackupResult::Saved(count)) => Ok(format!(
                "History backup saved · {count} records. Keep its passphrase somewhere safe."
            )),
            Ok(BackupResult::Opened(archive)) => {
                let count = archive.entries.len();
                self.archive = Some(archive);
                self.indexed_query = None;
                self.page = 0;
                Ok(format!("History backup opened · {count} records. Your current device and conversations are unchanged."))
            }
            Err(error) => Err(error),
        });
    }
}

impl RelayApp {
    pub(super) fn history_backup_page(&mut self, ui: &mut egui::Ui) {
        controls::section_heading(ui, "History backup", "Keep a private copy of messages this PC has already opened. Its separate passphrase lets you read the backup on another PC, even while signed out.");
        controls::description(ui, "A backup shows saved messages. It does not confirm a new device or let it send messages. Attached files must still be available separately.");
        if self.command_origin().is_none() {
            controls::description(
                ui,
                "You can open an existing backup here. Connect to a community to create a new one.",
            );
        }
        let can_export =
            self.command_origin().is_some() && !self.encryption_ui.status.own_device_id.is_empty();
        self.history_backup_controls(ui, can_export);
    }

    pub(super) fn history_backup_advanced(&self, ui: &mut egui::Ui) {
        if let Some(usage) = &self.encryption_ui.status.history {
            let fraction = (usage.archived_bytes as f64 / usage.byte_limit as f64)
                .max(usage.segments as f64 / usage.segment_limit as f64);
            ui.add(egui::ProgressBar::new(fraction as f32).text(format!(
                "Protected archive: {:.1} MiB of {:.1} MiB · {} records",
                usage.archived_bytes as f64 / (1024.0 * 1024.0),
                usage.byte_limit as f64 / (1024.0 * 1024.0),
                usage.records
            )));
            controls::description(ui, &format!("{} of {} archive segments on this device. The active device checkpoint and retired-device archives are separate.", usage.segments, usage.segment_limit));
            if fraction >= 0.8 {
                ui.add(egui::Label::new(RichText::new("Your protected history archive is nearly full. Make a history backup before moving to a new device. Relay preserves existing messages and keys when the limit is reached.").color(c_bad())).wrap(true));
            }
        }
    }

    fn history_backup_controls(&mut self, ui: &mut egui::Ui, can_export: bool) {
        let history = &mut self.encryption_ui.history;
        let available = history.pending.is_none();
        ui.add_enabled_ui(available, |ui| {
            ui.label("Backup passphrase");
            ui.add_sized(
                [ui.available_width(), 34.0],
                egui::TextEdit::singleline(&mut *history.password)
                    .password(true)
                    .char_limit(1024)
                    .hint_text("At least 12 characters · use a long, unique passphrase"),
            );
            if can_export {
                ui.label("Repeat passphrase to create a backup");
                ui.add_sized(
                    [ui.available_width(), 34.0],
                    egui::TextEdit::singleline(&mut *history.repeat)
                        .password(true)
                        .char_limit(1024)
                        .hint_text("Type the same passphrase again"),
                );
            }
        });
        let valid = crate::recovery::password_valid(&history.password);
        let mut export = false;
        let mut open = false;
        ui.horizontal_wrapped(|ui| {
            export = ui
                .add_enabled(
                    available && can_export && valid && history.password == history.repeat,
                    primary("Create History Backup"),
                )
                .clicked();
            open = ui
                .add_enabled(available && valid, secondary("Open History Backup"))
                .clicked();
            if !available {
                ui.spinner();
                ui.label("Protecting or opening history…");
            }
        });
        if let Some(result) = &history.result {
            let (text, color) = match result {
                Ok(text) => (text, c_good()),
                Err(text) => (text, c_bad()),
            };
            ui.add(egui::Label::new(RichText::new(text).color(color)).wrap(true));
        }
        if export || open {
            let dialog =
                rfd::FileDialog::new().add_filter("Relay history backup", &["relay-history"]);
            let path = if export {
                dialog
                    .set_file_name("Relay-history.relay-history")
                    .save_file()
            } else {
                dialog.pick_file()
            };
            if let Some(path) = path {
                let origin = self.command_origin();
                let request_id = self.next_request();
                let history = &mut self.encryption_ui.history;
                history.pending = Some(request_id);
                history.result = None;
                let password = std::mem::take(&mut history.password);
                history.repeat = Zeroizing::new(String::new());
                if let Err(error) = self.network.send(Command::HistoryBackup {
                    origin,
                    path,
                    password,
                    export,
                    request_id,
                }) {
                    self.encryption_ui.history.finish(request_id, Err(error));
                }
            }
        }
        self.history_archive_view(ui);
    }

    fn history_archive_view(&mut self, ui: &mut egui::Ui) {
        let history = &mut self.encryption_ui.history;
        let Some(archive) = &history.archive else {
            return;
        };
        ui.add_space(12.0);
        controls::section_heading(ui, "Saved History", "This view comes from the backup you opened. It is separate from live chat and does not verify new messages or devices.");
        controls::description(ui, &format!("Created: {}", archive.created_at));
        ui.collapsing("Advanced backup details", |ui| {
            controls::description(
                ui,
                &format!(
                    "Community ID: {} · Account ID: {}",
                    archive.source.server_id, archive.source.user_id
                ),
            );
        });
        let close = ui.add(secondary("Close Saved History")).clicked();
        ui.add_sized(
            [ui.available_width(), 34.0],
            egui::TextEdit::singleline(&mut history.query)
                .char_limit(256)
                .hint_text("Search saved text, conversation or author"),
        );
        if history.indexed_query.as_ref() != Some(&history.query) {
            let query = history.query.to_lowercase();
            history.matches = archive
                .entries
                .iter()
                .enumerate()
                .filter_map(|(i, row)| {
                    (row.envelope.action.is_none()
                        && (query.is_empty()
                            || row.envelope.content.to_lowercase().contains(&query)
                            || row.envelope.scope.to_lowercase().contains(&query)
                            || row.author.user_id.to_lowercase().contains(&query)))
                    .then_some(i)
                })
                .collect();
            history.indexed_query = Some(history.query.clone());
            history.page = 0;
        }
        let start = history.page * 50;
        let end = (start + 50).min(history.matches.len());
        if history.matches.is_empty() {
            controls::description(ui, "No saved messages match this search.");
        }
        for &index in &history.matches[start..end] {
            let row = &archive.entries[index];
            egui::Frame::none()
                .fill(c_card())
                .rounding(8.0)
                .inner_margin(Margin::same(12.0))
                .show(ui, |ui| {
                    controls::description(ui, &format!("Saved message {}", row.sequence));
                    ui.add(egui::Label::new(&row.envelope.content).wrap(true));
                    for file in &row.envelope.attachments {
                        controls::description(ui, &format!("Attachment: {}", file.name));
                    }
                    ui.collapsing("Advanced message details", |ui| {
                        controls::description(
                            ui,
                            &format!(
                                "Author ID: {} · Conversation ID: {}",
                                row.author.user_id, row.envelope.scope
                            ),
                        );
                    });
                });
            ui.add_space(8.0);
        }
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(start > 0, secondary("Previous Saved Messages"))
                .clicked()
            {
                history.page = history.page.saturating_sub(1);
            }
            ui.label(format!(
                "{}–{} of {}",
                if history.matches.is_empty() {
                    0
                } else {
                    start + 1
                },
                end,
                history.matches.len()
            ));
            if ui
                .add_enabled(
                    end < history.matches.len(),
                    secondary("Next Saved Messages"),
                )
                .clicked()
            {
                history.page += 1;
            }
        });
        if close {
            history.archive = None;
            history.matches.clear();
            history.indexed_query = None;
            history.query.clear();
            history.page = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stale_results_cannot_replace_history_or_clear_a_new_request() {
        let mut history = HistoryUi {
            pending: Some(19),
            ..Default::default()
        };
        history.finish(18, Ok(BackupResult::Saved(100)));
        assert_eq!(history.pending, Some(19));
        assert!(history.result.is_none());
        history.finish(19, Err("owned failure".into()));
        assert!(history.pending.is_none());
        assert_eq!(
            history.result.as_ref().unwrap().as_ref().unwrap_err(),
            "owned failure"
        );
        history.finish(19, Ok(BackupResult::Saved(100)));
        assert_eq!(
            history.result.as_ref().unwrap().as_ref().unwrap_err(),
            "owned failure"
        );
    }
}
