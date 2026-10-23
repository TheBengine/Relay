//! Native settings controls. Colors follow the active egui theme, including light themes.
use crate::appearance::{TEXT_BODY, TEXT_CAPTION, TEXT_HEADING, TEXT_TITLE};
use eframe::egui::{
    self, Color32, FontId, Pos2, Rect, Response, RichText, Rounding, Sense, Stroke, Vec2,
    WidgetInfo, WidgetType,
};
use std::{hash::Hash, ops::RangeInclusive, sync::Arc};

pub(crate) fn page_heading(ui: &mut egui::Ui, title: &str, body: &str) {
    ui.add(
        egui::Label::new(
            RichText::new(title)
                .size(text_size(ui, TEXT_TITLE))
                .strong(),
        )
        .wrap(true),
    );
    if !body.is_empty() {
        ui.add_space(6.0);
        description(ui, body);
    }
    ui.add_space(24.0);
}

pub(crate) fn section_heading(ui: &mut egui::Ui, title: &str, body: &str) {
    ui.add_space(6.0);
    ui.add(
        egui::Label::new(
            RichText::new(title)
                .size(text_size(ui, TEXT_HEADING))
                .strong(),
        )
        .wrap(true),
    );
    if !body.is_empty() {
        ui.add_space(2.0);
        description(ui, body);
    }
    ui.add_space(12.0);
}

pub(crate) fn description(ui: &mut egui::Ui, text: &str) {
    ui.add(
        egui::Label::new(
            RichText::new(text)
                .size(text_size(ui, TEXT_BODY))
                .color(crate::appearance::muted_text_color(ui)),
        )
        .wrap(true),
    );
}

pub(crate) fn divider(ui: &mut egui::Ui) {
    ui.add_space(20.0);
    let (_, rect) = ui.allocate_space(Vec2::new(ui.available_width(), 1.0));
    ui.painter().line_segment(
        [rect.left_top(), rect.right_top()],
        Stroke::new(1.0_f32, ui.visuals().widgets.noninteractive.bg_stroke.color),
    );
    ui.add_space(20.0);
}

pub(crate) fn nav_group(ui: &mut egui::Ui, label: &str) {
    ui.add_space(16.0);
    ui.label(
        RichText::new(label.to_uppercase())
            .size(text_size(ui, TEXT_CAPTION))
            .strong()
            .color(crate::appearance::muted_text_color(ui)),
    );
    ui.add_space(4.0);
}

pub(crate) fn nav_item(ui: &mut egui::Ui, id: impl Hash, label: &str, selected: bool) -> Response {
    let height = text_size(ui, TEXT_BODY) * 1.3 + 18.0;
    let response = full_row(ui, id, height);
    let visuals = ui.visuals();
    let fill = if selected {
        visuals.widgets.active.weak_bg_fill
    } else if response.hovered() {
        visuals.widgets.hovered.weak_bg_fill
    } else {
        Color32::TRANSPARENT
    };
    ui.painter()
        .rect_filled(response.rect, Rounding::same(4.0), fill);
    let color = if selected || response.hovered() {
        visuals.text_color()
    } else {
        crate::appearance::muted_text_color(ui)
    };
    let text = single_line(ui, label, TEXT_BODY, color, response.rect.width() - 20.0);
    let elided = text.elided;
    ui.painter().with_clip_rect(response.rect).galley(
        Pos2::new(
            response.rect.left() + 10.0,
            response.rect.center().y - text.size().y / 2.0,
        ),
        text,
        enabled_color(ui, color),
    );
    focus_ring(ui, &response, 4.0);
    response.widget_info(|| WidgetInfo::selected(WidgetType::SelectableLabel, selected, label));
    if elided {
        response.on_hover_text(label)
    } else {
        response
    }
}

