//! Per-event choices and a bounded, profile-local WAV library. File operations
//! run at startup or on an import worker; playback only reads cached samples.
use crate::audio_settings::{AudioCue, AudioSettings};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

pub(crate) const MAX_FILE_BYTES: usize = 10 * 1024 * 1024;
pub(crate) const MAX_SECONDS: usize = 5;
pub(crate) const SAMPLE_RATE: usize = 48_000;
pub(crate) const MAX_SAMPLES: usize = MAX_SECONDS * SAMPLE_RATE;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SoundSource {
    #[default]
    Default,
    BuiltIn {
        cue: AudioCue,
    },
    Custom {
        id: String,
        name: String,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EventSound {
    pub enabled: bool,
    pub volume: f32,
    pub source: SoundSource,
}

impl Default for EventSound {
    fn default() -> Self {
        Self {
            enabled: true,
            volume: 1.0,
            source: SoundSource::Default,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct EventSounds {
    cues: BTreeMap<AudioCue, EventSound>,
    #[serde(skip)]
    cache: BTreeMap<String, Arc<[f32]>>,
}

impl PartialEq for EventSounds {
    fn eq(&self, other: &Self) -> bool {
        // Opening controls may materialize defaults. Cached runtime data and
        // explicit default rows must not create unsaved preference changes.
        AudioCue::ALL
            .into_iter()
            .all(|cue| self.get(cue) == other.get(cue))
    }
}

impl EventSounds {
    pub fn get(&self, cue: AudioCue) -> EventSound {
        self.cues.get(&cue).cloned().unwrap_or_default()
    }

    pub fn get_mut(&mut self, cue: AudioCue) -> &mut EventSound {
        self.cues.entry(cue).or_default()
    }

    pub fn normalize(&mut self) {
        for sound in self.cues.values_mut() {
            sound.volume = if sound.volume.is_finite() {
                sound.volume.clamp(0.0, 1.0)
            } else {
                1.0
            };
        }
        self.prune_cache();
    }

    fn prune_cache(&mut self) {
        self.cache.retain(|id, _| {
            self.cues.values().any(|sound| {
            matches!(&sound.source, SoundSource::Custom {id: selected, ..} if selected == id)
        })
        });
    }

    pub fn install(&mut self, imported: ImportedSound) -> SoundSource {
        self.prune_cache();
        self.cache.insert(imported.id.clone(), imported.samples);
        SoundSource::Custom {
            id: imported.id,
            name: imported.name,
        }
    }

    pub fn is_loaded(&self, source: &SoundSource) -> bool {
        match source {
            SoundSource::Custom { id, .. } => self.cache.contains_key(id),
            _ => true,
        }
    }

    /// Accept a worker's cache without replacing newer editor choices. Only
    /// currently selected IDs are retained, so repeated imports stay bounded.
    pub fn merge_loaded(&mut self, loaded: &Self) {
        self.prune_cache();
        for sound in self.cues.values() {
            if let SoundSource::Custom { id, .. } = &sound.source {
                if let Some(samples) = loaded.cache.get(id) {
                    self.cache.insert(id.clone(), samples.clone());
                }
            }
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ImportedSound {
    pub id: String,
    pub name: String,
    samples: Arc<[f32]>,
}

pub(crate) fn library_root(preferences_path: &Path) -> PathBuf {
    preferences_path.with_file_name("sounds")
}

fn valid_id(id: &str) -> bool {
    id.len() == 64
        && id
            .bytes()
            .all(|value| value.is_ascii_digit() || (b'a'..=b'f').contains(&value))
}

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn local_unredirected(path: &Path) -> Result<(), String> {
    if !path.is_absolute() {
        return Err("Choose a WAV file or sound library on this PC using its full path.".into());
    }
    #[cfg(windows)]
    {
        use std::path::{Component, Prefix};
        if !matches!(path.components().next(), Some(Component::Prefix(prefix))
            if matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_)))
        {
            return Err("Sound files must be stored on this PC, not a network share.".into());
        }
    }
    // Inspect ancestors from the root before touching a descendant, so a
    // junction cannot redirect an apparently local library/file elsewhere.
    let ancestors: Vec<_> = path.ancestors().collect();
    for ancestor in ancestors.into_iter().rev() {
        let metadata = match std::fs::symlink_metadata(ancestor) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return Err("Cannot access the sound file or library folder.".into()),
        };
        let redirected = metadata.file_type().is_symlink();
        #[cfg(windows)]
        let redirected = {
            use std::os::windows::fs::MetadataExt;
            redirected || metadata.file_attributes() & 0x400 != 0
        };
        if redirected {
            return Err(
                "Sound files and their folders must not be links or redirected folders.".into(),
            );
        }
    }
    Ok(())
}

fn read_wav(path: &Path) -> Result<Vec<u8>, String> {
    local_unredirected(path)?;
    let metadata = std::fs::symlink_metadata(path).map_err(|_| "Cannot read this WAV file.")?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err("Choose an ordinary WAV file stored on this PC.".into());
    }
    if metadata.len() > MAX_FILE_BYTES as u64 {
        return Err("Choose a WAV file no larger than 10 MiB and no longer than 5 seconds.".into());
    }
    let file = std::fs::File::open(path).map_err(|_| "Cannot open this WAV file.")?;
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "Cannot read this WAV file.")?;
    if bytes.len() > MAX_FILE_BYTES {
        return Err("The WAV file exceeds 10 MiB.".into());
    }
    Ok(bytes)
}

/// Call off the UI/audio threads. The selection retains only a content hash and
/// display name; moving/deleting the original file does not break the sound.
pub(crate) fn import_wav(root: &Path, source: &Path) -> Result<ImportedSound, String> {
    let bytes = read_wav(source)?;
    let samples = decode_wav(&bytes)?;
    let id = hash(&bytes);
    local_unredirected(root)?;
    std::fs::create_dir_all(root).map_err(|_| "Cannot create your sound library.")?;
    if std::fs::symlink_metadata(root)
        .map_err(|_| "Cannot read your sound library.")?
        .file_type()
        .is_symlink()
    {
        return Err("The sound library must be an ordinary local folder.".into());
    }
    let destination = root.join(format!("{id}.wav"));
    if std::fs::symlink_metadata(&destination).is_ok() {
        if hash(&read_wav(&destination)?) != id {
            return Err(
                "This saved sound is damaged. Choose another sound or restore the library.".into(),
            );
        }
    } else {
        let temporary = root.join(format!("{}.sound-pending", uuid::Uuid::new_v4()));
        let saved = (|| {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
                .map_err(|_| "Cannot create the saved sound.")?;
            file.write_all(&bytes)
                .and_then(|_| file.sync_all())
                .map_err(|_| "Cannot save this sound.")?;
            drop(file);
            std::fs::rename(&temporary, &destination)
                .map_err(|_| "Cannot finish saving this sound.")
        })();
        if saved.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        saved?;
    }
    let name = source
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("Custom sound")
        .chars()
        .filter(|ch| !ch.is_control())
        .take(120)
        .collect();
    Ok(ImportedSound {
        id,
        name,
        samples: samples.into(),
    })
}

/// Call once during startup, before applying settings, or on a background
/// worker. Missing/damaged custom files leave the selected identity intact and
/// use the default event sound until the user repairs or replaces them.
pub(crate) fn hydrate(sounds: &mut EventSounds, root: &Path) -> Vec<String> {
    sounds.cache.clear();
    let mut errors = Vec::new();
    if let Err(error) = local_unredirected(root) {
        errors.push(error);
        return errors;
    }
    for cue in AudioCue::ALL {
        let SoundSource::Custom { id, name } = sounds.get(cue).source else {
            continue;
        };
        if sounds.cache.contains_key(&id) {
            continue;
        }
        let loaded = if valid_id(&id) {
            read_wav(&root.join(format!("{id}.wav"))).and_then(|bytes| {
                if hash(&bytes) != id {
                    return Err("The saved WAV file is damaged.".into());
                }
                decode_wav(&bytes)
            })
        } else {
            Err("The saved sound identifier is invalid.".into())
        };
        match loaded {
            Ok(samples) => {
                sounds.cache.insert(id, samples.into());
            }
            Err(error) => errors.push(format!(
                "{}: {name} could not be loaded. The default sound will be used. {error}",
                cue.label()
            )),
        }
    }
    errors
}

fn gain(settings: &AudioSettings, cue: AudioCue) -> f32 {
    let sound = settings.event_sounds.get(cue);
    if !settings.sounds_enabled
        || !sound.enabled
        || !settings.sound_volume.is_finite()
        || !sound.volume.is_finite()
    {
        return 0.0;
    }
    settings.sound_volume.clamp(0.0, 1.0) * sound.volume.clamp(0.0, 1.0)
}

pub(crate) fn audible(settings: &AudioSettings, cue: AudioCue) -> bool {
    gain(settings, cue) > 0.0
}

/// Repeated call alerts must finish their selected cached WAV before another
/// copy enters the bounded mixer queue. Every embedded cue is shorter than the
/// normal call cadence; missing custom files use that same embedded fallback.
pub(crate) fn repeat_interval(
    cue: AudioCue,
    settings: &AudioSettings,
    minimum: std::time::Duration,
) -> std::time::Duration {
    let SoundSource::Custom { id, .. } = settings.event_sounds.get(cue).source else {
        return minimum;
    };
    let Some(samples) = settings.event_sounds.cache.get(&id) else {
        return minimum;
    };
    let duration = std::time::Duration::from_micros(
        (samples.len() as u64 * 1_000_000).div_ceil(SAMPLE_RATE as u64),
    );
    minimum.max(duration + std::time::Duration::from_millis(150))
}

/// The only playback resolver: no files, decoding, device changes or recursion
/// through another event's preferences. A borrowed built-in uses its raw sound.
pub(crate) fn samples(cue: AudioCue, settings: &AudioSettings) -> Vec<f32> {
    let volume = gain(settings, cue);
    if volume <= 0.0 {
        return Vec::new();
    }
    match settings.event_sounds.get(cue).source {
        SoundSource::Custom { id, .. } => match settings.event_sounds.cache.get(&id) {
            Some(samples) => samples.iter().map(|sample| sample * volume).collect(),
            None => crate::sound_cues::samples(cue, volume),
        },
        SoundSource::BuiltIn { cue: source } => crate::sound_cues::samples(source, volume),
        SoundSource::Default => crate::sound_cues::samples(cue, volume),
    }
}

/// Little-endian RIFF WAV: mono/stereo PCM 8/16/24/32-bit or IEEE float32/64.
/// Resample in memory to the mixer's 48 kHz mono clock without extra libraries.
fn decode_wav(bytes: &[u8]) -> Result<Vec<f32>, String> {
    let invalid = || "Choose a valid uncompressed PCM or floating-point WAV file.".to_string();
    if bytes.len() < 12
        || bytes.len() > MAX_FILE_BYTES
        || &bytes[..4] != b"RIFF"
        || &bytes[8..12] != b"WAVE"
    {
        return Err(invalid());
    }
    let u16_at =
        |data: &[u8], offset| u16::from_le_bytes(data[offset..offset + 2].try_into().unwrap());
    let u32_at =
        |data: &[u8], offset| u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());
    let end = usize::try_from(u32_at(bytes, 4))
        .map_err(|_| invalid())?
        .checked_add(8)
        .ok_or_else(invalid)?;
    if end != bytes.len() {
        return Err(invalid());
    }
    let mut position = 12usize;
    let mut format = None;
    let mut data = None;
    while position < end {
        if end - position < 8 {
            return Err(invalid());
        }
        let length = u32_at(bytes, position + 4) as usize;
        let begin = position + 8;
        let next = begin.checked_add(length).ok_or_else(invalid)?;
        if next > end {
            return Err(invalid());
        }
        let duplicate = match &bytes[position..position + 4] {
            b"fmt " => format.replace(&bytes[begin..next]).is_some(),
            b"data" => data.replace(&bytes[begin..next]).is_some(),
            _ => false,
        };
        if duplicate {
            return Err(invalid());
        }
        position = next.checked_add(length % 2).ok_or_else(invalid)?;
        if position > end {
            return Err(invalid());
        }
    }
    let format = format
        .filter(|format| format.len() >= 16)
        .ok_or_else(invalid)?;
    let data = data.filter(|data| !data.is_empty()).ok_or_else(invalid)?;
    let mut encoding = u16_at(format, 0);
    let channels = u16_at(format, 2) as usize;
    let rate = u32_at(format, 4) as usize;
    let bits = u16_at(format, 14) as usize;
    if encoding == 0xfffe {
        if format.len() < 40
            || u16_at(format, 16) < 22
            || 18 + u16_at(format, 16) as usize > format.len()
            || !matches!(u16_at(format, 18) as usize, 0) && u16_at(format, 18) as usize != bits
            || format[26..40] != [0, 0, 0, 0, 0x10, 0, 0x80, 0, 0, 0xaa, 0, 0x38, 0x9b, 0x71]
        {
            return Err(invalid());
        }
        encoding = u16_at(format, 24);
    }
    if !(channels == 1 || channels == 2)
        || !(8_000..=192_000).contains(&rate)
        || !matches!((encoding, bits), (1, 8 | 16 | 24 | 32) | (3, 32 | 64))
    {
        return Err(invalid());
    }
    let width = bits / 8;
    let block = channels * width;
    if u16_at(format, 12) as usize != block
        || u32_at(format, 8) as usize != rate * block
        || data.len() % block != 0
    {
        return Err(invalid());
    }
    let frames = data.len() / block;
    if frames > rate * MAX_SECONDS {
        return Err("Choose a sound no longer than 5 seconds.".into());
    }
    let sample = |frame: usize, channel: usize| -> Result<f32, String> {
        let offset = frame * block + channel * width;
        let raw = &data[offset..offset + width];
        let value = match (encoding, bits) {
            (1, 8) => (raw[0] as f64 - 128.0) / 128.0,
            (1, 16) => i16::from_le_bytes(raw.try_into().unwrap()) as f64 / 32768.0,
            (1, 24) => {
                i32::from_le_bytes([
                    raw[0],
                    raw[1],
                    raw[2],
                    if raw[2] & 0x80 != 0 { 0xff } else { 0 },
                ]) as f64
                    / 8_388_608.0
            }
            (1, 32) => i32::from_le_bytes(raw.try_into().unwrap()) as f64 / 2_147_483_648.0,
            (3, 32) => f32::from_le_bytes(raw.try_into().unwrap()) as f64,
            (3, 64) => f64::from_le_bytes(raw.try_into().unwrap()),
            _ => unreachable!(),
        };
        if !value.is_finite() {
            return Err("The WAV file contains invalid audio samples.".into());
        }
        Ok(value.clamp(-1.0, 1.0) as f32)
    };
    let mono = |frame| -> Result<f32, String> {
        let first = sample(frame, 0)?;
        Ok(if channels == 2 {
            (first + sample(frame, 1)?) * 0.5
        } else {
            first
        })
    };
    // Validate every source sample, including frames skipped when downsampling.
    for frame in 0..frames {
        for channel in 0..channels {
            sample(frame, channel)?;
        }
    }
    let count = (frames as u64 * SAMPLE_RATE as u64).div_ceil(rate as u64) as usize;
    let mut output = Vec::with_capacity(count);
    for index in 0..count {
        let at = index as f64 * rate as f64 / SAMPLE_RATE as f64;
        let left = (at as usize).min(frames - 1);
        let right = (left + 1).min(frames - 1);
        let blend = (at - left as f64) as f32;
        output.push(mono(left)? * (1.0 - blend) + mono(right)? * blend);
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wav(encoding: u16, channels: u16, rate: u32, bits: u16, data: &[u8]) -> Vec<u8> {
        let block = channels * (bits / 8);
        let mut bytes = b"RIFF".to_vec();
        bytes.extend((36 + data.len() as u32 + data.len() as u32 % 2).to_le_bytes());
        bytes.extend(b"WAVEfmt ");
        bytes.extend(16u32.to_le_bytes());
        bytes.extend(encoding.to_le_bytes());
        bytes.extend(channels.to_le_bytes());
        bytes.extend(rate.to_le_bytes());
        bytes.extend((rate * block as u32).to_le_bytes());
        bytes.extend(block.to_le_bytes());
        bytes.extend(bits.to_le_bytes());
        bytes.extend(b"data");
        bytes.extend((data.len() as u32).to_le_bytes());
        bytes.extend(data);
        if !data.len().is_multiple_of(2) {
            bytes.push(0);
        }
        bytes
    }

    #[test]
    fn legacy_settings_keep_all_event_defaults_and_editor_defaults_stay_clean() {
        let mut settings: AudioSettings =
            serde_json::from_value(serde_json::json!({"sound_volume":0.4})).unwrap();
        let unchanged = settings.clone();
        for cue in AudioCue::ALL {
            assert!(audible(&settings, cue));
            assert_eq!(
                samples(cue, &settings),
                crate::sound_cues::samples(cue, 0.4)
            );
            settings.event_sounds.get_mut(cue);
        }
        assert_eq!(settings, unchanged);
        assert_eq!(
            AudioCue::ALL
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            10
        );
    }

    #[test]
    fn every_event_can_be_remapped_muted_and_scaled_without_other_event_preferences() {
        let mut settings = AudioSettings {
            sound_volume: 0.5,
            ..Default::default()
        };
        settings.event_sounds.get_mut(AudioCue::Leave).enabled = false;
        settings.event_sounds.get_mut(AudioCue::Message).source = SoundSource::BuiltIn {
            cue: AudioCue::Leave,
        };
        settings.event_sounds.get_mut(AudioCue::Message).volume = 0.4;
        assert_eq!(
            samples(AudioCue::Message, &settings),
            crate::sound_cues::samples(AudioCue::Leave, 0.2)
        );
        assert!(samples(AudioCue::Leave, &settings).is_empty());
        for cue in AudioCue::ALL {
            settings.event_sounds.get_mut(cue).enabled = false;
            assert!(!audible(&settings, cue));
            assert!(samples(cue, &settings).is_empty());
        }
        settings.event_sounds = Default::default();
        settings.sounds_enabled = false;
        assert!(samples(AudioCue::Join, &settings).is_empty());
        settings.sounds_enabled = true;
        settings.sound_volume = f32::NAN;
        assert!(samples(AudioCue::Join, &settings).is_empty());
    }

    #[test]
    fn pcm_and_float_formats_downmix_and_resample_to_finite_mono() {
        for (encoding, bits, data) in [
            (1, 8, vec![192]),
            (1, 16, 16384i16.to_le_bytes().to_vec()),
            (1, 24, vec![0, 0, 64]),
            (1, 32, 1_073_741_824i32.to_le_bytes().to_vec()),
            (3, 32, 0.5f32.to_le_bytes().to_vec()),
            (3, 64, 0.5f64.to_le_bytes().to_vec()),
        ] {
            assert_eq!(
                decode_wav(&wav(encoding, 1, 48_000, bits, &data)).unwrap(),
                vec![0.5]
            );
        }
        let stereo: Vec<u8> = [0i16, 32766, 0, 32766]
            .into_iter()
            .flat_map(i16::to_le_bytes)
            .collect();
        let result = decode_wav(&wav(1, 2, 24_000, 16, &stereo)).unwrap();
        assert_eq!(result.len(), 4);
        assert!(result
            .iter()
            .all(|value| (*value - 0.49996948).abs() < 0.00001));
    }

    #[test]
    fn malformed_oversize_and_overlong_wavs_are_rejected_without_truncation() {
        let valid = wav(1, 1, 48_000, 16, &[0, 0, 1, 0]);
        for bytes in [
            valid[..20].to_vec(),
            b"not a WAV".to_vec(),
            vec![0; MAX_FILE_BYTES + 1],
            wav(1, 1, 48_000, 16, &[]),
            wav(1, 1, 48_000, 16, &[0]),
            wav(1, 3, 48_000, 16, &[0; 6]),
            wav(7, 1, 48_000, 16, &[0, 0]),
            wav(3, 1, 48_000, 32, &f32::NAN.to_le_bytes()),
            wav(1, 1, 48_000, 16, &vec![0; (MAX_SAMPLES + 1) * 2]),
        ] {
            assert!(decode_wav(&bytes).is_err());
        }
        let maximum = decode_wav(&wav(1, 1, 48_000, 16, &vec![0; MAX_SAMPLES * 2])).unwrap();
        assert_eq!(maximum.len(), MAX_SAMPLES);
        let mut invalid_block = valid;
        invalid_block[32] = 4;
        assert!(decode_wav(&invalid_block).is_err());
    }

    #[test]
    fn imported_library_survives_source_removal_and_runtime_never_reopens_files() {
        let folder = tempfile::tempdir().unwrap();
        let source = folder.path().join("my notification.wav");
        let root = library_root(&folder.path().join("config").join("preferences.json"));
        std::fs::write(&source, wav(1, 1, 48_000, 16, &[0, 32, 0, 64])).unwrap();
        let imported = import_wav(&root, &source).unwrap();
        let id = imported.id.clone();
        let mut settings = AudioSettings {
            sound_volume: 0.5,
            ..Default::default()
        };
        settings.event_sounds.get_mut(AudioCue::Message).source =
            settings.event_sounds.install(imported);
        let original = samples(AudioCue::Message, &settings);
        let json = serde_json::to_string(&settings).unwrap();
        assert!(json.contains(&id));
        assert!(!json.contains(source.to_str().unwrap()));
        assert!(!json.contains("cache"));
        std::fs::remove_file(source).unwrap();
        let mut restarted: AudioSettings = serde_json::from_str(&json).unwrap();
        assert!(!restarted
            .event_sounds
            .is_loaded(&restarted.event_sounds.get(AudioCue::Message).source));
        assert!(hydrate(&mut restarted.event_sounds, &root).is_empty());
        std::fs::remove_file(root.join(format!("{id}.wav"))).unwrap();
        assert_eq!(samples(AudioCue::Message, &restarted), original);
        assert_eq!(settings, restarted);
        assert_eq!(hydrate(&mut restarted.event_sounds, &root).len(), 1);
        assert_eq!(
            restarted.event_sounds.get(AudioCue::Message).source,
            settings.event_sounds.get(AudioCue::Message).source
        );
        assert_eq!(
            samples(AudioCue::Message, &restarted),
            crate::sound_cues::samples(AudioCue::Message, 0.5)
        );
    }

    #[test]
    fn cache_completion_preserves_newer_choices_and_rejects_path_identifiers() {
        let mut edited = EventSounds::default();
        edited.get_mut(AudioCue::Message).source = SoundSource::BuiltIn {
            cue: AudioCue::Join,
        };
        edited.get_mut(AudioCue::Message).volume = 0.2;
        let before = edited.clone();
        let mut stale = EventSounds::default();
        stale.get_mut(AudioCue::Message).source = stale.install(ImportedSound {
            id: "a".repeat(64),
            name: "sound.wav".into(),
            samples: Arc::from([0.25f32]),
        });
        edited.merge_loaded(&stale);
        assert_eq!(edited, before);
        assert!(edited.cache.is_empty());
        edited.get_mut(AudioCue::Message).source = SoundSource::Custom {
            id: "../outside.wav".into(),
            name: "invalid".into(),
        };
        let folder = tempfile::tempdir().unwrap();
        assert_eq!(hydrate(&mut edited, folder.path()).len(), 1);
        assert!(edited.cache.is_empty());
    }

    #[test]
    fn repeat_interval_keeps_builtin_and_missing_custom_sound_cadence() {
        let mut settings = AudioSettings::default();
        let minimum = std::time::Duration::from_secs(3);
        for cue in [AudioCue::Ring, AudioCue::Ringback] {
            assert_eq!(repeat_interval(cue, &settings, minimum), minimum);
            settings.event_sounds.get_mut(cue).source = SoundSource::BuiltIn {
                cue: AudioCue::Join,
            };
            assert_eq!(repeat_interval(cue, &settings, minimum), minimum);
            settings.event_sounds.get_mut(cue).source = SoundSource::Custom {
                id: "a".repeat(64),
                name: "missing.wav".into(),
            };
            assert_eq!(repeat_interval(cue, &settings, minimum), minimum);
        }
    }
}
