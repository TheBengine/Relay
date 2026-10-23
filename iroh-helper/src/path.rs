//! How a connection currently reaches the other side, for the status line.
//!
//! Only the kind of path and its round trip are reported; the other side's
//! address and the relay's name stay inside the helper.
use iroh::endpoint::Connection;
use serde::Serialize;

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct PathInfo {
    /// `direct`, `relay` or `unknown`.
    pub kind: &'static str,
    pub rtt_ms: f64,
}

pub fn unknown() -> PathInfo {
    PathInfo {
        kind: "unknown",
        rtt_ms: 0.0,
    }
}

pub fn classify(is_relay: bool, is_ip: bool, rtt: Option<std::time::Duration>) -> PathInfo {
    let rtt_ms = rtt
        .map(|d| d.as_secs_f64() * 1000.0)
        .filter(|ms| ms.is_finite() && *ms >= 0.0 && *ms <= 60_000.0)
        .unwrap_or(0.0);
    let kind = if is_ip {
        "direct"
    } else if is_relay {
        "relay"
    } else {
        "unknown"
    };
    PathInfo { kind, rtt_ms }
}

/// The path iroh currently sends on, if it has chosen one.
pub fn of(connection: &Connection) -> PathInfo {
    connection
        .paths()
        .iter()
        .find(|path| path.is_selected())
        .map(|path| classify(path.is_relay(), path.is_ip(), Some(path.rtt())))
        .unwrap_or_else(unknown)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn kinds_and_delays_are_reported_without_addresses() {
        assert_eq!(
            classify(false, true, Some(Duration::from_millis(12))),
            PathInfo {
                kind: "direct",
                rtt_ms: 12.0
            }
        );
        let relay = classify(true, false, Some(Duration::from_micros(48_500)));
        assert_eq!(relay.kind, "relay");
        assert!((relay.rtt_ms - 48.5).abs() < 0.01);
        assert_eq!(classify(false, false, None), unknown());
        assert_eq!(
            classify(false, true, Some(Duration::from_secs(10_000))).rtt_ms,
            0.0,
            "an implausible delay is dropped"
        );
        let json = serde_json::to_string(&classify(false, true, None)).unwrap();
        assert_eq!(json, r#"{"kind":"direct","rtt_ms":0.0}"#);
    }
}
