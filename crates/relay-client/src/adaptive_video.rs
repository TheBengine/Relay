//! Stable output profiles under real RTC encoder feedback, bounded by user choice.
use crate::capture::{CaptureOptions, CaptureQuality, VideoPriority};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum VideoPressure {
    Healthy,
    Bandwidth,
    Cpu,
    Unknown,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct VideoProfile {
    pub quality: CaptureQuality,
    pub fps: u32,
}

pub(crate) struct AdaptiveVideo {
    profiles: Vec<VideoProfile>,
    index: usize,
    enabled: bool,
    bad: u32,
    good: u32,
    last_sample: Option<Instant>,
    last_change: Option<Instant>,
    pressure: VideoPressure,
    /// One step lower while incoming voice struggles, even with adaptation off.
    voice_first: bool,
}

impl AdaptiveVideo {
    pub(crate) fn new(options: CaptureOptions) -> Self {
        let options = options.normalized();
        let profiles = ladder(options);
        Self {
            profiles,
            index: 0,
            enabled: options.adaptive_video,
            bad: 0,
            good: 0,
            last_sample: None,
            last_change: None,
            pressure: VideoPressure::Unknown,
            voice_first: false,
        }
    }

    /// Voice first: temporary and released as soon as voice is healthy again.
    pub(crate) fn set_voice_first(&mut self, on: bool) {
        self.voice_first = on;
    }

    pub(crate) fn voice_first(&self) -> bool {
        self.voice_first
    }

    #[cfg(test)]
    pub(crate) fn profiles(&self) -> &[VideoProfile] {
        &self.profiles
    }

    pub(crate) fn profile(&self) -> VideoProfile {
        let index = self.index + usize::from(self.voice_first);
        self.profiles[index.min(self.profiles.len() - 1)]
    }

    pub(crate) fn observe(&mut self, pressure: VideoPressure, now: Instant) {
        self.pressure = pressure;
        if !self.enabled {
            return;
        }
        if self.last_sample.is_some_and(|at| {
            now < at || now.saturating_duration_since(at) > Duration::from_secs(5)
        }) {
            self.bad = 0;
            self.good = 0;
        }
        self.last_sample = Some(now);
        match pressure {
            VideoPressure::Bandwidth | VideoPressure::Cpu => {
                self.bad = self.bad.saturating_add(1);
                self.good = 0;
            }
            VideoPressure::Healthy => {
                self.good = self.good.saturating_add(1);
                self.bad = 0;
            }
            VideoPressure::Unknown => {
                self.good = 0;
                self.bad = 0;
            }
        }
        if self
            .last_change
            .is_some_and(|at| now.saturating_duration_since(at) < Duration::from_secs(8))
        {
            return;
        }
        if self.bad >= 3 && self.index + 1 < self.profiles.len() {
            self.index += 1;
            self.last_change = Some(now);
            self.bad = 0;
        } else if self.good >= 10 && self.index > 0 {
            self.index -= 1;
            self.last_change = Some(now);
            self.good = 0;
        }
    }

    pub(crate) fn description(&self) -> String {
        let profile = self.profile();
        let resolution = match profile.quality {
            CaptureQuality::P480 => "480p",
            CaptureQuality::P720 => "720p",
            CaptureQuality::P1080 => "1080p",
            CaptureQuality::P1440 => "1440p",
            CaptureQuality::P2160 => "4K",
            CaptureQuality::Source => "Source (up to 4K)",
        };
        let reason = match self.pressure {
            VideoPressure::Healthy => "connection stable",
            VideoPressure::Bandwidth => "bandwidth limited",
            VideoPressure::Cpu => "encoder busy",
            VideoPressure::Unknown => "waiting for RTC measurements",
        };
        let voice = if self.voice_first {
            " · lowered while voice is struggling"
        } else {
            ""
        };
        format!(
            "Automatic output: {resolution} · {} fps · {reason}{voice}",
            profile.fps
        )
    }
}

/// Output steps from the user's choice downwards. Every step keeps or lowers
/// both resolution and frame rate; the priority decides which goes first.
fn ladder(options: CaptureOptions) -> Vec<VideoProfile> {
    let selected = VideoProfile {
        quality: options.quality,
        fps: options.fps,
    };
    let selected_height = options.quality.dimensions().1;
    let lower = [
        CaptureQuality::P1440,
        CaptureQuality::P1080,
        CaptureQuality::P720,
        CaptureQuality::P480,
    ]
    .into_iter()
    .filter(|quality| selected_height > quality.dimensions().1)
    .collect::<Vec<_>>();
    let mut profiles = vec![selected];
    let mut push = |profile: VideoProfile| {
        if profiles.last() != Some(&profile) {
            profiles.push(profile);
        }
    };
    match options.priority {
        VideoPriority::Auto => {
            if options.fps > 30 {
                push(VideoProfile { quality: options.quality, fps: 30 });
            }
            for quality in lower.iter().copied().filter(|q| *q != CaptureQuality::P480) {
                push(VideoProfile { quality, fps: options.fps.min(30) });
            }
            if options.quality != CaptureQuality::P480 || options.fps > 15 {
                push(VideoProfile { quality: CaptureQuality::P480, fps: 15 });
            }
        }
        VideoPriority::Detail => {
            for fps in [30, 15] {
                if options.fps > fps {
                    push(VideoProfile { quality: options.quality, fps });
                }
            }
            for quality in lower {
                push(VideoProfile { quality, fps: options.fps.min(15) });
            }
        }
        VideoPriority::Motion => {
            for quality in lower.iter().copied() {
                push(VideoProfile { quality, fps: options.fps });
            }
            let floor = lower.last().copied().unwrap_or(options.quality);
            for fps in [30, 15] {
                if options.fps > fps {
                    push(VideoProfile { quality: floor, fps });
                }
            }
        }
    }
    profiles
}

/// What the encoder actually sent over a recent RTC sampling interval.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SentVideo {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) fps: f32,
    pub(crate) pressure: VideoPressure,
}

