//! "Set up your sound": a short first-time dialog built on Listen to Myself.
use super::*;
use crate::audio_setup::{AudioSetup, Heard, PreviewAction, Step};
use crate::self_listen as listen;
use crate::settings_ui as controls;

/// The offer waits for other first sign-in dialogs to settle.
const OFFER_SETTLE: Duration = Duration::from_secs(3);

impl RelayApp {
    /// Offered once after the first sign-in; always available from Voice settings.
    fn offer_audio_setup(&mut self) {
        let quiet = self.audio_setup.is_none()
            && !self.prefs.audio_setup_offered
            && self.session.is_some()
            && self.media.is_some()
            && self.voice_channel.is_none()
            && self.joining_voice.is_none()
            && !self.modal_open();
        if !quiet {
            self.audio_setup_offer_at = None;
            return;
        }
        let since = *self.audio_setup_offer_at.get_or_insert_with(Instant::now);
        if since.elapsed() >= OFFER_SETTLE {
            self.audio_setup_offer_at = None;
            self.prefs.audio_setup_offered = true;
            self.defer_preferences();
            self.open_audio_setup();
        }
    }

    /// Starts from the saved sound settings; unsaved Settings edits come back
    /// unless the setup finishes.
    pub(super) fn open_audio_setup(&mut self) {
        let mut setup = AudioSetup::default();
        if self.prefs.audio != self.audio_applied {
            setup.draft_before = Some(self.prefs.audio.clone());
            self.prefs.audio = self.audio_applied.clone();
        }
        self.audio_setup = Some(setup);
        if !self.inventory_loaded && self.inventory_rx.is_none() {
            self.refresh_devices();
        }
    }

    fn stop_setup_preview(&mut self) {
        if self.mic_test {
            if let Some(media) = &mut self.media {
                media.stop_microphone_test();
            }
            self.mic_test = false;
        }
        self.mic_meter = None;
    }

    /// Closing without Finish changes nothing that was saved.
    fn cancel_audio_setup(&mut self, setup: AudioSetup) {
        self.stop_setup_preview();
        self.prefs.audio = setup.draft_before.unwrap_or_else(|| self.audio_applied.clone());
    }

    /// Starts or stops the local preview to match the step, without retry loops.
    fn audio_setup_preview(&mut self, setup: &mut AudioSetup) {
        match setup.preview_action(self.mic_test) {
            PreviewAction::Keep => {}
            PreviewAction::Stop => self.stop_setup_preview(),
            PreviewAction::Start => {
                let draft = self.effective_audio(&self.prefs.audio);
                let Some(media) = &mut self.media else { return };
                media.set_push_to_talk_key(self.prefs.ptt_key);
                match media.preview_microphone(draft) {
                    Ok(()) => {
                        self.mic_test = true;
                        self.mic_meter = Some(listen::LevelAnalyzer::new(Instant::now()));
                        setup.preview_started(true);
                    }
                    Err(error) => {
                        self.error = Some(format!("Microphone preview: {error}"));
                        setup.preview_started(false);
                    }
                }
            }
        }
    }

