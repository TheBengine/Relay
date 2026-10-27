//! "Use a reset or recovery code": the way back in for a community password
//! that did not move with the community, or was forgotten. The code and new
//! password live only in the open form and are cleared after sending.
use super::*;

/// The fields under the sign-in form. True when the member asks to send.
pub(super) fn reset_section(ui: &mut egui::Ui, flow: &mut crate::connection_setup::JoinFlow, busy: bool) -> bool {
    if flow.registering {
        return false;
    }
    let mut send = false;
    egui::CollapsingHeader::new("Use a reset or recovery code")
        .id_source(("password-reset", &flow.url))
        .show(ui, |ui| {
            ui.label(
                RichText::new("For a community password: after the community moved to a new host, or if you forgot it. Ask an administrator for a reset code, or use the recovery code you saved.")
                    .size(font_size(TEXT_CAPTION))
                    .color(c_muted()),
            );
            ui.add(
                egui::TextEdit::singleline(&mut flow.reset_code)
                    .password(true)
                    .char_limit(64)
                    .hint_text("Reset or recovery code")
                    .desired_width(f32::INFINITY),
            );
            ui.add(
                egui::TextEdit::singleline(&mut flow.new_password)
                    .password(true)
                    .char_limit(256)
                    .hint_text("New password, at least 10 characters")
                    .desired_width(f32::INFINITY),
            );
            send = ui
                .add_enabled(ready(flow) && !busy, primary("Set new password and sign in"))
                .clicked();
        });
    send
}

pub(super) fn ready(flow: &crate::connection_setup::JoinFlow) -> bool {
    !flow.username.trim().is_empty()
        && flow.reset_code.chars().filter(char::is_ascii_hexdigit).count() == 32
        && (10..=256).contains(&flow.new_password.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reset_needs_a_name_a_whole_code_and_a_long_enough_password() {
        let mut flow = crate::connection_setup::JoinFlow {
            username: "Bo".into(),
            reset_code: "1a2b-3c4d-5e6f-7a8b-9c0d-1e2f-3a4b-5c6d".into(),
            new_password: "copper meadow lantern 81".into(),
            ..Default::default()
        };
        assert!(ready(&flow));
        flow.reset_code.truncate(20);
        assert!(!ready(&flow));
        flow.reset_code = "1a2b 3c4d 5e6f 7a8b 9c0d 1e2f 3a4b 5c6d".into();
        assert!(ready(&flow), "spaces instead of dashes are fine");
        flow.new_password = "short".into();
        assert!(!ready(&flow));
    }
}
