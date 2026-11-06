//! Packet redundancy for bursty links: a voice packet can carry exact copies
//! of the one or two Opus frames sent before it (RFC 2198 in spirit), so a
//! receiver rebuilds a lost frame from a later packet that is already queued.
//! What ships is one copy of the previous frame: see `PLAN`.
//!
//! The copies travel inside the Opus packet's own padding (RFC 6716 3.2.5).
//! Every Opus decoder ignores padding, so a packet with copies plays exactly
//! like the plain packet on an older Relay, and the relay server only ever
//! sees the same sealed bytes it already routes. The padding is laid out as
//! one Opus extension (the format libopus 1.5 reads for DRED) with a number
//! no Opus release uses, so a newer libopus also skips it.
//!
//! Capability: a client that can decode copies says so by sending the same
//! extension with no copies on the first packet of each talkspurt and on
//! every 50th packet. A sender adds copies only while every other person in
//! the call has said so; until then everyone gets today's packets.
use std::collections::{HashSet, VecDeque};

/// Opus extension number (long form, 32-127) used for Relay's copies.
pub(crate) const EXTENSION_ID: u8 = 120;
const MAGIC: u8 = b'R';
const VERSION: u8 = 1;
/// Extension header, magic and version.
const HEADER: [u8; 3] = [EXTENSION_ID << 1, MAGIC, VERSION];
/// Copies of frames further back than this are not accepted.
pub(crate) const MAX_DISTANCE: u8 = 2;
/// A capability note goes out on every this many packets.
pub(crate) const ANNOUNCE_EVERY: u64 = 50;
/// Largest Opus frame (RFC 6716 3.4 R2).
const MAX_FRAME: usize = 1275;

/// Which earlier frames each packet carries while redundancy is on, and how
/// the bitrate is shared between the new frame and its copies.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Plan {
    /// Distances back, in packets, of the copied frames.
    pub distances: &'static [u8],
    /// Whether Opus in-band FEC stays on while copies are sent.
    pub keep_fec: bool,
    /// Total voice payload while redundancy is on, as a share of the user's
    /// bitrate. 1.0 keeps the total within the setting; 2.0 allows double.
    pub budget: f32,
}

/// What ships: see docs/audio-loss-redundancy-20261030.md for the figures.
///
/// One copy of the previous frame. A copy from two packets back only helps
/// when the receive buffer holds 20 ms plus the jitter, which the adaptive
/// buffer does not on the busy Wi-Fi profile, so it only cost bitrate.
/// Opus FEC is switched off while copies go out: the copy covers the same
/// frame for every packet (FEC only for speech) at the full frame's quality,
/// and the encoder returns to its low-delay mode (2.5 ms instead of 6.5 ms).
/// The frame and its copy share 1.5 times the user's bitrate. Today's
/// encoder with FEC sends 0.74-0.89 times the setting (least with 20 ms
/// packets), so this keeps the payload within twice what the same link
/// carried before copies, at every packet size and bitrate measured.
pub(crate) const PLAN: Plan = Plan {
    distances: &[1],
    keep_fec: false,
    budget: 1.5,
};

impl Plan {
    /// Payload overhead per packet of the framing: frame count and padding
    /// length (2), extension header (3) and 3 bytes per copy.
    fn overhead_bytes(self) -> u32 {
        5 + 3 * self.distances.len() as u32
    }

    /// The encoder bitrate for the new frame while copies are sent, so that
    /// the frame, its copies and the framing together stay within the budget.
    /// Never above the user's bitrate and never below 16 kbit/s.
    pub(crate) fn primary_kbps(self, bitrate_kbps: u32, packet_samples: usize) -> u32 {
        let packets_per_second = (48_000 / packet_samples.max(1)) as f32;
        let overhead_kbps = self.overhead_bytes() as f32 * 8.0 * packets_per_second / 1000.0;
        let total = bitrate_kbps as f32 * self.budget - overhead_kbps;
        let share = total / (1 + self.distances.len()) as f32;
        (share.floor() as u32).clamp(16, bitrate_kbps.max(16))
    }
}

