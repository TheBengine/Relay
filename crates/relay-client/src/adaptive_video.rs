//! Stable output profiles under real RTC encoder feedback, bounded by user choice.
use crate::capture::{CaptureOptions, CaptureQuality};
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
}

impl AdaptiveVideo {
    pub(crate) fn new(options: CaptureOptions) -> Self {
        let options = options.normalized();
        let mut profiles = vec![VideoProfile {
            quality: options.quality,
            fps: options.fps,
        }];
        if options.fps > 30 {
            profiles.push(VideoProfile {
                quality: options.quality,
                fps: 30,
            });
        }
        let selected_height = options.quality.dimensions().1;
        for quality in [
            CaptureQuality::P1440,
            CaptureQuality::P1080,
            CaptureQuality::P720,
        ] {
            if selected_height > quality.dimensions().1 {
                profiles.push(VideoProfile {
                    quality,
                    fps: options.fps.min(30),
                });
            }
        }
        if options.quality != CaptureQuality::P480 || options.fps > 15 {
            profiles.push(VideoProfile {
                quality: CaptureQuality::P480,
                fps: 15,
            });
        }
        Self {
            profiles,
            index: 0,
            enabled: options.adaptive_video,
            bad: 0,
            good: 0,
            last_sample: None,
            last_change: None,
            pressure: VideoPressure::Unknown,
        }
    }

    pub(crate) fn profile(&self) -> VideoProfile {
        self.profiles[self.index]
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
        format!(
            "Automatic output: {resolution} · {} fps · {reason}",
            profile.fps
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
