//! Picking the most capable machine by measurement, and keeping it.
//!
//! One rule serves two choices: which machine is the best community host, and
//! which approved gateway serves the people on one LAN. A machine is scored
//! only from what was measured on it (upload, free processor and, for a
//! gateway, its round trip to the host). The leader is `best`: the highest
//! score, where scores within `MARGIN` points count as equal and the machine
//! that has stood longest wins (the running host, the gateway already in
//! use), then the smaller id, so everyone with the same list agrees. The
//! current choice stays until another machine has led for `SUSTAIN_MS`
//! without a break. After a change nothing changes again for `COOLDOWN_MS`.
//! Nothing is compared while the current choice itself is not measured, and
//! an unmeasured machine never displaces another or wins a tie.
//!
//! Pure rules, no I/O. Times are milliseconds on any monotonic clock.
use serde::{Deserialize, Serialize};

/// How long a challenger must stay clearly better before a change.
pub const SUSTAIN_MS: u64 = 60_000;
/// How many points (out of 100) a challenger must lead by.
pub const MARGIN: u32 = 15;
/// No second change within this time after one.
pub const COOLDOWN_MS: u64 = 5 * 60_000;

/// What was measured on one machine. Every field is optional: a missing one
/// is "not measured", never a guess.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capability {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upload_kbps: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_free_pct: Option<u8>,
    /// Round trip from this machine to the community host. Only a gateway has
    /// one; the host itself does not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_rtt_ms: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Score {
    /// 0..=100.
    pub points: u32,
    /// The measurements behind it, in plain words.
    pub reasons: String,
}

/// "40 Mbit/s" or "800 kbit/s".
pub fn rate(kbps: u32) -> String {
    if kbps >= 10_000 {
        format!("{} Mbit/s", kbps / 1000)
    } else if kbps >= 1000 {
        format!("{:.1} Mbit/s", f64::from(kbps) / 1000.0)
    } else {
        format!("{kbps} kbit/s")
    }
}

/// Scores a machine for carrying the voice of `participants` people (zero:
/// the default group size). `None` unless upload and processor were both
/// measured and readable.
pub fn score(c: &Capability, participants: u32) -> Option<Score> {
    let up = c.upload_kbps.filter(|u| *u > 0)?;
    let cpu = c.cpu_free_pct.filter(|p| *p <= 100)?;
    let people = if participants == 0 {
        crate::hosting::DEFAULT_PARTICIPANTS
    } else {
        participants
    };
    let need = crate::hosting::voice_upload_need_kbps(people).max(1);
    // Twice the estimated need is full marks; the need itself is half.
    let up_points = (u64::from(up) * 50 / u64::from(need)).min(100) as u32;
    let cpu_points = u32::from(cpu);
    let mut reasons = vec![
        format!(
            "upload {} ({:.1}x what {people} people need)",
            rate(up),
            f64::from(up) / f64::from(need)
        ),
        format!("processor {cpu}% free"),
    ];
    let points = match c.host_rtt_ms {
        Some(rtt) => {
            let rtt_points = 100u32.saturating_sub(rtt.saturating_sub(5)).min(100);
            reasons.push(format!("{rtt} ms to the host"));
            (up_points * 5 + cpu_points * 3 + rtt_points * 2) / 10
        }
        None => (up_points * 6 + cpu_points * 4) / 10,
    };
    Some(Score {
        points,
        reasons: reasons.join(", "),
    })
}

/// One machine that could be chosen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Contender {
    pub id: String,
    pub points: Option<u32>,
    /// How long it has been available, for ties: the longer-standing one wins,
    /// so people who arrive later pick what earlier ones already use.
    pub standing_ms: u64,
}

