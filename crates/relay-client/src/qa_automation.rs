//! QA builds only (`qa-automation`): egui 0.27 text fields ignore an
//! accessibility SetValue. Owned GUI checks drive background windows through
//! UI Automation and cannot take the user's foreground, so a SetValue on a text
//! field becomes an accessibility focus request, then select-all plus the text
//! on the next frame. Never compiled into a normal build.
use eframe::egui::{self, accesskit};

#[derive(Default)]
pub(crate) struct Pending(Option<(accesskit::NodeId, String, u8)>);

pub(crate) fn hook(pending: &mut Pending, ctx: &egui::Context, raw: &mut egui::RawInput) {
    if let Some((target, text, frames)) = pending.0.take() {
        let focused = ctx
            .memory(|m| m.focused())
            .is_some_and(|id| accesskit::NodeId::from(id.value()) == target);
        if focused {
            raw.events.push(egui::Event::Key {
                key: egui::Key::A,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::COMMAND,
            });
            raw.events.push(egui::Event::Text(text));
        } else if frames < 30 {
            pending.0 = Some((target, text, frames + 1));
        }
        ctx.request_repaint();
    }
    for event in &mut raw.events {
        let egui::Event::AccessKitActionRequest(request) = event else {
            continue;
        };
        if request.action != accesskit::Action::SetValue {
            continue;
        }
        let Some(accesskit::ActionData::Value(value)) = request.data.take() else {
            continue;
        };
        pending.0 = Some((request.target, value.into(), 0));
        request.action = accesskit::Action::Focus;
        ctx.request_repaint();
    }
}

/// Owned GUI checks read what the user would see as a notice or error from
/// the app's stderr log; nothing secret is in these texts.
pub(crate) fn log_update(update: &crate::network::Update) {
    use crate::network::Update;
    match update {
        Update::Notice(text) => eprintln!("QA notice: {text}"),
        Update::Error(text) => eprintln!("QA error: {text}"),
        Update::RequestFailed { error, .. } => eprintln!("QA request failed: {error}"),
        _ => {}
    }
}