/// Wraps a plain (code 0) Opus packet with copies of earlier frames, or with
/// no copies as a capability note. Returns false, leaving `out` empty, for
/// any packet that is not a single-frame code 0 packet.
pub(crate) fn pack(primary: &[u8], copies: &[(u8, &[u8])], out: &mut Vec<u8>) -> bool {
    out.clear();
    let Some((&toc, frame)) = primary.split_first() else {
        return false;
    };
    if toc & 3 != 0 || frame.len() > MAX_FRAME {
        return false;
    }
    let padding = HEADER.len() + copies.iter().map(|(_, copy)| 3 + copy.len()).sum::<usize>();
    out.reserve(4 + padding / 254 + frame.len() + padding);
    out.push(toc | 3);
    // CBR (one frame, its length implied), padding present, one frame.
    out.push(0x40 | 1);
    let mut remaining = padding;
    while remaining >= 255 {
        out.push(255);
        remaining -= 254;
    }
    out.push(remaining as u8);
    out.extend_from_slice(frame);
    out.extend_from_slice(&HEADER);
    for &(distance, copy) in copies {
        out.push(distance);
        out.extend_from_slice(&(copy.len() as u16).to_le_bytes());
        out.extend_from_slice(copy);
    }
    true
}

/// A received packet split into its plain Opus packet and the copies.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Unpacked {
    /// The new frame as a plain code 0 Opus packet.
    pub primary: Vec<u8>,
    /// Copies of earlier frames: distance back in packets, plain Opus packet.
    pub copies: Vec<(u8, Vec<u8>)>,
}

/// Reads Relay's extension from a packet's padding. `None` for every packet
/// without it, which then plays exactly as received.
pub(crate) fn unpack(packet: &[u8]) -> Option<Unpacked> {
    let (&toc, rest) = packet.split_first()?;
    let (&count, mut rest) = rest.split_first()?;
    // Code 3, CBR, padding flag, exactly one frame.
    if toc & 3 != 3 || count != 0x41 {
        return None;
    }
    let mut padding = 0usize;
    loop {
        let (&length, next) = rest.split_first()?;
        rest = next;
        if length == 255 {
            padding += 254;
        } else {
            padding += usize::from(length);
            break;
        }
    }
    let frame_len = rest.len().checked_sub(padding)?;
    let (frame, extension) = rest.split_at(frame_len);
    let mut entries = extension.strip_prefix(&HEADER)?;
    let mut primary = Vec::with_capacity(1 + frame.len());
    primary.push(toc & !3);
    primary.extend_from_slice(frame);
    let mut copies = Vec::new();
    // A malformed entry ends the list; the new frame still plays.
    while let [distance, low, high, tail @ ..] = entries {
        let length = usize::from(u16::from_le_bytes([*low, *high]));
        if !(1..=MAX_DISTANCE).contains(distance)
            || length == 0
            || length > tail.len()
            || copies.iter().any(|(d, _)| d == distance)
        {
            break;
        }
        let (copy, next) = tail.split_at(length);
        if copy[0] & 3 == 0 && copy.len() <= MAX_FRAME + 1 {
            copies.push((*distance, copy.to_vec()));
        }
        entries = next;
    }
    Some(Unpacked { primary, copies })
}

/// The sender's last frames, so a packet can copy them.
#[derive(Default)]
pub(crate) struct History {
    /// Source sequence, source timestamp, samples and plain Opus bytes.
    frames: VecDeque<(u64, u64, usize, Vec<u8>)>,
    packed: Vec<u8>,
}

impl History {
    /// The encoder history was discarded: nothing earlier may be copied.
    pub(crate) fn clear(&mut self) {
        self.frames.clear();
    }

