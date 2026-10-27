//! What video actually arrives on this PC, per stream.
//!
//! The sender chooses a resolution, frame rate and bitrate; the network and the
//! sender's PC decide what is delivered. This file measures only the delivered
//! side, from decoded frames as they reach the app: their size before any
//! scaling for the screen, and how many arrived in the last two seconds. It
//! never shows the sender's choice as if it were what you receive.
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

const SPAN: Duration = Duration::from_secs(2);
/// A stream with no frame for this long has stopped delivering.
const STALLED: Duration = Duration::from_secs(3);
const FORGET: Duration = Duration::from_secs(30);
const MAX_ARRIVALS: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Delivered {
    pub width: u32,
    pub height: u32,
    pub fps: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Delivery {
    /// Fewer than two frames so far; no rate can be stated yet.
    Measuring,
    Arriving(Delivered),
    Stalled { for_secs: u64 },
}

#[derive(Default)]
pub(super) struct DeliveryMeter {
    streams: HashMap<String, VecDeque<(Instant, u32, u32)>>,
}

impl DeliveryMeter {
    /// Record one decoded frame, with its size as decoded (before scaling).
    pub(super) fn record(&mut self, key: &str, size: [usize; 2], now: Instant) {
        let width = u32::try_from(size[0]).unwrap_or(u32::MAX);
        let height = u32::try_from(size[1]).unwrap_or(u32::MAX);
        let arrivals = self.streams.entry(key.to_owned()).or_default();
        arrivals.push_back((now, width, height));
        while arrivals
            .front()
            .is_some_and(|(at, _, _)| now.saturating_duration_since(*at) > SPAN)
            || arrivals.len() > MAX_ARRIVALS
        {
            arrivals.pop_front();
        }
        self.streams.retain(|_, list| {
            list.back()
                .is_some_and(|(at, _, _)| now.saturating_duration_since(*at) <= FORGET)
        });
    }

    pub(super) fn delivery(&self, key: &str, now: Instant) -> Delivery {
        let Some(arrivals) = self.streams.get(key) else {
            return Delivery::Measuring;
        };
        let Some(&(last, width, height)) = arrivals.back() else {
            return Delivery::Measuring;
        };
        let silent = now.saturating_duration_since(last);
        if silent > STALLED {
            return Delivery::Stalled { for_secs: silent.as_secs() };
        }
        let recent: Vec<_> = arrivals
            .iter()
            .filter(|(at, _, _)| now.saturating_duration_since(*at) <= SPAN)
            .collect();
        if recent.len() < 2 {
            return Delivery::Measuring;
        }
        let first = recent[0].0;
        let span = last.saturating_duration_since(first).as_secs_f32();
        if span <= 0.0 {
            return Delivery::Measuring;
        }
        let fps = (recent.len() - 1) as f32 / span;
        Delivery::Arriving(Delivered { width, height, fps })
    }
}

/// The caption under a received stream. Says where it was measured.
pub(super) fn delivered_line(delivery: Delivery) -> String {
    match delivery {
        Delivery::Measuring => "Arriving here: measuring…".into(),
        Delivery::Arriving(d) => format!(
            "Arriving here: {} × {} at {:.0} fps",
            d.width, d.height, d.fps
        ),
        Delivery::Stalled { for_secs } => {
            format!("Arriving here: no new frames for {for_secs} s")
        }
    }
}

pub(super) const DELIVERED_HINT: &str = "Received: measured from frames decoded on this PC. Chosen: the sender's own setting, sent with their voice state. What arrives can be lower: their connection, their PC and yours decide.";

/// The ceiling this PC chose for one source, as sent to the others.
pub(super) fn chosen(options: &crate::capture::CaptureOptions) -> relay_protocol::ChosenVideo {
    use crate::capture::VideoBitrate;
    let (max_width, max_height) = options.quality.dimensions();
    relay_protocol::ChosenVideo {
        max_width,
        max_height,
        fps: options.fps,
        bitrate_kbps: match options.bitrate {
            VideoBitrate::Auto => None,
            VideoBitrate::Mbps2 => Some(2_000),
            VideoBitrate::Mbps4 => Some(4_000),
            VideoBitrate::Mbps8 => Some(8_000),
            VideoBitrate::Mbps12 => Some(12_000),
            VideoBitrate::Mbps20 => Some(20_000),
            VideoBitrate::Mbps32 => Some(32_000),
        },
    }
}

fn chosen_text(chosen: &relay_protocol::ChosenVideo) -> String {
    let rate = match chosen.bitrate_kbps {
        Some(kbps) => format!(" · {} Mbps", kbps as f32 / 1000.0),
        None => String::new(),
    };
    format!(
        "Chosen: up to {} × {} at {} fps{rate}",
        chosen.max_width, chosen.max_height, chosen.fps
    )
}

/// The caption under a received stream: the sender's choice, if their app
/// sent one, next to what arrives here. The two are never merged.
pub(super) fn received_caption(
    chosen: Option<&relay_protocol::ChosenVideo>,
    delivery: Delivery,
) -> String {
    let chosen = chosen.map_or_else(
        || "Chosen: not shared by the sender's Relay".to_owned(),
        chosen_text,
    );
    let received = delivered_line(delivery).replacen("Arriving here", "Received here", 1);
    format!("{chosen} · {received}")
}

/// The sender's own view: what they chose next to what their capture
/// produced. What viewers receive is only known on the viewers' PCs.
pub(super) fn sent_line(
    chosen: (u32, u32, u32),
    captured: Option<(u32, u32, f32)>,
    upload_mbps: Option<f64>,
) -> String {
    let (cw, ch, cfps) = chosen;
    let mut text = format!("Chosen: up to {cw} × {ch} at {cfps} fps");
    match captured {
        Some((w, h, fps)) => text.push_str(&format!(" · Captured: {w} × {h} at {fps:.0} fps")),
        None => text.push_str(" · Captured: waiting for frames"),
    }
    match upload_mbps {
        Some(rate) => text.push_str(&format!(" · Sent: {rate:.1} Mbps measured")),
        None => text.push_str(" · Sent: not measured yet"),
    }
    text.push_str(". Each viewer sees what arrives on their own screen.");
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivered_rate_and_size_come_from_arriving_frames() {
        let start = Instant::now();
        let mut meter = DeliveryMeter::default();
        assert_eq!(meter.delivery("a", start), Delivery::Measuring);
        meter.record("a", [1280, 720], start);
        assert_eq!(meter.delivery("a", start), Delivery::Measuring);
        for i in 1..=30 {
            meter.record("a", [1280, 720], start + Duration::from_millis(i * 33));
        }
        let now = start + Duration::from_millis(30 * 33);
        let Delivery::Arriving(d) = meter.delivery("a", now) else {
            panic!("expected arriving");
        };
        assert_eq!((d.width, d.height), (1280, 720));
        assert!((d.fps - 30.3).abs() < 1.0, "{}", d.fps);
        assert_eq!(delivered_line(Delivery::Arriving(d)), "Arriving here: 1280 × 720 at 30 fps");
    }

    #[test]
    fn a_resolution_drop_shows_the_newest_size() {
        let start = Instant::now();
        let mut meter = DeliveryMeter::default();
        meter.record("a", [1920, 1080], start);
        meter.record("a", [1920, 1080], start + Duration::from_millis(50));
        meter.record("a", [960, 540], start + Duration::from_millis(100));
        let Delivery::Arriving(d) = meter.delivery("a", start + Duration::from_millis(100)) else {
            panic!();
        };
        assert_eq!((d.width, d.height), (960, 540));
    }

    #[test]
    fn a_silent_stream_is_called_stalled_and_old_streams_are_forgotten() {
        let start = Instant::now();
        let mut meter = DeliveryMeter::default();
        meter.record("a", [640, 360], start);
        meter.record("a", [640, 360], start + Duration::from_millis(40));
        let later = start + Duration::from_secs(5);
        assert_eq!(meter.delivery("a", later), Delivery::Stalled { for_secs: 4 });
        assert_eq!(delivered_line(Delivery::Stalled { for_secs: 4 }), "Arriving here: no new frames for 4 s");
        meter.record("b", [640, 360], start + FORGET + Duration::from_secs(1));
        assert_eq!(meter.delivery("a", later), Delivery::Measuring);
        assert!(meter.streams.len() == 1);
    }

    #[test]
    fn the_receiver_sees_chosen_and_received_side_by_side() {
        let chosen = relay_protocol::ChosenVideo { max_width: 1920, max_height: 1080, fps: 60, bitrate_kbps: Some(8_000) };
        let arriving = Delivery::Arriving(Delivered { width: 1280, height: 720, fps: 29.7 });
        assert_eq!(
            received_caption(Some(&chosen), arriving),
            "Chosen: up to 1920 × 1080 at 60 fps · 8 Mbps · Received here: 1280 × 720 at 30 fps"
        );
        let auto = relay_protocol::ChosenVideo { bitrate_kbps: None, ..chosen };
        assert_eq!(
            received_caption(Some(&auto), Delivery::Measuring),
            "Chosen: up to 1920 × 1080 at 60 fps · Received here: measuring…"
        );
        assert!(received_caption(None, arriving).starts_with("Chosen: not shared by the sender's Relay · Received here: 1280"));
    }

    #[test]
    fn the_chosen_value_follows_the_settings_and_passes_the_server_bounds() {
        use crate::capture::{CaptureOptions, CaptureQuality, VideoBitrate};
        for quality in [CaptureQuality::P480, CaptureQuality::P720, CaptureQuality::P1080, CaptureQuality::P1440, CaptureQuality::P2160, CaptureQuality::Source] {
            for bitrate in [VideoBitrate::Auto, VideoBitrate::Mbps2, VideoBitrate::Mbps32] {
                for fps in [15, 24, 30, 60] {
                    let options = CaptureOptions { quality, fps, bitrate, ..CaptureOptions::default() };
                    let value = chosen(&options);
                    assert_eq!((value.max_width, value.max_height), quality.dimensions());
                    assert_eq!(value.fps, fps);
                    assert_eq!(value.checked(), Some(value), "{quality:?} {fps}");
                }
            }
        }
        let options = CaptureOptions { bitrate: VideoBitrate::Mbps12, ..CaptureOptions::default() };
        assert_eq!(chosen(&options).bitrate_kbps, Some(12_000));
    }

    #[test]
    fn the_sender_line_keeps_chosen_captured_and_sent_apart() {
        let line = sent_line((1920, 1080, 60), Some((1280, 720, 29.6)), Some(3.24));
        assert_eq!(line, "Chosen: up to 1920 × 1080 at 60 fps · Captured: 1280 × 720 at 30 fps · Sent: 3.2 Mbps measured. Each viewer sees what arrives on their own screen.");
        let waiting = sent_line((1280, 720, 30), None, None);
        assert!(waiting.contains("waiting for frames") && waiting.contains("not measured yet"));
        assert!(DELIVERED_HINT.contains("decoded on this PC"));
    }
}
