//! Separate chat/API checks from evidence on the active voice transport.
use super::*;
use crate::connection_path::{self, Quality, SampleState};
use crate::settings_ui as controls;

fn quality_color(quality: Quality) -> Color32 {
    match quality {
        Quality::Good => c_good(),
        Quality::Poor => c_bad(),
        Quality::Fair | Quality::Unknown => c_muted(),
    }
}

fn chat_status(address: &str) -> (String, Quality, String) {
    let sample = connection_path::sample(address);
    let (line, quality) = match sample.as_ref() {
        Some(sample) if sample.state == SampleState::Current => match sample.report.as_ref() {
            Some(report) => (format!("Chat/API: {}", report.headline()), report.quality()),
            None => ("Chat/API: checking…".into(), Quality::Unknown),
        },
        Some(sample) if sample.state == SampleState::Failed => {
            ("Chat/API: check unavailable".into(), Quality::Unknown)
        }
        Some(sample) if sample.state == SampleState::Stale => {
            ("Chat/API: last check out of date".into(), Quality::Unknown)
        }
        _ => ("Chat/API: checking…".into(), Quality::Unknown),
    };
    let mut detail = "This checks the chat/API endpoint. An HTTP check includes server processing. It does not measure voice or video latency.".to_owned();
    if let Some(sample) = &sample {
        if let Some(age) = sample.age {
            detail.push_str(&format!(
                " Last successful check: {:.0} s ago.",
                age.as_secs_f32()
            ));
        }
        if sample.state == SampleState::Failed {
            detail.push_str(
                " Check whether this server is online; Relay will retry while this panel is open.",
            );
        }
        if sample.state == SampleState::Current {
            if let Some(hint) = sample.report.as_ref().and_then(|report| report.hint()) {
                detail.push(' ');
                detail.push_str(hint);
            }
        }
    }
    (line, quality, detail)
}

impl RelayApp {
    pub(super) fn connection_path_caption(&self, ui: &mut egui::Ui, width: f32) {
        let Some(media) = self.media.as_ref().filter(|m| m.is_active()) else {
            return;
        };
        // The helper topology is context only. Voice RTT comes from the media
        // worker's authenticated probes, including the WebSocket fallback.
        connection_path::observe(&self.login.url, ui.ctx());
        let stats = media.network_stats();
        let status = crate::voice_connection::Status::from_stats(&stats);
        let (chat, _, chat_detail) = chat_status(&self.login.url);
        ui.add_sized(
            [width, 18.0],
            egui::Label::new(
                RichText::new(status.headline_for(connection_path::transport_for(&self.login.url)))
                    .size(font_size(TEXT_CAPTION))
                    .color(quality_color(status.quality())),
            )
            .truncate(true),
        )
        .on_hover_text(format!(
            "{}\n\n{chat}\n{chat_detail}",
            status.explanation(&stats)
        ));
        ui.ctx().request_repaint_after(Duration::from_secs(1));
    }

    pub(super) fn connection_path_details(&self, ui: &mut egui::Ui) {
        connection_path::observe(&self.login.url, ui.ctx());
        if let Some(media) = self.media.as_ref().filter(|m| m.is_active()) {
            let stats = media.network_stats();
            let voice = crate::voice_connection::Status::from_stats(&stats);
            ui.label(
                RichText::new(voice.headline_for(connection_path::transport_for(&self.login.url)))
                    .color(quality_color(voice.quality())),
            );
            controls::description(ui, &voice.explanation(&stats));
        }
        let (line, quality, detail) = chat_status(&self.login.url);
        ui.label(RichText::new(line).color(quality_color(quality)));
        controls::description(ui, &detail);
        ui.ctx().request_repaint_after(Duration::from_secs(1));
    }
}
