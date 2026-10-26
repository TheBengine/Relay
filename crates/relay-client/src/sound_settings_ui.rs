//! One place to choose and preview all event sounds. File work stays off the UI thread.
use super::*;
use crate::settings_ui as controls;
use crate::sound_customization::{self, EventSounds, ImportedSound, SoundSource};
use std::sync::mpsc::Sender;

enum ResultMessage {
    Hydrated(EventSounds, Vec<String>),
    Imported(AudioCue, u64, Result<Option<ImportedSound>, String>),
}

#[derive(Default)]
struct ImportGuard {
    generation: u64,
    pending: Option<(AudioCue, u64)>,
}

impl ImportGuard {
    fn start(&mut self, cue: AudioCue) -> Option<u64> {
        if self.pending.is_some() {
            return None;
        }
        self.generation = self.generation.wrapping_add(1);
        self.pending = Some((cue, self.generation));
        Some(self.generation)
    }

    fn discard(&mut self, cue: Option<AudioCue>) {
        if self
            .pending
            .is_some_and(|(pending, _)| cue.is_none_or(|cue| cue == pending))
        {
            self.generation = self.generation.wrapping_add(1);
        }
    }

    fn finish(&mut self, cue: AudioCue, generation: u64) -> bool {
        if self.pending != Some((cue, generation)) {
            return false;
        }
        self.pending = None;
        self.generation == generation
    }
}

pub(super) struct State {
    root: PathBuf,
    tx: Sender<ResultMessage>,
    rx: Receiver<ResultMessage>,
    loading: bool,
    import_guard: ImportGuard,
    feedback: Option<String>,
    warnings: Vec<String>,
}

impl State {
    pub(super) fn new(root: PathBuf, mut sounds: EventSounds, ctx: egui::Context) -> Self {
        let (tx, rx) = mpsc::channel();
        let worker_tx = tx.clone();
        let worker_root = root.clone();
        std::thread::spawn(move || {
            let warnings = sound_customization::hydrate(&mut sounds, &worker_root);
            let _ = worker_tx.send(ResultMessage::Hydrated(sounds, warnings));
            ctx.request_repaint();
        });
        Self {
            root,
            tx,
            rx,
            loading: true,
            import_guard: ImportGuard::default(),
            feedback: None,
            warnings: Vec::new(),
        }
    }

    fn import(&mut self, cue: AudioCue, ctx: egui::Context) {
        let Some(generation) = self.import_guard.start(cue) else {
            return;
        };
        self.feedback = None;
        let tx = self.tx.clone();
        let root = self.root.clone();
        std::thread::spawn(move || {
            let result = rfd::FileDialog::new()
                .set_title(format!("Choose a sound for {}", cue.label()))
                .set_directory(root.parent().unwrap_or(&root))
                .add_filter("WAV sound (up to 5 seconds)", &["wav"])
                .pick_file()
                .map(|path| sound_customization::import_wav(&root, &path))
                .transpose();
            let _ = tx.send(ResultMessage::Imported(cue, generation, result));
            ctx.request_repaint();
        });
    }

    pub(super) fn discard_import(&mut self, cue: Option<AudioCue>) {
        self.import_guard.discard(cue);
    }
}

fn source_label(cue: AudioCue, source: &SoundSource) -> String {
    match source {
        SoundSource::Default => format!("Default · {}", cue.label()),
        SoundSource::BuiltIn { cue: other } => other.label().to_owned(),
        SoundSource::Custom { name, .. } => name.clone(),
    }
}

