//! egui 0.27 handles native text selection but not AccessKit SetValue for text.
//! Complete that native editing contract on explicitly editable Relay fields.
use eframe::egui::{
    self,
    accesskit::{Action, ActionData},
};

pub fn apply(ui: &egui::Ui, response: &mut egui::Response, value: &mut String, char_limit: usize) {
    if !ui.is_enabled() || !response.enabled() || !ui.clip_rect().intersects(response.rect) {
        return;
    }
    ui.ctx()
        .accesskit_node_builder(response.id, |node| node.add_action(Action::SetValue));
    let replacement = ui.input(|input| {
        input
            .accesskit_action_requests(response.id, Action::SetValue)
            .filter_map(|request| match &request.data {
                Some(ActionData::Value(text))
                    if text.len() <= 16 * 1024
                        && text.chars().count() <= char_limit
                        && !text.contains(['\r', '\n', '\0']) =>
                {
                    Some(text.to_string())
                }
                _ => None,
            })
            .last()
    });
    if let Some(replacement) = replacement.filter(|text| text != value) {
        *value = replacement;
        response.mark_changed();
        // The existing widget will repaint/mask the edited model next frame;
        // never publish a password as an accessibility value ourselves.
        ui.ctx().request_repaint();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::accesskit::{ActionRequest, NodeId};

    fn action(id: egui::Id, text: &str) -> egui::Event {
        egui::Event::AccessKitActionRequest(ActionRequest {
            action: Action::SetValue,
            target: NodeId(id.value()),
            data: Some(ActionData::Value(text.into())),
        })
    }
    fn frame(
        ctx: &egui::Context,
        text: &mut String,
        events: Vec<egui::Event>,
        enabled: bool,
        adapted: bool,
        limit: usize,
    ) -> (egui::Response, egui::FullOutput) {
        let mut response = None;
        let output = ctx.run(
            egui::RawInput {
                events,
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.set_enabled(enabled);
                    let mut field = ui.add(
                        egui::TextEdit::singleline(text)
                            .id_source("password")
                            .password(true)
                            .char_limit(limit),
                    );
                    if adapted {
                        apply(ui, &mut field, text, limit);
                    }
                    response = Some(field);
                });
            },
        );
        (response.unwrap(), output)
    }

    #[test]
    fn native_set_value_edits_the_password_model_and_validation_without_exposing_it() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut text = String::new();
        let (field, _) = frame(&ctx, &mut text, vec![], true, false, 256);
        frame(
            &ctx,
            &mut text,
            vec![action(field.id, "own-test-password")],
            true,
            false,
            256,
        );
        assert!(
            text.is_empty(),
            "the pinned framework alone ignores native SetValue"
        );
        let (field, _) = frame(
            &ctx,
            &mut text,
            vec![action(field.id, "own-test-password")],
            true,
            true,
            256,
        );
        assert_eq!(text, "own-test-password");
        assert!(field.changed(), "normal form validation must see the edit");
        let (_, output) = frame(&ctx, &mut text, vec![], true, true, 256);
        assert!(output
            .platform_output
            .accesskit_update
            .as_ref()
            .is_some_and(|update| update
                .nodes
                .iter()
                .any(|(id, node)| id.0 == field.id.value()
                    && node.role() == egui::accesskit::Role::PasswordInput)));
        assert!(
            !format!("{:?}", output.platform_output.accesskit_update).contains("own-test-password"),
            "password content must stay out of the native accessibility readback"
        );
    }

    #[test]
    fn native_edit_cannot_touch_a_different_or_disabled_field() {
        let ctx = egui::Context::default();
        let mut text = "unchanged".to_owned();
        let (field, _) = frame(&ctx, &mut text, vec![], true, true, 64);
        let (response, _) = frame(
            &ctx,
            &mut text,
            vec![action(egui::Id::new("some-other-widget"), "wrong")],
            true,
            true,
            64,
        );
        assert_eq!(text, "unchanged");
        assert!(!response.changed());
        let (response, _) = frame(
            &ctx,
            &mut text,
            vec![action(field.id, "disabled-write")],
            false,
            true,
            64,
        );
        assert_eq!(text, "unchanged");
        assert!(!response.changed());
    }

    #[test]
    fn native_single_line_edits_respect_field_limits_and_do_not_replay_next_frame() {
        let ctx = egui::Context::default();
        let mut text = String::new();
        let (field, _) = frame(&ctx, &mut text, vec![], true, true, 6);
        for invalid in ["1234567", "line\nbreak", "123\0"] {
            frame(
                &ctx,
                &mut text,
                vec![action(field.id, invalid)],
                true,
                true,
                6,
            );
            assert!(text.is_empty());
        }
        let (response, _) = frame(
            &ctx,
            &mut text,
            vec![action(field.id, "123456")],
            true,
            true,
            6,
        );
        assert!(response.changed());
        assert_eq!(text, "123456");
        text = "typed-later".into();
        frame(&ctx, &mut text, vec![], true, true, 6);
        assert_eq!(text, "typed-later");
    }
}
