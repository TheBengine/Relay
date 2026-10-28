//! Per-source media timing is separate from transport replay and encryption counters.
use crate::adaptive_audio::AdaptiveDelay;
use opus::{Channels, Decoder};
use relay_protocol::{AUDIO_FRAME_SAMPLES, AUDIO_SAMPLE_RATE};
use std::{
    collections::{BTreeMap, VecDeque},
    time::{Duration, Instant},
};

#[cfg(test)]
const FRAME: Duration = Duration::from_millis(20);
const MAX_PACKETS: usize = 32;
// A sub-packet worker slip can use the already queued audio. This is a
// catch-up ceiling, not extra jitter or device buffering.
pub(crate) const SHORT_WORKER_STALL: Duration = Duration::from_millis(10);
// Queue shortening must only remove near-silence. A peak-only 0.01 gate
// also removed quiet words and fricatives. Check both peak and energy.
fn near_silence(samples: &[f32]) -> bool {
    !samples.is_empty()
        && samples.iter().all(|s| s.is_finite() && s.abs() < 0.001)
        && samples.iter().map(|s| s * s).sum::<f32>() / (samples.len() as f32) < 1.0e-8
}

/// Whether an Opus packet carries redundancy for the frame before it (in-band
/// FEC). Only SILK and hybrid packets can. For one frame of up to 20 ms the
/// SILK layer starts with two equiprobable range-coded flags: voice activity,
/// then redundancy (RFC 6716 4.2.3). This decodes exactly those two bits.
fn carries_redundancy(packet: &[u8]) -> bool {
    let Some((&toc, frame)) = packet.split_first() else {
        return false;
    };
    // Configurations 0-15 are SILK or hybrid; code 0 is one frame.
    if toc >> 3 >= 16 || toc & 3 != 0 || frame.is_empty() {
        return false;
    }
    if !opus::packet::get_nb_samples(packet, AUDIO_SAMPLE_RATE).is_ok_and(|n| n <= 960) {
        return false;
    }
    // Range decoder of RFC 6716 4.1 / libopus entdec.c.
    let mut bytes = frame.iter().copied();
    let mut rem = u32::from(bytes.next().unwrap_or(0));
    let mut rng: u32 = 128;
    let mut val: u32 = 127 - (rem >> 1);
    let mut normalize = |rng: &mut u32, val: &mut u32| {
        while *rng <= 1 << 23 {
            *rng <<= 8;
            let previous = rem;
            rem = u32::from(bytes.next().unwrap_or(0));
            let symbol = ((previous << 8) | rem) >> 1;
            *val = ((*val << 8) + (255 & !symbol)) & ((1 << 31) - 1);
        }
    };
    normalize(&mut rng, &mut val);
    let mut bit = |rng: &mut u32, val: &mut u32| {
        let s = *rng >> 1;
        let set = *val < s;
        if !set {
            *val -= s;
        }
        *rng = if set { s } else { *rng - s };
        normalize(rng, val);
        set
    };
    let _voice_activity = bit(&mut rng, &mut val);
    bit(&mut rng, &mut val)
}

/// Queue depth is judged over the last one to two of these windows.
const QUEUE_WINDOW: Duration = Duration::from_millis(500);

/// Lowest number of queued packets seen at playout deadlines. Arrival jitter
/// makes the queue swing; only depth that never went away is standing delay,
/// such as a backlog delivered at once after congestion or a switch to a
/// faster network path.
#[derive(Default)]
struct QueueFloor {
    started: Option<Instant>,
    current: usize,
    previous: Option<usize>,
}

impl QueueFloor {
    fn observe(&mut self, depth: usize, now: Instant) {
        let elapsed = self
            .started
            .map(|started| now.saturating_duration_since(started));
        match elapsed {
            Some(elapsed) if elapsed < QUEUE_WINDOW => self.current = self.current.min(depth),
            _ => {
                self.previous = elapsed
                    .filter(|elapsed| *elapsed < QUEUE_WINDOW * 2)
                    .map(|_| self.current);
                self.current = depth;
                self.started = Some(now);
            }
        }
    }

    fn standing(&self) -> Option<usize> {
        self.previous.map(|previous| previous.min(self.current))
    }

    fn removed_one(&mut self) {
        self.current = self.current.saturating_sub(1);
        self.previous = self.previous.map(|previous| previous.saturating_sub(1));
    }
}

pub(crate) struct Packet {
    timestamp: u64,
    restart: bool,
    bytes: Vec<u8>,
    legacy: bool,
    received: Instant,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct JitterStats {
    pub late: u64,
    pub reordered: u64,
    /// Missing frames played without their packet and without redundancy.
    pub concealed: u64,
    /// Missing frames rebuilt from Opus redundancy in the next packet.
    pub recovered: u64,
    pub held: u64,
    pub quiet_skips: u64,
}

pub(crate) struct Speaker {
    decoder: Decoder,
    frame_samples: usize,
    pending_pcm: VecDeque<f32>,
    pub(crate) packets: BTreeMap<u64, Packet>,
    first_packet: Instant,
    pub(crate) last_received: Instant,
    next_deadline: Option<Instant>,
    expected: Option<(u64, u64)>,
    last_played: Option<u64>,
    last_packet_played: Option<(u64, u64)>,
    highest_received: u64,
    missing: usize,
    jitter_ms: u32,
    adaptation: Option<AdaptiveDelay>,
    actual_delay_us: u32,
    queue_floor: QueueFloor,
    pub(crate) stats: JitterStats,
}

impl Speaker {
    pub(crate) fn new(now: Instant) -> Result<Self, opus::Error> {
        Ok(Self {
            decoder: Decoder::new(AUDIO_SAMPLE_RATE, Channels::Mono)?,
            frame_samples: AUDIO_FRAME_SAMPLES,
            pending_pcm: VecDeque::with_capacity(AUDIO_FRAME_SAMPLES),
            packets: BTreeMap::new(),
            first_packet: now,
            last_received: now,
            next_deadline: None,
            expected: None,
            last_played: None,
            last_packet_played: None,
            highest_received: 0,
            missing: 0,
            jitter_ms: 0,
            adaptation: None,
            actual_delay_us: 20_000,
            queue_floor: QueueFloor::default(),
            stats: JitterStats::default(),
        })
    }
    pub(crate) fn configure_adaptation(&mut self, enabled: bool) {
        if enabled != self.adaptation.is_some() {
            self.adaptation = enabled.then(AdaptiveDelay::default);
            self.expected = None;
            self.next_deadline = None;
            self.pending_pcm.clear();
            self.missing = 0;
            if let Some((_, packet)) = self.packets.first_key_value() {
                self.first_packet = packet.received;
            }
        }
    }

    pub(crate) fn buffer_target_ms(&self, manual_ms: u32) -> u32 {
        match &self.adaptation {
            Some(policy) if self.expected.is_none() => policy.target_ms(),
            Some(_) => self.actual_delay_us.div_ceil(1000),
            None => manual_ms,
        }
    }
    // Legacy packets have no source clock. Keep compatibility without pretending their
    // recipient transport counter can identify source-frame loss.
    #[cfg(test)]
    pub(crate) fn push(&mut self, sequence: u64, bytes: Vec<u8>, now: Instant) {
        self.push_timed(
            sequence,
            sequence.saturating_mul(AUDIO_FRAME_SAMPLES as u64),
            false,
            bytes,
            now,
        );
        if let Some(packet) = self.packets.get_mut(&sequence) {
            packet.legacy = true;
        }
    }
    pub(crate) fn push_timed(
        &mut self,
        sequence: u64,
        timestamp: u64,
        restart: bool,
        bytes: Vec<u8>,
        now: Instant,
    ) {
        let Ok(samples) = opus::packet::get_nb_samples(&bytes, AUDIO_SAMPLE_RATE) else {
            return;
        };
        if !matches!(samples, 120 | 240 | 480 | 960) {
            return;
        }
        if self.expected.is_none() && self.packets.is_empty() {
            self.frame_samples = samples;
        }
        if self.last_played.is_some_and(|played| sequence <= played) {
            // PLC advances deadlines for packets that may be intentional gated
            // silence rather than lost transmissions. A fresh talkspurt restart
            // may therefore overlap only those concealed sequence positions.
            // It must still follow the last real source packet and source clock.
            if restart
                && self
                    .last_packet_played
                    .is_some_and(|(played, at)| sequence > played && timestamp > at)
            {
                self.expected = None;
                self.next_deadline = None;
                self.last_played = self.last_packet_played.map(|(played, _)| played);
                self.packets
                    .retain(|seq, packet| *seq >= sequence && packet.timestamp >= timestamp);
            } else {
                self.stats.late += 1;
                if let Some(policy) = &mut self.adaptation {
                    policy.late_packet(now);
                }
                return;
            }
        }
        if self.packets.contains_key(&sequence) {
            return;
        }
        if let Some(policy) = &mut self.adaptation {
            policy.observe(timestamp, restart, now);
        }
        if sequence < self.highest_received {
            self.stats.reordered += 1;
        }
        self.highest_received = self.highest_received.max(sequence);
        if self.expected.is_none() && self.packets.is_empty() {
            self.first_packet = now;
        }
        self.last_received = now;
        self.packets.insert(
            sequence,
            Packet {
                timestamp,
                restart,
                bytes,
                legacy: false,
                received: now,
            },
        );
        while self.packets.len() > MAX_PACKETS
            || self
                .packets
                .values()
                .map(|packet| {
                    opus::packet::get_nb_samples(&packet.bytes, AUDIO_SAMPLE_RATE).unwrap_or(0)
                })
                .sum::<usize>()
                > AUDIO_FRAME_SAMPLES * 8
        {
            self.packets.pop_first();
            self.stats.late += 1;
        }
    }
    /// Receive playout is independent of the microphone's frame-collection phase.
    /// An unprimed source becomes ready immediately when enough packets arrive;
    /// the configured target is an upper bound on its initial wait, not a second
    /// fixed delay after the packet-count threshold has already been reached.
    pub(crate) fn next_playout_deadline(&self, now: Instant, jitter_ms: u32) -> Option<Instant> {
        if self.jitter_ms == jitter_ms && self.expected.is_some() {
            return self.next_deadline;
        }
        if self.packets.is_empty() {
            return None;
        }
        let jitter_ms = self.buffer_target_ms(jitter_ms);
        if self.packets.len() >= (jitter_ms as usize * 48).div_ceil(self.frame_samples) {
            Some(now)
        } else {
            Some(self.first_packet + Duration::from_millis(u64::from(jitter_ms)))
        }
    }
    pub(crate) fn render_quantum<const N: usize>(
        &mut self,
        now: Instant,
        output: &mut [f32; N],
        jitter_ms: u32,
    ) -> bool {
        // Keep the decoded tail through a sub-packet mixer slip. Discard it
        // after a whole source packet (at most 10 ms), or a buffer change,
        // so larger stalls still resume without playing stale decoded audio.
        let frame_time =
            Duration::from_micros(self.frame_samples as u64 * 1_000_000 / AUDIO_SAMPLE_RATE as u64);
        let pending_time = Duration::from_micros(
            self.pending_pcm.len() as u64 * 1_000_000 / AUDIO_SAMPLE_RATE as u64,
        );
        // next_deadline is for the next source packet, not the next fraction
        // already decoded. Otherwise a long stall inside a 20 ms packet could
        // appear short and retain its stale tail.
        let quantum_deadline = self
            .next_deadline
            .and_then(|deadline| deadline.checked_sub(pending_time));
        if self.jitter_ms != jitter_ms
            || quantum_deadline.is_some_and(|deadline| {
                now.saturating_duration_since(deadline) >= frame_time.min(SHORT_WORKER_STALL)
            })
        {
            self.pending_pcm.clear();
        }
        if self.pending_pcm.is_empty() {
            let mut decoded = [0.0; AUDIO_FRAME_SAMPLES];
            if !self.render(now, &mut decoded, jitter_ms) {
                output.fill(0.0);
                return false;
            }
            self.pending_pcm.extend(&decoded[..self.frame_samples]);
        }
        for sample in output {
            *sample = self.pending_pcm.pop_front().unwrap_or(0.0);
        }
        true
    }

