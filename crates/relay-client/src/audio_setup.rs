//! First-time sound setup: microphone → output → input mode → short local
//! test. Only the step logic lives here; the dialog draws it.
use crate::audio_settings::AudioSettings;
use crate::self_listen::Advice;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    Microphone,
    Output,
    InputMode,
    Test,
}

impl Step {
    pub(crate) const ALL: [Step; 4] = [Step::Microphone, Step::Output, Step::InputMode, Step::Test];
    pub(crate) const LABELS: [&'static str; 4] = ["Microphone", "Output", "Input mode", "Test"];

    pub(crate) fn index(self) -> usize {
        Self::ALL.iter().position(|step| *step == self).unwrap_or(0)
    }

    /// The local preview runs on the microphone and test steps only.
    pub(crate) fn listens(self) -> bool {
        matches!(self, Step::Microphone | Step::Test)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Heard {
    Unknown,
    Yes,
    No,
}

#[derive(Clone, Debug)]
pub(crate) struct AudioSetup {
    pub step: Step,
    /// The microphone reached a good level at least once.
    pub microphone_good: bool,
    pub heard_test_sound: Heard,
    /// The setup started the preview and it is expected to be running.
    pub previewing: bool,
    /// The preview failed; it is retried only on request or a new device.
    pub preview_failed: bool,
    /// Unsaved Settings edits from before the setup, restored unless it finishes.
    pub draft_before: Option<AudioSettings>,
}

impl Default for AudioSetup {
    fn default() -> Self {
        Self {
            step: Step::Microphone,
            microphone_good: false,
            heard_test_sound: Heard::Unknown,
            previewing: false,
            preview_failed: false,
            draft_before: None,
        }
    }
}

/// What the dialog should do with the local preview this frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PreviewAction {
    Keep,
    Start,
    Stop,
}

impl AudioSetup {
    /// `running` is whether the preview is running now. A preview that this
    /// setup started and that stopped by itself has failed: never restart it
    /// every frame.
    pub(crate) fn preview_action(&mut self, running: bool) -> PreviewAction {
        if self.previewing && !running {
            self.previewing = false;
            self.preview_failed = true;
        }
        if !self.step.listens() {
            self.previewing = false;
            return if running { PreviewAction::Stop } else { PreviewAction::Keep };
        }
        if running || self.preview_failed {
            return PreviewAction::Keep;
        }
        PreviewAction::Start
    }

    pub(crate) fn preview_started(&mut self, ok: bool) {
        self.previewing = ok;
        self.preview_failed = !ok;
    }

    /// A retry button or a different microphone clears the failure.
    pub(crate) fn retry_preview(&mut self) {
        self.preview_failed = false;
    }

    pub(crate) fn observe(&mut self, advice: Advice) {
        if advice == Advice::Good {
            self.microphone_good = true;
        }
    }

    /// The forward button's text: honest when a step was not confirmed.
    pub(crate) fn continue_label(&self) -> &'static str {
        match self.step {
            Step::Microphone if !self.microphone_good => "Continue anyway",
            Step::Output if self.heard_test_sound != Heard::Yes => "Continue anyway",
            Step::Test => "Finish",
            _ => "Continue",
        }
    }

    /// Advances; returns true when the last step was finished.
    pub(crate) fn advance(&mut self) -> bool {
        match self.step {
            Step::Microphone => self.step = Step::Output,
            Step::Output => self.step = Step::InputMode,
            Step::InputMode => self.step = Step::Test,
            Step::Test => return true,
        }
        false
    }

    pub(crate) fn back(&mut self) {
        self.step = match self.step {
            Step::Microphone | Step::Output => Step::Microphone,
            Step::InputMode => Step::Output,
            Step::Test => Step::InputMode,
        };
    }

    /// A new output device must be tested again.
    pub(crate) fn output_changed(&mut self) {
        self.heard_test_sound = Heard::Unknown;
    }

    /// Steps the person skipped without confirming, for the closing summary.
    pub(crate) fn unconfirmed(&self) -> Vec<&'static str> {
        let mut steps = Vec::new();
        if !self.microphone_good {
            steps.push("microphone level");
        }
        if self.heard_test_sound != Heard::Yes {
            steps.push("output test sound");
        }
        steps
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_follow_the_plan_and_back_returns() {
        let mut setup = AudioSetup::default();
        assert_eq!(setup.step, Step::Microphone);
        assert!(setup.step.listens());
        assert!(!setup.advance());
        assert_eq!(setup.step, Step::Output);
        assert!(!setup.step.listens(), "no microphone playback while testing the speakers");
        assert!(!setup.advance());
        assert_eq!(setup.step, Step::InputMode);
        setup.back();
        assert_eq!(setup.step, Step::Output);
        setup.advance();
        assert!(!setup.advance());
        assert_eq!(setup.step, Step::Test);
        assert_eq!(setup.continue_label(), "Finish");
        assert!(setup.advance(), "finishing the test step ends the setup");
        assert_eq!(Step::LABELS.len(), Step::ALL.len());
        assert_eq!(Step::Test.index(), 3);
    }

    #[test]
    fn a_failed_preview_is_not_restarted_every_frame() {
        let mut setup = AudioSetup::default();
        assert_eq!(setup.preview_action(false), PreviewAction::Start);
        setup.preview_started(true);
        assert_eq!(setup.preview_action(true), PreviewAction::Keep);
        // The device failed and the preview stopped by itself.
        assert_eq!(setup.preview_action(false), PreviewAction::Keep);
        assert!(setup.preview_failed);
        assert_eq!(setup.preview_action(false), PreviewAction::Keep, "no retry loop");
        setup.retry_preview();
        assert_eq!(setup.preview_action(false), PreviewAction::Start);
        setup.preview_started(false);
        assert_eq!(setup.preview_action(false), PreviewAction::Keep);
        // Steps without listening stop a running preview.
        setup.retry_preview();
        setup.advance();
        assert_eq!(setup.preview_action(true), PreviewAction::Stop);
        assert_eq!(setup.preview_action(false), PreviewAction::Keep);
    }

    #[test]
    fn unconfirmed_steps_are_named_and_change_the_button() {
        let mut setup = AudioSetup::default();
        assert_eq!(setup.continue_label(), "Continue anyway");
        setup.observe(Advice::TooQuiet);
        assert!(!setup.microphone_good);
        setup.observe(Advice::Good);
        assert_eq!(setup.continue_label(), "Continue");
        setup.advance();
        assert_eq!(setup.continue_label(), "Continue anyway");
        setup.heard_test_sound = Heard::Yes;
        assert_eq!(setup.continue_label(), "Continue");
        assert!(setup.unconfirmed().is_empty());
        setup.output_changed();
        assert_eq!(setup.unconfirmed(), vec!["output test sound"]);
    }
}
