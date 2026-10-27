//! The client side of the host overview's real numbers (#19).
//!
//! While in a call, Relay reports to `/api/hosting/measurements`:
//!
//! * the capacity of this machine, only when its member holds an approved
//!   co-host role: free processor, measured upload and free disk;
//! * voice quality to each approved co-host in the same call: this member's
//!   own round trip to the host, and the jitter, loss and late packets measured
//!   here on that co-host's own voice stream.
//!
//! Nothing is reported about anyone else's machine as a capacity, nobody
//! reports to themselves, and an empty report is never sent.
use crate::voice_quality::Quality;
use relay_protocol::hosting::{
    CapacityFacts, HostingView, MeasurementReport, ParticipantQuality, VoiceSample,
    MAX_VOICE_SAMPLES,
};
use std::{
    sync::atomic::{AtomicU32, Ordering},
    time::Duration,
};

/// Bytes sent once to time this machine's upload.
pub(crate) const UPLOAD_PROBE_BYTES: usize = 256 * 1024;

static UPLOAD_KBPS: AtomicU32 = AtomicU32::new(0);

/// The last measured upload of this machine, if any.
pub(crate) fn upload_kbps() -> Option<u32> {
    let value = UPLOAD_KBPS.load(Ordering::Relaxed);
    (value > 0).then_some(value)
}

pub(crate) fn set_upload_kbps(value: u32) {
    UPLOAD_KBPS.store(value, Ordering::Relaxed);
}

/// kbit/s from a timed transfer; `None` for a sample too short to mean anything.
pub(crate) fn kbps(bytes: usize, elapsed: Duration) -> Option<u32> {
    let ms = elapsed.as_secs_f64() * 1000.0;
    if bytes == 0 || !(5.0..=120_000.0).contains(&ms) {
        return None;
    }
    let value = bytes as f64 * 8.0 / ms;
    (1.0..10_000_000.0).contains(&value).then_some(value as u32)
}

/// Free processor share between two samples.
#[derive(Default)]
pub(crate) struct CpuSampler {
    last: Option<(u64, u64)>,
}

impl CpuSampler {
    /// Percent of processor time idle since the previous call; `None` on the first.
    pub(crate) fn sample(&mut self) -> Option<u8> {
        let now = system_times()?;
        let previous = self.last.replace(now);
        free_percent(previous?, now)
    }
}

/// `(idle, total)` processor time; the total includes idle time.
fn free_percent(previous: (u64, u64), now: (u64, u64)) -> Option<u8> {
    let idle = now.0.checked_sub(previous.0)?;
    let total = now.1.checked_sub(previous.1)?;
    if total == 0 || idle > total {
        return None;
    }
    Some((idle * 100 / total) as u8)
}

#[cfg(windows)]
fn system_times() -> Option<(u64, u64)> {
    use windows::Win32::{Foundation::FILETIME, System::Threading::GetSystemTimes};
    let (mut idle, mut kernel, mut user) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    // SAFETY: three valid out-parameters on this stack frame.
    unsafe { GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user)) }.ok()?;
    let value = |t: FILETIME| (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime);
    // Kernel time already includes idle time.
    Some((value(idle), value(kernel).checked_add(value(user))?))
}

#[cfg(not(windows))]
fn system_times() -> Option<(u64, u64)> {
    None
}

/// Free space where Relay keeps its data, in MB.
#[cfg(windows)]
pub(crate) fn free_disk_mb(path: &std::path::Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows::{core::PCWSTR, Win32::Storage::FileSystem::GetDiskFreeSpaceExW};
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut free = 0u64;
    // SAFETY: a NUL-terminated path and one valid out-parameter.
    unsafe { GetDiskFreeSpaceExW(PCWSTR(wide.as_ptr()), Some(&mut free), None, None) }.ok()?;
    Some(free / (1024 * 1024))
}

#[cfg(not(windows))]
pub(crate) fn free_disk_mb(_path: &std::path::Path) -> Option<u64> {
    None
}

