//! The same restrained card, type and spacing as Relay's browser sign-in.
use super::*;

/// In-app actions keep a button role, with the browser page's quieter link style.
pub(super) fn text_action(ui: &mut egui::Ui, label: &str) -> egui::Response {
    ui.scope(|ui| {
        ui.spacing_mut().interact_size.y = 20.0;
        ui.spacing_mut().button_padding = Vec2::new(0.0, 2.0);
        ui.add(
            egui::Button::new(RichText::new(label).color(ui.visuals().hyperlink_color))
                .frame(false),
        )
    })
    .inner
}

pub(super) fn page(ctx: &egui::Context, name: &'static str, contents: impl FnOnce(&mut egui::Ui)) {
    let narrow = ctx.screen_rect().width() < 540.0;
    let outer = if narrow { 16.0 } else { 24.0 };
    let padding = if narrow { 24.0 } else { 32.0 };
    egui::CentralPanel::default()
        .frame(
            egui::Frame::none()
                .fill(c_rail())
                .inner_margin(Margin::same(outer)),
        )
        .show(ctx, |ui| {
            let viewport_height = ui.available_height();
            let height_id = egui::Id::new((name, "card-height"));
            let previous_height = ctx
                .data(|data| data.get_temp::<f32>(height_id))
                .unwrap_or(360.0);
            egui::ScrollArea::vertical()
                .id_source(name)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    let width = ui.available_width().min(520.0);
                    ui.add_space(((viewport_height - previous_height) / 2.0).max(0.0));
                    ui.horizontal_top(|ui| {
                        ui.add_space(((ui.available_width() - width) / 2.0).max(0.0));
                        let card = egui::Frame::none()
                            .fill(c_bg())
                            .stroke(Stroke::new(1.0_f32, c_line()))
                            .rounding(12.0)
                            .inner_margin(Margin::same(padding))
                            .shadow(egui::epaint::Shadow {
                                offset: Vec2::new(0.0, 12.0),
                                blur: 32.0,
                                spread: 0.0,
                                color: Color32::from_black_alpha(38),
                            })
                            .show(ui, |ui| {
                                ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                                    ui.set_width((width - 2.0 * padding - 2.0).max(0.0));
                                    let heading = semibold(ui, font_size(26.0));
                                    ui.style_mut()
                                        .text_styles
                                        .insert(egui::TextStyle::Heading, heading);
                                    ui.style_mut().text_styles.insert(
                                        egui::TextStyle::Button,
                                        FontId::proportional(font_size(14.0)),
                                    );
                                    ui.visuals_mut().extreme_bg_color = c_rail();
                                    ui.spacing_mut().interact_size.y = 44.0;
                                    ui.visuals_mut().widgets.inactive.rounding =
                                        Rounding::same(6.0);
                                    ui.visuals_mut().widgets.inactive.bg_stroke =
                                        Stroke::new(1.0_f32, c_line());
                                    if ui.visuals().dark_mode {
                                        ui.visuals_mut().hyperlink_color =
                                            Color32::from_rgb(160, 167, 255);
                                    }
                                    ui.horizontal(|ui| {
                                        ui.spacing_mut().item_spacing.x = 10.0;
                                        mark(ui, font_size(32.0));
                                        ui.label(
                                            RichText::new("Relay")
                                                .size(font_size(25.0))
                                                .color(c_text()),
                                        );
                                    });
                                    ui.add_space(24.0);
                                    contents(ui);
                                });
                            });
                        let height = card.response.rect.height();
                        if (height - previous_height).abs() > 0.5 {
                            ctx.data_mut(|data| data.insert_temp(height_id, height));
                            ctx.request_repaint();
                        }
                    });
                });
        });
}
