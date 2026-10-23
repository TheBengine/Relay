//! R6.2, first part: one reusable dialog header. A left-aligned title, a
//! keyboard-reachable Close button and Escape, drawn inside a dialog whose
//! own title bar is turned off. Uses the app's theme, icons and text scale.
use super::*;

/// Draws the header and returns true when the dialog should close: a click
/// or keyboard activation of Close, or Escape while this dialog is the top
/// window. Escape is consumed, so nothing behind the dialog also reacts.
pub(in crate::app) fn dialog_header(ui: &mut egui::Ui, title: &str) -> bool {
    let mut close = false;
    ui.horizontal(|ui| {
        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
            close = icon_button(ui, Icon::Close, false, "Close").clicked();
            ui.with_layout(egui::Layout::left_to_right(Align::Center), |ui| {
                ui.add(
                    egui::Label::new(RichText::new(title).size(font_size(18.0)).strong())
                        .wrap(true),
                );
            });
        });
    });
    ui.separator();
    // egui keeps hidden windows in its layer order (`top_layer_id` returns
    // them too), so only a *visible* window above this one keeps Escape.
    let layer = ui.layer_id();
    let covered = ui.ctx().memory(|memory| {
        let visible = memory.areas().visible_layer_ids();
        memory
            .layer_ids()
            .skip_while(|other| *other != layer)
            .skip(1)
            .any(|other| other.order == layer.order && visible.contains(&other))
    });
    if !covered && ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
    {
        close = true;
    }
    close
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(key: egui::Key) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: Some(key),
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }
    }

    /// One frame of a dialog with the header, plus an optional window that
    /// was opened on top of it. Returns (close, header rect, escape left over).
    fn frame(
        ctx: &egui::Context,
        events: Vec<egui::Event>,
        other_on_top: bool,
    ) -> (bool, Rect, bool) {
        let mut close = false;
        let mut header = Rect::NOTHING;
        let mut leftover = false;
        let _ = ctx.run(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(700.0, 500.0))),
                focused: true,
                events,
                ..Default::default()
            },
            |ctx| {
                desktop_ui::dialog(ctx, "Header test")
                    .id(egui::Id::new("header-test"))
                    // Like the profile card: fixed width and its own scrolling.
                    .default_width(360.0)
                    .resizable(false)
                    .vscroll(false)
                    .title_bar(false)
                    .show(ctx, |ui| {
                        let top = ui.cursor().top();
                        close = dialog_header(ui, "Header test");
                        header =
                            Rect::from_x_y_ranges(ui.max_rect().x_range(), top..=ui.cursor().top());
                        ui.label("Body");
                    });
                if other_on_top {
                    let other = egui::Window::new("On top").id(egui::Id::new("other-window"));
                    let shown = other.show(ctx, |ui| ui.label("Another dialog"));
                    if let Some(shown) = shown {
                        ctx.move_to_top(shown.response.layer_id);
                    }
                }
                leftover = ctx.input(|input| input.key_pressed(egui::Key::Escape));
            },
        );
        (close, header, leftover)
    }

    #[test]
    fn escape_closes_only_the_top_dialog_and_is_consumed() {
        let ctx = egui::Context::default();
        configure(&ctx);
        frame(&ctx, Vec::new(), false);
        let (close, _, leftover) = frame(&ctx, vec![key(egui::Key::Escape)], false);
        assert!(close, "Escape closes the dialog");
        assert!(!leftover, "nothing behind the dialog also sees Escape");
        // With another window above it, Escape belongs to that window.
        frame(&ctx, Vec::new(), true);
        let (close, _, leftover) = frame(&ctx, vec![key(egui::Key::Escape)], true);
        assert!(!close);
        assert!(leftover, "Escape is left for the window on top");
        // Other keys never close it.
        let (close, _, _) = frame(&ctx, vec![key(egui::Key::Enter)], false);
        assert!(!close);
    }

    #[test]
    fn a_closed_higher_dialog_does_not_keep_escape() {
        let ctx = egui::Context::default();
        configure(&ctx);
        for _ in 0..4 {
            frame(&ctx, Vec::new(), false);
        }
        // A higher dialog is shown, then closed (no longer drawn).
        for _ in 0..3 {
            frame(&ctx, Vec::new(), true);
        }
        for _ in 0..3 {
            frame(&ctx, Vec::new(), false);
        }
        let (close, _, leftover) = frame(&ctx, vec![key(egui::Key::Escape)], false);
        assert!(
            close,
            "Escape closes the dialog once the higher one is gone"
        );
        assert!(!leftover);
        // A higher dialog that is visible again still keeps Escape.
        let ctx = egui::Context::default();
        configure(&ctx);
        for _ in 0..4 {
            frame(&ctx, Vec::new(), false);
        }
        for _ in 0..3 {
            frame(&ctx, Vec::new(), true);
        }
        let (close, _, leftover) = frame(&ctx, vec![key(egui::Key::Escape)], true);
        assert!(!close && leftover);
    }

    #[test]
    fn close_works_with_the_pointer_and_with_the_keyboard() {
        let ctx = egui::Context::default();
        configure(&ctx);
        // The centred dialog settles once egui knows its size.
        let mut header = Rect::NOTHING;
        for _ in 0..4 {
            header = frame(&ctx, Vec::new(), false).1;
        }
        // Close is the last item on the right of the header row.
        let at = Pos2::new(header.right() - 15.0, header.top() + 15.0);
        let press = |pressed| egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        let (close, ..) = frame(
            &ctx,
            vec![egui::Event::PointerMoved(at), press(true), press(false)],
            false,
        );
        assert!(close, "clicking Close closes");
        // Tab reaches Close, and Enter activates it. In egui 0.27 the window's
        // own area is the first Tab stop of every window; Close is the next.
        let ctx = egui::Context::default();
        configure(&ctx);
        for _ in 0..4 {
            frame(&ctx, Vec::new(), false);
        }
        frame(&ctx, vec![key(egui::Key::Tab)], false);
        frame(&ctx, vec![key(egui::Key::Tab)], false);
        let (close, ..) = frame(&ctx, vec![key(egui::Key::Enter)], false);
        assert!(close, "Close is reachable and usable with the keyboard");
    }
}