pub(crate) fn top_tab(ui: &mut egui::Ui, id: impl Hash, label: &str, selected: bool) -> Response {
    let color = if selected {
        ui.visuals().text_color()
    } else {
        crate::appearance::muted_text_color(ui)
    };
    let text = ui.painter().layout_no_wrap(
        label.to_owned(),
        FontId::proportional(text_size(ui, TEXT_BODY)),
        enabled_color(ui, color),
    );
    let (_, rect) = ui.allocate_space(Vec2::new(text.size().x + 28.0, text.size().y + 24.0));
    let response = ui
        .interact(rect, ui.make_persistent_id(id), Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    if response.hovered() {
        ui.painter().rect_filled(
            rect.shrink2(Vec2::new(2.0, 4.0)),
            Rounding::same(4.0),
            ui.visuals().widgets.hovered.weak_bg_fill,
        );
    }
    ui.painter().galley(
        Pos2::new(rect.center().x - text.size().x / 2.0, rect.top() + 8.0),
        text,
        enabled_color(ui, color),
    );
    if selected {
        ui.painter().rect_filled(
            Rect::from_min_max(
                Pos2::new(rect.left() + 12.0, rect.bottom() - 3.0),
                Pos2::new(rect.right() - 12.0, rect.bottom()),
            ),
            Rounding::same(1.5),
            ui.visuals().hyperlink_color,
        );
    }
    focus_ring(ui, &response, 4.0);
    response.widget_info(|| WidgetInfo::selected(WidgetType::SelectableLabel, selected, label));
    response
}

pub(crate) fn device_combo(
    ui: &mut egui::Ui,
    id: impl Hash,
    label: &str,
    selected: &mut Option<String>,
    names: &[String],
) -> Response {
    ui.push_id(id, |ui| {
        ui.label(
            RichText::new(label.to_uppercase())
                .size(text_size(ui, TEXT_CAPTION))
                .strong()
                .color(crate::appearance::muted_text_color(ui)),
        );
        ui.add_space(4.0);
        let mut changed = false;
        let mut response = ui
            .scope(|ui| {
                ui.spacing_mut().interact_size.y = 42.0;
                ui.spacing_mut().button_padding = Vec2::new(12.0, 10.0);
                let selected_name = selected.as_deref().unwrap_or("System default").to_owned();
                let text = single_line(
                    ui,
                    &selected_name,
                    TEXT_BODY,
                    ui.visuals().text_color(),
                    ui.available_width()
                        - 24.0
                        - ui.spacing().icon_width
                        - ui.spacing().icon_spacing,
                );
                egui::ComboBox::from_id_source("device")
                    .width(ui.available_width())
                    .selected_text(text)
                    .show_ui(ui, |ui| {
                        changed |= ui
                            .selectable_value(selected, None, "System default")
                            .changed();
                        for name in names {
                            let text = single_line(
                                ui,
                                name,
                                TEXT_BODY,
                                ui.visuals().text_color(),
                                ui.available_width() - 24.0,
                            );
                            changed |= ui
                                .selectable_value(selected, Some(name.clone()), text)
                                .on_hover_text(name)
                                .changed();
                        }
                    })
                    .response
                    .on_hover_text(selected_name)
            })
            .inner;
        if changed {
            response.mark_changed();
        }
        ui.add_space(12.0);
        response
    })
    .inner
}

/// The whole label row toggles the switch; its fixed right slot survives wrapped text.
pub(crate) fn switch_row(
    ui: &mut egui::Ui,
    id: impl Hash,
    label: &str,
    body: &str,
    value: &mut bool,
) -> Response {
    let width = ui.available_width();
    let text = RowText::new(ui, label, body, (width - 68.0).max(1.0));
    let mut response = full_row(ui, id, (text.height + 20.0).max(44.0));
    if response.clicked() {
        *value = !*value;
        response.mark_changed();
    }
    text.paint(
        ui,
        Pos2::new(response.rect.left(), response.rect.top() + 10.0),
    );
    let track = Rect::from_center_size(
        Pos2::new(response.rect.right() - 20.0, response.rect.top() + 22.0),
        Vec2::new(40.0, 24.0),
    );
    let fill = enabled_color(
        ui,
        if *value {
            ui.visuals().hyperlink_color
        } else {
            crate::appearance::muted_text_color(ui).gamma_multiply(0.6)
        },
    );
    ui.painter().rect_filled(track, Rounding::same(12.0), fill);
    let center = Pos2::new(
        if *value {
            track.right() - 12.0
        } else {
            track.left() + 12.0
        },
        track.center().y,
    );
    ui.painter()
        .circle_filled(center, 9.0, enabled_color(ui, ui.visuals().text_color()));
    if *value {
        ui.painter().line_segment(
            [center + Vec2::new(-4.0, 0.0), center + Vec2::new(-1.0, 3.0)],
            Stroke::new(1.8_f32, fill),
        );
        ui.painter().line_segment(
            [center + Vec2::new(-1.0, 3.0), center + Vec2::new(4.0, -3.0)],
            Stroke::new(1.8_f32, fill),
        );
    } else {
        for sign in [-1.0_f32, 1.0] {
            ui.painter().line_segment(
                [
                    center + Vec2::new(-3.0, -3.0 * sign),
                    center + Vec2::new(3.0, 3.0 * sign),
                ],
                Stroke::new(1.8_f32, fill),
            );
        }
    }
    focus_ring(ui, &response, 4.0);
    response.widget_info(|| WidgetInfo::selected(WidgetType::Checkbox, *value, label));
    response
}

pub(crate) fn radio_value<T: PartialEq>(
    ui: &mut egui::Ui,
    id: impl Hash,
    value: &mut T,
    option: T,
    label: &str,
    body: &str,
) -> Response {
    let mut selected = *value == option;
    let text = RowText::new(ui, label, body, (ui.available_width() - 58.0).max(1.0));
    let mut response = full_row(ui, id, (text.height + 24.0).max(58.0));
    if response.clicked() && !selected {
        *value = option;
        selected = true;
        response.mark_changed();
    }
    let visuals = ui.visuals();
    let fill = if response.hovered() {
        visuals.widgets.hovered.weak_bg_fill
    } else {
        visuals.widgets.inactive.weak_bg_fill
    };
    ui.painter()
        .rect_filled(response.rect, Rounding::same(4.0), enabled_color(ui, fill));
    let ring = if selected {
        visuals.hyperlink_color
    } else {
        crate::appearance::muted_text_color(ui)
    };
    let center = Pos2::new(response.rect.left() + 22.0, response.rect.top() + 26.0);
    ui.painter()
        .circle_stroke(center, 9.0, Stroke::new(2.0_f32, enabled_color(ui, ring)));
    if selected {
        ui.painter()
            .circle_filled(center, 5.0, enabled_color(ui, ring));
    }
    text.paint(ui, response.rect.left_top() + Vec2::new(44.0, 12.0));
    focus_ring(ui, &response, 4.0);
    response.widget_info(|| WidgetInfo::selected(WidgetType::RadioButton, selected, label));
    response
}

/// Values and the range are in display units; percent callers pass 0..100, for example.
pub(crate) fn slider_f32(
    ui: &mut egui::Ui,
    id: impl Hash,
    label: &str,
    body: &str,
    value: &mut f32,
    range: RangeInclusive<f32>,
    suffix: &str,
) -> Response {
    ui.push_id(id, |ui| {
        let magnitude = range.start().abs().max(range.end().abs());
        let precision = if magnitude <= 0.1 {
            3
        } else if magnitude <= 3.0 {
            2
        } else {
            0
        };
        let numeric = ui.painter().layout_no_wrap(
            format!("{value:.precision$}{suffix}"),
            FontId::proportional(text_size(ui, TEXT_BODY)),
            enabled_color(ui, crate::appearance::muted_text_color(ui)),
        );
        let title = ui.painter().layout(
            label.to_owned(),
            FontId::proportional(text_size(ui, TEXT_BODY)),
            enabled_color(ui, ui.visuals().text_color()),
            (ui.available_width() - numeric.size().x - 16.0).max(1.0),
        );
        let (_, header) = ui.allocate_space(Vec2::new(
            ui.available_width(),
            title.size().y.max(numeric.size().y),
        ));
        ui.painter()
            .galley(header.left_top(), title, ui.visuals().text_color());
        ui.painter().galley(
            Pos2::new(header.right() - numeric.size().x, header.top()),
            numeric,
            crate::appearance::muted_text_color(ui),
        );
        if !body.is_empty() {
            description(ui, body);
        }
        ui.add_space(6.0);
        let response = ui
            .scope(|ui| {
                ui.spacing_mut().slider_width = ui.available_width();
                ui.spacing_mut().interact_size.y = 24.0;
                let text = ui.visuals().text_color();
                let widgets = &mut ui.visuals_mut().widgets;
                widgets.inactive.bg_fill = text.gamma_multiply(0.3);
                widgets.hovered.bg_fill = text;
                widgets.active.bg_fill = text;
                widgets.inactive.fg_stroke = Stroke::new(1.5_f32, text);
                widgets.hovered.fg_stroke = Stroke::new(1.5_f32, text);
                widgets.active.fg_stroke = Stroke::new(1.5_f32, text);
                ui.add(
                    egui::Slider::new(value, range)
                        .show_value(false)
                        .trailing_fill(true),
                )
            })
            .inner;
        ui.add_space(12.0);
        response
    })
    .inner
}

struct RowText {
    title: Arc<egui::Galley>,
    body: Option<Arc<egui::Galley>>,
    height: f32,
}

impl RowText {
    fn new(ui: &egui::Ui, title: &str, body: &str, width: f32) -> Self {
        let title = ui.painter().layout(
            title.to_owned(),
            FontId::proportional(text_size(ui, TEXT_BODY)),
            enabled_color(ui, ui.visuals().text_color()),
            width,
        );
        let body = (!body.is_empty()).then(|| {
            ui.painter().layout(
                body.to_owned(),
                FontId::proportional(text_size(ui, TEXT_BODY)),
                enabled_color(ui, crate::appearance::muted_text_color(ui)),
                width,
            )
        });
        let height = title.size().y + body.as_ref().map_or(0.0, |body| body.size().y + 4.0);
        Self {
            title,
            body,
            height,
        }
    }

    fn paint(&self, ui: &egui::Ui, origin: Pos2) {
        ui.painter().galley(
            origin,
            self.title.clone(),
            enabled_color(ui, ui.visuals().text_color()),
        );
        if let Some(body) = &self.body {
            ui.painter().galley(
                origin + Vec2::new(0.0, self.title.size().y + 4.0),
                body.clone(),
                enabled_color(ui, crate::appearance::muted_text_color(ui)),
            );
        }
    }
}

fn full_row(ui: &mut egui::Ui, id: impl Hash, height: f32) -> Response {
    let (_, rect) = ui.allocate_space(Vec2::new(ui.available_width(), height));
    ui.interact(rect, ui.make_persistent_id(id), Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn focus_ring(ui: &egui::Ui, response: &Response, rounding: f32) {
    if response.has_focus() {
        ui.painter().rect_stroke(
            response.rect.shrink(1.0),
            Rounding::same(rounding),
            Stroke::new(2.0_f32, ui.visuals().hyperlink_color),
        );
    }
}

fn enabled_color(ui: &egui::Ui, color: Color32) -> Color32 {
    if ui.is_enabled() {
        color
    } else {
        color.gamma_multiply(0.45)
    }
}

/// The one place settings text is scaled: the Body style already carries the
/// user's text scale, so a base size is multiplied by that ratio exactly once.
fn text_size(ui: &egui::Ui, base: f32) -> f32 {
    base * egui::TextStyle::Body.resolve(ui.style()).size / TEXT_BODY
}

pub(crate) fn single_line(
    ui: &egui::Ui,
    text: &str,
    base_size: f32,
    color: Color32,
    width: f32,
) -> Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::simple(
        text.to_owned(),
        FontId::proportional(text_size(ui, base_size)),
        enabled_color(ui, color),
        width.max(1.0),
    );
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    ui.painter().layout_job(job)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn switch_frame(
        context: &egui::Context,
        value: &mut bool,
        events: Vec<egui::Event>,
        enabled: bool,
        width: f32,
    ) -> (egui::Id, Rect, bool) {
        let mut result = None;
        let _ = context.run(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(width, 600.0))),
                focused: true,
                events,
                ..Default::default()
            },
            |context| {
                egui::CentralPanel::default().show(context, |ui| {
                    ui.add_enabled_ui(enabled, |ui| {
                        let response = switch_row(
                            ui,
                            "test-switch",
                            "Suppress background noise",
                            "Reduce steady background noise while preserving your voice. This description wraps in a narrow settings column.",
                            value,
                        );
                        result = Some((response.id, response.rect, response.changed()));
                    });
                });
            },
        );
        result.unwrap()
    }

    fn key(key: egui::Key) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: Some(key),
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }
    }

    #[test]
    fn switch_supports_keyboard_and_label_clicks_but_cannot_change_while_disabled() {
        let context = egui::Context::default();
        let mut value = false;
        let (id, rect, _) = switch_frame(&context, &mut value, vec![], true, 480.0);
        context.memory_mut(|memory| memory.request_focus(id));
        let (_, _, changed) = switch_frame(
            &context,
            &mut value,
            vec![key(egui::Key::Space)],
            true,
            480.0,
        );
        assert!(value && changed);
        let (_, _, changed) = switch_frame(
            &context,
            &mut value,
            vec![key(egui::Key::Enter)],
            false,
            480.0,
        );
        assert!(value && !changed);
        switch_frame(&context, &mut value, vec![], true, 480.0);
        let position = rect.left_top() + Vec2::new(8.0, 20.0);
        let (_, _, changed) = switch_frame(
            &context,
            &mut value,
            vec![
                egui::Event::PointerMoved(position),
                egui::Event::PointerButton {
                    pos: position,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
                egui::Event::PointerButton {
                    pos: position,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
            true,
            480.0,
        );
        assert!(!value && changed);
    }

    /// One settings frame with the real appearance applied (twice, like a
    /// repeated Apply), a narrow navigation column and a narrow page.
    fn scaled_settings_frame(font_scale: f32) -> (f32, Vec<(Rect, Rect)>, Rect, Rect, Rect) {
        let context = egui::Context::default();
        let appearance = crate::appearance::AppearanceSettings {
            font_scale,
            ..Default::default()
        };
        appearance.apply(&context, None);
        appearance.apply(&context, None);
        let mut body = 0.0;
        let mut items = Vec::new();
        let mut heading = Rect::NOTHING;
        let mut page = Rect::NOTHING;
        let mut column = Rect::NOTHING;
        let _ = context.run(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(460.0, 700.0))),
                ..Default::default()
            },
            |context| {
                egui::SidePanel::left("nav")
                    .exact_width(150.0)
                    .resizable(false)
                    .show(context, |ui| {
                        body = text_size(ui, TEXT_BODY);
                        column = ui.max_rect();
                        nav_group(ui, "User settings");
                        for label in [
                            "Voice & Video",
                            "Encryption",
                            "Keyboard shortcuts and accessibility",
                        ] {
                            let response = nav_item(ui, label, label, false);
                            let text = single_line(
                                ui,
                                label,
                                TEXT_BODY,
                                Color32::WHITE,
                                response.rect.width() - 20.0,
                            );
                            let text = Rect::from_center_size(response.rect.center(), text.size());
                            items.push((response.rect, text));
                        }
                    });
                egui::CentralPanel::default().show(context, |ui| {
                    page_heading(
                        ui,
                        "Voice & Video",
                        "Choose the microphone and speakers Relay uses for calls.",
                    );
                    let top = ui.cursor().top();
                    section_heading(ui, "Input sensitivity and noise suppression", "");
                    heading =
                        Rect::from_x_y_ranges(ui.min_rect().x_range(), top..=ui.cursor().top());
                    page = ui.max_rect();
                });
            },
        );
        (body, items, heading, page, column)
    }

    #[test]
    fn text_scale_applies_once_and_navigation_labels_fit_at_large_text() {
        let (normal, normal_items, normal_heading, _, _) = scaled_settings_frame(1.0);
        let (large, large_items, large_heading, page, column) = scaled_settings_frame(1.4);
        // Repeated Apply scales the body text once, never 1.4 x 1.4.
        assert!((normal - TEXT_BODY).abs() < 0.01, "{normal}");
        assert!((large - TEXT_BODY * 1.4).abs() < 0.01, "{large}");
        for (row, text) in normal_items.iter().chain(&large_items) {
            assert!(
                row.height() >= text.height() + 8.0,
                "navigation text {text:?} is clipped by its row {row:?}"
            );
            assert!(
                row.right() <= column.right() + 0.5,
                "row {row:?} leaves the column {column:?}"
            );
        }
        // The longest label is elided to the column, not drawn beyond it.
        let (row, text) = large_items[2];
        assert!(text.width() <= row.width() - 20.0 + 0.5);
        assert!(large_items[0].0.height() > normal_items[0].0.height());
        // A long section heading wraps inside a narrow page and grows with text.
        assert!(large_heading.right() <= page.right() + 0.5);
        assert!(large_heading.height() > normal_heading.height());
    }

    #[test]
    fn narrow_settings_column_wraps_descriptions_instead_of_expanding_the_row() {
        let context = egui::Context::default();
        let mut value = false;
        let (_, wide, _) = switch_frame(&context, &mut value, vec![], true, 700.0);
        let (_, narrow, _) = switch_frame(&context, &mut value, vec![], true, 280.0);
        assert!(narrow.height() > wide.height());
        assert!(narrow.width() <= 280.0);
    }

    #[test]
    fn long_device_names_keep_the_settings_column_width_and_selection() {
        let context = egui::Context::default();
        let name = "Very long USB audio interface name with hardware model and port information";
        let mut selected = Some(name.to_owned());
        let mut device_rect = Rect::NOTHING;
        let _ = context.run(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(280.0, 600.0))),
                ..Default::default()
            },
            |context| {
                egui::CentralPanel::default().show(context, |ui| {
                    device_rect = device_combo(
                        ui,
                        "test-device",
                        "Input device",
                        &mut selected,
                        &[name.to_owned()],
                    )
                    .rect;
                });
            },
        );
        assert!(device_rect.right() <= 280.0);
        assert_eq!(selected.as_deref(), Some(name));
    }
}