impl Contender {
    pub fn new(id: impl Into<String>, points: Option<u32>, standing_ms: u64) -> Self {
        Self {
            id: id.into(),
            points,
            standing_ms,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Nobody to choose from.
    Nobody,
    /// The first choice, or a replacement for one that is gone. Immediate:
    /// there is nothing to keep.
    Pick { to: String },
    Keep { current: String, why: Keep },
    /// A challenger leads, but not for long enough yet.
    Wait {
        current: String,
        challenger: String,
        held_ms: u64,
        lead: Lead,
    },
    Change {
        from: String,
        to: String,
        lead: Lead,
    },
}

/// Why a challenger leads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lead {
    /// Its score is `MARGIN` points or more above the current one's.
    ClearlyBetter,
    /// About equal, and it has stood longer (or has the smaller id).
    StoodLonger,
}

impl Lead {
    fn words(self) -> &'static str {
        match self {
            Lead::ClearlyBetter => "clearly better by measurement than",
            Lead::StoodLonger => "about as good as, and longer in use than,",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Keep {
    /// No other machine leads by `MARGIN` points on measurements.
    NoneClearlyBetter,
    /// A change happened less than `COOLDOWN_MS` ago.
    Cooldown,
    /// The current choice is not fully measured, so nothing is compared.
    NotMeasured,
}

impl Decision {
    /// The machine chosen after this decision.
    pub fn chosen(&self) -> Option<&str> {
        match self {
            Decision::Nobody => None,
            Decision::Pick { to } | Decision::Change { to, .. } => Some(to),
            Decision::Keep { current, .. } | Decision::Wait { current, .. } => Some(current),
        }
    }

    /// Plain words, with `name` turning an id into a person's or machine's name.
    pub fn text(&self, name: impl Fn(&str) -> String) -> String {
        match self {
            Decision::Nobody => "No machine is available.".into(),
            Decision::Pick { to } => format!("{} is the best measured choice.", name(to)),
            Decision::Keep {
                current,
                why: Keep::NoneClearlyBetter,
            } => format!(
                "{} stays: no other machine is clearly better by measurement.",
                name(current)
            ),
            Decision::Keep {
                current,
                why: Keep::NotMeasured,
            } => format!(
                "{} stays: it is not fully measured yet, so nothing is compared.",
                name(current)
            ),
            Decision::Keep {
                current,
                why: Keep::Cooldown,
            } => format!(
                "{} stays: it was chosen less than {} minutes ago.",
                name(current),
                COOLDOWN_MS / 60_000
            ),
            Decision::Wait {
                current,
                challenger,
                held_ms,
                lead,
            } => format!(
                "{} has been {} {} for {} s. It is preferred once that holds for {} s without a break.",
                name(challenger),
                lead.words(),
                name(current),
                held_ms / 1000,
                SUSTAIN_MS / 1000
            ),
            Decision::Change { from, to, lead } => format!(
                "{} is now preferred: it was {} {} for {} s.",
                name(to),
                lead.words(),
                name(from),
                SUSTAIN_MS / 1000
            ),
        }
    }
}

/// The best contender: measured before unmeasured, then points within
/// `MARGIN` of the best count as equal and the longer-standing one wins, then
/// the smaller id. Everyone with the same list picks the same one.
pub fn best(contenders: &[Contender]) -> Option<&Contender> {
    let top = contenders.iter().filter_map(|c| c.points).max();
    contenders
        .iter()
        .filter(|c| match (top, c.points) {
            (Some(top), Some(p)) => p.saturating_add(MARGIN) > top,
            (Some(_), None) => false,
            (None, _) => true,
        })
        .max_by(|a, b| {
            a.standing_ms
                .cmp(&b.standing_ms)
                .then_with(|| b.id.cmp(&a.id))
        })
}

/// Remembers the current choice and a challenger with the time it took the lead.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Preference {
    current: Option<String>,
    challenger: Option<(String, u64)>,
    changed_ms: Option<u64>,
}

impl Preference {
    /// Starts from a choice that is already in use, such as the running host.
    pub fn starting_with(current: impl Into<String>) -> Self {
        Self {
            current: Some(current.into()),
            ..Default::default()
        }
    }

    pub fn current(&self) -> Option<&str> {
        self.current.as_deref()
    }