/// Builds this member's report from the host view, what was heard here per
/// speaker, and this member's own round trip to the host. `None` when there
/// is nothing honest to report.
pub(crate) fn report(
    view: &HostingView,
    heard: &[(String, Quality)],
    own_rtt_ms: Option<f32>,
    capacity: Option<CapacityFacts>,
) -> Option<MeasurementReport> {
    let approved = |user: &str| {
        view.candidates
            .iter()
            .any(|c| c.candidate.user_id == user && !c.candidate.effective().is_empty())
    };
    let capacity = capacity.filter(|_| approved(&view.you));
    let voice = match own_rtt_ms.filter(|rtt| rtt.is_finite() && *rtt > 0.0) {
        Some(rtt_ms) => view
            .candidates
            .iter()
            .filter(|c| c.candidate.user_id != view.you && approved(&c.candidate.user_id))
            .filter_map(|c| {
                let (_, q) = heard
                    .iter()
                    .find(|(user, _)| *user == c.candidate.user_id)?;
                Some(VoiceSample {
                    host_user_id: c.candidate.user_id.clone(),
                    quality: ParticipantQuality {
                        rtt_ms: rtt_ms.min(10_000.0),
                        jitter_ms: q.jitter_ms.clamp(0.0, 10_000.0),
                        loss_pct: q.loss_percent.clamp(0.0, 100.0),
                        late_pct: q.late_percent.clamp(0.0, 100.0),
                    },
                })
            })
            .take(MAX_VOICE_SAMPLES)
            .collect(),
        None => Vec::new(),
    };
    let report = MeasurementReport { capacity, voice };
    report.validate().ok()?;
    Some(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use relay_protocol::hosting::{ActiveHost, Candidate, CandidateState, CandidateView, HostRole};

    fn candidate(user: &str, approved: bool) -> CandidateView {
        CandidateView {
            candidate: Candidate {
                user_id: user.into(),
                state: if approved {
                    CandidateState::Approved
                } else {
                    CandidateState::Pending
                },
                offered: vec![HostRole::VoiceGateway],
                accepted: if approved {
                    vec![HostRole::VoiceGateway]
                } else {
                    Vec::new()
                },
                offered_by: "admin".into(),
                updated_ms: 1,
            },
            name: user.into(),
            assessments: Vec::new(),
        }
    }

    fn view(you: &str, candidates: Vec<CandidateView>) -> HostingView {
        let mut view: HostingView = serde_json::from_value(serde_json::json!({
            "schema":1,"revision":1,"active_host":{"name":"Host","instance_id":"i"},
            "abilities":{"planned_handover":false,"unexpected_takeover":false},
            "you":you,"can_manage":false,"candidates":[]
        }))
        .unwrap();
        view.active_host = ActiveHost {
            name: "Host".into(),
            instance_id: "i".into(),
        };
        view.candidates = candidates;
        view
    }

    fn heard(user: &str, loss: f32) -> (String, Quality) {
        (
            user.into(),
            Quality {
                loss_percent: loss,
                late_percent: 1.5,
                jitter_ms: 4.0,
                expected: 250,
            },
        )
    }

    const CAPACITY: CapacityFacts = CapacityFacts {
        upload_kbps: Some(8000),
        cpu_free_pct: Some(70),
        free_disk_mb: Some(50_000),
        participants: 0,
    };

    #[test]
    fn a_member_reports_voice_measured_on_each_approved_cohost_only() {
        let v = view("me", vec![candidate("gus", true), candidate("otto", false)]);
        let report = report(
            &v,
            &[heard("gus", 2.0), heard("otto", 9.0), heard("lena", 1.0)],
            Some(42.0),
            Some(CAPACITY),
        )
        .unwrap();
        assert!(
            report.capacity.is_none(),
            "not a co-host: no capacity of its own"
        );
        assert_eq!(report.voice.len(), 1);
        let sample = &report.voice[0];
        assert_eq!(sample.host_user_id, "gus");
        assert_eq!(sample.quality.rtt_ms, 42.0);
        assert_eq!(sample.quality.loss_pct, 2.0);
        assert_eq!(sample.quality.late_pct, 1.5);
        assert_eq!(sample.quality.jitter_ms, 4.0);
    }

    #[test]
    fn a_cohost_reports_its_own_capacity_and_never_itself_as_voice() {
        let v = view("gus", vec![candidate("gus", true), candidate("otto", true)]);
        let report = report(
            &v,
            &[heard("gus", 0.0), heard("otto", 1.0)],
            Some(30.0),
            Some(CAPACITY),
        )
        .unwrap();
        assert_eq!(report.capacity, Some(CAPACITY));
        assert_eq!(report.voice.len(), 1);
        assert_eq!(report.voice[0].host_user_id, "otto");
        // Without a round trip of its own there is no voice sample, only capacity.
        let report = report_without_rtt(&v);
        assert!(report.voice.is_empty());
        // Nothing honest to say: no report at all.
        let lone = view("me", vec![candidate("gus", true)]);
        assert!(super::report(&lone, &[], Some(30.0), None).is_none());
        assert!(super::report(&lone, &[heard("gus", 0.0)], None, None).is_none());
    }

    fn report_without_rtt(v: &HostingView) -> MeasurementReport {
        report(v, &[heard("otto", 1.0)], None, Some(CAPACITY)).unwrap()
    }

    #[test]
    fn upload_and_processor_samples_are_plausible_or_nothing() {
        assert_eq!(
            kbps(UPLOAD_PROBE_BYTES, Duration::from_millis(262)),
            Some(8004)
        );
        assert_eq!(kbps(UPLOAD_PROBE_BYTES, Duration::from_millis(1)), None);
        assert_eq!(kbps(0, Duration::from_millis(100)), None);
        assert_eq!(free_percent((100, 1000), (180, 1100)), Some(80));
        assert_eq!(free_percent((100, 1000), (100, 1000)), None);
        assert_eq!(
            free_percent((200, 1000), (100, 1100)),
            None,
            "a clock going back"
        );
        let mut cpu = CpuSampler::default();
        let _ = cpu.sample();
        std::thread::sleep(Duration::from_millis(30));
        if cfg!(windows) {
            assert!(cpu.sample().is_some_and(|free| free <= 100));
            assert!(free_disk_mb(&std::env::temp_dir()).is_some_and(|mb| mb > 0));
        }
    }
}
