//! Original embedded PCM cues and generated call rings, without disk I/O.
use crate::audio_settings::AudioCue;
use relay_protocol::Bootstrap;
use std::collections::HashSet;

fn wav(cue: AudioCue) -> &'static [u8] {
    match cue {
        AudioCue::Ring | AudioCue::Ringback => unreachable!("ring cues are generated PCM"),
        AudioCue::Join => include_bytes!("../assets/sounds/join.wav"),
        AudioCue::Leave => include_bytes!("../assets/sounds/leave.wav"),
        AudioCue::Mute => include_bytes!("../assets/sounds/mute.wav"),
        AudioCue::Unmute => include_bytes!("../assets/sounds/unmute.wav"),
        AudioCue::Deafen => include_bytes!("../assets/sounds/deafen.wav"),
        AudioCue::Undeafen => include_bytes!("../assets/sounds/undeafen.wav"),
        AudioCue::CallEnd => include_bytes!("../assets/sounds/call-end.wav"),
        AudioCue::Message => include_bytes!("../assets/sounds/message.wav"),
    }
}

pub(crate) fn samples(cue: AudioCue, volume: f32) -> Vec<f32> {
    // These immutable embedded assets are verified as a canonical 44-byte WAV
    // header followed by 48kHz mono signed 16-bit PCM before they are shipped.
    let gain = if volume.is_finite() {
        volume.clamp(0.0, 1.0)
    } else {
        0.0
    };
    if matches!(cue, AudioCue::Ring | AudioCue::Ringback) {
        // Two short, smoothly enveloped notes. Ringback is deliberately quieter
        // and lower; neither cadence adds latency to the voice packet path.
        let (frequencies, amplitude) = if cue == AudioCue::Ring {
            ([660.0, 880.0], 0.14)
        } else {
            ([440.0, 520.0], 0.045)
        };
        return (0..19200)
            .map(|i| {
                if i == 19199 {
                    return 0.0;
                }
                let t = i as f32 / 48000.0;
                let local = t % 0.22;
                let envelope = if local < 0.16 {
                    (local / 0.008).min(1.0) * ((0.16 - local) / 0.025).min(1.0)
                } else {
                    0.0
                };
                // Finish the second note before the final sample.
                let envelope = envelope * ((0.4 - t) / 0.025).clamp(0.0, 1.0);
                let phase = std::f32::consts::TAU * t;
                (phase * frequencies[0])
                    .sin()
                    .mul_add(0.65, (phase * frequencies[1]).sin() * 0.35)
                    * amplitude
                    * envelope
                    * gain
            })
            .collect();
    }
    wav(cue)[44..]
        .chunks_exact(2)
        .map(|pair| i16::from_le_bytes([pair[0], pair[1]]) as f32 / 32768.0 * gain)
        .collect()
}

