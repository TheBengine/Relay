//! A short, bounded speech-start history, never shared across privacy boundaries.
const MAX_HISTORY: usize = 960;
pub(crate) const MAX_OUTPUT: usize = MAX_HISTORY * 2;

pub(crate) struct SpeechPreroll {
    samples: [f32; MAX_HISTORY],
    len: usize,
    end: Option<u64>,
}

impl Default for SpeechPreroll {
    fn default() -> Self {
        Self {
            samples: [0.0; MAX_HISTORY],
            len: 0,
            end: None,
        }
    }
}

impl SpeechPreroll {
    pub(crate) fn clear(&mut self) {
        self.samples.fill(0.0);
        self.len = 0;
        self.end = None;
    }

    /// Only Voice Activity may retain closed-gate audio. The caller clears this
    /// history on mute, deafen, recipient/key changes and capture interruptions.
    /// Returned source time includes the prepended samples; encryption counters
    /// remain exclusively owned by the sender and are never rolled back.
    #[allow(clippy::too_many_arguments)] // One call site; each argument is a distinct per-block fact.
    pub(crate) fn stage<'a>(
        &mut self,
        input: &[f32],
        timestamp: u64,
        packet_samples: usize,
        retain: bool,
        transmit: bool,
        fade_in_samples: usize,
        output: &'a mut [f32; MAX_OUTPUT],
    ) -> Option<(u64, &'a [f32])> {
        if !retain || self.end != Some(timestamp) {
            self.clear();
        }
        debug_assert!(input.len() <= MAX_HISTORY && input.len().is_multiple_of(packet_samples));
        if transmit {
            let len = self.len + input.len();
            let start = timestamp.saturating_sub(self.len as u64);
            output[..self.len].copy_from_slice(&self.samples[..self.len]);
            output[self.len..len].copy_from_slice(input);
            // The gate opened at full level; the retained start carries its attack.
            crate::voice_gate::fade_in(&mut output[..self.len], fade_in_samples);
            self.clear();
            return Some((start, &output[..len]));
        }
        if retain {
            // 10 ms, rounded to a whole packet (20 ms for a 20 ms sender).
            let limit = packet_samples.max(480);
            let keep = self.len.min(limit.saturating_sub(input.len()));
            self.samples.copy_within(self.len - keep..self.len, 0);
            let take = input.len().min(limit);
            self.samples[keep..keep + take].copy_from_slice(&input[input.len() - take..]);
            self.len = keep + take;
            self.end = timestamp.checked_add(input.len() as u64);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_only_the_last_complete_packet_before_speech_at_original_source_time() {
        for packet in [120, 240, 480, 960] {
            let mut history = SpeechPreroll::default();
            let mut out = [0.0; MAX_OUTPUT];
            let block = packet.max(480);
            assert!(history
                .stage(&vec![0.1; block], 0, packet, true, false, 0, &mut out)
                .is_none());
            assert!(history
                .stage(
                    &vec![0.2; block],
                    block as u64,
                    packet,
                    true,
                    false,
                    0,
                    &mut out
                )
                .is_none());
            let (at, audio) = history
                .stage(
                    &vec![0.3; block],
                    (block * 2) as u64,
                    packet,
                    true,
                    true,
                    0,
                    &mut out,
                )
                .unwrap();
            assert_eq!(at, block as u64);
            assert_eq!(audio.len(), block * 2);
            assert!(audio[..block].iter().all(|v| *v == 0.2));
            assert!(audio[block..].iter().all(|v| *v == 0.3));
            assert_eq!(audio.len() % packet, 0);
            let (_, audio) = history
                .stage(
                    &vec![0.4; block],
                    (block * 3) as u64,
                    packet,
                    true,
                    true,
                    0,
                    &mut out,
                )
                .unwrap();
            assert_eq!(audio.len(), block, "ongoing speech has no further history");
        }
    }

    #[test]
    fn ptt_disabled_history_capture_gaps_and_explicit_privacy_reset_never_replay_audio() {
        for boundary in 0..4 {
            let mut history = SpeechPreroll::default();
            let mut out = [0.0; MAX_OUTPUT];
            history.stage(&[0.9; 480], 0, 480, true, false, 0, &mut out);
            if boundary == 2 {
                history.clear();
            }
            let at = if boundary == 3 { 4800 } else { 480 };
            let retain = boundary >= 2;
            let (start, audio) = history
                .stage(&[0.1; 480], at, 480, retain, true, 0, &mut out)
                .unwrap();
            assert_eq!(start, at);
            assert_eq!(audio, &[0.1; 480]);
            assert_eq!(history.len, 0);
            assert!(history.samples.iter().all(|v| *v == 0.0));
        }
    }
}