    /// Frames this packet (the next after the ones recorded) should carry:
    /// only frames that run on without a gap from the same source clock.
    fn copies(
        frames: &VecDeque<(u64, u64, usize, Vec<u8>)>,
        sequence: u64,
        timestamp: u64,
        samples: usize,
        plan: Plan,
    ) -> Vec<(u8, &[u8])> {
        plan.distances
            .iter()
            .filter_map(|&distance| {
                let back = u64::from(distance);
                frames
                    .iter()
                    .find(|(seq, ts, n, _)| {
                        sequence.checked_sub(back) == Some(*seq)
                            && *n == samples
                            && timestamp.checked_sub(back * samples as u64) == Some(*ts)
                    })
                    .map(|(.., bytes)| (distance, bytes.as_slice()))
            })
            .collect()
    }

    /// Records the new frame and returns the bytes to send: the plain packet,
    /// the packet with copies by `copies` while it is set, or a capability
    /// note on a talkspurt start and every `ANNOUNCE_EVERY` packets. A packed
    /// packet longer than `max_len` goes out plain.
    pub(crate) fn frame<'a>(
        &'a mut self,
        primary: &'a [u8],
        sequence: u64,
        timestamp: u64,
        restart: bool,
        copies: Option<Plan>,
        max_len: usize,
    ) -> &'a [u8] {
        let Ok(samples) = opus::packet::get_nb_samples(primary, relay_protocol::AUDIO_SAMPLE_RATE)
        else {
            self.clear();
            return primary;
        };
        if restart {
            self.clear();
        }
        let redundant = copies.is_some();
        let copies = copies.map_or_else(Vec::new, |plan| {
            Self::copies(&self.frames, sequence, timestamp, samples, plan)
        });
        let announce = redundant || restart || sequence.is_multiple_of(ANNOUNCE_EVERY);
        let packed =
            announce && pack(primary, &copies, &mut self.packed) && self.packed.len() <= max_len;
        while self.frames.len() >= usize::from(MAX_DISTANCE) {
            self.frames.pop_front();
        }
        self.frames
            .push_back((sequence, timestamp, samples, primary.to_vec()));
        if packed {
            &self.packed
        } else {
            primary
        }
    }
}

/// People in the call who said they decode copies.
#[derive(Default)]
pub(crate) struct Peers {
    decoding: HashSet<String>,
}

impl Peers {
    /// Called for every authenticated voice packet from `user`.
    pub(crate) fn heard(&mut self, user: &str, opus: &[u8]) {
        if !self.decoding.contains(user) && unpack(opus).is_some() {
            self.decoding.insert(user.to_owned());
        }
    }

