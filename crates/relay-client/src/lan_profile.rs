//! LAN profile: a low-latency voice preset for when everyone is on the same
//! local network (a LAN party).
//!
//! Turning it on saves the exact voice values it changes and sets shorter
//! packets, a fixed minimal receive buffer, the highest voice bitrate and
//! direct voice between PCs. Turning it off puts the saved values back. It
//! does not change devices, noise filtering, push-to-talk, video or which
//! server you use. On Wi-Fi or over the internet these values cause more
//! dropouts, which the description says plainly.
use super::*;
use crate::audio_settings::AudioSettings;

pub(super) const PACKET_MS: u32 = 5;
pub(super) const JITTER_MS: u32 = 5;
pub(super) const BITRATE_KBPS: u32 = 128;

pub(super) const TITLE: &str = "LAN profile";
pub(super) const SUMMARY: &str = "For a LAN party where everyone is on the same wired local network. Sends voice in 5 ms packets with a fixed 5 ms receive buffer, at 128 kbps, directly between PCs. On Wi-Fi or over the internet this causes more dropouts, so turn it off when you leave.";
pub(super) const DIRECT_NOTE: &str = "Direct voice lets the other PCs in the call see this PC's local network address.";
pub(super) const OFF_NOTE: &str = "Turning it off restores your previous voice settings exactly.";

/// The voice values the profile changes, as they were before it was turned on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Saved {
    pub packet_ms: u32,
    pub ultra_short_packets: bool,
    pub jitter_ms: u32,
    pub adaptive_jitter: bool,
    pub bitrate_kbps: u32,
    pub recover_packet_loss: bool,
    pub direct_voice_enabled: bool,
    pub low_latency_device: bool,
    pub voice_qos: bool,
}

impl Saved {
    fn from(settings: &AudioSettings) -> Self {
        Self {
            packet_ms: settings.packet_ms,
            ultra_short_packets: settings.ultra_short_packets,
            jitter_ms: settings.jitter_ms,
            adaptive_jitter: settings.adaptive_jitter,
            bitrate_kbps: settings.bitrate_kbps,
            recover_packet_loss: settings.recover_packet_loss,
            direct_voice_enabled: settings.direct_voice_enabled,
            low_latency_device: settings.low_latency_device,
            voice_qos: settings.voice_qos,
        }
    }

    fn restore(&self, settings: &mut AudioSettings) {
        settings.packet_ms = self.packet_ms;
        settings.ultra_short_packets = self.ultra_short_packets;
        settings.jitter_ms = self.jitter_ms;
        settings.adaptive_jitter = self.adaptive_jitter;
        settings.bitrate_kbps = self.bitrate_kbps;
        settings.recover_packet_loss = self.recover_packet_loss;
        settings.direct_voice_enabled = self.direct_voice_enabled;
        settings.low_latency_device = self.low_latency_device;
        settings.voice_qos = self.voice_qos;
    }
}

fn set_lan_values(settings: &mut AudioSettings) {
    settings.packet_ms = PACKET_MS;
    settings.ultra_short_packets = false;
    settings.adaptive_jitter = false;
    settings.jitter_ms = JITTER_MS;
    settings.bitrate_kbps = BITRATE_KBPS;
    // Redundancy is inactive below 10 ms packets; say so by turning it off.
    settings.recover_packet_loss = false;
    settings.direct_voice_enabled = true;
    settings.low_latency_device = true;
    settings.voice_qos = true;
}

/// True when the current values are exactly the profile's.
pub(super) fn matches(settings: &AudioSettings) -> bool {
    let mut lan = settings.clone();
    set_lan_values(&mut lan);
    Saved::from(&lan) == Saved::from(settings)
}

/// Turn the profile on: remember the old values once, then set the LAN ones.
/// Turning it on again keeps the first saved values.
pub(super) fn turn_on(settings: &mut AudioSettings, saved: &mut Option<Saved>) {
    if saved.is_none() {
        *saved = Some(Saved::from(settings));
    }
    set_lan_values(settings);
}

/// Turn it off: restore what was saved. Without saved values nothing changes.
pub(super) fn turn_off(settings: &mut AudioSettings, saved: &mut Option<Saved>) -> bool {
    match saved.take() {
        Some(previous) => {
            previous.restore(settings);
            true
        }
        None => false,
    }
}

/// The profile counts as on only while saved values exist and the voice
/// values still match it. A later manual change ends it without a restore.
pub(super) fn is_on(settings: &AudioSettings, saved: &Option<Saved>) -> bool {
    saved.is_some() && matches(settings)
}

/// A hint that the profile may fit: the community's address is on this
/// network. It never turns anything on by itself.
/// A server on this PC itself is not a LAN party, so loopback never counts.
pub(super) fn suggest(server_url: &str) -> bool {
    let Some(host) = url::Url::parse(server_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
    else {
        return false;
    };
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    let loopback = bare.eq_ignore_ascii_case("localhost")
        || bare.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback());
    !loopback && crate::connection_path::is_same_network(&host)
}

