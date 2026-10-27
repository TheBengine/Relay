//! Choosing between approved hosts, and when a switch is worth it.
//!
//! The community decides how its host may change: keep the current host, use a
//! named approved host, keep named hosts as backups, or allow automatic
//! switching. Automatic switching never acts on one good ping. A candidate must
//! be clearly better on every required aspect (permission, data copy, voice,
//! capacity) for a sustained period, there is a cooldown after each switch,
//! and nothing switches at all until host handover and takeover exist.
//!
//! Pure rules, no I/O. The server stores the policy; the advice is shown.
use crate::hosting::{Assessment, Verdict};
use serde::{Deserialize, Serialize};

/// How much better (score points out of 100) a candidate must be.
pub const MIN_GAIN: u32 = 20;
/// How long it must stay that much better before a switch.
pub const SUSTAIN_MS: u64 = 10 * 60_000;
/// No new switch within this time after the last one.
pub const COOLDOWN_MS: u64 = 60 * 60_000;
pub const MAX_BACKUPS: usize = 8;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwitchMode {
    /// Keep the current host until an administrator moves the community.
    #[default]
    KeepCurrent,
    /// Move to `preferred` when that is possible.
    UseThisHost,
    /// Switch on its own after a sustained, significant improvement.
    Automatic,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SwitchPolicy {
    #[serde(default)]
    pub mode: SwitchMode,
    /// Member whose machine should host, for `UseThisHost`.
    #[serde(default)]
    pub preferred: Option<String>,
    /// Members whose machines are kept ready as backups, in order.
    #[serde(default)]
    pub backups: Vec<String>,
}

impl SwitchPolicy {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.backups.len() > MAX_BACKUPS {
            return Err("Too many backup hosts");
        }
        let mut seen: Vec<&str> = Vec::new();
        for b in &self.backups {
            if b.is_empty() || seen.contains(&b.as_str()) {
                return Err("Each backup host is listed once");
            }
            seen.push(b);
        }
        match (self.mode, &self.preferred) {
            (SwitchMode::UseThisHost, None) => Err("Choose which host to use"),
            (SwitchMode::UseThisHost, Some(p)) if p.is_empty() => Err("Choose which host to use"),
            _ => Ok(()),
        }
    }
}

/// 0..=100 from an assessment. Any required aspect that is red or unknown
/// disqualifies the candidate: a switch needs everything measured and good.
pub fn score(assessment: &Assessment) -> Option<u32> {
    let mut total = 0u32;
    let mut count = 0u32;
    for d in assessment.dimensions.iter().filter(|d| d.required) {
        total += match d.verdict {
            Verdict::Green => 100,
            Verdict::Yellow => 60,
            Verdict::Red | Verdict::Unknown => return None,
        };
        count += 1;
    }
    (count > 0).then(|| total / count)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Advice {
    Stay { reason: String },
    /// Better, but not for long enough yet.
    Wait { candidate: String, remaining_ms: u64 },
    /// An administrator asked for this host; move when handover allows.
    Move { candidate: String },
    Switch { candidate: String },
}

impl Advice {
    pub fn text(&self, name: impl Fn(&str) -> String) -> String {
        match self {
            Advice::Stay { reason } => reason.clone(),
            Advice::Wait {
                candidate,
                remaining_ms,
            } => format!(
                "{} is clearly better. Switching if that holds for {} more minute(s).",
                name(candidate),
                remaining_ms.div_ceil(60_000)
            ),
            Advice::Move { candidate } => format!("Moving to {} is requested.", name(candidate)),
            Advice::Switch { candidate } => format!("Switching to {}.", name(candidate)),
        }
    }
}

/// Remembers who has been leading and since when. One per community.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Advisor {
    leading: Option<(String, u64)>,
    last_switch_ms: Option<u64>,
}