impl RelayApp {
    pub(super) fn poll_sound_settings(&mut self) {
        let mut cache_changed = false;
        while let Ok(result) = self.sound_settings.rx.try_recv() {
            match result {
                ResultMessage::Hydrated(sounds, warnings) => {
                    self.sound_settings.loading = false;
                    self.sound_settings.warnings = warnings;
                    self.prefs.audio.event_sounds.merge_loaded(&sounds);
                    self.audio_applied.event_sounds.merge_loaded(&sounds);
                    cache_changed = true;
                }
                ResultMessage::Imported(cue, generation, result) => {
                    if !self.sound_settings.import_guard.finish(cue, generation) {
                        continue;
                    }
                    match result {
                        Ok(Some(imported)) => {
                            let source = self.prefs.audio.event_sounds.install(imported);
                            self.audio_applied
                                .event_sounds
                                .merge_loaded(&self.prefs.audio.event_sounds);
                            self.prefs.audio.event_sounds.get_mut(cue).source = source;
                            self.sound_settings.feedback = Some(format!(
                                "Sound imported for {}.",
                                cue.label()
                            ));
                            cache_changed = true;
                        }
                        Ok(None) => {}
                        Err(error) => self.sound_settings.feedback = Some(error),
                    }
                }
            }
        }
        // Update only the runtime cache. Unsaved choices remain preview-only.
        if cache_changed {
            if let Some(media) = &mut self.media {
                media.configure(self.audio_applied.clone());
            }
            self.sync_call_alert_preferences();
        }
    }

