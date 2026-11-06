//! Measured facts for the host overview, instead of nothing.
//!
//! Capacity: an approved co-host reports what its own machine measured
//! (free processor, upload, free storage). Voice: members report the ping,
//! jitter, loss and late packets they measured to a co-host. The assessment
//! then judges by the worst-off participant. Reports live in memory only, are
//! bounded, expire after `REPORT_FRESH_MS`, and are never trusted beyond what
//! they claim: a member cannot report capacity for someone else, and nobody
//! reports voice quality to themselves.
//!
//! The group size is not reported but counted here: the members in voice now.
use super::*;
use relay_protocol::hosting::{
    CapacityFacts, HostRole, MeasurementReport, ParticipantQuality, VoiceFacts,
    REPORT_FRESH_MS,
};
use std::collections::HashMap;

/// More reporters than this are dropped oldest first; nothing grows unbounded.
const MAX_REPORTERS: usize = 512;

struct Stored {
    at_ms: u64,
    report: MeasurementReport,
}

#[derive(Default)]
pub(crate) struct Measurements {
    by_reporter: Mutex<HashMap<String, Stored>>,
    host_cpu: Mutex<HostCpu>,
}

/// Free processor of the machine this server runs on, from two samples at
/// least `CPU_SAMPLE` apart.
#[derive(Default)]
struct HostCpu {
    last: Option<((u64, u64), std::time::Instant)>,
    free: Option<u8>,
}
const CPU_SAMPLE: std::time::Duration = std::time::Duration::from_secs(2);

/// `(idle, total)` processor time; the total includes idle time.
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

#[cfg(target_os = "linux")]
fn system_times() -> Option<(u64, u64)> {
    proc_stat_times(&std::fs::read_to_string("/proc/stat").ok()?)
}

#[cfg(not(any(windows, target_os = "linux")))]
fn system_times() -> Option<(u64, u64)> {
    None
}

/// The first `cpu` line of `/proc/stat`: idle and iowait count as idle.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn proc_stat_times(text: &str) -> Option<(u64, u64)> {
    let line = text.lines().find(|l| l.starts_with("cpu "))?;
    let fields: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .take(8)
        .map(|f| f.parse().ok())
        .collect::<Option<_>>()?;
    if fields.len() < 5 {
        return None;
    }
    let idle = fields[3].checked_add(fields[4])?;
    let total = fields.iter().try_fold(0u64, |sum, v| sum.checked_add(*v))?;
    Some((idle, total))
}

fn free_percent(previous: (u64, u64), now: (u64, u64)) -> Option<u8> {
    let idle = now.0.checked_sub(previous.0)?;
    let total = now.1.checked_sub(previous.1)?;
    if total == 0 || idle > total {
        return None;
    }
    Some((idle * 100 / total) as u8)
}

impl Measurements {
    /// The running host's own measured capability: its free processor,
    /// sampled here, and at least the fastest download a member timed from it.
    pub(crate) fn host(&self, now_ms: u64) -> relay_protocol::host_choice::Capability {
        let upload_kbps = lock(&self.by_reporter)
            .values()
            .filter(|s| now_ms.saturating_sub(s.at_ms) <= REPORT_FRESH_MS)
            .filter_map(|s| s.report.host_download_kbps)
            .max();
        let cpu_free_pct = {
            let mut cpu = lock(&self.host_cpu);
            let now = std::time::Instant::now();
            match (cpu.last, system_times()) {
                (Some((before, at)), Some(times)) if now.duration_since(at) >= CPU_SAMPLE => {
                    cpu.free = free_percent(before, times).or(cpu.free);
                    cpu.last = Some((times, now));
                }
                (None, Some(times)) => cpu.last = Some((times, now)),
                _ => {}
            }
            cpu.free
        };
        relay_protocol::host_choice::Capability {
            upload_kbps,
            cpu_free_pct,
            host_rtt_ms: None,
        }
    }

    fn put(&self, reporter: &str, report: MeasurementReport, now_ms: u64) {
        let mut map = lock(&self.by_reporter);
        map.retain(|_, s| now_ms.saturating_sub(s.at_ms) <= REPORT_FRESH_MS);
        if map.len() >= MAX_REPORTERS && !map.contains_key(reporter) {
            if let Some(oldest) = map
                .iter()
                .min_by_key(|(_, s)| s.at_ms)
                .map(|(k, _)| k.clone())
            {
                map.remove(&oldest);
            }
        }
        map.insert(reporter.to_owned(), Stored { at_ms: now_ms, report });
    }