    pub(super) fn audio_setup_dialog(&mut self, ctx: &egui::Context) {
        self.offer_audio_setup();
        let Some(mut setup) = self.audio_setup.take() else { return };
        if self.voice_channel.is_some() || self.joining_voice.is_some() || self.settings_open {
            // A call or the Settings editor owns the devices and the draft now.
            let call = !self.settings_open;
            self.cancel_audio_setup(setup);
            if call {
                self.notice = Some(("Sound setup closed because a call started. Nothing was changed; run it again from Settings › Voice & Video.".into(), Instant::now()));
            }
            return;
        }
        let mut open = true;
        let mut later = false;
        let mut finished = false;
        let audio_before = self.prefs.audio.clone();
        self.audio_setup_preview(&mut setup);
        desktop_ui::dialog(ctx, "Set up your sound")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(520.0)
            .show(ctx, |ui| {
                host_setup_ui::step_line(ui, &Step::LABELS, setup.step.index());
                ui.add_space(10.0);
                match setup.step {
                    Step::Microphone => self.audio_setup_microphone(ui, &mut setup),
                    Step::Output => self.audio_setup_output(ui, &mut setup),
                    Step::InputMode => self.audio_setup_input_mode(ui),
                    Step::Test => self.audio_setup_test(ui, &mut setup),
                }
                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    if setup.step != Step::Microphone && ui.add(secondary("Back")).clicked() {
                        setup.back();
                    }
                    if ui.add(primary(setup.continue_label())).clicked() {
                        finished = setup.advance();
                    }
                    if ui.add(secondary("Later")).clicked() {
                        later = true;
                    }
                });
                controls::description(ui, "Nothing you hear here is sent or recorded. Later keeps your saved settings; run this again from Settings › Voice & Video.");
            });
        open &= !later;
        if self.prefs.audio != audio_before {
            if audio_before.output_device != self.prefs.audio.output_device {
                setup.output_changed();
            }
            if audio_before.input_device != self.prefs.audio.input_device {
                setup.retry_preview();
            }
            self.sync_microphone_preview();
        }
        if finished {
            let unconfirmed = setup.unconfirmed();
            self.stop_setup_preview();
            self.apply_audio_settings();
            self.notice = Some((
                if unconfirmed.is_empty() {
                    "Sound is set up. Your choices are saved for calls.".into()
                } else {
                    format!("Sound settings saved. Not confirmed: {}. You can test again in Settings › Voice & Video.", unconfirmed.join(" and "))
                },
                Instant::now(),
            ));
        } else if !open {
            self.cancel_audio_setup(setup);
        } else {
            self.audio_setup = Some(setup);
            ctx.request_repaint_after(Duration::from_millis(50));
        }
    }

    fn audio_setup_level(&mut self, ui: &mut egui::Ui, setup: &mut AudioSetup) {
        if setup.preview_failed {
            ui.label(RichText::new("⚠ The microphone could not be opened.").color(c_bad()));
            controls::description(ui, "Choose another input device, check that no other app holds it exclusively, and allow microphone access in Windows Settings › Privacy › Microphone.");
            if ui.add(secondary("Try the microphone again")).clicked() {
                setup.retry_preview();
            }
            return;
        }
        let now = Instant::now();
        let meter = self.media.as_ref().and_then(MediaEngine::microphone_preview_meter);
        let analyzer = self.mic_meter.get_or_insert_with(|| listen::LevelAnalyzer::new(now));
        let (sample, _, _, key_held) = meter.unwrap_or_default();
        let reading = analyzer.update(sample, &self.prefs.audio, key_held, now);
        setup.observe(reading.advice);
        settings_pages::microphone_meter(ui, reading, listen::threshold_db(&self.prefs.audio), self.muted);
        // Same colours and marker as Listen to Myself in Settings.
        let color = match reading.advice {
            listen::Advice::Good => c_good(),
            listen::Advice::NoSignal | listen::Advice::Clipping(_) | listen::Advice::ActivationClosed => c_bad(),
            listen::Advice::TooQuiet | listen::Advice::TooLoud => c_accent(),
            _ => c_text(),
        };
        let prefix = if reading.advice.is_problem() { "⚠ " } else { "" };
        let text = reading.advice.text(ptt_key_name(self.prefs.ptt_key));
        ui.add(egui::Label::new(RichText::new(format!("{prefix}{text}")).color(color)).wrap(true));
    }

    fn audio_setup_microphone(&mut self, ui: &mut egui::Ui, setup: &mut AudioSetup) {
        ui.heading("Your microphone");
        controls::description(ui, "Use headphones. Speak normally; you hear yourself through the same processing as in a call.");
        controls::device_combo(ui, "setup-input", "INPUT DEVICE", &mut self.prefs.audio.input_device, &self.inventory.audio.inputs);
        settings_pages::percent_slider(ui, "INPUT VOLUME", &mut self.prefs.audio.microphone_gain, 0.0..=4.0);
        self.audio_setup_level(ui, setup);
    }

    fn audio_setup_output(&mut self, ui: &mut egui::Ui, setup: &mut AudioSetup) {
        ui.heading("Your speakers or headset");
        controls::device_combo(ui, "setup-output", "OUTPUT DEVICE", &mut self.prefs.audio.output_device, &self.inventory.audio.outputs);
        settings_pages::percent_slider(ui, "OUTPUT VOLUME", &mut self.prefs.audio.playback_volume, 0.0..=2.0);
        ui.horizontal_wrapped(|ui| {
            if ui.add(primary("Play test sound")).clicked() {
                if let Some(media) = &mut self.media {
                    if let Err(error) = media.preview_cue(AudioCue::Ringback, test_sound_settings(&self.prefs.audio)) {
                        self.error = Some(error);
                    }
                }
            }
            if ui.add(secondary("I heard it")).clicked() {
                setup.heard_test_sound = Heard::Yes;
            }
            if ui.add(secondary("I heard nothing")).clicked() {
                setup.heard_test_sound = Heard::No;
            }
        });
        let missing = self.inventory_loaded
            && self
                .prefs
                .audio
                .output_device
                .as_ref()
                .is_some_and(|name| !self.inventory.audio.outputs.contains(name));
        let problem = crate::voice_problem::diagnose(&crate::voice_problem::Inputs {
            input_volume: 1.0,
            output_volume: self.prefs.audio.playback_volume,
            output_device_missing: missing,
            test_sound_not_heard: setup.heard_test_sound == Heard::No,
            ..Default::default()
        });
        match (problem, setup.heard_test_sound) {
            (Some(problem), _) => {
                ui.add(egui::Label::new(RichText::new(format!("⚠ {} · {}", problem.title(), problem.next_step())).color(c_bad())).wrap(true));
            }
            (None, Heard::Yes) => {
                ui.label(RichText::new("Output works.").color(c_good()));
            }
            _ => controls::description(ui, "Play the sound, then tell Relay whether you heard it."),
        }
    }

    fn audio_setup_input_mode(&mut self, ui: &mut egui::Ui) {
        ui.heading("How others hear you");
        for (mode, label, help) in [
            (TransmissionMode::VoiceActivation, "Voice activation (recommended)", "Relay sends when you speak and stays quiet otherwise."),
            (TransmissionMode::PushToTalk, "Push-to-talk", "You are heard only while you hold the talk key."),
            (TransmissionMode::Continuous, "Always on", "Everything your microphone picks up is sent. Best with a quiet room."),
        ] {
            ui.radio_value(&mut self.prefs.audio.transmission, mode, label);
            controls::description(ui, help);
        }
        if self.prefs.audio.transmission == TransmissionMode::PushToTalk {
            ui.label(format!("Talk key: {}. Change it in Settings › Keybinds.", ptt_key_name(self.prefs.ptt_key)));
        }
    }

    fn audio_setup_test(&mut self, ui: &mut egui::Ui, setup: &mut AudioSetup) {
        ui.heading("Short test");
        controls::description(ui, match self.prefs.audio.transmission {
            TransmissionMode::PushToTalk => "Hold the talk key and say a sentence. You hear exactly what others would hear.",
            _ => "Say a sentence. You hear exactly what others would hear, including when Relay starts and stops sending.",
        });
        self.audio_setup_level(ui, setup);
    }
}

/// The built-in ringback at full event volume, whatever the Sounds page says:
/// the output test must not be silenced by an event setting.
fn test_sound_settings(audio: &AudioSettings) -> AudioSettings {
    let mut settings = audio.clone();
    settings.sounds_enabled = true;
    settings.sound_volume = 1.0;
    *settings.event_sounds.get_mut(AudioCue::Ringback) = crate::sound_customization::EventSound::default();
    settings
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_output_test_sound_plays_even_when_event_sounds_are_off() {
        let mut audio = AudioSettings { sounds_enabled: false, sound_volume: 0.0, ..Default::default() };
        let ringback = audio.event_sounds.get_mut(AudioCue::Ringback);
        ringback.enabled = false;
        ringback.volume = 0.0;
        let test = test_sound_settings(&audio);
        assert!(crate::sound_customization::audible(&test, AudioCue::Ringback));
        assert!(!crate::sound_customization::audible(&audio, AudioCue::Ringback));
        assert_eq!(test.output_device, audio.output_device, "plays on the chosen output");
    }
}
