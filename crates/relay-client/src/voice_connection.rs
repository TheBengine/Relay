//! Display evidence from the active voice transport. HTTP checks and video
//! metrics cannot supply this call's round-trip time.
use crate::{
    connection_path::{Quality, Transport},
    media::NetworkStats,
};
use std::time::Duration;

const MAX_AGE: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    DirectPeer,
    LocalGateway,
    Udp,
    WebSocket,
    Reconnecting,
    Checking,
}

pub struct Status {
    pub route: Route,
    pub rtt_ms: Option<f32>,
    pub age: Option<Duration>,
    /// Incoming voice on this call, whichever route carries it.
    pub voice: Option<crate::voice_quality::Quality>,
}

impl Status {
    pub fn from_stats(stats: &NetworkStats) -> Self {
        let (route, rtt, age) = if stats.gateway_active {
            (Route::LocalGateway, stats.gateway_round_trip_ms, stats.gateway_round_trip_age)
        } else if stats.direct_active && stats.udp_reachable {
            (
                Route::DirectPeer,
                stats.direct_round_trip_ms,
                stats.direct_round_trip_age,
            )
        } else if stats.tcp_active && !stats.udp_reachable {
            (
                Route::WebSocket,
                stats.tcp_round_trip_ms,
                stats.tcp_round_trip_age,
            )
        } else if stats.udp_reachable {
            (
                Route::Udp,
                stats.udp_round_trip_ms,
                stats.udp_round_trip_age,
            )
        } else if stats.udp_probe_timed_out {
            (Route::Reconnecting, None, None)
        } else {
            (Route::Checking, None, None)
        };
        Self {
            route,
            rtt_ms: rtt
                .filter(|ms| ms.is_finite() && *ms > 0.0 && age.is_some_and(|age| age < MAX_AGE)),
            age,
            voice: stats
                .voice_quality
                .filter(|_| !matches!(route, Route::Reconnecting | Route::Checking)),
        }
    }

    #[cfg(test)]
    pub fn headline(&self) -> String {
        self.headline_for(Transport::Direct)
    }

    pub fn headline_for(&self, transport: Transport) -> String {
        let name = match self.route {
            Route::DirectPeer => "Voice · direct peer",
            Route::LocalGateway => "Voice · local connection to the voice gateway",
            Route::Udp => match transport {
                Transport::Iroh => "Voice · iroh",
                Transport::Tailscale => "Voice · Tailscale",
                Transport::Direct => "Voice · UDP",
            },
            Route::WebSocket => "Voice · WebSocket",
            Route::Reconnecting => return "Voice · reconnecting…".into(),
            Route::Checking => return "Voice · checking…".into(),
        };
        let mut text = match self.rtt_ms {
            Some(ms) if self.route == Route::LocalGateway => {
                format!("{name} · {ms:.0} ms RTT to the voice server")
            }
            Some(ms) => format!("{name} · {ms:.0} ms RTT"),
            None if self.age.is_some_and(|age| age >= MAX_AGE) => {
                format!("{name} · RTT out of date")
            }
            None => format!("{name} · RTT unavailable"),
        };
        if let Some(voice) = self.voice {
            // Name the figure that matters most, so a downgraded colour is explained.
            text.push_str(&if voice.loss_percent <= 1.0 && voice.late_percent > 1.0 {
                format!(" · {:.1} % late", voice.late_percent)
            } else if voice.loss_percent <= 1.0 && voice.jitter_ms > 20.0 {
                format!(" · jitter {:.0} ms", voice.jitter_ms)
            } else {
                format!(" · {:.1} % loss", voice.loss_percent)
            });
        }
        text
    }

    /// The worse of route RTT and incoming loss, lateness and jitter.
    pub fn quality(&self) -> Quality {
        let rtt = match self.rtt_ms {
            Some(ms) if ms <= 60.0 => Quality::Good,
            Some(ms) if ms <= 120.0 => Quality::Fair,
            Some(_) => Quality::Poor,
            // Without a fresh RTT, incoming voice alone can still show trouble.
            None if self.voice.is_some() => Quality::Good,
            None => return Quality::Unknown,
        };
        let voice = match self.voice {
            Some(v) if v.loss_percent > 5.0 || v.late_percent > 5.0 || v.jitter_ms > 40.0 => {
                Quality::Poor
            }
            Some(v) if v.loss_percent > 1.0 || v.late_percent > 1.0 || v.jitter_ms > 20.0 => {
                Quality::Fair
            }
            _ => Quality::Good,
        };
        let rank = |quality: Quality| match quality {
            Quality::Good => 0,
            Quality::Fair => 1,
            _ => 2,
        };
        if rank(voice) > rank(rtt) {
            voice
        } else {
            rtt
        }
    }