    pub(super) fn sounds_page(&mut self, ui: &mut egui::Ui) {
        controls::page_heading(
            ui,
            "Sounds",
            "Make every notification and call sound your own.",
        );
        controls::switch_row(
            ui,
            "event-sounds-enabled",
            "Enable Sounds",
            "Play event sounds. Notification rules and Do Not Disturb still apply.",
            &mut self.prefs.audio.sounds_enabled,
        );
        settings_pages::percent_slider(
            ui,
            "MASTER VOLUME",
            &mut self.prefs.audio.sound_volume,
            0.0..=1.0,
        );
        controls::description(ui, "Preview listens to your unsaved choices through your selected output device. Save Changes applies them everywhere in Relay.");
        controls::description(ui, "Choose a built-in sound or import your own WAV file, up to 5 seconds and 10 MB. Imported sounds are copied into Relay, so moving the original file is fine.");
        if self.sound_settings.loading {
            controls::description(ui, "Loading your saved sounds…");
        }
        if let Some((cue, _)) = self.sound_settings.import_guard.pending {
            controls::description(ui, &format!("Choosing a sound for {}…", cue.label()));
        }
        if let Some(feedback) = &self.sound_settings.feedback {
            ui.add(egui::Label::new(feedback).wrap(true));
        }
        if !self.sound_settings.warnings.is_empty() {
            egui::CollapsingHeader::new("Some saved sounds could not be loaded").show(ui, |ui| {
                for warning in &self.sound_settings.warnings {
                    ui.add(egui::Label::new(warning).wrap(true));
                }
                controls::description(
                    ui,
                    "The default sound is used until you choose a replacement.",
                );
            });
        }
        controls::divider(ui);

        let mut preview = None;
        let mut import = None;
        let can_preview = self.media.is_some()
            && self.prefs.audio.sounds_enabled
            && self.prefs.audio.sound_volume > 0.0;
        for (title, cues) in [
            ("Messages", &[AudioCue::Message][..]),
            (
                "Calls",
                &[AudioCue::Ring, AudioCue::Ringback, AudioCue::CallEnd][..],
            ),
            ("Voice Channels", &[AudioCue::Join, AudioCue::Leave][..]),
            (
                "Microphone & Headphones",
                &[
                    AudioCue::Mute,
                    AudioCue::Unmute,
                    AudioCue::Deafen,
                    AudioCue::Undeafen,
                ][..],
            ),
        ] {
            ui.label(RichText::new(title).strong());
            ui.add_space(6.0);
            for &cue in cues {
                ui.push_id(("event-sound", cue), |ui| {
                    let mut sound = self.prefs.audio.event_sounds.get(cue);
                    let loaded = self.prefs.audio.event_sounds.is_loaded(&sound.source);
                    egui::Frame::none()
                        .fill(c_panel())
                        .rounding(6.0)
                        .inner_margin(Margin::same(12.0))
                        .show(ui, |ui| {
                            ui.horizontal_wrapped(|ui| {
                                ui.checkbox(&mut sound.enabled, cue.label());
                                ui.add(egui::Slider::new(&mut sound.volume, 0.0..=1.0)
                                    .text("Volume")
                                    .custom_formatter(|value, _| format!("{:.0}%", value * 100.0))
                                    .custom_parser(|value| value.trim_end_matches('%').trim().parse::<f64>().ok().map(|number| number / 100.0)));
                            });
                            ui.horizontal_wrapped(|ui| {
                                egui::ComboBox::from_id_source("source")
                                    .width(210.0)
                                    .selected_text(source_label(cue, &sound.source))
                                    .show_ui(ui, |ui| {
                                        ui.selectable_value(&mut sound.source, SoundSource::Default, format!("Default · {}", cue.label()));
                                        for builtin in AudioCue::ALL {
                                            ui.selectable_value(&mut sound.source, SoundSource::BuiltIn { cue: builtin }, builtin.label());
                                        }
                                    });
                                if ui.add_enabled(can_preview && sound.enabled && sound.volume > 0.0, secondary("Preview")).clicked() {
                                    preview = Some(cue);
                                }
                                if ui.add_enabled(self.sound_settings.import_guard.pending.is_none(), secondary("Choose WAV…")).clicked() {
                                    import = Some(cue);
                                }
                                if ui.add(secondary("Reset")).on_hover_text("Restore this event's default sound and volume").clicked() {
                                    self.sound_settings.discard_import(Some(cue));
                                    sound = Default::default();
                                }
                            });
                            if !loaded && matches!(sound.source, SoundSource::Custom { .. }) && !self.sound_settings.loading {
                                controls::description(ui, "This sound is unavailable. Relay will use the default until you choose a replacement.");
                            }
                        });
                    if sound != self.prefs.audio.event_sounds.get(cue) {
                        self.sound_settings.discard_import(Some(cue));
                        *self.prefs.audio.event_sounds.get_mut(cue) = sound;
                    }
                });
                ui.add_space(8.0);
            }
            ui.add_space(10.0);
        }
        if ui.add(secondary("Reset All Sounds")).clicked() {
            self.sound_settings.discard_import(None);
            self.prefs.audio.event_sounds = Default::default();
            self.prefs.audio.sounds_enabled = true;
            self.prefs.audio.sound_volume = AudioSettings::default().sound_volume;
            self.sound_settings.feedback =
                Some("Default sounds restored.".into());
        }
        if let Some(cue) = import {
            self.sound_settings.import(cue, ui.ctx().clone());
        }
        if let (Some(cue), Some(media)) = (preview, &mut self.media) {
            if let Err(error) = media.preview_cue(cue, self.prefs.audio.clone()) {
                self.sound_settings.feedback = Some(error);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delayed_import_cannot_restore_discarded_or_replaced_choices() {
        let mut imports = ImportGuard::default();
        let first = imports.start(AudioCue::Message).unwrap();
        imports.discard(None); // Settings Reset or Reset All Sounds.
        assert!(
            imports.start(AudioCue::Join).is_none(),
            "only one native chooser may be open"
        );
        assert!(!imports.finish(AudioCue::Message, first));
        let second = imports.start(AudioCue::Join).unwrap();
        imports.discard(Some(AudioCue::Leave)); // Another event is independent.
        assert!(imports.finish(AudioCue::Join, second));
        let third = imports.start(AudioCue::Ring).unwrap();
        imports.discard(Some(AudioCue::Ring)); // Reset or a newer edit in the same row.
        assert!(!imports.finish(AudioCue::Ring, third));
        let fourth = imports.start(AudioCue::Ring).unwrap();
        assert!(
            !imports.finish(AudioCue::Ring, third),
            "a stale result cannot end a newer import"
        );
        assert!(imports.finish(AudioCue::Ring, fourth));
    }
}