    /// Whether copies may be sent: at least one other person is in the call
    /// and every other person has said they decode them. Anyone who has not
    /// spoken yet counts as an older Relay.
    pub(crate) fn all_decode<'a>(
        &self,
        members: impl IntoIterator<Item = &'a String>,
        local: &str,
    ) -> bool {
        let mut others = members.into_iter().filter(|user| *user != local).peekable();
        others.peek().is_some() && others.all(|user| self.decoding.contains(user))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opus::{Channels, Decoder};

    fn speech_packets(count: usize, kbps: u32) -> Vec<Vec<u8>> {
        let mut encoder =
            crate::audio_settings::make_voice_encoder_with_recovery(kbps, false).unwrap();
        let speech =
            crate::voice_test_signals::talker(count as f32 / 100.0 + 0.1, (105.0, 135.0), 13, 0.3);
        (0..count)
            .map(|index| {
                encoder
                    .encode_vec_float(&speech[index * 480..(index + 1) * 480], 1500)
                    .unwrap()
            })
            .collect()
    }

    #[test]
    fn copies_round_trip_through_opus_padding() {
        let packets = speech_packets(4, 64);
        let mut packed = Vec::new();
        assert!(pack(
            &packets[3],
            &[(1, &packets[2]), (2, &packets[1])],
            &mut packed
        ));
        assert_eq!(packed[0] & 3, 3);
        let unpacked = unpack(&packed).unwrap();
        assert_eq!(unpacked.primary, packets[3]);
        assert_eq!(
            unpacked.copies,
            vec![(1, packets[2].clone()), (2, packets[1].clone())]
        );
        // A capability note: no copies, five bytes more than the plain packet.
        assert!(pack(&packets[0], &[], &mut packed));
        assert_eq!(packed.len(), packets[0].len() + 5);
        assert_eq!(
            unpack(&packed).unwrap(),
            Unpacked {
                primary: packets[0].clone(),
                copies: vec![]
            }
        );
        // Long padding uses the 255 continuation of the padding length.
        let mut big = vec![packets[1][0]];
        big.extend(std::iter::repeat_n(7u8, 600));
        assert!(pack(&packets[2], &[(1, &big)], &mut packed));
        assert_eq!(unpack(&packed).unwrap().copies[0].1, big);
        // Plain packets and foreign padding are not Relay's.
        assert_eq!(unpack(&packets[2]), None);
        let mut foreign = packed.clone();
        let header_at = foreign.len() - big.len() - 3 - HEADER.len();
        foreign[header_at] = 126 << 1;
        assert_eq!(unpack(&foreign), None);
        assert_eq!(unpack(&[]), None);
        assert_eq!(unpack(&[3]), None);
        assert_eq!(unpack(&[3, 0x41, 255]), None);
    }

    #[test]
    fn malformed_entries_keep_the_new_frame() {
        let packets = speech_packets(3, 32);
        let mut packed = Vec::new();
        assert!(pack(&packets[2], &[(1, &packets[1])], &mut packed));
        let length_at = packed.len() - packets[1].len() - 2;
        packed[length_at] = 0xFF;
        packed[length_at + 1] = 0xFF;
        let unpacked = unpack(&packed).unwrap();
        assert_eq!(unpacked.primary, packets[2]);
        assert!(unpacked.copies.is_empty());
        for distance in [0, 3, 200] {
            assert!(pack(&packets[2], &[(distance, &packets[1])], &mut packed));
            assert!(unpack(&packed).unwrap().copies.is_empty(), "{distance}");
        }
        // Only plain packets can be wrapped.
        assert!(!pack(&packed.clone(), &[], &mut packed));
        assert!(packed.is_empty());
        assert!(!pack(&[], &[], &mut packed));
    }

    /// The old-peer case: an older Relay hands the received bytes straight to
    /// libopus. With copies in the padding it must hear exactly what it
    /// hears from today's packets, sample for sample, including Opus
    /// recovery from the next packet.
    #[test]
    fn an_older_relay_decodes_padded_packets_exactly_like_plain_ones() {
        let mut encoder =
            crate::audio_settings::make_voice_encoder_with_recovery(48, true).unwrap();
        let speech = crate::voice_test_signals::talker(1.2, (105.0, 135.0), 13, 0.3);
        let plain: Vec<Vec<u8>> = (0..100)
            .map(|index| {
                encoder
                    .encode_vec_float(&speech[index * 480..(index + 1) * 480], 1500)
                    .unwrap()
            })
            .collect();
        let mut history = History::default();
        let sent: Vec<Vec<u8>> = plain
            .iter()
            .enumerate()
            .map(|(index, packet)| {
                let sequence = index as u64 + 1;
                history
                    .frame(
                        packet,
                        sequence,
                        index as u64 * 480,
                        index == 0,
                        (index >= 10).then_some(PLAN),
                        1400,
                    )
                    .to_vec()
            })
            .collect();
        assert_eq!(
            unpack(&sent[20]).map(|unpacked| unpacked.copies.len()),
            Some(PLAN.distances.len()),
            "copies were not sent"
        );
        let (mut old_plain, mut old_padded) = (
            Decoder::new(48_000, Channels::Mono).unwrap(),
            Decoder::new(48_000, Channels::Mono).unwrap(),
        );
        for index in 0..100 {
            let (mut a, mut b) = ([0.0f32; 480], [0.0f32; 480]);
            // Packet 50 is lost: both decode it from the next packet's FEC.
            let (fec, from) = if index == 50 {
                (true, 51)
            } else {
                (false, index)
            };
            assert_eq!(
                old_plain.decode_float(&plain[from], &mut a, fec).unwrap(),
                480
            );
            assert_eq!(
                old_padded.decode_float(&sent[from], &mut b, fec).unwrap(),
                480
            );
            assert_eq!(a, b, "frame {index}");
            assert_eq!(
                opus::packet::get_nb_samples(&sent[index], 48_000).unwrap(),
                480,
                "an older Relay's duration check"
            );
        }
        // Capability notes alone stay rare while copies are off.
        let mut history = History::default();
        let notes = plain
            .iter()
            .enumerate()
            .filter(|(index, packet)| {
                let out = history.frame(
                    packet,
                    *index as u64 + 1,
                    *index as u64 * 480,
                    *index == 0,
                    None,
                    1400,
                );
                out.len() != packet.len()
            })
            .count();
        assert_eq!(notes, 3, "the talkspurt start and packets 50 and 100");
    }

    #[test]
    fn history_copies_only_frames_that_run_on_from_the_same_clock() {
        let packets = speech_packets(6, 48);
        let mut history = History::default();
        let two = Plan {
            distances: &[1, 2],
            keep_fec: false,
            budget: 2.0,
        };
        let sent =
            |history: &mut History, index: usize, sequence: u64, timestamp: u64, restart: bool| {
                unpack(history.frame(
                    &packets[index],
                    sequence,
                    timestamp,
                    restart,
                    Some(two),
                    1400,
                ))
                .map(|u| u.copies.iter().map(|(d, _)| *d).collect::<Vec<_>>())
            };
        assert_eq!(sent(&mut history, 0, 1, 0, true), Some(vec![]));
        assert_eq!(sent(&mut history, 1, 2, 480, false), Some(vec![1]));
        assert_eq!(sent(&mut history, 2, 3, 960, false), Some(vec![1, 2]));
        // A clock jump (gated silence) breaks the run.
        assert_eq!(sent(&mut history, 3, 4, 5_000, false), Some(vec![]));
        // A restart forgets everything before it, even on a running clock
        // (the encoder history was discarded, or who may hear it changed).
        assert_eq!(sent(&mut history, 4, 5, 5_480, true), Some(vec![]));
        assert_eq!(sent(&mut history, 5, 6, 5_960, false), Some(vec![1]));
        // Too large for the packet: sent plain.
        let mut history = History::default();
        let out = history
            .frame(&packets[0], 1, 0, true, Some(PLAN), 4)
            .to_vec();
        assert_eq!(out, packets[0]);
    }

    #[test]
    fn copies_wait_for_every_other_person_to_say_they_decode_them() {
        let packets = speech_packets(1, 32);
        let mut note = Vec::new();
        assert!(pack(&packets[0], &[], &mut note));
        let members: Vec<String> = ["me", "ana", "bo"].map(String::from).to_vec();
        let mut peers = Peers::default();
        assert!(!peers.all_decode(&members, "me"));
        peers.heard("ana", &note);
        peers.heard("bo", &packets[0]);
        assert!(!peers.all_decode(&members, "me"), "bo runs an older Relay");
        peers.heard("bo", &note);
        assert!(peers.all_decode(&members, "me"));
        assert!(!peers.all_decode(&members[..1], "me"), "alone in the call");
    }

    #[test]
    fn the_bitrate_share_keeps_the_total_within_the_budget() {
        let within = Plan {
            distances: &[2],
            keep_fec: true,
            budget: 1.0,
        };
        // 10 ms packets: 8 bytes of framing per packet is 6.4 kbit/s.
        assert_eq!(within.primary_kbps(96, 480), 44);
        assert_eq!(within.primary_kbps(32, 960), 16);
        let double = Plan {
            budget: 2.0,
            ..within
        };
        assert_eq!(double.primary_kbps(64, 480), 60);
        assert_eq!(double.primary_kbps(64, 960), 62);
        assert_eq!(double.primary_kbps(128, 480), 124);
    }
}