impl Advisor {
    /// `ready` is true only when this build can move a community (planned
    /// handover and takeover). `current` is the active host's score, `None` when
    /// it is not fully measured. `candidates` are approved hosts with scores.
    pub fn observe(
        &mut self,
        now_ms: u64,
        policy: &SwitchPolicy,
        ready: bool,
        current: Option<u32>,
        candidates: &[(String, Option<u32>)],
    ) -> Advice {
        let stay = |reason: &str| Advice::Stay {
            reason: reason.into(),
        };
        match policy.mode {
            SwitchMode::KeepCurrent => {
                self.leading = None;
                return stay("Keeping the current host.");
            }
            SwitchMode::UseThisHost => {
                self.leading = None;
                let Some(p) = policy.preferred.clone() else {
                    return stay("Keeping the current host.");
                };
                if !ready {
                    return stay("A move is requested, but moving a community is not available in this version yet.");
                }
                return Advice::Move { candidate: p };
            }
            SwitchMode::Automatic => {}
        }
        if !ready {
            self.leading = None;
            return stay("Automatic switching is allowed, but it waits until moving a community is available in this version.");
        }
        if self
            .last_switch_ms
            .is_some_and(|at| now_ms.saturating_sub(at) < COOLDOWN_MS)
        {
            return stay("Recently switched. The host stays for at least an hour.");
        }
        let Some(current) = current else {
            self.leading = None;
            return stay("The current host is not fully measured, so nothing is compared.");
        };
        let best = candidates
            .iter()
            .filter_map(|(id, s)| s.map(|s| (id, s)))
            .filter(|(_, s)| *s >= current.saturating_add(MIN_GAIN))
            .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(a.0)));
        let Some((id, _)) = best else {
            self.leading = None;
            return stay("No approved host is clearly better than the current one.");
        };
        let since = match &self.leading {
            Some((leader, since)) if leader == id => *since,
            _ => {
                self.leading = Some((id.clone(), now_ms));
                now_ms
            }
        };
        let held = now_ms.saturating_sub(since);
        if held < SUSTAIN_MS {
            return Advice::Wait {
                candidate: id.clone(),
                remaining_ms: SUSTAIN_MS - held,
            };
        }
        self.leading = None;
        self.last_switch_ms = Some(now_ms);
        Advice::Switch {
            candidate: id.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hosting::{Aspect, Dimension, Task};

    fn auto() -> SwitchPolicy {
        SwitchPolicy {
            mode: SwitchMode::Automatic,
            ..Default::default()
        }
    }
    fn c(id: &str, s: u32) -> (String, Option<u32>) {
        (id.into(), Some(s))
    }

    #[test]
    fn automatic_switching_needs_a_sustained_significant_gain_and_respects_a_cooldown() {
        let mut a = Advisor::default();
        let p = auto();
        // A small gain is never enough, however long it lasts.
        assert!(matches!(a.observe(0, &p, true, Some(70), &[c("bo", 85)]), Advice::Stay { .. }));
        // A big gain must hold for SUSTAIN_MS.
        assert!(matches!(a.observe(1_000, &p, true, Some(60), &[c("bo", 100)]), Advice::Wait { .. }));
        // A dip resets the clock: one good moment is not enough.
        assert!(matches!(a.observe(300_000, &p, true, Some(60), &[c("bo", 70)]), Advice::Stay { .. }));
        assert!(matches!(a.observe(400_000, &p, true, Some(60), &[c("bo", 100)]), Advice::Wait { .. }));
        assert!(matches!(
            a.observe(400_000 + SUSTAIN_MS - 1, &p, true, Some(60), &[c("bo", 100)]),
            Advice::Wait { remaining_ms: 1, .. }
        ));
        assert_eq!(
            a.observe(400_000 + SUSTAIN_MS, &p, true, Some(60), &[c("bo", 100)]),
            Advice::Switch { candidate: "bo".into() }
        );
        // Cooldown: even a huge gain waits an hour.
        let after = 400_000 + SUSTAIN_MS + 1;
        assert!(matches!(a.observe(after, &p, true, Some(0), &[c("clara", 100)]), Advice::Stay { .. }));
    }

    #[test]
    fn nothing_switches_by_itself_unless_allowed_and_possible() {
        let mut a = Advisor::default();
        let better = [c("bo", 100)];
        let keep = SwitchPolicy::default();
        assert_eq!(keep.mode, SwitchMode::KeepCurrent);
        assert!(matches!(a.observe(SUSTAIN_MS * 9, &keep, true, Some(0), &better), Advice::Stay { .. }));
        // Allowed, but this build cannot move a community yet.
        for t in [0, SUSTAIN_MS * 2] {
            assert!(matches!(a.observe(t, &auto(), false, Some(0), &better), Advice::Stay { .. }));
        }
        // An unmeasured current host or candidate is never compared.
        assert!(matches!(a.observe(0, &auto(), true, None, &better), Advice::Stay { .. }));
        assert!(matches!(
            a.observe(0, &auto(), true, Some(0), &[("bo".into(), None)]),
            Advice::Stay { .. }
        ));
        let use_bo = SwitchPolicy {
            mode: SwitchMode::UseThisHost,
            preferred: Some("bo".into()),
            backups: vec![],
        };
        assert!(matches!(a.observe(0, &use_bo, false, None, &[]), Advice::Stay { .. }));
        assert_eq!(a.observe(0, &use_bo, true, None, &[]), Advice::Move { candidate: "bo".into() });
    }

    #[test]
    fn a_score_needs_every_required_aspect_measured_and_not_red() {
        let dim = |aspect, required, verdict| Dimension {
            aspect,
            required,
            verdict,
            text: String::new(),
        };
        let mut assessment = Assessment {
            task: Task::PlannedHandover,
            dimensions: vec![
                dim(Aspect::Permission, true, Verdict::Green),
                dim(Aspect::VoiceQuality, true, Verdict::Yellow),
                dim(Aspect::Capacity, false, Verdict::Red),
            ],
            overall: Verdict::Yellow,
            summary: String::new(),
        };
        assert_eq!(score(&assessment), Some(80));
        assessment.dimensions[1].verdict = Verdict::Unknown;
        assert_eq!(score(&assessment), None, "best ping alone never qualifies");
    }

    #[test]
    fn policies_are_checked() {
        let mut p = SwitchPolicy {
            mode: SwitchMode::UseThisHost,
            ..Default::default()
        };
        assert!(p.validate().is_err());
        p.preferred = Some("bo".into());
        assert!(p.validate().is_ok());
        p.backups = vec!["clara".into(), "clara".into()];
        assert!(p.validate().is_err());
    }
}