impl SentVideo {
    #[cfg_attr(not(any(test, feature = "native-rtc")), allow(dead_code))]
    pub(crate) fn new(
        width: u32,
        height: u32,
        encoded_frames: u32,
        elapsed: Duration,
        pressure: VideoPressure,
    ) -> Option<Self> {
        if width == 0
            || height == 0
            || elapsed < Duration::from_millis(500)
            || elapsed > Duration::from_secs(5)
        {
            return None;
        }
        let fps = encoded_frames as f32 / elapsed.as_secs_f32();
        (fps.is_finite() && fps <= 240.0).then_some(Self { width, height, fps, pressure })
    }

    /// Chosen versus sent, as text; colour is never the only signal. `chosen`
    /// is what the live publication started with, `source` the captured size.
    /// "Below" is said only when RTC reports bandwidth or encoder pressure:
    /// a small window or a static screen is not a reduction.
    pub(crate) fn summary(&self, chosen: CaptureOptions, source: (u32, u32)) -> String {
        let chosen = chosen.normalized();
        let mut text = format!("Sending {} × {} · {:.0} fps", self.width, self.height, self.fps);
        match self.pressure {
            VideoPressure::Bandwidth => text.push_str(" · limited by upload bandwidth"),
            VideoPressure::Cpu => text.push_str(" · limited by the encoder (CPU/GPU busy)"),
            VideoPressure::Healthy | VideoPressure::Unknown => {}
        }
        let (box_width, box_height) = chosen.quality.dimensions();
        let limit = |chosen: u32, source: u32| if source > 0 { chosen.min(source) } else { chosen };
        let (width, height) = (limit(box_width, source.0), limit(box_height, source.1));
        let smaller = (self.width as f32) < width as f32 * 0.95
            && (self.height as f32) < height as f32 * 0.95;
        let slower = self.fps < chosen.fps as f32 * 0.85;
        let pressured = matches!(self.pressure, VideoPressure::Bandwidth | VideoPressure::Cpu);
        if pressured && (smaller || slower) {
            text.push_str(&format!(
                " · below your choice of {} · {} fps",
                quality_label(chosen.quality),
                chosen.fps
            ));
        }
        text
    }
}