    pub fn explanation(&self, stats: &NetworkStats) -> String {
        let mut text = match self.route {
            Route::DirectPeer => "Measured to the other person's voice endpoint.",
            Route::LocalGateway => "Your voice goes over the local network to your approved voice gateway, which forwards it to the voice server. The time is measured through the gateway to the voice server, not to the other person, and is not mouth-to-ear delay. Video uses its own route.",
            Route::Udp | Route::WebSocket => {
                "Measured to the voice server, not all the way to the other person."
            }
            Route::Checking => "Waiting for an authenticated reply on the voice connection.",
            Route::Reconnecting => {
                "Keep Relay open while voice reconnects. If recovery ends, join the room again."
            }
        }
        .to_owned();
        if let Some(age) = self.age {
            text.push_str(&format!(
                " Last voice sample: {:.1} s ago.",
                age.as_secs_f32()
            ));
        }
        text.push('\n');
        text.push_str(&match self.voice {
            Some(voice) => format!(
                "Incoming voice, last 5–10 s, worst figure across speakers: {:.1} % lost · jitter {:.0} ms · {:.1} % arrived too late to play.",
                voice.loss_percent, voice.jitter_ms, voice.late_percent,
            ),
            None => "Incoming voice loss and jitter: not measured yet. Someone else needs to speak for about a second.".into(),
        });
        if let Some(target) = stats.receive_target_ms {
            text.push_str(&format!(" Receive buffer target: {target} ms."));
        }
        text.push_str(&format!(
            "\nSince joining: {} late · {} concealed audio frames. Loss is counted from missing packet numbers; a pause or a whisper is not loss.\nRTT excludes microphone, codec and speaker delay. Video uses a separate connection.",
            stats.late_voice_frames, stats.concealed_voice_frames,
        ));
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_local_gateway_says_which_hop_is_local_and_what_was_measured() {
        let stats = NetworkStats {
            gateway_active: true,
            gateway_round_trip_ms: Some(23.0),
            gateway_round_trip_age: Some(Duration::from_millis(200)),
            ..Default::default()
        };
        let status = Status::from_stats(&stats);
        assert_eq!(status.route, Route::LocalGateway);
        assert_eq!(
            status.headline(),
            "Voice · local connection to the voice gateway · 23 ms RTT to the voice server"
        );
        let text = status.explanation(&stats);
        assert!(text.contains("local network") && text.contains("not to the other person"));
    }

    #[test]
    fn fallback_uses_its_own_authenticated_sample_instead_of_old_udp_rtt() {
        let stats = NetworkStats {
            tcp_active: true,
            udp_round_trip_ms: Some(7.0),
            udp_round_trip_age: Some(Duration::from_secs(20)),
            tcp_round_trip_ms: Some(85.0),
            tcp_round_trip_age: Some(Duration::from_secs(1)),
            ..Default::default()
        };
        let status = Status::from_stats(&stats);
        assert_eq!(status.route, Route::WebSocket);
        assert_eq!(status.rtt_ms, Some(85.0));
        assert!(status.headline().contains("85 ms RTT"));
    }

    #[test]
    fn stale_or_unmeasured_values_cannot_look_like_low_voice_latency() {
        for age in [None, Some(MAX_AGE), Some(Duration::from_secs(60))] {
            let stats = NetworkStats {
                udp_reachable: true,
                udp_round_trip_ms: Some(2.0),
                udp_round_trip_age: age,
                ..Default::default()
            };
            let status = Status::from_stats(&stats);
            assert_eq!(status.rtt_ms, None);
            assert_eq!(status.quality(), Quality::Unknown);
        }
    }

    #[test]
    fn incoming_loss_jitter_and_lateness_are_shown_and_lower_the_quality() {
        use crate::voice_quality::Quality as Voice;
        let mut stats = NetworkStats {
            udp_reachable: true,
            udp_round_trip_ms: Some(20.0),
            udp_round_trip_age: Some(Duration::from_millis(500)),
            receive_target_ms: Some(30),
            ..Default::default()
        };
        let status = Status::from_stats(&stats);
        assert_eq!(status.quality(), Quality::Good);
        assert!(status.explanation(&stats).contains("not measured yet"));
        assert!(!status.headline().contains("loss"));
        stats.voice_quality = Some(Voice { loss_percent: 2.5, late_percent: 0.4, jitter_ms: 6.0, expected: 0 });
        let status = Status::from_stats(&stats);
        assert_eq!(status.headline(), "Voice · UDP · 20 ms RTT · 2.5 % loss");
        assert_eq!(status.quality(), Quality::Fair, "a good RTT cannot hide 2.5 % loss");
        let text = status.explanation(&stats);
        assert!(text.contains("2.5 % lost · jitter 6 ms · 0.4 % arrived too late"), "{text}");
        assert!(text.contains("Receive buffer target: 30 ms."));
        stats.voice_quality = Some(Voice { loss_percent: 0.0, late_percent: 0.0, jitter_ms: 55.0, expected: 0 });
        let status = Status::from_stats(&stats);
        assert_eq!(status.quality(), Quality::Poor);
        assert!(status.headline().ends_with("jitter 55 ms"), "the headline names the cause");
        // Heavy loss shows even when the RTT sample is stale.
        stats.udp_round_trip_age = Some(Duration::from_secs(30));
        stats.voice_quality = Some(Voice { loss_percent: 30.0, late_percent: 0.0, jitter_ms: 2.0, expected: 0 });
        assert_eq!(Status::from_stats(&stats).quality(), Quality::Poor);
        // While the route is reconnecting, old loss figures are not shown as current.
        stats.udp_reachable = false;
        stats.udp_probe_timed_out = true;
        assert_eq!(Status::from_stats(&stats).voice, None);
    }

    #[test]
    fn direct_route_uses_peer_sample_and_cannot_survive_relay_recovery() {
        let mut stats = NetworkStats {
            direct_active: true,
            udp_reachable: true,
            direct_round_trip_ms: Some(8.0),
            direct_round_trip_age: Some(Duration::from_millis(40)),
            udp_round_trip_ms: Some(50.0),
            udp_round_trip_age: Some(Duration::from_secs(1)),
            ..Default::default()
        };
        assert_eq!(Status::from_stats(&stats).rtt_ms, Some(8.0));
        stats.udp_reachable = false;
        stats.udp_probe_timed_out = true;
        let status = Status::from_stats(&stats);
        assert_eq!(status.route, Route::Reconnecting);
        assert_eq!(status.rtt_ms, None);
    }
}