impl RelayApp {
    pub(super) fn set_lan_profile(&mut self, on: bool) {
        let mut settings = self.prefs.audio.clone();
        if on {
            turn_on(&mut settings, &mut self.prefs.lan_profile);
        } else if !turn_off(&mut settings, &mut self.prefs.lan_profile) {
            return;
        }
        self.prefs.audio = settings;
        self.apply_audio_settings();
        self.notice = Some((
            if on { "LAN profile on" } else { "LAN profile off; previous voice settings restored" }.into(),
            Instant::now(),
        ));
    }

    pub(super) fn lan_profile_on(&self) -> bool {
        is_on(&self.prefs.audio, &self.prefs.lan_profile)
    }

    /// Settings section: one switch, what it changes, and when it fits.
    pub(super) fn lan_profile_section(&mut self, ui: &mut egui::Ui) {
        crate::settings_ui::section_heading(ui, TITLE, SUMMARY);
        let mut on = self.lan_profile_on();
        let before = on;
        crate::settings_ui::switch_row(
            ui,
            "lan-profile",
            "Use the LAN profile for voice",
            "Shortcut: Ctrl + Shift + L while Relay is focused.",
            &mut on,
        );
        if on != before {
            self.set_lan_profile(on);
        }
        let caption = |text: &str| {
            egui::Label::new(
                RichText::new(text)
                    .size(font_size(crate::appearance::TEXT_CAPTION))
                    .color(c_muted()),
            )
            .wrap(true)
        };
        ui.add(caption(DIRECT_NOTE));
        if self.prefs.lan_profile.is_some() && !self.lan_profile_on() {
            ui.add(caption("You changed a voice setting after turning the LAN profile on, so it is no longer fully applied. Turn it off to restore your earlier values."));
            if ui.add(secondary("Restore earlier voice settings")).clicked() {
                self.set_lan_profile(false);
            }
        } else {
            ui.add(caption(OFF_NOTE));
        }
        if !self.lan_profile_on() && self.session.is_some() && suggest(&self.login.url) {
            ui.add(caption("This community's server is on your local network, so the LAN profile may suit this call."));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn custom() -> AudioSettings {
        AudioSettings {
            packet_ms: 20,
            jitter_ms: 60,
            adaptive_jitter: true,
            bitrate_kbps: 64,
            recover_packet_loss: true,
            direct_voice_enabled: false,
            low_latency_device: false,
            voice_qos: false,
            noise_suppression: true,
            ..AudioSettings::default()
        }
    }

    #[test]
    fn on_then_off_restores_every_changed_value_exactly() {
        let before = custom();
        let mut settings = before.clone();
        let mut saved = None;
        turn_on(&mut settings, &mut saved);
        assert!(is_on(&settings, &saved));
        assert_eq!((settings.packet_ms, settings.jitter_ms, settings.bitrate_kbps), (5, 5, 128));
        assert!(!settings.adaptive_jitter && settings.direct_voice_enabled);
        assert!(!settings.recover_packet_loss);
        // Nothing outside the profile changes.
        assert_eq!(settings.noise_suppression, before.noise_suppression);
        assert_eq!(settings.input_device, before.input_device);
        assert_eq!(settings.transmission, before.transmission);
        assert!(turn_off(&mut settings, &mut saved));
        assert!(saved.is_none());
        assert_eq!(Saved::from(&settings), Saved::from(&before));
        assert!(!turn_off(&mut settings, &mut saved));
    }

    #[test]
    fn turning_on_twice_keeps_the_first_saved_values() {
        let before = custom();
        let mut settings = before.clone();
        let mut saved = None;
        turn_on(&mut settings, &mut saved);
        turn_on(&mut settings, &mut saved);
        turn_off(&mut settings, &mut saved);
        assert_eq!(Saved::from(&settings), Saved::from(&before));
    }

    #[test]
    fn the_profile_survives_normalisation_and_a_manual_change_ends_it() {
        let mut settings = custom();
        let mut saved = None;
        turn_on(&mut settings, &mut saved);
        let normalized = settings.clone().normalized();
        assert!(is_on(&normalized, &saved), "normalising must keep the LAN values");
        let mut changed = normalized;
        changed.jitter_ms = 40;
        assert!(!is_on(&changed, &saved));
        assert!(!is_on(&AudioSettings::default(), &None));
    }

    #[test]
    fn saved_values_round_trip_through_preferences() {
        let saved = Saved::from(&custom());
        let text = serde_json::to_string(&saved).unwrap();
        assert_eq!(serde_json::from_str::<Saved>(&text).unwrap(), saved);
    }

    #[test]
    fn the_description_states_the_cost_and_the_privacy_trade() {
        assert!(SUMMARY.contains("Wi-Fi") && SUMMARY.contains("dropouts"));
        assert!(DIRECT_NOTE.contains("local network address"));
        assert!(OFF_NOTE.contains("restores"));
        assert!(suggest("http://192.168.1.20:4580"));
        assert!(!suggest("http://127.0.0.1:4580"));
        assert!(!suggest("http://localhost:4580"));
        assert!(!suggest("http://[::1]:4580"));
        assert!(!suggest("https://relay.example.com"));
        assert!(!suggest("not a url"));
    }
}
