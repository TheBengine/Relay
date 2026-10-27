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
}

impl Measurements {
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