    /// The candidate's own fresh capacity report, with the group size counted
    /// from the people in voice now.
    pub(crate) fn capacity(
        &self,
        candidate: &str,
        in_voice: &[String],
        now_ms: u64,
    ) -> Option<CapacityFacts> {
        let map = lock(&self.by_reporter);
        let stored = map.get(candidate)?;
        if now_ms.saturating_sub(stored.at_ms) > REPORT_FRESH_MS {
            return None;
        }
        let mut facts = stored.report.capacity?;
        facts.participants = in_voice.len() as u32;
        Some(facts)
    }

    /// Fresh measurements other members took to `candidate`. People in voice
    /// now who have not measured it are counted as unmeasured, so a missing
    /// participant keeps the answer grey instead of looking fine.
    pub(crate) fn voice(
        &self,
        candidate: &str,
        in_voice: &[String],
        now_ms: u64,
    ) -> Option<VoiceFacts> {
        let map = lock(&self.by_reporter);
        let mut measured: Vec<ParticipantQuality> = Vec::new();
        let mut reporters: Vec<&str> = Vec::new();
        for (reporter, stored) in map.iter() {
            if reporter == candidate || now_ms.saturating_sub(stored.at_ms) > REPORT_FRESH_MS {
                continue;
            }
            if let Some(sample) = stored
                .report
                .voice
                .iter()
                .find(|s| s.host_user_id == candidate)
            {
                measured.push(sample.quality);
                reporters.push(reporter);
            }
        }
        let unmeasured = in_voice
            .iter()
            .filter(|u| u.as_str() != candidate && !reporters.contains(&u.as_str()))
            .count() as u32;
        (!measured.is_empty() || unmeasured > 0).then_some(VoiceFacts {
            measured,
            unmeasured,
        })
    }
}

impl AppState {
    /// Distinct members in voice now.
    pub(crate) fn members_in_voice(&self) -> Vec<String> {
        let mut users: Vec<String> = lock(&self.0.voice)
            .values()
            .map(|v| v.user.id.clone())
            .collect();
        users.sort();
        users.dedup();
        users
    }
}

pub(super) async fn report(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<MeasurementReport>,
) -> ApiResult<StatusCode> {
    let auth = state.auth(&headers)?;
    input.validate().map_err(bad)?;
    {
        let db = lock(&state.0.db);
        active_user(&db, &auth.user.id)?;
        if input.capacity.is_some()
            && !hosting::holds_role(&db, &auth.user.id, HostRole::Replica)
            && !hosting::holds_role(&db, &auth.user.id, HostRole::VoiceGateway)
        {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "Only an approved co-host reports the capacity of its own machine",
            ));
        }
        if input.voice.iter().any(|s| s.host_user_id == auth.user.id) {
            return Err(bad("Measure the connection to another member's machine"));
        }
    }
    state
        .0
        .measurements
        .put(&auth.user.id, input, now_ms());
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod host_machine_tests {
    use super::*;

    #[test]
    fn the_host_upload_is_the_fastest_fresh_download_and_its_processor_is_sampled() {
        let m = Measurements::default();
        let report = |kbps| MeasurementReport {
            host_download_kbps: Some(kbps),
            ..Default::default()
        };
        m.put("ada", report(9_000), 1_000);
        m.put("bo", report(24_000), 1_000);
        m.put("cy", report(80_000), 0);
        assert_eq!(m.host(1_000).upload_kbps, Some(80_000));
        assert_eq!(m.host(REPORT_FRESH_MS + 500).upload_kbps, Some(24_000), "cy's is stale");
        assert_eq!(Measurements::default().host(0).upload_kbps, None);
        assert_eq!(
            proc_stat_times("cpu  100 0 50 800 50 0 0 0 0 0
cpu0 1 2 3 4
"),
            Some((850, 1000))
        );
        assert_eq!(proc_stat_times("intr 1 2"), None);
        assert_eq!(free_percent((100, 1000), (180, 1100)), Some(80));
        if cfg!(any(windows, target_os = "linux")) {
            m.host(0);
            std::thread::sleep(CPU_SAMPLE + std::time::Duration::from_millis(50));
            assert!(m.host(0).cpu_free_pct.is_some_and(|free| free <= 100));
        }
    }
}