/// One cue for peer membership changes in an already joined room. Ignore the
/// first snapshot, self joins/leaves, other rooms, metadata changes and reconnects.
pub(crate) fn room_change(
    previous: &Bootstrap,
    next: &Bootstrap,
    channel: &str,
) -> Option<AudioCue> {
    if previous.server_id.is_empty()
        || previous.server_id != next.server_id
        || previous.instance_id != next.instance_id
        || previous.user.id != next.user.id
    {
        return None;
    }
    let own = |b: &Bootstrap| {
        b.voice_members
            .iter()
            .find(|m| m.channel_id == channel && m.user.id == b.user.id)
            .map(|m| m.session_id)
    };
    let old_session = own(previous)?;
    if old_session == 0 || own(next)? != old_session {
        return None;
    }
    let members = |b: &Bootstrap| {
        b.voice_members
            .iter()
            .filter(|m| m.channel_id == channel && m.user.id != b.user.id)
            .map(|m| m.user.id.clone())
            .collect::<HashSet<_>>()
    };
    let before = members(previous);
    let after = members(next);
    if before.iter().any(|id| !after.contains(id)) {
        Some(AudioCue::Leave)
    } else if after.iter().any(|id| !before.contains(id)) {
        Some(AudioCue::Join)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn all_supplied_cues_keep_original_pcm_format_and_duration() {
        for cue in [
            AudioCue::Join,
            AudioCue::Leave,
            AudioCue::Mute,
            AudioCue::Unmute,
            AudioCue::Deafen,
            AudioCue::Undeafen,
            AudioCue::CallEnd,
            AudioCue::Message,
        ] {
            let bytes = wav(cue);
            assert_eq!(bytes.len(), 16364);
            assert_eq!(&bytes[..4], b"RIFF");
            assert_eq!(&bytes[8..16], b"WAVEfmt ");
            assert_eq!(&bytes[20..24], &[1, 0, 1, 0]); // PCM, mono
            assert_eq!(u32::from_le_bytes(bytes[24..28].try_into().unwrap()), 48000);
            assert_eq!(&bytes[32..36], &[2, 0, 16, 0]); // two-byte block, 16 bits
            assert_eq!(&bytes[36..40], b"data");
            assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()), 16320);
            let audio = samples(cue, 1.0);
            assert_eq!(audio.len(), 8160);
            assert!(audio.iter().all(|v| v.is_finite() && v.abs() <= 1.0));
            assert!(audio.iter().any(|v| v.abs() > 0.01));
            assert_eq!(audio[0], 0.0);
            assert_eq!(*audio.last().unwrap(), 0.0);
        }
        assert_eq!(wav(AudioCue::Join), wav(AudioCue::Message));
        assert_ne!(wav(AudioCue::Leave), wav(AudioCue::CallEnd));
        assert_ne!(wav(AudioCue::Mute), wav(AudioCue::Unmute));
        assert_ne!(wav(AudioCue::Deafen), wav(AudioCue::Undeafen));
    }
    #[test]
    fn volume_preserves_waveform_and_mutes_invalid_values() {
        let full = samples(AudioCue::Message, 1.0);
        let quarter = samples(AudioCue::Message, 0.25);
        assert!(full.iter().zip(quarter).all(|(a, b)| *a * 0.25 == b));
        assert_eq!(full, samples(AudioCue::Message, 9.0));
        for value in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            assert!(samples(AudioCue::Message, value).iter().all(|v| *v == 0.0));
        }
    }
    fn bootstrap(ids: &[(&str, &str)]) -> Bootstrap {
        Bootstrap {
            server_id: "owned-community".into(),
            instance_id: "owned-runtime".into(),
            user: relay_protocol::User {
                id: "self".into(),
                ..Default::default()
            },
            voice_members: ids
                .iter()
                .map(|(id, room)| relay_protocol::VoiceMember {
                    user: relay_protocol::User {
                        id: (*id).into(),
                        ..Default::default()
                    },
                    channel_id: (*room).into(),
                    session_id: 1,
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }
    }
    #[test]
    fn peer_cues_ignore_initial_sync_self_metadata_other_rooms_and_reconnects() {
        let alone = bootstrap(&[("self", "room")]);
        let joined = bootstrap(&[("self", "room"), ("peer", "room")]);
        assert_eq!(room_change(&alone, &joined, "room"), Some(AudioCue::Join));
        assert_eq!(room_change(&joined, &alone, "room"), Some(AudioCue::Leave));
        assert_eq!(room_change(&joined, &joined, "room"), None);
        assert_eq!(
            room_change(&bootstrap(&[("peer", "room")]), &joined, "room"),
            None
        );
        assert_eq!(
            room_change(&joined, &bootstrap(&[("peer", "room")]), "room"),
            None
        );
        assert_eq!(
            room_change(
                &alone,
                &bootstrap(&[("self", "room"), ("peer", "other")]),
                "room"
            ),
            None
        );
        let mut changed = joined.clone();
        changed.voice_members[1].muted = true;
        assert_eq!(room_change(&joined, &changed, "room"), None);
        changed.instance_id.push_str("-restart");
        assert_eq!(room_change(&joined, &changed, "room"), None);
        let mut changed = joined.clone();
        changed.voice_members[0].session_id = 2;
        assert_eq!(room_change(&joined, &changed, "room"), None);
        let mut changed = joined.clone();
        changed.server_id.push_str("-other");
        assert_eq!(room_change(&joined, &changed, "room"), None);
    }
}
