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
        }
    }

    #[cfg(test)]
    pub fn headline(&self) -> String {
        self.headline_for(Transport::Direct)
    }

    pub fn headline_for(&self, transport: Transport) -> String {
        let name = match self.route {
            Route::DirectPeer => "Voice · direct peer",
            Route::LocalGateway => "Voice · local gateway",
            Route::Udp => match transport {
                Transport::Iroh => "Voice · iroh",
                Transport::Tailscale => "Voice · Tailscale",
                Transport::Direct => "Voice · UDP",
            },
            Route::WebSocket => "Voice · WebSocket",
            Route::Reconnecting => return "Voice · reconnecting…".into(),
            Route::Checking => return "Voice · checking…".into(),
        };
        match self.rtt_ms {
            Some(ms) => format!("{name} · {ms:.0} ms RTT"),
            None if self.age.is_some_and(|age| age >= MAX_AGE) => {
                format!("{name} · RTT out of date")
            }
            None => format!("{name} · RTT unavailable"),
        }
    }

    pub fn quality(&self) -> Quality {
        match self.rtt_ms {
            Some(ms) if ms <= 60.0 => Quality::Good,
            Some(ms) if ms <= 120.0 => Quality::Fair,
            Some(_) => Quality::Poor,
            None => Quality::Unknown,
        }
    }

    pub fn explanation(&self, stats: &NetworkStats) -> String {
        let mut text = match self.route {
            Route::DirectPeer => "Measured to the other person's voice endpoint.",
            Route::LocalGateway => "Measured through your approved local gateway to the voice server. This is not mouth-to-ear delay; video uses its own route.",
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
        text.push_str(&format!(
            "\nLate audio frames: {} · Concealed frames: {}. Packet loss is not measured separately.\nRTT excludes microphone, codec and speaker delay. Video uses a separate connection.",
            stats.late_voice_frames, stats.concealed_voice_frames,
        ));
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
