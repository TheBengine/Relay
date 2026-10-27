//! "Why can't they hear me / why can't I hear them?" — one named cause at a
//! time, each with the next step. Pure; the app gathers the inputs.
use std::time::Duration;

/// Microphone silence this long, while sending is allowed, is a problem.
pub(crate) const SILENT_MICROPHONE: Duration = Duration::from_secs(8);
/// Raw capture RMS (before gain and processing) below about -90 dBFS: the
/// device delivers practically digital silence. A quiet room is far above it.
pub(crate) const SILENT_LEVEL: f32 = 0.00003;
/// A gap in observations (the voice view was not shown) restarts the clock.
const OBSERVATION_GAP: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OutputCause {
    Deafened,
    VolumeZero,
    /// The chosen speaker or headset is not connected any more.
    DeviceMissing,
    /// The person played the test sound and heard nothing.
    NotHeard,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VoiceProblem {
    /// Encrypted calls need this device to be verified first.
    DeviceNotApproved,
    /// The voice connection itself is down; nobody hears anybody.
    RouteDown,
    /// The microphone delivers no sound to Relay.
    MicrophoneSilent { input_volume_zero: bool },
    OutputSilent(OutputCause),
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Inputs {
    pub waiting_for_call_keys: bool,
    pub route_down: bool,
    /// How long the input level has stayed below `SILENT_LEVEL` while sending
    /// was allowed (unmuted, and the talk key held in push-to-talk).
    pub microphone_silent_for: Option<Duration>,
    pub input_volume: f32,
    pub deafened: bool,
    pub output_volume: f32,
    pub output_device_missing: bool,
    pub test_sound_not_heard: bool,
}

/// The first cause that explains the symptom. Order matters: without keys or
/// a route, device advice would send the person in the wrong direction.
pub(crate) fn diagnose(inputs: &Inputs) -> Option<VoiceProblem> {
    if inputs.waiting_for_call_keys {
        return Some(VoiceProblem::DeviceNotApproved);
    }
    if inputs.route_down {
        return Some(VoiceProblem::RouteDown);
    }
    if inputs.input_volume <= 0.0
        || inputs
            .microphone_silent_for
            .is_some_and(|silent| silent >= SILENT_MICROPHONE)
    {
        return Some(VoiceProblem::MicrophoneSilent {
            input_volume_zero: inputs.input_volume <= 0.0,
        });
    }
    let output = if inputs.deafened {
        Some(OutputCause::Deafened)
    } else if inputs.output_volume <= 0.0 {
        Some(OutputCause::VolumeZero)
    } else if inputs.output_device_missing {
        Some(OutputCause::DeviceMissing)
    } else if inputs.test_sound_not_heard {
        Some(OutputCause::NotHeard)
    } else {
        None
    };
    output.map(VoiceProblem::OutputSilent)
}

impl VoiceProblem {
    /// Who is affected, in a few words.
    pub(crate) fn title(self) -> &'static str {
        match self {
            Self::DeviceNotApproved => "This device is not approved for the encrypted call yet",
            Self::RouteDown => "The voice connection is down",
            Self::MicrophoneSilent { .. } => "No sound from your microphone",
            Self::OutputSilent(OutputCause::Deafened) => "You are deafened",
            Self::OutputSilent(_) => "Your speaker or headset is silent or wrong",
        }
    }

    pub(crate) fn next_step(self) -> &'static str {
        match self {
            Self::DeviceNotApproved => "Ask someone in the call to verify this device in Encryption, and verify them too, then press Try call again. Your microphone and speakers are fine.",
            Self::RouteDown => "Nobody can hear anybody until it is back. Relay keeps trying; check your internet or the host. Your devices are not the cause.",
            Self::MicrophoneSilent { input_volume_zero: true } => "Input Volume is 0 %. Raise Input Volume.",
            Self::MicrophoneSilent { input_volume_zero: false } => "Others hear nothing from you. Check the headset's mute switch, choose the right input device, and allow microphone access in Windows Settings › Privacy › Microphone.",
            Self::OutputSilent(OutputCause::Deafened) => "Undeafen to hear the others.",
            Self::OutputSilent(OutputCause::VolumeZero) => "Output Volume is 0 %. Raise Output Volume.",
            Self::OutputSilent(OutputCause::DeviceMissing) => "The chosen output device is not connected. Plug it in or choose another output device.",
            Self::OutputSilent(OutputCause::NotHeard) => "Choose another output device, check the volume on the device and in Windows, then play the test sound again.",
        }
    }
}

/// Tracks how long the microphone has stayed silent while sending was allowed.
#[derive(Default)]
pub(crate) struct SilenceTimer {
    since: Option<std::time::Instant>,
    last: Option<std::time::Instant>,
}

