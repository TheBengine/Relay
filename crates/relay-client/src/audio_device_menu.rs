//! R8.2: the microphone and speaker menu of the user panel, moved unchanged
//! out of `app.rs`; only the method's visibility became `pub(super)`.
use super::*;

impl RelayApp {
    pub(super) fn audio_device_menu(&mut self, ui: &mut egui::Ui, input: bool) {
        let applied = self.audio_applied.clone();
        let mut audio = applied.clone();
        let label = if input {
            "Input device options"
        } else {
            "Output device options"
        };
        let popup_id = ui.id().with(label);
        let (rect, response) = ui.allocate_exact_size(Vec2::new(16.0, 30.0), Sense::click());
        if response.clicked() {
            ui.memory_mut(|memory| memory.toggle_popup(popup_id));
            self.refresh_devices();
        }
        let opened = ui.memory(|memory| memory.is_popup_open(popup_id));
        if response.hovered() || response.has_focus() || opened {
            ui.painter().rect_filled(rect, 4.0, c_card());
            if response.has_focus() {
                ui.painter()
                    .rect_stroke(rect, 4.0, Stroke::new(1.0_f32, c_accent()));
            }
        }
        let center = rect.center();
        ui.painter().add(egui::Shape::line(
            vec![
                center + Vec2::new(-3.0, -1.5),
                center + Vec2::new(0.0, 1.5),
                center + Vec2::new(3.0, -1.5),
            ],
            Stroke::new(1.4_f32, c_muted()),
        ));
        response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, label));
        let mut changed = false;
        if opened {
            let popup = egui::Area::new(popup_id)
                .order(egui::Order::Foreground)
                .fixed_pos(rect.left_top())
                .pivot(Align2::LEFT_BOTTOM)
                .constrain_to(ui.ctx().screen_rect().shrink(8.0))
                .show(ui.ctx(), |ui| {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        egui::ScrollArea::vertical()
                            .max_height((ui.ctx().screen_rect().height() - 48.0).max(1.0))
                            .show(ui, |ui| {
                                ui.set_width(
                                    (ui.ctx().screen_rect().width() - 48.0).clamp(120.0, 240.0),
                                );
                                ui.label(
                                    RichText::new(if input {
                                        "Input device"
                                    } else {
                                        "Output device"
                                    })
                                    .strong(),
                                );
                                ui.separator();
                                let names = if input {
                                    &self.inventory.audio.inputs
                                } else {
                                    &self.inventory.audio.outputs
                                };
                                let selected = if input {
                                    &mut audio.input_device
                                } else {
                                    &mut audio.output_device
                                };
                                let default_name = if input {
                                    self.inventory.audio.default_input.as_deref()
                                } else {
                                    self.inventory.audio.default_output.as_deref()
                                };
                                let default_label = default_name.map_or_else(
                                    || "System default".to_owned(),
                                    |name| format!("System default · {name}"),
                                );
                                egui::ScrollArea::vertical()
                                    .max_height(180.0)
                                    .show(ui, |ui| {
                                        let default_text = crate::settings_ui::single_line(
                                            ui,
                                            &default_label,
                                            14.0,
                                            ui.visuals().text_color(),
                                            ui.available_width() - 20.0,
                                        );
                                        changed |= ui
                                            .selectable_value(selected, None, default_text)
                                            .on_hover_text(&default_label)
                                            .changed();
                                        // Keep a disconnected selection visible until the user chooses another device.
                                        if let Some(name) =
                                            selected.clone().filter(|name| !names.contains(name))
                                        {
                                            ui.add_enabled(
                                                false,
                                                egui::Label::new(format!("{} · unavailable", name))
                                                    .truncate(true),
                                            );
                                        }
                                        for name in names {
                                            let text = crate::settings_ui::single_line(
                                                ui,
                                                name,
                                                14.0,
                                                ui.visuals().text_color(),
                                                ui.available_width() - 20.0,
                                            );
                                            changed |= ui
                                                .selectable_value(
                                                    selected,
                                                    Some(name.clone()),
                                                    text,
                                                )
                                                .on_hover_text(name)
                                                .changed();
                                        }
                                    });
                                ui.separator();
                                let (volume, maximum) = if input {
                                    (&mut audio.microphone_gain, 4.0)
                                } else {
                                    (&mut audio.playback_volume, 2.0)
                                };
                                ui.label(if input {
                                    "Input volume"
                                } else {
                                    "Output volume"
                                });
                                changed |= ui
                                    .add(egui::Slider::new(volume, 0.0..=maximum).custom_formatter(
                                        |value, _| format!("{:.0}%", value * 100.0),
                                    ))
                                    .changed();
                                if ui
                                    .add_enabled(
                                        self.inventory_rx.is_none(),
                                        egui::Button::new(if self.inventory_rx.is_some() {
                                            "Finding devices…"
                                        } else {
                                            "Refresh devices"
                                        }),
                                    )
                                    .clicked()
                                {
                                    self.refresh_devices();
                                }
                                if ui.button("Voice & Video settings").clicked() {
                                    self.settings_tab = SettingsTab::VoiceVideo;
                                    self.voice_video_tab = VoiceVideoTab::Voice;
                                    self.settings_open = true;
                                    ui.memory_mut(|memory| memory.close_popup());
                                }
                            });
                    });
                });
            if ui.input(|input| input.key_pressed(egui::Key::Escape))
                || (popup.response.clicked_elsewhere() && !response.clicked())
            {
                ui.memory_mut(|memory| memory.close_popup());
            }
        }
        response.on_hover_text(label);
        if changed {
            self.apply_quick_audio_change(|next| {
                if audio.input_device != applied.input_device {
                    next.input_device = audio.input_device.clone();
                }
                if audio.output_device != applied.output_device {
                    next.output_device = audio.output_device.clone();
                }
                if audio.microphone_gain != applied.microphone_gain {
                    next.microphone_gain = audio.microphone_gain;
                }
                if audio.playback_volume != applied.playback_volume {
                    next.playback_volume = audio.playback_volume;
                }
            });
        }
    }
}
