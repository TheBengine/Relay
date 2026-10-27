//! App-to-app delay, each part labelled set / observed / unknown. Network
//! time is half the measured round trip, which assumes both directions take
//! equally long; the other person's settings are never guessed.
use crate::audio_settings::AudioSettings;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Part {
    /// From settings on this PC.
    Set(f32),
    /// Measured on this call.
    Observed(f32),
    Unknown,
}

impl Part {
    fn ms(self) -> Option<f32> {
        match self {
            Part::Set(ms) | Part::Observed(ms) => Some(ms),
            Part::Unknown => None,
        }
    }

    fn label(self, name: &str) -> String {
        match self {
            Part::Set(ms) => format!("{name} {ms:.0} ms (set)"),
            Part::Observed(ms) => format!("{name} {ms:.0} ms (observed)"),
            Part::Unknown => format!("{name} unknown"),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Direction {
    pub parts: Vec<(&'static str, Part)>,
}

impl Direction {
    /// Sum of known parts, and whether anything is missing.
    pub(crate) fn known_ms(&self) -> (f32, bool) {
        let known = self.parts.iter().filter_map(|(_, part)| part.ms()).sum();
        let missing = self.parts.iter().any(|(_, part)| *part == Part::Unknown);
        (known, missing)
    }

    pub(crate) fn summary(&self, title: &str) -> String {
        let (known, missing) = self.known_ms();
        let total = if missing {
            format!("known parts {known:.0} ms + unknown parts")
        } else {
            format!("about {known:.0} ms")
        };
        let parts = self
            .parts
            .iter()
            .map(|(name, part)| part.label(name))
            .collect::<Vec<_>>()
            .join(" + ");
        format!("{title}: {total} = {parts}")
    }

    #[cfg(test)]
    pub(crate) fn part(&self, name: &str) -> Option<Part> {
        self.parts.iter().find(|(label, _)| *label == name).map(|(_, part)| *part)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Estimate {
    pub to_them: Direction,
    pub to_you: Direction,
}

/// What this PC knows about the call.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Inputs {
    pub input_period_ms: Option<f32>,
    pub output_period_ms: Option<f32>,
    /// The active voice route's measured round trip.
    pub rtt_ms: Option<f32>,
    /// True when that round trip is to the other person (direct peer route);
    /// otherwise it only reaches the voice server.
    pub rtt_to_peer: bool,
    /// Highest receive buffer target among current speakers.
    pub receive_ms: Option<u32>,
}

fn device(period: Option<f32>) -> Part {
    period
        .filter(|ms| ms.is_finite() && *ms > 0.0)
        .map_or(Part::Unknown, Part::Observed)
}

pub(crate) fn estimate(audio: &AudioSettings, inputs: Inputs) -> Estimate {
    let budget = audio.voice_delay_budget_for_output(inputs.output_period_ms);
    let processing = budget.capture_ms + budget.suppression_ms as f32 + budget.speech_start_ms;
    let codec = budget.codec_ms.map_or(Part::Unknown, Part::Set);
    let half = inputs
        .rtt_ms
        .filter(|ms| ms.is_finite() && *ms > 0.0)
        .map_or(Part::Unknown, |ms| Part::Observed(ms / 2.0));
    let buffer = match inputs.receive_ms {
        // Manual receive latency is a setting; automatic is what it chose now.
        Some(ms) if audio.adaptive_jitter => Part::Observed(ms as f32),
        Some(ms) => Part::Set(ms as f32),
        None => Part::Unknown,
    };
    let mut to_them = vec![
        ("your microphone buffer", device(inputs.input_period_ms)),
        ("your processing", Part::Set(processing)),
        ("voice codec", codec),
    ];
    let mut to_you = vec![("their processing", Part::Unknown)];
    if inputs.rtt_to_peer {
        to_them.push(("network", half));
        to_you.push(("network", half));
    } else {
        to_them.push(("you to server", half));
        to_them.push(("server to them", Part::Unknown));
        to_you.push(("them to server", Part::Unknown));
        to_you.push(("server to you", half));
    }
    to_them.push(("their buffer and playback", Part::Unknown));
    to_you.push(("your buffer (highest speaker)", buffer));
    to_you.push(("your playback", Part::Set(budget.playback_ms)));
    Estimate {
        to_them: Direction { parts: to_them },
        to_you: Direction { parts: to_you },
    }
}

/// Always shown with the estimate, so nobody reads it as a measurement.
pub(crate) const ASSUMPTION: &str = "Estimate, not a measurement. Network parts are half the measured round trip, which assumes both directions take equally long. Unknown parts (the other person's processing and buffer, and on a server route the leg between the server and them) are not added, so the real delay is higher than the known parts.";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio_settings::TransmissionMode;

    fn audio() -> AudioSettings {
        let mut audio = AudioSettings {
            noise_suppression: false,
            echo_cancellation: false,
            preserve_speech_start: false,
            adaptive_jitter: true,
            ..Default::default()
        };
        audio.set_packet_samples(480);
        audio
    }

    fn inputs(rtt_to_peer: bool) -> Inputs {
        Inputs {
            input_period_ms: Some(10.0),
            output_period_ms: Some(10.0),
            rtt_ms: Some(40.0),
            rtt_to_peer,
            receive_ms: Some(30),
        }
    }

    #[test]
    fn processing_matches_the_settings_budget_exactly() {
        for transmission in [TransmissionMode::VoiceActivation, TransmissionMode::PushToTalk] {
            let mut settings = audio();
            settings.noise_suppression = true;
            settings.preserve_speech_start = true;
            settings.transmission = transmission;
            let budget = settings.voice_delay_budget_for_output(Some(10.0));
            let estimate = estimate(&settings, inputs(true));
            let Some(Part::Set(processing)) = estimate.to_them.part("your processing") else { panic!() };
            let outgoing = budget.capture_ms + budget.suppression_ms as f32 + budget.speech_start_ms;
            assert!((processing - outgoing).abs() < 1e-4, "{transmission:?}");
            if transmission == TransmissionMode::PushToTalk {
                assert_eq!(budget.speech_start_ms, 0.0, "no speech-start history with push-to-talk");
            }
            if let Some(codec) = budget.codec_ms {
                assert_eq!(estimate.to_them.part("voice codec"), Some(Part::Set(codec)));
            }
            assert_eq!(estimate.to_you.part("your playback"), Some(Part::Set(budget.playback_ms)));
        }
    }

    #[test]
    fn a_direct_route_measures_to_them_and_a_server_route_only_to_the_server() {
        let direct = estimate(&audio(), inputs(true));
        assert_eq!(direct.to_them.part("network"), Some(Part::Observed(20.0)));
        assert_eq!(direct.to_you.part("network"), Some(Part::Observed(20.0)));
        assert_eq!(direct.to_them.part("server to them"), None);
        let relay = estimate(&audio(), inputs(false));
        assert_eq!(relay.to_them.part("you to server"), Some(Part::Observed(20.0)));
        assert_eq!(relay.to_them.part("server to them"), Some(Part::Unknown));
        assert_eq!(relay.to_you.part("them to server"), Some(Part::Unknown));
        assert_eq!(relay.to_you.part("server to you"), Some(Part::Observed(20.0)));
        let text = relay.to_them.summary("You → them");
        assert!(text.contains("server to them unknown") && text.contains("+ unknown parts"), "{text}");
        assert!(ASSUMPTION.contains("assumes both directions") && ASSUMPTION.contains("server"));
    }

    #[test]
    fn known_parts_add_up_exactly() {
        let settings = audio();
        let budget = settings.voice_delay_budget_for_output(Some(10.0));
        let estimate = estimate(&settings, inputs(true));
        let (to_you, missing) = estimate.to_you.known_ms();
        assert!(missing);
        assert!((to_you - (20.0 + 30.0 + budget.playback_ms)).abs() < 1e-4);
        let (to_them, _) = estimate.to_them.known_ms();
        let expected = 10.0 + budget.capture_ms + budget.codec_ms.unwrap_or(0.0) + 20.0;
        assert!((to_them - expected).abs() < 1e-4);
    }

    #[test]
    fn unmeasured_parts_stay_unknown_and_manual_buffers_are_settings() {
        let mut settings = audio();
        let blank = estimate(&settings, Inputs::default());
        assert_eq!(blank.to_them.part("your microphone buffer"), Some(Part::Unknown));
        assert_eq!(blank.to_them.part("you to server"), Some(Part::Unknown));
        assert_eq!(blank.to_you.part("your buffer (highest speaker)"), Some(Part::Unknown));
        assert!(blank.to_you.summary("Them → you").contains("server to you unknown"));
        settings.adaptive_jitter = false;
        let manual = estimate(&settings, inputs(true));
        assert_eq!(manual.to_you.part("your buffer (highest speaker)"), Some(Part::Set(30.0)));
    }
}