impl SilenceTimer {
    /// `raw_level` is the capture RMS before gain and noise suppression.
    pub(crate) fn observe(
        &mut self,
        raw_level: f32,
        sending_allowed: bool,
        now: std::time::Instant,
    ) -> Option<Duration> {
        let gap = self
            .last
            .is_none_or(|last| now.saturating_duration_since(last) > OBSERVATION_GAP);
        self.last = Some(now);
        if !sending_allowed || raw_level >= SILENT_LEVEL {
            self.since = None;
            return None;
        }
        if gap {
            self.since = Some(now);
        }
        let since = *self.since.get_or_insert(now);
        Some(now.saturating_duration_since(since))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn healthy() -> Inputs {
        Inputs { input_volume: 1.0, output_volume: 1.0, ..Default::default() }
    }

    #[test]
    fn healthy_inputs_have_no_problem() {
        assert_eq!(diagnose(&healthy()), None);
    }

    #[test]
    fn each_cause_is_named_separately_with_its_own_next_step() {
        let cases = [
            (Inputs { waiting_for_call_keys: true, ..healthy() }, VoiceProblem::DeviceNotApproved),
            (Inputs { route_down: true, ..healthy() }, VoiceProblem::RouteDown),
            (
                Inputs { microphone_silent_for: Some(Duration::from_secs(9)), ..healthy() },
                VoiceProblem::MicrophoneSilent { input_volume_zero: false },
            ),
            (Inputs { input_volume: 0.0, ..healthy() }, VoiceProblem::MicrophoneSilent { input_volume_zero: true }),
            (Inputs { deafened: true, ..healthy() }, VoiceProblem::OutputSilent(OutputCause::Deafened)),
            (Inputs { output_volume: 0.0, ..healthy() }, VoiceProblem::OutputSilent(OutputCause::VolumeZero)),
            (Inputs { output_device_missing: true, ..healthy() }, VoiceProblem::OutputSilent(OutputCause::DeviceMissing)),
            (Inputs { test_sound_not_heard: true, ..healthy() }, VoiceProblem::OutputSilent(OutputCause::NotHeard)),
        ];
        let mut steps = std::collections::BTreeSet::new();
        for (inputs, expected) in cases {
            let problem = diagnose(&inputs).unwrap();
            assert_eq!(problem, expected);
            assert!(!problem.title().is_empty());
            steps.insert(problem.next_step());
        }
        assert_eq!(steps.len(), 8, "every cause has its own advice");
    }

    #[test]
    fn missing_keys_or_route_come_before_device_advice() {
        let everything = Inputs {
            waiting_for_call_keys: true,
            route_down: true,
            microphone_silent_for: Some(Duration::from_secs(60)),
            deafened: true,
            ..healthy()
        };
        assert_eq!(diagnose(&everything), Some(VoiceProblem::DeviceNotApproved));
        let no_keys = Inputs { waiting_for_call_keys: false, ..everything };
        assert_eq!(diagnose(&no_keys), Some(VoiceProblem::RouteDown));
        // A short silence (a pause in speech) is not a microphone problem.
        let pause = Inputs { microphone_silent_for: Some(Duration::from_secs(3)), ..healthy() };
        assert_eq!(diagnose(&pause), None);
    }

    #[test]
    fn the_unverified_device_join_error_is_recognised_and_nothing_else() {
        use crate::e2ee::{device_not_approved, VERIFY_BEFORE_CALL};
        assert!(device_not_approved(VERIFY_BEFORE_CALL));
        assert!(!device_not_approved("Waiting for Mira's Relay to prepare fresh call keys."));
        assert!(!device_not_approved("Voice receive failed: connection reset"));
    }

    #[test]
    fn silence_counts_only_while_sending_is_allowed() {
        let start = Instant::now();
        let mut timer = SilenceTimer::default();
        let step = Duration::from_millis(100);
        let mut now = start;
        assert_eq!(timer.observe(0.0, true, now), Some(Duration::ZERO));
        for _ in 0..90 {
            now += step;
            timer.observe(0.0, true, now);
        }
        assert_eq!(timer.observe(0.0, true, now + step), Some(Duration::from_millis(9100)));
        now += step * 2;
        // Muted or push-to-talk key up: the clock resets.
        assert_eq!(timer.observe(0.0, false, now), None);
        now += step;
        assert_eq!(timer.observe(0.0, true, now), Some(Duration::ZERO));
        // Any real sound resets it too, even a quiet room.
        now += step;
        assert_eq!(timer.observe(0.0005, true, now), None);
        // Not observed for a while (another page was open): start again.
        now += step;
        timer.observe(0.0, true, now);
        now += Duration::from_secs(30);
        assert_eq!(timer.observe(0.0, true, now), Some(Duration::ZERO));
    }
}