fn quality_label(quality: CaptureQuality) -> &'static str {
    match quality {
        CaptureQuality::P480 => "480p",
        CaptureQuality::P720 => "720p",
        CaptureQuality::P1080 => "1080p",
        CaptureQuality::P1440 => "1440p",
        CaptureQuality::P2160 => "4K",
        CaptureQuality::Source => "Source (up to 4K)",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn priority_decides_whether_frame_rate_or_resolution_drops_first() {
        let at = |quality, fps| VideoProfile { quality, fps };
        let options = |priority| CaptureOptions {
            quality: CaptureQuality::P1080,
            fps: 60,
            adaptive_video: true,
            priority,
            ..Default::default()
        };
        use CaptureQuality::*;
        assert_eq!(
            AdaptiveVideo::new(options(VideoPriority::Detail)).profiles(),
            [at(P1080, 60), at(P1080, 30), at(P1080, 15), at(P720, 15), at(P480, 15)]
        );
        assert_eq!(
            AdaptiveVideo::new(options(VideoPriority::Motion)).profiles(),
            [at(P1080, 60), at(P720, 60), at(P480, 60), at(P480, 30), at(P480, 15)]
        );
        assert_eq!(
            AdaptiveVideo::new(options(VideoPriority::Auto)).profiles(),
            [at(P1080, 60), at(P1080, 30), at(P720, 30), at(P480, 15)],
            "automatic keeps the previous ladder"
        );
        let smallest = CaptureOptions { quality: P480, fps: 15, ..options(VideoPriority::Motion) };
        assert_eq!(AdaptiveVideo::new(smallest).profiles(), [at(P480, 15)]);
    }

    #[test]
    fn voice_first_lowers_one_step_temporarily_even_without_adaptation() {
        let mut manual = AdaptiveVideo::new(CaptureOptions {
            quality: CaptureQuality::P1080,
            fps: 30,
            adaptive_video: false,
            ..Default::default()
        });
        let chosen = manual.profile();
        manual.set_voice_first(true);
        assert_eq!(manual.profile(), VideoProfile { quality: CaptureQuality::P720, fps: 30 });
        assert!(manual.description().contains("voice is struggling"));
        manual.set_voice_first(false);
        assert_eq!(manual.profile(), chosen, "released when voice is healthy");
        let mut smallest = AdaptiveVideo::new(CaptureOptions {
            quality: CaptureQuality::P480,
            fps: 15,
            ..Default::default()
        });
        smallest.set_voice_first(true);
        assert_eq!(smallest.profile(), VideoProfile { quality: CaptureQuality::P480, fps: 15 });
    }

    #[test]
    fn every_priority_ladder_only_steps_down_within_the_user_cap() {
        for priority in [VideoPriority::Auto, VideoPriority::Detail, VideoPriority::Motion] {
            for quality in [
                CaptureQuality::P480,
                CaptureQuality::P720,
                CaptureQuality::P1080,
                CaptureQuality::P1440,
                CaptureQuality::P2160,
                CaptureQuality::Source,
            ] {
                for fps in [15, 24, 30, 60] {
                    let policy = AdaptiveVideo::new(CaptureOptions {
                        quality,
                        fps,
                        adaptive_video: true,
                        priority,
                        ..Default::default()
                    });
                    let profiles = policy.profiles();
                    assert_eq!(profiles[0], VideoProfile { quality, fps });
                    for pair in profiles.windows(2) {
                        assert!(pair[1].fps <= pair[0].fps, "{priority:?} {quality:?} {fps}");
                        assert!(pair[1].quality.dimensions().1 <= pair[0].quality.dimensions().1);
                        assert_ne!(pair[0], pair[1]);
                    }
                    let last = profiles.last().unwrap();
                    assert!(last.fps == 15 || fps < 15);
                    assert!(last.quality == CaptureQuality::P480 || quality == CaptureQuality::P480);
                }
            }
        }
    }

    #[test]
    fn sent_video_reports_measured_output_against_the_choice() {
        let chosen = CaptureOptions { quality: CaptureQuality::P1080, fps: 30, ..Default::default() };
        let screen = (2560, 1440);
        let full = SentVideo::new(1920, 1080, 58, Duration::from_secs(2), VideoPressure::Healthy).unwrap();
        assert_eq!(full.fps, 29.0);
        assert_eq!(full.summary(chosen, screen), "Sending 1920 × 1080 · 29 fps");
        let reduced = SentVideo::new(1280, 720, 40, Duration::from_secs(2), VideoPressure::Bandwidth).unwrap();
        assert_eq!(
            reduced.summary(chosen, screen),
            "Sending 1280 × 720 · 20 fps · limited by upload bandwidth · below your choice of 1080p · 30 fps"
        );
        // A narrow window inside the 1080p box is not a reduction.
        let window = SentVideo::new(1000, 1080, 60, Duration::from_secs(2), VideoPressure::Cpu).unwrap();
        assert_eq!(window.summary(chosen, screen), "Sending 1000 × 1080 · 30 fps · limited by the encoder (CPU/GPU busy)");
        // A small window or a 720p webcam sent at full size is not below the choice.
        let small = SentVideo::new(1200, 800, 60, Duration::from_secs(2), VideoPressure::Bandwidth).unwrap();
        assert!(!small.summary(chosen, (1200, 800)).contains("below"));
        // A static screen sends few frames without pressure: not a reduction either.
        let still = SentVideo::new(1920, 1080, 4, Duration::from_secs(2), VideoPressure::Healthy).unwrap();
        assert!(!still.summary(chosen, screen).contains("below"));
        let source = CaptureOptions { quality: CaptureQuality::Source, ..chosen };
        assert!(!full.summary(source, screen).contains("below"));
        assert!(reduced.summary(source, screen).contains("below"), "source size still bounds Source");
        assert!(SentVideo::new(0, 720, 30, Duration::from_secs(1), VideoPressure::Healthy).is_none());
        assert!(SentVideo::new(1280, 720, 30, Duration::from_millis(100), VideoPressure::Healthy).is_none());
        assert!(SentVideo::new(1280, 720, 30, Duration::from_secs(9), VideoPressure::Healthy).is_none());
    }

    #[test]
    fn profiles_react_to_pressure_have_cooldown_and_never_exceed_user_choice() {
        let now = Instant::now();
        let mut policy = AdaptiveVideo::new(CaptureOptions {
            quality: CaptureQuality::P1080,
            fps: 60,
            adaptive_video: true,
            ..Default::default()
        });
        for n in 0..3 {
            policy.observe(VideoPressure::Bandwidth, now + Duration::from_secs(n * 2));
        }
        assert_eq!(
            policy.profile(),
            VideoProfile {
                quality: CaptureQuality::P1080,
                fps: 30
            }
        );
        for n in 3..6 {
            policy.observe(VideoPressure::Cpu, now + Duration::from_secs(n * 2));
        }
        assert_eq!(policy.profile().quality, CaptureQuality::P1080);
        for n in 6..16 {
            policy.observe(VideoPressure::Bandwidth, now + Duration::from_secs(n * 2));
        }
        assert_eq!(
            policy.profile(),
            VideoProfile {
                quality: CaptureQuality::P480,
                fps: 15
            }
        );
        for n in 16..60 {
            policy.observe(VideoPressure::Healthy, now + Duration::from_secs(n * 2));
        }
        assert_eq!(
            policy.profile(),
            VideoProfile {
                quality: CaptureQuality::P1080,
                fps: 60
            }
        );
    }

    #[test]
    fn missing_or_interrupted_samples_never_count_as_recovery() {
        let now = Instant::now();
        let mut policy = AdaptiveVideo::new(CaptureOptions {
            quality: CaptureQuality::P720,
            adaptive_video: true,
            ..Default::default()
        });
        for n in 0..3 {
            policy.observe(VideoPressure::Bandwidth, now + Duration::from_secs(n * 2));
        }
        assert_eq!(policy.profile().quality, CaptureQuality::P480);
        for n in 3..30 {
            policy.observe(
                if n % 3 == 0 {
                    VideoPressure::Unknown
                } else {
                    VideoPressure::Healthy
                },
                now + Duration::from_secs(n * 2),
            );
        }
        assert_eq!(policy.profile().quality, CaptureQuality::P480);
        for n in 1..20 {
            policy.observe(
                VideoPressure::Healthy,
                now + Duration::from_secs(60 + n * 10),
            );
        }
        assert_eq!(policy.profile().quality, CaptureQuality::P480);
        let mut manual = AdaptiveVideo::new(CaptureOptions {
            adaptive_video: false,
            ..Default::default()
        });
        let original = manual.profile();
        for n in 0..100 {
            manual.observe(VideoPressure::Bandwidth, now + Duration::from_secs(n * 2));
        }
        assert_eq!(manual.profile(), original);
    }

    #[test]
    fn every_user_cap_has_a_non_increasing_bounded_profile_ladder() {
        for quality in [
            CaptureQuality::P480,
            CaptureQuality::P720,
            CaptureQuality::P1080,
            CaptureQuality::P1440,
            CaptureQuality::P2160,
            CaptureQuality::Source,
        ] {
            for fps in [15, 24, 30, 60] {
                let policy = AdaptiveVideo::new(CaptureOptions {
                    quality,
                    fps,
                    adaptive_video: true,
                    ..Default::default()
                });
                for profile in policy.profiles {
                    assert!(profile.fps <= fps && profile.fps >= 15);
                    assert!(profile.quality.dimensions().1 <= quality.dimensions().1);
                }
            }
        }
    }

    #[test]
    fn four_k_and_source_degrade_through_every_resolution_and_recover_to_the_user_cap() {
        for quality in [CaptureQuality::P2160, CaptureQuality::Source] {
            let now = Instant::now();
            let mut policy = AdaptiveVideo::new(CaptureOptions {
                quality,
                fps: 60,
                adaptive_video: true,
                ..Default::default()
            });
            assert_eq!(
                policy.profiles,
                vec![
                    VideoProfile { quality, fps: 60 },
                    VideoProfile { quality, fps: 30 },
                    VideoProfile {
                        quality: CaptureQuality::P1440,
                        fps: 30
                    },
                    VideoProfile {
                        quality: CaptureQuality::P1080,
                        fps: 30
                    },
                    VideoProfile {
                        quality: CaptureQuality::P720,
                        fps: 30
                    },
                    VideoProfile {
                        quality: CaptureQuality::P480,
                        fps: 15
                    },
                ]
            );
            for sample in 0..25 {
                policy.observe(
                    VideoPressure::Bandwidth,
                    now + Duration::from_secs(sample * 2),
                );
            }
            assert_eq!(policy.profile().quality, CaptureQuality::P480);
            for sample in 25..90 {
                policy.observe(
                    VideoPressure::Healthy,
                    now + Duration::from_secs(sample * 2),
                );
            }
            assert_eq!(policy.profile(), VideoProfile { quality, fps: 60 });
        }
    }
}