    pub fn observe(&mut self, now_ms: u64, contenders: &[Contender]) -> Decision {
        let present = self
            .current
            .as_ref()
            .and_then(|id| contenders.iter().find(|c| &c.id == id));
        let Some(current) = present.cloned() else {
            // First choice, or the current one is gone.
            self.challenger = None;
            let Some(pick) = best(contenders) else {
                self.current = None;
                return Decision::Nobody;
            };
            self.current = Some(pick.id.clone());
            self.changed_ms = Some(now_ms);
            return Decision::Pick {
                to: pick.id.clone(),
            };
        };
        let Some(points) = current.points else {
            self.challenger = None;
            return Decision::Keep {
                current: current.id,
                why: Keep::NotMeasured,
            };
        };
        let leader = best(contenders).filter(|leader| leader.id != current.id);
        let Some(leader) = leader else {
            self.challenger = None;
            return Decision::Keep {
                current: current.id,
                why: Keep::NoneClearlyBetter,
            };
        };
        if self
            .changed_ms
            .is_some_and(|at| now_ms.saturating_sub(at) < COOLDOWN_MS)
        {
            self.challenger = None;
            return Decision::Keep {
                current: current.id,
                why: Keep::Cooldown,
            };
        }
        let since = match &self.challenger {
            Some((id, since)) if *id == leader.id => *since,
            _ => {
                self.challenger = Some((leader.id.clone(), now_ms));
                now_ms
            }
        };
        let held_ms = now_ms.saturating_sub(since);
        let lead = if leader
            .points
            .is_some_and(|p| p >= points.saturating_add(MARGIN))
        {
            Lead::ClearlyBetter
        } else {
            Lead::StoodLonger
        };
        if held_ms < SUSTAIN_MS {
            return Decision::Wait {
                current: current.id,
                challenger: leader.id.clone(),
                held_ms,
                lead,
            };
        }
        self.challenger = None;
        self.current = Some(leader.id.clone());
        self.changed_ms = Some(now_ms);
        Decision::Change {
            from: current.id,
            to: leader.id.clone(),
            lead,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// "host" has stood longest, as the running host does on a server.
    fn c(id: &str, points: Option<u32>) -> Contender {
        Contender::new(id, points, if id == "host" { u64::MAX } else { 0 })
    }

    #[test]
    fn a_score_needs_upload_and_processor_and_rewards_headroom() {
        assert!(score(&Capability::default(), 4).is_none());
        let only_up = Capability {
            upload_kbps: Some(20_000),
            ..Default::default()
        };
        assert!(score(&only_up, 4).is_none(), "processor not measured");
        let strong = Capability {
            upload_kbps: Some(20_000),
            cpu_free_pct: Some(80),
            host_rtt_ms: None,
        };
        let weak = Capability {
            upload_kbps: Some(800),
            cpu_free_pct: Some(20),
            host_rtt_ms: None,
        };
        let s = score(&strong, 4).unwrap();
        let w = score(&weak, 4).unwrap();
        assert!(s.points >= w.points + MARGIN, "{s:?} vs {w:?}");
        assert!(s.reasons.contains("20 Mbit/s"), "{}", s.reasons);
        assert!(s.reasons.contains("processor 80% free"));
        // A far gateway loses points against a near one with the same machine.
        let near = score(&Capability { host_rtt_ms: Some(5), ..strong }, 4).unwrap();
        let far = score(&Capability { host_rtt_ms: Some(90), ..strong }, 4).unwrap();
        assert!(near.points > far.points);
        assert!(far.reasons.contains("90 ms to the host"));
        assert!(score(&Capability { cpu_free_pct: Some(101), ..strong }, 4).is_none());
    }

    #[test]
    fn the_first_pick_is_immediate_and_everyone_with_the_same_list_agrees() {
        let list = [c("bo", Some(70)), c("ada", Some(80)), c("cy", None)];
        let mut p = Preference::default();
        assert_eq!(p.observe(0, &list), Decision::Pick { to: "ada".into() });
        // Within the margin the longer-standing one wins, so later arrivals
        // pick what is already in use, whatever their own order of the list.
        let standing = [
            Contender::new("ada", Some(80), 1_000),
            Contender::new("bo", Some(70), 90_000),
        ];
        assert_eq!(best(&standing).unwrap().id, "bo");
        let mut reversed = standing.clone();
        reversed.reverse();
        assert_eq!(best(&reversed).unwrap().id, "bo");
        // Nothing measured: the longest-standing, then the smallest id.
        assert_eq!(best(&[c("b", None), c("a", None)]).unwrap().id, "a");
        // A measured machine always wins over an unmeasured one.
        assert_eq!(
            best(&[Contender::new("old", None, 9_999_999), c("new", Some(1))])
                .unwrap()
                .id,
            "new"
        );
        assert_eq!(Preference::default().observe(0, &[]), Decision::Nobody);
    }

    #[test]
    fn a_change_needs_a_clear_lead_held_for_sixty_seconds() {
        let mut p = Preference::starting_with("host");
        // A small lead never changes anything, however long it lasts.
        for t in [0, SUSTAIN_MS, 10 * SUSTAIN_MS] {
            let d = p.observe(t, &[c("host", Some(60)), c("bo", Some(74))]);
            assert!(matches!(d, Decision::Keep { why: Keep::NoneClearlyBetter, .. }), "{d:?}");
        }
        // A clear lead must hold for SUSTAIN_MS.
        let better = [c("host", Some(60)), c("bo", Some(80))];
        assert!(matches!(p.observe(1_000_000, &better), Decision::Wait { held_ms: 0, .. }));
        assert!(matches!(
            p.observe(1_000_000 + SUSTAIN_MS - 1, &better),
            Decision::Wait { .. }
        ));
        assert_eq!(
            p.observe(1_000_000 + SUSTAIN_MS, &better),
            Decision::Change { from: "host".into(), to: "bo".into(), lead: Lead::ClearlyBetter }
        );
        assert_eq!(p.current(), Some("bo"));
    }

    #[test]
    fn flapping_measurements_never_cause_a_change() {
        let mut p = Preference::starting_with("host");
        // Bo is clearly better for 50 s, dips for one sample, and so on: the
        // clock restarts on every dip.
        let mut t = 0;
        for _ in 0..30 {
            for _ in 0..5 {
                let d = p.observe(t, &[c("host", Some(50)), c("bo", Some(90))]);
                assert!(!matches!(d, Decision::Change { .. }), "{t}: {d:?}");
                t += 10_000;
            }
            p.observe(t, &[c("host", Some(50)), c("bo", Some(55))]);
            t += 10_000;
        }
        assert_eq!(p.current(), Some("host"));
        // Two machines trading the lead back and forth: the margin and the
        // cooldown keep the choice still.
        let mut p = Preference::starting_with("a");
        let mut changes = 0;
        for step in 0..200u64 {
            let (a, b) = if step % 12 < 6 { (90, 60) } else { (60, 90) };
            if let Decision::Change { .. } = p.observe(step * 10_000, &[c("a", Some(a)), c("b", Some(b))]) {
                changes += 1;
            }
        }
        assert!(changes <= 4, "{changes} changes in 2000 s");
    }

    #[test]
    fn after_a_change_the_choice_rests_and_unmeasured_machines_never_displace() {
        let mut p = Preference::starting_with("host");
        p.observe(0, &[c("host", Some(40)), c("bo", Some(90))]);
        assert!(matches!(
            p.observe(SUSTAIN_MS, &[c("host", Some(40)), c("bo", Some(90))]),
            Decision::Change { .. }
        ));
        let later = SUSTAIN_MS + 1;
        assert!(matches!(
            p.observe(later, &[c("host", Some(100)), c("bo", Some(10))]),
            Decision::Keep { why: Keep::Cooldown, .. }
        ));
        // Unmeasured challengers never displace, even a measured-poor current.
        let mut p = Preference::starting_with("host");
        for t in [0, SUSTAIN_MS * 3] {
            assert!(matches!(
                p.observe(t, &[c("host", Some(1)), c("bo", None)]),
                Decision::Keep { .. }
            ));
        }
        // A current choice that is not measured is never compared at all.
        let mut p = Preference::starting_with("host");
        for t in [0, SUSTAIN_MS * 3] {
            assert!(matches!(
                p.observe(t, &[c("host", None), c("bo", Some(99))]),
                Decision::Keep { why: Keep::NotMeasured, .. }
            ));
        }
        // The current one disappears: the best remaining is picked at once.
        assert_eq!(
            p.observe(SUSTAIN_MS * 4, &[c("bo", None), c("cy", Some(30))]),
            Decision::Pick { to: "cy".into() }
        );
    }

    #[test]
    fn a_lan_settles_on_one_gateway_and_late_arrivals_join_it() {
        // Gus has served for ten minutes, Ada just started, both about equal.
        let lan = [
            Contender::new("gus", Some(70), 600_000),
            Contender::new("ada", Some(78), 0),
        ];
        for order in [lan.clone(), [lan[1].clone(), lan[0].clone()]] {
            let mut newcomer = Preference::default();
            assert_eq!(newcomer.observe(0, &order), Decision::Pick { to: "gus".into() });
        }
        // Ada's own app sees the same list, so it knows it is not the LAN's
        // gateway and steps back; Gus's app knows it is.
        assert_eq!(best(&lan).unwrap().id, "gus");
        // Someone who had picked Ada (before Gus was listed) moves to Gus after
        // the lead has held, and says why.
        let mut early = Preference::starting_with("ada");
        let d = early.observe(0, &lan);
        assert!(matches!(d, Decision::Wait { lead: Lead::StoodLonger, .. }), "{d:?}");
        assert!(d.text(|id| id.to_string()).contains("about as good as, and longer in use than,"));
        assert!(matches!(early.observe(SUSTAIN_MS, &lan), Decision::Change { .. }));
        assert_eq!(early.current(), Some("gus"));
        // A clearly better newcomer takes over after the sustain, everywhere.
        let strong = [
            Contender::new("gus", Some(40), 600_000),
            Contender::new("ada", Some(90), 0),
        ];
        assert_eq!(best(&strong).unwrap().id, "ada");
    }

    #[test]
    fn decisions_read_as_plain_english() {
        let name = |id: &str| format!("{id}'s PC");
        let wait = Decision::Wait {
            current: "host".into(),
            challenger: "bo".into(),
            held_ms: 20_500,
            lead: Lead::ClearlyBetter,
        };
        assert_eq!(
            wait.text(name),
            "bo's PC has been clearly better by measurement than host's PC for 20 s. It is preferred once that holds for 60 s without a break."
        );
        assert_eq!(rate(800), "800 kbit/s");
        assert_eq!(rate(1500), "1.5 Mbit/s");
        assert_eq!(rate(40_000), "40 Mbit/s");
    }
}