    pub(crate) fn render(
        &mut self,
        now: Instant,
        output: &mut [f32; AUDIO_FRAME_SAMPLES],
        jitter_ms: u32,
    ) -> bool {
        output.fill(0.0);
        if let Some((_, packet)) = self.packets.first_key_value() {
            if let Ok(samples) = opus::packet::get_nb_samples(&packet.bytes, AUDIO_SAMPLE_RATE) {
                if !matches!(samples, 120 | 240 | 480 | 960) {
                    return false;
                }
                self.frame_samples = samples;
            }
        }
        let frame_samples = self.frame_samples;
        let frame_us = (frame_samples * 1_000_000 / AUDIO_SAMPLE_RATE as usize) as u32;
        let frame_time = Duration::from_micros(u64::from(frame_us));
        let output = &mut output[..frame_samples];
        if self.jitter_ms != jitter_ms {
            self.jitter_ms = jitter_ms;
            self.next_deadline = None;
            self.expected = None;
        }
        if let Some(policy) = self.adaptation.as_ref().filter(|_| {
            self.next_deadline
                .is_some_and(|deadline| now.saturating_duration_since(deadline) >= frame_time * 4)
        }) {
            // A locally stalled worker must resume near the newest available
            // source time instead of playing its bounded but stale backlog.
            let keep = (policy.target_ms() * 1000).div_ceil(frame_us) as usize;
            while self.packets.len() > keep {
                self.packets.pop_first();
                self.stats.late += 1;
            }
            self.expected = None;
            self.next_deadline = None;
            self.first_packet = self
                .packets
                .first_key_value()
                .map_or(now, |(_, packet)| packet.received);
            let _ = self.decoder.reset_state();
        }
        // A fresh talkspurt adopts the learned target at its own receive time.
        // It never copies or resets a transport/replay counter. A restart that
        // continues the source clock exactly is a rebuilt encoder mid-sentence
        // (loss recovery switching, a packet size change), not a talkspurt:
        // playout keeps its timing and only the decoder is reset below.
        if self.adaptation.is_some() && self.expected.is_some() {
            if let (Some((&sequence, first)), Some((played, at))) =
                (self.packets.first_key_value(), self.last_packet_played)
            {
                let continues = self.expected == Some((sequence, first.timestamp));
                if first.restart
                    && !continues
                    && sequence >= self.expected.unwrap().0
                    && sequence > played
                    && first.timestamp > at
                {
                    self.expected = None;
                    self.next_deadline = None;
                    self.first_packet = first.received;
                }
            }
        }
        if self.expected.is_none() {
            let Some(deadline) = self.next_playout_deadline(now, jitter_ms) else {
                return false;
            };
            if now < deadline {
                return false;
            }
            let Some((&sequence, first)) = self.packets.first_key_value() else {
                return false;
            };
            self.expected = Some((sequence, first.timestamp));
            self.actual_delay_us = self.adaptation.as_ref().map_or(jitter_ms * 1000, |policy| {
                // Holds and quiet skips operate on whole source packets.
                // Align accounting so 20 ms peers cannot oscillate below
                // the learned floor or overshoot the 80 ms ceiling.
                (policy.target_ms() * 1000).div_ceil(frame_us) * frame_us
            });
            self.next_deadline = Some(now);
            self.missing = 0;
            self.queue_floor = QueueFloor::default();
        }
        let deadline = self.next_deadline.unwrap();
        if now < deadline {
            return false;
        }
        // A stalled worker must not rush a backlog through the device to catch up.
        self.next_deadline = Some(if now.saturating_duration_since(deadline) >= frame_time {
            now + frame_time
        } else {
            deadline + frame_time
        });
        let (mut sequence, mut timestamp) = self.expected.unwrap();
        if let Some((&first_sequence, first)) = self.packets.first_key_value() {
            if first.legacy && first_sequence >= sequence {
                sequence = first_sequence;
                timestamp = first.timestamp;
            } else if first.restart && first_sequence >= sequence {
                sequence = first_sequence;
                timestamp = first.timestamp;
                let _ = self.decoder.reset_state();
                self.missing = 0;
                self.queue_floor = QueueFloor::default();
            } else if first_sequence == sequence
                && first.timestamp > timestamp.saturating_add(frame_samples as u64)
            {
                // Contiguous source sequence with a clock gap is deliberate gated silence.
                timestamp = first.timestamp;
                let _ = self.decoder.reset_state();
            }
        }
        self.queue_floor.observe(self.packets.len(), now);
        // When packets are arriving behind their deadline, buy one codec frame
        // of room with concealment. Keep the source position so the real packet
        // can still play once it arrives. The learned ceiling bounds all holds.
        if self
            .packets
            .first_key_value()
            .is_some_and(|(&next, packet)| next >= sequence && !packet.restart)
        {
            if let Some(policy) = &mut self.adaptation {
                if policy.target_ms() * 1000 > self.actual_delay_us
                    && self.actual_delay_us + frame_us <= crate::adaptive_audio::MAX_DELAY_MS * 1000
                {
                    self.actual_delay_us += frame_us;
                    self.stats.held += 1;
                    return self
                        .decoder
                        .decode_float(&[], output, false)
                        .is_ok_and(|n| n == frame_samples);
                }
            }
        }
        let mut decoded_restart = false;
        let decoded = if let Some(packet) = self.packets.remove(&sequence) {
            decoded_restart = packet.restart;
            self.missing = 0;
            timestamp = packet.timestamp;
            self.last_packet_played = Some((sequence, timestamp));
            self.decoder.decode_float(&packet.bytes, output, false)
        } else {
            self.missing += 1;
            if self.missing > 3 {
                self.expected = None;
                self.next_deadline = None;
                self.first_packet = now;
                return false;
            }
            // Recover from an already queued successor when the sender supplied
            // Opus redundancy. Never wait for it or consume its normal playout.
            // Without redundant data Opus performs ordinary concealment. Source
            // continuity excludes gated silence, restarts and duration changes.
            // The successor's redundancy is for exactly this frame, so the last
            // frame of a short burst is rebuilt too.
            let successor = sequence
                .checked_add(1)
                .and_then(|next| self.packets.get(&next))
                .filter(|packet| {
                    !packet.restart
                        && !packet.legacy
                        && packet.timestamp == timestamp.saturating_add(frame_samples as u64)
                        && opus::packet::get_nb_samples(&packet.bytes, AUDIO_SAMPLE_RATE)
                            .is_ok_and(|samples| samples == frame_samples)
                });
            if successor.is_some_and(|packet| carries_redundancy(&packet.bytes)) {
                self.stats.recovered += 1;
            } else {
                self.stats.concealed += 1;
            }
            if let Some(next) = successor {
                self.decoder.decode_float(&next.bytes, output, true)
            } else {
                self.decoder.decode_float(&[], output, false)
            }
        };
        // Recover one frame only at an already decoded quiet interval, with a
        // contiguous queued successor. Decode both packets for Opus continuity;
        // omit the interval below the amplitude threshold without speeding up.
        // Either the adaptive target fell, or more packets have stood in the
        // queue for a whole window than the fixed or adaptive target needs.
        let target_ms = self
            .adaptation
            .as_ref()
            .map_or(jitter_ms, AdaptiveDelay::target_ms);
        let target_packets = (target_ms * 1000).div_ceil(frame_us).max(1) as usize;
        let above_target = self.adaptation.as_ref().is_some_and(|policy| {
            self.actual_delay_us.saturating_sub(frame_us)
                >= (policy.target_ms() * 1000).div_ceil(frame_us) * frame_us
        });
        let standing_queue = self
            .queue_floor
            .standing()
            .is_some_and(|floor| floor > target_packets);
        let shrink = decoded.as_ref().is_ok_and(|&n| n == frame_samples)
            && (decoded_restart || above_target || standing_queue)
            && self.missing == 0
            && near_silence(output)
            && sequence
                .checked_add(1)
                .and_then(|next| self.packets.get(&next))
                .is_some_and(|packet| {
                    !packet.restart
                        && !packet.legacy
                        && packet.timestamp == timestamp.saturating_add(frame_samples as u64)
                });
        let decoded = if shrink {
            sequence += 1;
            let packet = self.packets.remove(&sequence).unwrap();
            timestamp = packet.timestamp;
            self.last_packet_played = Some((sequence, timestamp));
            if above_target {
                self.actual_delay_us = self.actual_delay_us.saturating_sub(frame_us);
            }
            self.queue_floor.removed_one();
            self.stats.quiet_skips += 1;
            self.decoder.decode_float(&packet.bytes, output, false)
        } else {
            decoded
        };
        self.last_played = Some(sequence);
        self.expected = sequence
            .checked_add(1)
            .map(|next| (next, timestamp.saturating_add(frame_samples as u64)));
        match decoded {
            Ok(n) if n == frame_samples => true,
            _ => {
                output.fill(0.0);
                false
            }
        }
    }
}

/// Deterministic network profiles for the receive path. Real Opus packets go
/// through the real `Speaker` on a simulated clock (1 ms ticks, 5 ms mixer
/// quanta), like `stable_ten_ms_path_starts_ten_ms_earlier_without_losing_audio`.
/// No devices, sockets or wall-clock sleeps.
#[cfg(test)]
pub(crate) mod network_profiles {
    use super::*;
    use crate::audio_settings::LossRecovery;
    use crate::voice_quality::{ArrivalStats, LossRecoveryPolicy};

    #[derive(Clone, Copy, Debug)]
    pub(crate) struct NetworkProfile {
        pub name: &'static str,
        /// Fixed one-way network delay.
        pub one_way_ms: u32,
        /// Extra queueing delay per packet, drawn evenly from 0 to this value.
        /// Packets keep their order unless `reorder_pct` swaps them.
        pub jitter_ms: u32,
        /// Share of packets that never arrive, 0–100.
        pub loss_pct: f32,
        /// Share of packets that arrive just after their successor, 0–100.
        pub reorder_pct: f32,
        /// Packets lost together in one loss event.
        pub burst_len: u32,
    }

    pub(crate) const PROFILES: [NetworkProfile; 5] = [
        NetworkProfile {
            name: "lan_wired",
            one_way_ms: 5,
            jitter_ms: 0,
            loss_pct: 0.0,
            reorder_pct: 0.0,
            burst_len: 1,
        },
        NetworkProfile {
            name: "wifi_good",
            one_way_ms: 8,
            jitter_ms: 5,
            loss_pct: 0.5,
            reorder_pct: 0.0,
            burst_len: 1,
        },
        NetworkProfile {
            name: "wifi_busy",
            one_way_ms: 10,
            jitter_ms: 20,
            loss_pct: 3.0,
            reorder_pct: 0.0,
            burst_len: 2,
        },
        NetworkProfile {
            name: "dsl_far",
            one_way_ms: 40,
            jitter_ms: 10,
            loss_pct: 1.0,
            reorder_pct: 0.0,
            burst_len: 1,
        },
        NetworkProfile {
            name: "mobile_hotspot",
            one_way_ms: 30,
            jitter_ms: 40,
            loss_pct: 5.0,
            reorder_pct: 2.0,
            burst_len: 1,
        },
    ];

    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub(crate) struct ReceiveReport {
        /// Frames played without their own packet and without redundancy.
        pub concealed: u64,
        /// Missing frames rebuilt from the next packet's redundancy.
        pub recovered: u64,
        pub late: u64,
        pub held: u64,
        /// Simulated time of the first audible output; packet 1 leaves at 10 ms.
        pub first_playout_ms: Option<u64>,
        /// Output samples from the first playout until the last source frame.
        pub rendered_samples: usize,
        /// Output quanta with no audio between first playout and the last frame.
        pub silent_gaps: u64,
        /// Packets the network dropped.
        pub lost: u64,
        /// When the sender first turned loss recovery on.
        pub recovery_on_ms: Option<u64>,
        /// Sent packets that carry redundancy for the frame before them.
        pub redundant_packets: u64,
        /// Near-silent frames left out to shorten a standing queue.
        pub quiet_skips: u64,
        /// Receive buffering in use at the end of the run.
        pub final_delay_ms: u32,
    }

    /// splitmix64: a fixed seed gives the same network on every run.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }
        fn unit(&mut self) -> f64 {
            (self.next() >> 11) as f64 / (1u64 << 53) as f64
        }
    }

    /// Arrival time in µs for each packet (index = sequence - 1), or None if lost.
    /// Drawn before any audio is encoded, so every sender mode sees one network.
    fn network(profile: &NetworkProfile, packets: u64) -> Vec<Option<u64>> {
        let seed = profile.name.bytes().fold(0x5EED_D00D_u64, |seed, byte| {
            seed.rotate_left(8) ^ u64::from(byte)
        });
        let mut rng = Rng(seed);
        let start = f64::from(profile.loss_pct) / 100.0 / f64::from(profile.burst_len.max(1));
        let mut burst = 0;
        let mut previous = 0;
        let mut arrivals: Vec<Option<u64>> = (1..=packets)
            .map(|sequence| {
                let sent = sequence * 10_000;
                let jitter = (rng.unit() * f64::from(profile.jitter_ms) * 1000.0) as u64;
                let lost = if burst > 0 {
                    burst -= 1;
                    true
                } else if rng.unit() < start {
                    burst = profile.burst_len.saturating_sub(1);
                    true
                } else {
                    false
                };
                // One queue: a packet never overtakes the one sent before it.
                let arrival = (sent + u64::from(profile.one_way_ms) * 1000 + jitter).max(previous);
                previous = arrival;
                (!lost).then_some(arrival)
            })
            .collect();
        for index in 0..arrivals.len().saturating_sub(1) {
            if rng.unit() * 100.0 < f64::from(profile.reorder_pct) {
                if let (Some(_), Some(next)) = (arrivals[index], arrivals[index + 1]) {
                    arrivals[index] = Some(next + 500);
                }
            }
        }
        arrivals
    }

    /// The factory sender setting.
    pub(crate) fn simulate(profile: &NetworkProfile, packets: u64) -> ReceiveReport {
        simulate_with(profile, packets, LossRecovery::default())
    }

    /// The sender decides recovery from the loss the receiver measures: the
    /// simulated path is symmetric, which is what a real sender assumes when
    /// it uses its own incoming loss.
    pub(crate) fn simulate_with(
        profile: &NetworkProfile,
        packets: u64,
        choice: LossRecovery,
    ) -> ReceiveReport {
        let origin = Instant::now();
        let fate = network(profile, packets);
        // The audio bench's talker and seed: syllables, formants and short
        // pauses, sent continuously as with an open voice gate.
        let speech =
            crate::voice_test_signals::talker(packets as f32 / 100.0, (105.0, 135.0), 13, 0.3);
        let mut policy = LossRecoveryPolicy::default();
        let mut recovery = choice.active(false, 480);
        let mut encoder =
            crate::audio_settings::make_voice_encoder_with_recovery(96, recovery).unwrap();
        let mut speaker = Speaker::new(origin).unwrap();
        // Factory receive setting: Automatic with a 10 ms manual fallback.
        speaker.configure_adaptation(true);
        let mut stats = ArrivalStats::new(origin);
        let mut in_flight: Vec<(u64, u64, bool, Vec<u8>)> = Vec::new();
        let mut report = ReceiveReport {
            lost: fate.iter().filter(|arrival| arrival.is_none()).count() as u64,
            ..ReceiveReport::default()
        };
        let mut encoded = [0u8; 1500];
        let last_send_ms = packets * 10;
        let mut done = false;
        let mut tick = 0u64;
        while !done && tick <= last_send_ms + 2_000 {
            let now = origin + Duration::from_millis(tick);
            if tick.is_multiple_of(10) && (1..=packets).contains(&(tick / 10)) {
                let sequence = tick / 10;
                let wanted = choice.active(policy.observe(stats.quality(now), now), 480);
                // Switching rebuilds the encoder without a restart flag, as
                // the call worker does: the listener keeps decoding.
                let restart = sequence == 1;
                if wanted != recovery {
                    encoder = crate::audio_settings::make_voice_encoder_with_recovery(96, wanted)
                        .unwrap();
                    recovery = wanted;
                    if wanted {
                        report.recovery_on_ms.get_or_insert(tick);
                    }
                }
                let start = (sequence as usize - 1) * 480;
                let size = encoder
                    .encode_float(&speech[start..start + 480], &mut encoded)
                    .unwrap();
                report.redundant_packets += u64::from(carries_redundancy(&encoded[..size]));
                if let Some(arrival) = fate[sequence as usize - 1] {
                    in_flight.push((arrival, sequence, restart, encoded[..size].to_vec()));
                }
            }
            in_flight.sort_by_key(|&(arrival, sequence, _, _)| (arrival, sequence));
            while in_flight
                .first()
                .is_some_and(|&(arrival, ..)| arrival <= tick * 1000)
            {
                let (arrival, sequence, restart, bytes) = in_flight.remove(0);
                let at = origin + Duration::from_micros(arrival);
                let timestamp = (sequence - 1) * 480;
                stats.observe(sequence, timestamp, restart, at);
                let late = speaker.stats.late;
                speaker.push_timed(sequence, timestamp, restart, bytes, at);
                for _ in late..speaker.stats.late {
                    stats.late(at);
                }
            }
            if tick.is_multiple_of(5) {
                let mut output = [0.0; 240];
                let played = speaker.render_quantum(now, &mut output, 10);
                assert!(output.iter().all(|sample| sample.is_finite()));
                if played {
                    report.first_playout_ms.get_or_insert(tick);
                    report.rendered_samples += output.len();
                } else if report.first_playout_ms.is_some() {
                    report.silent_gaps += 1;
                }
                done = speaker.last_played.is_some_and(|played| played >= packets)
                    && speaker.pending_pcm.is_empty();
            }
            tick += 1;
        }
        assert!(done, "{}: the last source frame never played", profile.name);
        report.concealed = speaker.stats.concealed;
        report.recovered = speaker.stats.recovered;
        report.late = speaker.stats.late;
        report.held = speaker.stats.held;
        report.quiet_skips = speaker.stats.quiet_skips;
        report.final_delay_ms = speaker.actual_delay_us / 1000;
        report
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opus::{Application, Encoder};
    fn packet() -> Vec<u8> {
        let mut encoder =
            Encoder::new(AUDIO_SAMPLE_RATE, Channels::Mono, Application::LowDelay).unwrap();
        let mut bytes = [0; 400];
        let n = encoder
            .encode_float(&[0.1; AUDIO_FRAME_SAMPLES], &mut bytes)
            .unwrap();
        bytes[..n].to_vec()
    }

    fn preroll_fixture(amplitude: f32, count: u64) -> Speaker {
        let now = Instant::now();
        let mut speaker = Speaker::new(now).unwrap();
        let mut encoder = Encoder::new(48_000, Channels::Mono, Application::LowDelay).unwrap();
        for sequence in 0..count {
            let pcm = vec![if sequence == 0 { amplitude } else { 0.0 }; 480];
            speaker.push_timed(
                sequence,
                sequence * 480,
                sequence == 0,
                encoder.encode_vec_float(&pcm, 1500).unwrap(),
                now,
            );
        }
        speaker
    }

    #[test]
    fn a_silent_preroll_packet_is_skipped_at_talk_start() {
        let mut speaker = preroll_fixture(0.0, 2);
        let mut output = [0.0; AUDIO_FRAME_SAMPLES];
        assert!(speaker.render(speaker.first_packet, &mut output, 10));
        assert_eq!(speaker.stats.quiet_skips, 1);
        assert_eq!(speaker.last_played, Some(1));
    }

    #[test]
    fn a_quiet_word_onset_in_the_preroll_is_kept() {
        let mut speaker = preroll_fixture(0.005, 2);
        let mut output = [0.0; AUDIO_FRAME_SAMPLES];
        assert!(speaker.render(speaker.first_packet, &mut output, 10));
        assert_eq!(speaker.stats.quiet_skips, 0);
        assert!(!near_silence(&output[..480]));
        assert_eq!(speaker.last_played, Some(0));
    }

    #[test]
    fn only_one_preroll_skip_per_talkspurt() {
        let mut speaker = preroll_fixture(0.0, 3);
        let mut output = [0.0; AUDIO_FRAME_SAMPLES];
        let now = speaker.first_packet;
        assert!(speaker.render(now, &mut output, 10));
        assert!(speaker.render(now + Duration::from_millis(10), &mut output, 10));
        assert_eq!(speaker.stats.quiet_skips, 1);
        assert_eq!(speaker.last_played, Some(2));
    }

    #[test]
    fn queued_redundancy_recovers_a_missing_packet_without_waiting_or_consuming_its_successor() {
        let origin = Instant::now();
        let mut encoder = Encoder::new(48_000, Channels::Mono, Application::Voip).unwrap();
        encoder.set_bitrate(opus::Bitrate::Bits(32000)).unwrap();
        encoder.set_inband_fec(true).unwrap();
        encoder.set_packet_loss_perc(10).unwrap();
        let mut packets = Vec::new();
        for seq in 0..30 {
            let pcm: Vec<_> = (0..960)
                .map(|i| (((seq * 960 + i) as f32) * 0.07).sin() * 0.2)
                .collect();
            packets.push(encoder.encode_vec_float(&pcm, 1500).unwrap());
        }
        let mut speaker = Speaker::new(origin).unwrap();
        let mut reference = Decoder::new(48000, Channels::Mono).unwrap();
        for index in 0..20 {
            let sent = if index == 18 { 19 } else { index };
            speaker.push_timed(
                sent as u64 + 1,
                sent as u64 * 960,
                sent == 0,
                packets[sent].clone(),
                origin,
            );
            let mut expected = [0.0; 960];
            reference
                .decode_float(
                    &packets[if index == 18 { 19 } else { index }],
                    &mut expected,
                    index == 18,
                )
                .unwrap();
            let mut output = [0.0; AUDIO_FRAME_SAMPLES];
            let at = origin + Duration::from_millis(index as u64 * 20);
            assert!(speaker.render(at, &mut output, 20));
            assert_eq!(output, expected, "packet {index}");
            if index == 18 {
                assert!(
                    speaker.packets.contains_key(&20),
                    "recovery consumed the next packet"
                );
                assert_eq!(speaker.next_deadline, Some(at + FRAME));
            }
        }
        // Rebuilt from redundancy, not guessed.
        assert_eq!((speaker.stats.concealed, speaker.stats.recovered), (0, 1));
    }

    fn fec_packets(count: usize, fec: bool) -> Vec<Vec<u8>> {
        let mut encoder = crate::audio_settings::make_voice_encoder_with_recovery(96, fec).unwrap();
        (0..count)
            .map(|seq| {
                let pcm: Vec<_> = (0..480)
                    .map(|i| (((seq * 480 + i) as f32) * 0.07).sin() * 0.2)
                    .collect();
                encoder.encode_vec_float(&pcm, 1500).unwrap()
            })
            .collect()
    }

    #[test]
    fn an_encoder_restart_that_continues_the_source_clock_keeps_playout_timing() {
        // Automatic loss recovery rebuilds the sender's encoder mid-sentence.
        // That restart is not a new talkspurt and must not re-prime the buffer.
        let origin = Instant::now();
        let plain = fec_packets(10, false);
        let protected = fec_packets(10, true);
        let mut speaker = Speaker::new(origin).unwrap();
        speaker.configure_adaptation(true);
        let frame = Duration::from_millis(10);
        let mut output = [0.0; AUDIO_FRAME_SAMPLES];
        let mut gaps = 0;
        for index in 0..20u64 {
            let bytes = if index < 10 {
                plain[index as usize].clone()
            } else {
                protected[index as usize - 10].clone()
            };
            let at = origin + frame * index as u32;
            speaker.push_timed(index + 1, index * 480, index == 0 || index == 10, bytes, at);
            if index == 10 {
                // The link just got worse: the learned target rises to 20 ms.
                speaker.adaptation.as_mut().unwrap().late_packet(at);
            }
            if !speaker.render(at, &mut output, 10) {
                gaps += 1;
            }
        }
        assert_eq!(gaps, 0, "silence inserted mid-sentence");
        assert_eq!(speaker.last_packet_played.unwrap().0, 19);
        // The higher target is reached with one concealed hold instead.
        assert_eq!((speaker.stats.concealed, speaker.stats.held), (0, 1));
    }

    #[test]
    fn redundancy_flag_reads_the_silk_header_of_real_opus_packets() {
        let with = fec_packets(20, true);
        let without = fec_packets(20, false);
        // The first packet has no earlier frame to protect.
        assert!(!carries_redundancy(&with[0]));
        assert!(with[2..].iter().all(|packet| carries_redundancy(packet)));
        assert!(without.iter().all(|packet| !carries_redundancy(packet)));
        assert!(!carries_redundancy(&[]));
        assert!(!carries_redundancy(&[with[5][0]]));
    }

    #[test]
    fn a_two_packet_burst_recovers_its_second_frame_from_redundancy() {
        let origin = Instant::now();
        let packets = fec_packets(30, true);
        let mut speaker = Speaker::new(origin).unwrap();
        let mut reference = Decoder::new(48_000, Channels::Mono).unwrap();
        let frame = Duration::from_millis(10);
        // Packets 11 and 12 are lost; 13 is queued when 11 is due. Three
        // packets wait ahead of playout, well inside the storage cap.
        let push = |speaker: &mut Speaker, index: usize| {
            if index != 10 && index != 11 {
                // No restart flag: a quiet first frame would be skipped as preroll.
                speaker.push_timed(
                    index as u64 + 1,
                    index as u64 * 480,
                    false,
                    packets[index].clone(),
                    origin,
                );
            }
        };
        for index in 0..3 {
            push(&mut speaker, index);
        }
        for index in 0..18usize {
            push(&mut speaker, index + 3);
            let mut expected = [0.0; 480];
            match index {
                10 => reference.decode_float(&[], &mut expected, false),
                11 => reference.decode_float(&packets[12], &mut expected, true),
                _ => reference.decode_float(&packets[index], &mut expected, false),
            }
            .unwrap();
            let mut output = [0.0; AUDIO_FRAME_SAMPLES];
            assert!(speaker.render(origin + frame * index as u32, &mut output, 20));
            assert_eq!(output[..480], expected, "frame {index}");
        }
        assert_eq!((speaker.stats.concealed, speaker.stats.recovered), (1, 1));
        assert_eq!(speaker.last_packet_played.unwrap().0, 18);
    }

    #[test]
    fn quiet_speech_survives_a_standing_backlog_without_shortening_words() {
        let origin = Instant::now();
        let mut encoder = Encoder::new(48_000, Channels::Mono, Application::LowDelay).unwrap();
        let mut speaker = Speaker::new(origin).unwrap();
        // Enough queued speech to activate standing-queue recovery. Its level
        // is below the old peak threshold, but must still be heard in full.
        for seq in 1..=200u64 {
            let pcm: Vec<_> = (0..480)
                .map(|i| (((seq - 1) * 480 + i) as f32 * 0.07).sin() * 0.003)
                .collect();
            let mut bytes = [0; 400];
            let n = encoder.encode_float(&pcm, &mut bytes).unwrap();
            let arrival = origin + Duration::from_millis((seq - 1) * 10);
            speaker.push_timed(seq, (seq - 1) * 480, seq == 1, bytes[..n].to_vec(), arrival);
            if seq >= 7 {
                let mut output = [0.0; AUDIO_FRAME_SAMPLES];
                assert!(speaker.render(arrival, &mut output, 10));
            }
        }
        assert_eq!(speaker.stats.quiet_skips, 0, "quiet speech was shortened");
        assert!(near_silence(&[0.00001; 480]));
        assert!(!near_silence(&[0.0005; 480]));
        assert!(!near_silence(&[f32::NAN; 480]));
    }

    #[test]
    fn a_long_mixer_stall_discards_the_tail_even_before_the_next_packet_is_very_late() {
        let now = Instant::now();
        let mut speaker = Speaker::new(now).unwrap();
        speaker.push_timed(1, 0, true, packet(), now);
        speaker.push_timed(2, AUDIO_FRAME_SAMPLES as u64, false, packet(), now);
        let mut output = [0.0; 120];
        assert!(speaker.render_quantum(now, &mut output, 20));
        assert_eq!(speaker.pending_pcm.len(), AUDIO_FRAME_SAMPLES - 120);
        // The next 20 ms source packet is only 5 ms late, but the next
        // decoded mixer fraction is 22.5 ms late and must be discarded.
        assert!(speaker.render_quantum(now + Duration::from_millis(25), &mut output, 20));
        assert_eq!(speaker.last_played, Some(2));
        assert_eq!(speaker.pending_pcm.len(), AUDIO_FRAME_SAMPLES - 120);
        assert_eq!(speaker.stats.concealed, 0);
    }

    #[test]
    fn stable_ten_ms_path_starts_ten_ms_earlier_without_losing_audio() {
        let origin = Instant::now();
        let mut encoder = Encoder::new(48_000, Channels::Mono, Application::LowDelay).unwrap();
        let mut baseline = Speaker::new(origin).unwrap();
        let mut optimized = Speaker::new(origin).unwrap();
        optimized.configure_adaptation(true);
        let mut first_output = [None, None];
        let mut output_samples = [0usize; 2];
        let mut rendered_audio = [Vec::new(), Vec::new()];
        // 1,000 real Opus packets over a deterministic, stable 5 ms network.
        // Source and recipient clocks are explicit; no wall-clock sleeps or devices.
        for millis in 0..10_030u64 {
            let now = origin + Duration::from_millis(millis);
            if (15..=10_005).contains(&millis) && (millis - 15) % 10 == 0 {
                let sequence = (millis - 15) / 10 + 1;
                let pcm: Vec<_> = (0..480)
                    .map(|i| ((i as u64 + (sequence - 1) * 480) as f32 * 0.07).sin() * 0.2)
                    .collect();
                let mut bytes = [0; 400];
                let size = encoder.encode_float(&pcm, &mut bytes).unwrap();
                for speaker in [&mut baseline, &mut optimized] {
                    speaker.push_timed(
                        sequence,
                        (sequence - 1) * 480,
                        sequence == 1,
                        bytes[..size].to_vec(),
                        now,
                    );
                }
            }
            if millis % 5 == 0 {
                for (index, speaker) in [&mut baseline, &mut optimized].into_iter().enumerate() {
                    // Stop after the last real packet's tail; end-of-call silence
                    // is not packet loss and must not enter the comparison.
                    if output_samples[index] == 480_000 {
                        continue;
                    }
                    let mut output = [0.0; 240];
                    if speaker.render_quantum(now, &mut output, 20) {
                        first_output[index].get_or_insert(millis);
                        output_samples[index] += output.len();
                        assert!(output.iter().all(|sample| sample.is_finite()));
                        rendered_audio[index].extend_from_slice(&output);
                    }
                }
            }
        }
        assert_eq!(first_output, [Some(25), Some(15)]);
        assert_eq!(output_samples, [480_000, 480_000]);
        assert_eq!(rendered_audio[0], rendered_audio[1]);
        for speaker in [&baseline, &optimized] {
            assert_eq!(speaker.last_packet_played.unwrap().0, 1000);
            assert_eq!(speaker.stats.concealed, 0);
            assert_eq!(speaker.stats.late, 0);
            assert_eq!(speaker.stats.held, 0);
            assert_eq!(speaker.stats.quiet_skips, 0);
        }
        println!("Stable 10 ms Opus path: first playout 25 -> 15 ms; all 480000 samples retained; zero concealment. Excludes physical audio devices.");
    }

    #[test]
    fn two_point_five_ms_opus_keeps_exact_clock_over_ten_seconds() {
        let origin = Instant::now();
        let mut speaker = Speaker::new(origin).unwrap();
        speaker.configure_adaptation(true);
        let mut encoder = Encoder::new(48_000, Channels::Mono, Application::LowDelay).unwrap();
        let mut encoded = [0; 400];
        let mut first = None;
        let mut rendered = 0;
        for tick in 0..4010_u64 {
            let now = origin + Duration::from_micros(tick * 2500);
            if tick < 4000 {
                let pcm: Vec<_> = (0..120)
                    .map(|i| ((tick * 120 + i) as f32 * 0.03).sin() * 0.2)
                    .collect();
                let size = encoder.encode_float(&pcm, &mut encoded).unwrap();
                assert_eq!(
                    opus::packet::get_nb_samples(&encoded[..size], 48_000).unwrap(),
                    120
                );
                speaker.push_timed(
                    tick + 1,
                    tick * 120,
                    tick == 0,
                    encoded[..size].to_vec(),
                    now,
                );
            }
            if rendered < 480_000 {
                let mut output = [0.; 120];
                if speaker.render_quantum(now, &mut output, 10) {
                    first.get_or_insert(tick);
                    rendered += output.len();
                    assert!(output.iter().all(|x| x.is_finite()));
                }
            }
        }
        assert_eq!(first, Some(3)); // Four packets provide the 10 ms priming target.
        assert_eq!(rendered, 480_000);
        assert_eq!(speaker.expected, Some((4001, 480_000)));
        assert_eq!(
            speaker.next_deadline,
            Some(origin + Duration::from_micros(4003 * 2500))
        );
        assert_eq!(speaker.stats.concealed, 0);
        assert_eq!(speaker.stats.late, 0);
    }

    #[test]
    fn adaptive_packet_quantization_never_shrinks_below_floor_or_holds_above_ceiling() {
        for samples in [120, 240, 480, 960] {
            let origin = Instant::now();
            let frame_us = (samples * 1_000_000 / 48_000) as u64;
            let mut speaker = Speaker::new(origin).unwrap();
            speaker.configure_adaptation(true);
            let mut encoder = Encoder::new(48_000, Channels::Mono, Application::LowDelay).unwrap();
            let mut bytes = [0; 400];
            let size = encoder
                .encode_float(&vec![0.0; samples], &mut bytes)
                .unwrap();
            for sequence in 1..=8 {
                speaker.push_timed(
                    sequence,
                    (sequence - 1) * samples as u64,
                    sequence == 1,
                    bytes[..size].to_vec(),
                    origin,
                );
            }
            let policy = speaker.adaptation.as_mut().unwrap();
            for second in 0..5 {
                let now = origin + Duration::from_secs(second);
                policy.late_packet(now);
                policy.late_packet(now);
            }
            let mut output = [0.; AUDIO_FRAME_SAMPLES];
            for index in 0..8 {
                speaker.render(
                    origin + Duration::from_micros(index * frame_us),
                    &mut output,
                    20,
                );
                assert!(speaker.actual_delay_us <= crate::adaptive_audio::MAX_DELAY_MS * 1000);
            }
            // Silence can shorten a recovered buffer by whole packets only.
            speaker.adaptation = Some(AdaptiveDelay::default());
            for sequence in 9..=200 {
                let now = origin + Duration::from_micros((sequence - 1) * frame_us);
                speaker.push_timed(
                    sequence,
                    (sequence - 1) * samples as u64,
                    false,
                    bytes[..size].to_vec(),
                    now,
                );
                speaker.render(now, &mut output, 20);
                assert!(speaker.actual_delay_us >= crate::adaptive_audio::MIN_DELAY_MS * 1000);
                assert!(speaker.actual_delay_us <= crate::adaptive_audio::MAX_DELAY_MS * 1000);
            }
        }
    }
    #[test]
    fn all_packet_durations_mix_in_two_point_five_ms_quanta_and_restart_without_stretching() {
        let now = Instant::now();
        let mut speakers: Vec<_> = [120, 240, 480, 960]
            .into_iter()
            .map(|samples| {
                let mut speaker = Speaker::new(now).unwrap();
                let mut encoder =
                    Encoder::new(48_000, Channels::Mono, Application::LowDelay).unwrap();
                for sequence in 1..=4 {
                    let pcm: Vec<_> = (0..samples)
                        .map(|i| (i as f32 * 0.07).sin() * 0.2)
                        .collect();
                    let mut bytes = [0; 400];
                    let size = encoder.encode_float(&pcm, &mut bytes).unwrap();
                    speaker.push_timed(
                        sequence,
                        (sequence - 1) * samples as u64,
                        sequence == 1,
                        bytes[..size].to_vec(),
                        now,
                    );
                }
                (samples, speaker)
            })
            .collect();
        for (samples, speaker) in &mut speakers {
            for quantum in 0..(*samples / 120 * 4) {
                let mut output = [0.; 120];
                assert!(speaker.render_quantum(
                    now + Duration::from_micros(quantum as u64 * 2500),
                    &mut output,
                    5
                ));
                assert!(output.iter().all(|x| x.is_finite()));
                if quantum > 0 {
                    assert!(output.iter().any(|x| x.abs() > 0.01));
                }
            }
            assert!(speaker.pending_pcm.is_empty());
            assert_eq!(speaker.last_packet_played, Some((4, 3 * *samples as u64)));
            assert_eq!(speaker.stats.concealed, 0);
            let mut encoder = Encoder::new(48_000, Channels::Mono, Application::LowDelay).unwrap();
            let mut bytes = [0; 400];
            let size = encoder.encode_float(&[0.2; 240], &mut bytes).unwrap();
            let at = now + Duration::from_millis(100);
            speaker.push_timed(5, 4800, true, bytes[..size].to_vec(), at);
            assert!(speaker.render_quantum(at, &mut [0.; 240], 5));
            assert_eq!(speaker.frame_samples, 240);
            assert_eq!(speaker.expected, Some((6, 5040)));
            assert_eq!(speaker.next_deadline, Some(at + Duration::from_millis(5)));
        }
    }

    #[test]
    fn short_packet_loss_conceals_exactly_one_packet_and_reorder_keeps_source_clock() {
        for samples in [240, 480] {
            let now = Instant::now();
            let mut speaker = Speaker::new(now).unwrap();
            let mut encoder = Encoder::new(48_000, Channels::Mono, Application::LowDelay).unwrap();
            let mut encoded = [0; 400];
            let size = encoder
                .encode_float(&vec![0.2; samples], &mut encoded)
                .unwrap();
            let bytes = encoded[..size].to_vec();
            speaker.push_timed(1, 0, true, bytes.clone(), now);
            speaker.push_timed(4, 3 * samples as u64, false, bytes.clone(), now);
            speaker.push_timed(3, 2 * samples as u64, false, bytes, now);
            let mut output = [0.; AUDIO_FRAME_SAMPLES];
            for frame in 0..4 {
                assert!(speaker.render(
                    now + Duration::from_millis((frame * samples / 48) as u64),
                    &mut output,
                    5
                ));
            }
            assert_eq!(speaker.stats.concealed, 1);
            assert_eq!(speaker.stats.reordered, 1);
            assert_eq!(speaker.expected, Some((5, 4 * samples as u64)));
            assert!(output[samples..].iter().all(|x| *x == 0.));
            assert_eq!(speaker.last_packet_played, Some((4, 3 * samples as u64)));
        }
    }

    #[test]
    fn lost_duration_change_restart_still_recovers_from_the_next_opus_packet() {
        let now = Instant::now();
        let mut speaker = Speaker::new(now).unwrap();
        speaker.push_timed(1, 0, true, packet(), now);
        let mut output = [0.; AUDIO_FRAME_SAMPLES];
        assert!(speaker.render(now, &mut output, 5));
        let mut encoder = Encoder::new(48_000, Channels::Mono, Application::LowDelay).unwrap();
        let mut bytes = [0; 400];
        // Source packet 2 (the new restart) was lost; packet 3 is still playable.
        let size = encoder.encode_float(&[0.2; 240], &mut bytes).unwrap();
        speaker.push_timed(
            3,
            1200,
            false,
            bytes[..size].to_vec(),
            now + Duration::from_millis(20),
        );
        assert!(speaker.render(now + Duration::from_millis(20), &mut output, 5));
        assert!(speaker.render(now + Duration::from_millis(25), &mut output, 5));
        assert_eq!(speaker.stats.concealed, 1);
        assert_eq!(speaker.last_packet_played, Some((3, 1200)));
        assert_eq!(speaker.expected, Some((4, 1440)));
    }

    #[test]
    fn short_packet_priming_counts_audio_duration_and_rejects_oversized_opus() {
        let now = Instant::now();
        let mut speaker = Speaker::new(now).unwrap();
        let mut encoder = Encoder::new(48_000, Channels::Mono, Application::LowDelay).unwrap();
        let mut encoded = [0; 800];
        let size = encoder.encode_float(&[0.2; 240], &mut encoded).unwrap();
        let bytes = encoded[..size].to_vec();
        speaker.push_timed(1, 0, true, bytes.clone(), now);
        assert_eq!(
            speaker.next_playout_deadline(now, 20),
            Some(now + Duration::from_millis(20))
        );
        for sequence in 2..=3 {
            speaker.push_timed(
                sequence,
                (sequence - 1) * 240,
                false,
                bytes.clone(),
                now + Duration::from_millis((sequence - 1) * 5),
            );
        }
        assert_eq!(
            speaker.next_playout_deadline(now + Duration::from_millis(10), 20),
            Some(now + Duration::from_millis(20))
        );
        let at = now + Duration::from_millis(15);
        speaker.push_timed(4, 720, false, bytes, at);
        assert_eq!(speaker.next_playout_deadline(at, 20), Some(at));
        let mut output = [0.; 240];
        assert!(speaker.render_quantum(at, &mut output, 20));
        let size = encoder.encode_float(&[0.2; 1920], &mut encoded).unwrap();
        speaker.push_timed(5, 960, false, encoded[..size].to_vec(), at);
        assert!(!speaker.packets.contains_key(&5));
        let size = encoder.encode_float(&[0.2; 960], &mut encoded).unwrap();
        let mut fresh = Speaker::new(now).unwrap();
        fresh.push_timed(1, 0, true, encoded[..size].to_vec(), now);
        assert!(fresh.render_quantum(now, &mut output, 20));
        assert_eq!(fresh.pending_pcm.len(), 720);
        let _ = fresh.render_quantum(now + Duration::from_millis(100), &mut output, 20);
        assert_eq!(
            fresh.stats.concealed, 1,
            "stalled output decodes a new interval instead of draining the old tail"
        );
    }

    #[test]
    fn minimum_target_plays_first_packet_immediately_without_capture_phase_wait() {
        let origin = Instant::now();
        let arrival = origin + Duration::from_millis(7);
        let mut speaker = Speaker::new(origin).unwrap();
        assert_eq!(speaker.next_playout_deadline(origin, 20), None);
        speaker.push_timed(1, 0, true, packet(), arrival);
        assert_eq!(speaker.next_playout_deadline(arrival, 20), Some(arrival));
        let mut out = [0.0; AUDIO_FRAME_SAMPLES];
        assert!(speaker.render(arrival, &mut out, 20));
        assert_eq!(speaker.last_played, Some(1));
        assert_eq!(
            speaker.next_playout_deadline(arrival, 20),
            Some(arrival + FRAME)
        );
        assert!(!speaker.render(arrival, &mut out, 20));
        assert_eq!(speaker.stats.concealed, 0);
    }
    #[test]
    fn initial_deadline_starts_on_packet_count_or_target_timer_whichever_is_first() {
        let now = Instant::now();
        let mut speaker = Speaker::new(now).unwrap();
        let mut out = [0.0; AUDIO_FRAME_SAMPLES];
        speaker.push_timed(1, 0, true, packet(), now);
        assert_eq!(
            speaker.next_playout_deadline(now + Duration::from_millis(9), 40),
            Some(now + Duration::from_millis(40))
        );
        assert!(!speaker.render(now + Duration::from_millis(19), &mut out, 40));
        let second = now + FRAME;
        speaker.push_timed(2, 960, false, packet(), second);
        assert_eq!(speaker.next_playout_deadline(second, 40), Some(second));
        assert!(speaker.render(second, &mut out, 40));
        assert_eq!(
            speaker.next_playout_deadline(second, 40),
            Some(second + FRAME)
        );
        let mut lone = Speaker::new(now).unwrap();
        lone.push_timed(1, 0, true, packet(), now);
        assert!(!lone.render(now + Duration::from_millis(39), &mut out, 40));
        assert!(lone.render(now + Duration::from_millis(40), &mut out, 40));
        assert_eq!(
            lone.next_playout_deadline(now + Duration::from_millis(40), 40),
            Some(now + Duration::from_millis(60))
        );
    }
    #[test]
    fn deadlines_include_bounded_loss_concealment_then_stop_when_source_is_idle() {
        let now = Instant::now();
        let mut speaker = Speaker::new(now).unwrap();
        let mut out = [0.0; AUDIO_FRAME_SAMPLES];
        speaker.push_timed(1, 0, true, packet(), now);
        assert!(speaker.render(now, &mut out, 20));
        for missing in 1..=3 {
            let due = now + FRAME * missing;
            assert_eq!(speaker.next_playout_deadline(due, 20), Some(due));
            assert!(speaker.render(due, &mut out, 20));
            assert!(!speaker.render(due, &mut out, 20));
        }
        let idle = now + FRAME * 4;
        assert!(!speaker.render(idle, &mut out, 20));
        assert_eq!(speaker.stats.concealed, 3);
        assert_eq!(speaker.next_playout_deadline(idle, 20), None);
        let resume = idle + Duration::from_millis(7);
        speaker.push_timed(5, 48_000, true, packet(), resume);
        assert_eq!(speaker.next_playout_deadline(resume, 20), Some(resume));
        assert!(speaker.render(resume, &mut out, 20));
        assert_eq!(speaker.last_played, Some(5));
        assert_eq!(speaker.stats.concealed, 3);
    }
    #[test]
    fn stalled_playout_never_bursts_even_when_exactly_one_frame_late() {
        for late in [FRAME, Duration::from_millis(100)] {
            let now = Instant::now();
            let mut speaker = Speaker::new(now).unwrap();
            let mut out = [0.0; AUDIO_FRAME_SAMPLES];
            for sequence in 1..=6 {
                speaker.push_timed(sequence, (sequence - 1) * 960, false, packet(), now);
            }
            assert!(speaker.render(now, &mut out, 20));
            let stalled = now + FRAME + late;
            assert!(speaker.render(stalled, &mut out, 20));
            assert_eq!(
                speaker.next_playout_deadline(stalled, 20),
                Some(stalled + FRAME)
            );
            assert!(!speaker.render(stalled, &mut out, 20));
            assert!(!speaker.render(stalled + FRAME - Duration::from_millis(1), &mut out, 20));
            assert!(speaker.render(stalled + FRAME, &mut out, 20));
            assert_eq!(speaker.last_played, Some(3));
            assert_eq!(speaker.stats.concealed, 0);
        }
    }
    #[test]
    fn gated_restart_does_not_lose_real_packets_to_concealed_sequence_positions() {
        for concealed in [1, 3] {
            let now = Instant::now();
            let mut speaker = Speaker::new(now).unwrap();
            let mut out = [0.0; AUDIO_FRAME_SAMPLES];
            speaker.push_timed(1, 0, true, packet(), now);
            assert!(speaker.render(now, &mut out, 20));
            for missing in 1..=concealed {
                assert!(speaker.render(now + FRAME * missing, &mut out, 20));
            }
            let resume = now + Duration::from_millis(200);
            speaker.push_timed(2, 9600, true, packet(), resume);
            speaker.push_timed(3, 10560, false, packet(), resume);
            assert_eq!(speaker.next_playout_deadline(resume, 20), Some(resume));
            assert!(speaker.render(resume, &mut out, 20));
            assert_eq!(speaker.last_played, Some(2));
            assert!(speaker.render(resume + FRAME, &mut out, 20));
            assert_eq!(speaker.last_played, Some(3));
            assert_eq!(speaker.stats.late, 0);
            // The restart exception cannot revive a previously played real
            // packet or a timestamp from before the new talkspurt.
            speaker.push_timed(2, 9600, true, packet(), resume);
            speaker.push_timed(3, 0, true, packet(), resume);
            assert_eq!(speaker.stats.late, 2);
        }
    }
    #[test]
    fn reorder_loss_and_deadlines_preserve_source_duration() {
        let now = Instant::now();
        let mut speaker = Speaker::new(now).unwrap();
        speaker.push_timed(1, 0, true, packet(), now);
        speaker.push_timed(4, 2880, false, packet(), now);
        speaker.push_timed(2, 960, false, packet(), now);
        let mut out = [0.0; AUDIO_FRAME_SAMPLES];
        assert!(speaker.render(now, &mut out, 40));
        assert!(!speaker.render(now + Duration::from_millis(5), &mut out, 40));
        assert!(speaker.render(now + Duration::from_millis(20), &mut out, 40));
        assert!(speaker.render(now + Duration::from_millis(40), &mut out, 40));
        assert_eq!(speaker.stats.concealed, 1);
        assert!(
            speaker.packets.contains_key(&4),
            "future audio cannot replace a lost deadline"
        );
        assert!(speaker.render(now + Duration::from_millis(60), &mut out, 40));
        assert_eq!(speaker.stats.reordered, 1);
        speaker.push_timed(3, 1920, false, packet(), now);
        assert_eq!(speaker.stats.late, 1);
    }
    #[test]
    fn interleaved_transport_counters_do_not_change_per_speaker_clock() {
        let now = Instant::now();
        let mut a = Speaker::new(now).unwrap();
        let mut b = Speaker::new(now).unwrap();
        for speaker in [&mut a, &mut b] {
            speaker.push_timed(1, 0, true, packet(), now);
            speaker.push_timed(2, 960, false, packet(), now);
        }
        let mut out = [0.0; AUDIO_FRAME_SAMPLES];
        for millis in [0, 20] {
            for speaker in [&mut a, &mut b] {
                assert!(speaker.render(now + Duration::from_millis(millis), &mut out, 40));
            }
        }
        assert_eq!(a.stats.concealed + b.stats.concealed, 0);
    }
    #[test]
    fn gated_resume_uses_restart_without_old_loss_or_unbounded_storage() {
        let now = Instant::now();
        let mut speaker = Speaker::new(now).unwrap();
        let mut out = [0.0; AUDIO_FRAME_SAMPLES];
        speaker.push_timed(1, 0, true, packet(), now);
        assert!(speaker.render(now + FRAME, &mut out, 20));
        speaker.push_timed(2, 48_000, true, packet(), now + Duration::from_secs(1));
        assert!(speaker.render(now + Duration::from_secs(1), &mut out, 20));
        assert_eq!(speaker.stats.concealed, 0);
        for sequence in 3..50 {
            speaker.push_timed(sequence, sequence * 960, false, packet(), now);
        }
        assert_eq!(speaker.packets.len(), 8); // 160 ms cap for 20 ms packets.
    }

    #[test]
    fn adaptive_hold_preserves_a_late_packet_and_never_replays_real_audio() {
        let now = Instant::now();
        let frame = Duration::from_millis(10);
        let mut encoder = Encoder::new(48_000, Channels::Mono, Application::LowDelay).unwrap();
        let encoded = encoder.encode_vec_float(&[0.1; 480], 1500).unwrap();
        let mut speaker = Speaker::new(now).unwrap();
        speaker.configure_adaptation(true);
        let mut out = [0.0; AUDIO_FRAME_SAMPLES];
        speaker.push_timed(1, 0, true, encoded.clone(), now);
        assert!(speaker.render(now, &mut out, 20));
        speaker.push_timed(3, 960, false, encoded.clone(), now + frame);
        assert!(speaker.render(now + frame, &mut out, 20));
        assert_eq!(speaker.last_played, Some(2)); // One real loss concealment.
        speaker.push_timed(
            2,
            480,
            false,
            encoded.clone(),
            now + frame + Duration::from_millis(1),
        );
        assert!(speaker.render(now + frame * 2, &mut out, 20));
        assert_eq!(speaker.last_packet_played.unwrap().0, 1);
        assert_eq!(speaker.expected.unwrap().0, 3); // Real packet 3 waits through one hold.
        speaker.push_timed(5, 1920, false, encoded.clone(), now + frame * 3);
        assert!(speaker.render(now + frame * 3, &mut out, 20));
        assert_eq!(speaker.stats.held, 1);
        assert_eq!(speaker.expected.unwrap().0, 4);
        assert_eq!(speaker.last_played, Some(3));
        assert_eq!(speaker.buffer_target_ms(20), 20);
        speaker.push_timed(
            4,
            1440,
            false,
            encoded.clone(),
            now + frame * 3 + Duration::from_millis(5),
        );
        assert!(speaker.render(now + frame * 4, &mut out, 20));
        assert_eq!(speaker.last_packet_played.unwrap().0, 4);
        assert!(!speaker.render(now + frame * 4, &mut out, 20));
        assert!(speaker.render(now + frame * 5, &mut out, 20));
        assert_eq!(speaker.last_packet_played.unwrap().0, 5);
        assert_eq!(speaker.stats.concealed, 1);
    }

    #[test]
    fn adaptive_reorder_trace_has_bounded_memory_and_live_recovery() {
        let origin = Instant::now();
        let encoded = packet();
        let mut encoder =
            Encoder::new(AUDIO_SAMPLE_RATE, Channels::Mono, Application::LowDelay).unwrap();
        let mut silent_bytes = [0; 400];
        let count = encoder
            .encode_float(&[0.0; AUDIO_FRAME_SAMPLES], &mut silent_bytes)
            .unwrap();
        let quiet = silent_bytes[..count].to_vec();
        let mut arrivals = (1..=2000u64)
            .map(|sequence| {
                let source_ms = (sequence - 1) * 20;
                let delay = if sequence < 300 && sequence % 3 == 0 {
                    55
                } else {
                    20
                };
                (source_ms + delay, sequence)
            })
            .collect::<Vec<_>>();
        arrivals.sort_unstable();
        let mut speaker = Speaker::new(origin).unwrap();
        speaker.configure_adaptation(true);
        let mut next = 0;
        let mut max_target = 20;
        let mut last_real = 0;
        let mut out = [0.0; AUDIO_FRAME_SAMPLES];
        for millis in 0..=40_100 {
            let now = origin + Duration::from_millis(millis);
            while next < arrivals.len() && arrivals[next].0 <= millis {
                let sequence = arrivals[next].1;
                speaker.push_timed(
                    sequence,
                    (sequence - 1) * 960,
                    sequence == 1,
                    if sequence < 300 {
                        encoded.clone()
                    } else {
                        quiet.clone()
                    },
                    now,
                );
                next += 1;
            }
            if millis % 2 == 0 {
                speaker.render(now, &mut out, 20);
                if let Some((played, _)) = speaker.last_packet_played {
                    assert!(played >= last_real);
                    last_real = played;
                }
                assert!(out.iter().all(|sample| sample.is_finite()));
            }
            let target = speaker.buffer_target_ms(20);
            assert!((10..=80).contains(&target));
            max_target = max_target.max(target);
            assert!(speaker.packets.len() <= MAX_PACKETS);
        }
        assert!(
            max_target >= 40,
            "arrival variation must change actual playout, not only a displayed setting"
        );
        assert_eq!(speaker.buffer_target_ms(20), 10);
        assert!(speaker.stats.reordered > 0 && speaker.stats.held > 0);
        assert!(speaker.stats.quiet_skips > 0);
        assert_eq!(last_real, 2000);
    }

    #[test]
    fn adaptive_stalled_worker_discards_backlog_and_waits_for_future_deadline() {
        let now = Instant::now();
        let mut speaker = Speaker::new(now).unwrap();
        speaker.configure_adaptation(true);
        let mut out = [0.0; AUDIO_FRAME_SAMPLES];
        speaker.push_timed(1, 0, true, packet(), now);
        assert!(speaker.render(now, &mut out, 20));
        for seq in 2..=16u64 {
            speaker.push_timed(
                seq,
                (seq - 1) * 960,
                false,
                packet(),
                now + FRAME * (seq as u32 - 1),
            );
        }
        let resumed = now + Duration::from_millis(300);
        assert!(speaker.render(resumed, &mut out, 20));
        assert_eq!(speaker.last_packet_played.unwrap().0, 16);
        assert!(!speaker.render(resumed, &mut out, 20));
        assert_eq!(
            speaker.next_playout_deadline(resumed, 20),
            Some(resumed + FRAME)
        );
        assert!(speaker.packets.is_empty());
    }

    #[test]
    fn adaptive_talkspurt_and_manual_switch_preserve_source_positions() {
        let now = Instant::now();
        let mut speaker = Speaker::new(now).unwrap();
        speaker.configure_adaptation(true);
        let mut out = [0.0; AUDIO_FRAME_SAMPLES];
        speaker.push_timed(1, 0, true, packet(), now);
        assert!(speaker.render(now, &mut out, 20));
        speaker
            .adaptation
            .as_mut()
            .unwrap()
            .late_packet(now + FRAME);
        speaker
            .adaptation
            .as_mut()
            .unwrap()
            .late_packet(now + FRAME);
        let resumed = now + Duration::from_secs(1);
        speaker.push_timed(2, 48_000, true, packet(), resumed);
        // One +10 ms raise reaches 20 ms, exactly one 20 ms source packet.
        // The new talkspurt need not wait for a second packet.
        assert!(speaker.render(resumed, &mut out, 20));
        assert_eq!(speaker.last_packet_played, Some((2, 48_000)));
        speaker.configure_adaptation(false);
        assert_eq!(speaker.buffer_target_ms(120), 120);
        speaker.push_timed(2, 48_000, true, packet(), resumed + FRAME * 3);
        assert!(speaker.packets.is_empty()); // Already played, never duplicated.
    }

    /// 10 ms Opus frames of one talkspurt: 0.7 s voiced, 0.3 s pause, repeated.
    fn talker(frames: usize) -> (Vec<Vec<u8>>, Vec<bool>) {
        let mut encoder = Encoder::new(48_000, Channels::Mono, Application::LowDelay).unwrap();
        let mut encoded = [0; 400];
        (0..frames)
            .map(|index| {
                let voiced = index % 100 < 70;
                let pcm: Vec<f32> = (0..480)
                    .map(|i| {
                        if voiced {
                            ((index * 480 + i) as f32 * 0.05).sin() * 0.3
                        } else {
                            0.0
                        }
                    })
                    .collect();
                let size = encoder.encode_float(&pcm, &mut encoded).unwrap();
                (encoded[..size].to_vec(), voiced)
            })
            .unzip()
    }

    struct Played {
        speaker: Speaker,
        /// (sequence, send-to-playout delay in ms) for every real packet heard.
        heard: Vec<(u64, f64)>,
        /// Real packets decoded but left out of the output by a quiet skip.
        omitted: Vec<u64>,
        /// Counters when the final packet was heard, before the up to three
        /// concealed frames that follow a talker who stopped.
        stats: JitterStats,
    }

    /// Source frame `n` is sent at `(n - 1) * 10 ms`; `arrivals` holds
    /// (sequence, arrival µs). Output runs in 2.5 ms device quanta.
    fn play(frames: &[Vec<u8>], arrivals: &[(u64, u64)], jitter_ms: u32, adaptive: bool) -> Played {
        let origin = Instant::now();
        let mut arrivals = arrivals.to_vec();
        arrivals.sort_by_key(|&(sequence, at)| (at, sequence));
        let mut speaker = Speaker::new(origin).unwrap();
        speaker.configure_adaptation(adaptive);
        let (mut heard, mut omitted) = (Vec::new(), Vec::new());
        let mut stats = JitterStats::default();
        let last = arrivals
            .iter()
            .map(|&(sequence, _)| sequence)
            .max()
            .unwrap();
        let mut next = 0;
        let end = arrivals.last().unwrap().1 + 200_000;
        for tick in (0..=end).step_by(2500) {
            while let Some(&(sequence, at)) = arrivals.get(next).filter(|(_, at)| *at <= tick) {
                let bytes = frames[sequence as usize - 1].clone();
                let timestamp = (sequence - 1) * 480;
                let at = origin + Duration::from_micros(at);
                speaker.push_timed(sequence, timestamp, sequence == 1, bytes, at);
                next += 1;
            }
            let before = (speaker.last_packet_played, speaker.stats.quiet_skips);
            let now = origin + Duration::from_micros(tick);
            speaker.render_quantum(now, &mut [0.0; 120], jitter_ms);
            if let Some((sequence, _)) = speaker
                .last_packet_played
                .filter(|_| speaker.last_packet_played != before.0)
            {
                assert!(
                    before.0.is_none_or(|(old, _)| sequence > old),
                    "replayed old audio"
                );
                if speaker.stats.quiet_skips > before.1 {
                    omitted.push(sequence - 1);
                }
                let sent = (sequence - 1) as f64 * 10.0;
                heard.push((sequence, tick as f64 / 1000.0 - sent));
                if sequence == last {
                    stats = speaker.stats;
                }
            }
        }
        Played {
            speaker,
            heard,
            omitted,
            stats,
        }
    }

    fn median_delay(played: &Played, sequences: std::ops::RangeInclusive<u64>) -> f64 {
        let mut delays: Vec<f64> = played
            .heard
            .iter()
            .filter(|(sequence, _)| sequences.contains(sequence))
            .map(|&(_, delay)| delay)
            .collect();
        delays.sort_by(f64::total_cmp);
        delays[delays.len() / 2]
    }

    fn target_after(played: &Played, jitter_ms: u32) -> f64 {
        let speaker = &played.speaker;
        f64::from(
            speaker
                .adaptation
                .as_ref()
                .map_or(jitter_ms, |p| p.target_ms()),
        )
    }

    #[test]
    fn faster_path_after_switch_drains_standing_queue_in_quiet_frames_only() {
        let (frames, voiced) = talker(800);
        // A 50 ms relayed path, then a 1 ms direct path from frame 201,
        // which is mid-word: the drain must wait for the next pause.
        let arrivals: Vec<_> = (1..=800u64)
            .map(|sequence| {
                let network = if sequence <= 200 { 50_000 } else { 1_000 };
                (sequence, (sequence - 1) * 10_000 + network)
            })
            .collect();
        for (jitter_ms, adaptive) in [(10, false), (40, false), (10, true)] {
            let played = play(&frames, &arrivals, jitter_ms, adaptive);
            let stats = played.stats;
            assert_eq!(
                (stats.late, stats.concealed),
                (0, 0),
                "{jitter_ms} {adaptive}"
            );
            assert!(stats.quiet_skips > 0);
            for sequence in &played.omitted {
                assert!(
                    !voiced[*sequence as usize - 1],
                    "speech frame {sequence} skipped"
                );
                assert!(
                    *sequence > 270,
                    "drained before the first pause: {sequence}"
                );
            }
            for (index, _) in voiced.iter().enumerate().filter(|(_, voiced)| **voiced) {
                let sequence = index as u64 + 1;
                assert!(played.heard.iter().any(|(heard, _)| *heard == sequence));
            }
            let relayed = median_delay(&played, 100..=200);
            let direct = median_delay(&played, 780..=800);
            let target = target_after(&played, jitter_ms);
            println!(
                "Relay 50 ms -> direct 1 ms, {jitter_ms} ms adaptive={adaptive}: send-to-playout {relayed} -> {direct} ms, {} quiet frames left out. Virtual clock, excludes devices.",
                played.omitted.len()
            );
            assert!(relayed >= 50.0, "{relayed}");
            assert!(
                direct <= 1.0 + target + 5.0,
                "{jitter_ms} ms adaptive={adaptive}: {relayed} ms -> {direct} ms, target {target}"
            );
        }
    }

    #[test]
    fn standing_queue_after_overload_drains_back_to_target() {
        let (frames, voiced) = talker(1000);
        // From 2 s to 5 s each 10 ms packet needs 20 ms of uplink time and the
        // router drops packets that would wait over 250 ms. Afterwards its queue
        // empties at 0.5 ms per packet, so the backlog arrives as one burst.
        let mut link_free = 0;
        let mut arrivals = Vec::new();
        for sequence in 1..=1000u64 {
            let sent = (sequence - 1) * 10_000;
            let start = sent.max(link_free);
            if start - sent > 250_000 {
                continue;
            }
            link_free = start
                + if (201..=500).contains(&sequence) {
                    20_000
                } else {
                    500
                };
            arrivals.push((sequence, link_free + 20_000));
        }
        for (jitter_ms, adaptive) in [(10, false), (40, false), (10, true)] {
            let played = play(&frames, &arrivals, jitter_ms, adaptive);
            for sequence in &played.omitted {
                assert!(
                    !voiced[*sequence as usize - 1],
                    "speech frame {sequence} skipped"
                );
            }
            for (index, _) in voiced.iter().enumerate().skip(700).filter(|(_, v)| **v) {
                let sequence = index as u64 + 1;
                assert!(played.heard.iter().any(|(heard, _)| *heard == sequence));
            }
            let settled = median_delay(&played, 900..=1000);
            let target = target_after(&played, jitter_ms);
            println!(
                "Overload 2-5 s, {jitter_ms} ms adaptive={adaptive}: send-to-playout settles at {settled} ms (network 20.5 ms), {} quiet frames left out. Virtual clock, excludes devices.",
                played.omitted.len()
            );
            assert!(
                settled <= 20.5 + target + 5.0,
                "{jitter_ms} ms adaptive={adaptive}: settled at {settled} ms, target {target}"
            );
        }
    }

    #[test]
    fn permanent_loss_does_not_inflate_automatic_delay_beyond_arrival_jitter() {
        let (frames, _) = talker(1200);
        let trace = |loss: bool| {
            (1..=1200u64)
                .filter(|sequence| !loss || sequence % 37 != 0)
                .map(|sequence| {
                    (
                        sequence,
                        (sequence - 1) * 10_000
                            + 35_000
                            + [0, 18_000, 3_000, 22_000, 1_000, 10_000][sequence as usize % 6],
                    )
                })
                .collect::<Vec<_>>()
        };
        let jitter = play(&frames, &trace(false), 10, true);
        let lossy = play(&frames, &trace(true), 10, true);
        let jitter_delay = median_delay(&jitter, 900..=1200);
        let loss_delay = median_delay(&lossy, 900..=1200);
        println!("Identical uneven arrivals, adding 1/37 loss: automatic send-to-playout {jitter_delay} -> {loss_delay} ms; target {} -> {} ms", target_after(&jitter, 10), target_after(&lossy, 10));
        assert!(target_after(&lossy, 10) <= target_after(&jitter, 10) + 10.0);
        assert!(
            loss_delay <= jitter_delay + 10.0,
            "unrecoverable loss added wait: {jitter_delay} -> {loss_delay}"
        );
        assert!(
            lossy.stats.late <= jitter.stats.late + 2,
            "late packets regressed"
        );
        assert!(
            lossy.stats.concealed <= jitter.stats.concealed + 34,
            "concealment beyond actual dropped packets"
        );
    }

    #[test]
    #[ignore = "virtual network timing comparison; generated Opus only, no devices or WAN"]
    fn report_real_opus_playout_under_uneven_arrival_and_loss() {
        let (frames, voiced) = talker(1200);
        for (trace, uneven, loss) in [
            ("steady", false, false),
            ("uneven", true, false),
            ("uneven-loss", true, true),
        ] {
            let arrivals: Vec<_> = (1..=1200u64)
                .filter(|sequence| !loss || sequence % 37 != 0)
                .map(|sequence| {
                    let variation = if uneven {
                        [0, 18_000, 3_000, 22_000, 1_000, 10_000][sequence as usize % 6]
                    } else {
                        0
                    };
                    (sequence, (sequence - 1) * 10_000 + 35_000 + variation)
                })
                .collect();
            let sent_voiced = arrivals
                .iter()
                .filter(|(sequence, _)| voiced[*sequence as usize - 1])
                .count();
            for (mode, target, automatic) in [
                ("fixed10", 10, false),
                ("fixed40", 40, false),
                ("automatic", 10, true),
            ] {
                let played = play(&frames, &arrivals, target, automatic);
                let heard_voiced = played
                    .heard
                    .iter()
                    .filter(|(sequence, _)| {
                        voiced[*sequence as usize - 1] && !played.omitted.contains(sequence)
                    })
                    .count();
                let stats = played.speaker.stats;
                let median = median_delay(&played, 900..=1200);
                println!("trace={trace} mode={mode} sent_voiced={sent_voiced} decoded_voiced={heard_voiced} late={} concealed={} held={} median_send_to_play_ms={median:.2} final_target_ms={}",
                    stats.late, stats.concealed, stats.held, played.speaker.buffer_target_ms(target));
                assert!(played.heard.len() <= arrivals.len());
                if !uneven {
                    assert_eq!(
                        heard_voiced, sent_voiced,
                        "steady delivery must preserve speech"
                    );
                }
            }
        }
    }
    #[test]
    fn every_profile_renders_all_samples_and_reports_concealment() {
        use super::network_profiles::{simulate, PROFILES};
        for profile in &PROFILES {
            let report = simulate(profile, 1000);
            println!("{}: {report:?}", profile.name);
            assert!(
                report.rendered_samples >= 1000 * 480,
                "{}: lost rendered audio {report:?}",
                profile.name
            );
            assert_eq!(report.silent_gaps, 0, "{}: {report:?}", profile.name);
            if profile.loss_pct > 0.0 {
                assert!(report.lost > 0, "{}: no network loss drawn", profile.name);
                assert!(report.concealed > 0, "{}: {report:?}", profile.name);
            } else {
                assert_eq!(report.concealed, 0, "{}: {report:?}", profile.name);
            }
        }
        let lan = simulate(&PROFILES[0], 1000);
        assert_eq!(PROFILES[0].name, "lan_wired");
        assert_eq!(lan.first_playout_ms, Some(15));
        assert_eq!((lan.late, lan.held), (0, 0));
        assert_eq!(lan.rendered_samples, 480_000);
        // A fixed seed: the same profile always produces the same report.
        assert_eq!(simulate(&PROFILES[2], 1000), simulate(&PROFILES[2], 1000));
    }

    #[test]
    fn automatic_loss_recovery_rebuilds_lost_speech_without_touching_a_clean_lan() {
        use super::network_profiles::{simulate_with, PROFILES};
        use crate::audio_settings::LossRecovery;
        for profile in &PROFILES {
            let off = simulate_with(profile, 1000, LossRecovery::Off);
            let automatic = simulate_with(profile, 1000, LossRecovery::Automatic);
            println!(
                "{}: off {off:?}\n{}: automatic {automatic:?}",
                profile.name, profile.name
            );
            assert!(
                automatic.rendered_samples >= 480_000,
                "{}: {automatic:?}",
                profile.name
            );
            assert_eq!(automatic.silent_gaps, 0, "{}: {automatic:?}", profile.name);
            assert!(automatic.concealed <= off.concealed, "{}", profile.name);
            if profile.name == "lan_wired" {
                assert_eq!(automatic, off, "a clean network never switches recovery on");
                assert_eq!(automatic.first_playout_ms, Some(15));
                assert_eq!(automatic.recovery_on_ms, None);
            }
            if profile.name == "wifi_busy" {
                // Measured 74 -> 66 with 8 rebuilt. Opus redundancy covers one
                // frame, so the first frame of each two-packet burst and the
                // packets that arrive late stay concealed: see
                // docs/network-profiles-20261028.md. It switches on once a
                // second of packets (100) shows the loss.
                assert!(automatic.recovery_on_ms.is_some_and(|at| at <= 1100));
                assert!(automatic.recovered > 0, "{automatic:?}");
                assert!(automatic.concealed < off.concealed, "{automatic:?}");
                assert!(
                    automatic.final_delay_ms <= off.final_delay_ms,
                    "recovery added buffering"
                );
            }
        }
    }

    #[test]
    fn steady_path_never_skips_audio() {
        let (frames, _) = talker(600);
        let arrivals: Vec<_> = (1..=600u64)
            .map(|sequence| (sequence, (sequence - 1) * 10_000 + 20_000))
            .collect();
        for (jitter_ms, adaptive) in [(10, false), (40, false), (10, true)] {
            let played = play(&frames, &arrivals, jitter_ms, adaptive);
            assert!(played.omitted.is_empty(), "{jitter_ms} {adaptive}");
            assert_eq!(played.heard.len(), 600);
            assert_eq!((played.stats.late, played.stats.concealed), (0, 0));
        }
    }
}
