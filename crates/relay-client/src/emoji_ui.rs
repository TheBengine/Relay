use super::*;

pub(super) fn expression_panel_rect(screen: Rect, composer: Option<Rect>) -> Rect {
    let usable = Rect::from_min_max(
        screen.min + Vec2::new(8.0, 40.0),
        screen.max - Vec2::splat(8.0),
    );
    let anchor = composer.unwrap_or_else(|| {
        Rect::from_min_size(usable.right_bottom() - Vec2::new(440.0, 60.0), Vec2::ZERO)
    });
    let width = 440.0_f32.min(usable.width()).max(1.0);
    let bottom = (anchor.top() - 8.0).clamp(usable.top() + 1.0, usable.bottom());
    let height = 450.0_f32.min(bottom - usable.top()).max(1.0);
    let right = anchor.right().clamp(usable.left() + width, usable.right());
    Rect::from_min_size(
        Pos2::new(right - width, bottom - height),
        Vec2::new(width, height),
    )
}

impl RelayApp {
    pub(super) fn remember_emoji(&mut self, index: usize) {
        let value = crate::emoji::catalog().entries[index].unicode.clone();
        self.prefs.emoji_recent.retain(|recent| recent != &value);
        self.prefs.emoji_recent.insert(0, value);
        self.prefs.emoji_recent.truncate(32);
        self.defer_preferences();
    }

    pub(super) fn standard_emoji_picker(
        &mut self,
        ui: &mut egui::Ui,
        reserve_bottom: f32,
    ) -> Option<usize> {
        let data = crate::emoji::catalog();
        let search = ui.add(
            egui::TextEdit::singleline(&mut self.admin_ui.expression_query)
                .hint_text("Search emoji or :shortcode:")
                .desired_width(f32::INFINITY),
        );
        if std::mem::take(&mut self.admin_ui.expression_focus) {
            search.request_focus();
        }
        let texture = crate::emoji::texture(ui.ctx());
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("Skin tone")
                    .size(font_size(TEXT_CAPTION))
                    .color(c_muted()),
            );
            egui::ComboBox::from_id_source("emoji-tone")
                .selected_text(match self.prefs.emoji_tone {
                    1 => "Light",
                    2 => "Medium light",
                    3 => "Medium",
                    4 => "Medium dark",
                    5 => "Dark",
                    _ => "Default",
                })
                .show_ui(ui, |ui| {
                    for (tone, name) in [
                        (0, "Default"),
                        (1, "Light"),
                        (2, "Medium light"),
                        (3, "Medium"),
                        (4, "Medium dark"),
                        (5, "Dark"),
                    ] {
                        ui.horizontal(|ui| {
                            let index = data.with_tone(data.shortcode("wave").unwrap(), tone);
                            ui.add(
                                egui::Image::new((texture.id(), Vec2::splat(20.0)))
                                    .uv(data.uv(index)),
                            );
                            if ui
                                .selectable_value(&mut self.prefs.emoji_tone, tone, name)
                                .changed()
                            {
                                self.defer_preferences();
                            }
                        });
                    }
                });
        });
        ui.add_space(6.0);
        let matches = data.search(
            &self.admin_ui.expression_query,
            if self.admin_ui.expression_query.trim().is_empty() {
                self.admin_ui.expression_category
            } else {
                None
            },
            !self.admin_ui.expression_query.is_empty(),
        );
        let body_height =
            (ui.available_height() - font_size(45.0) - reserve_bottom - 12.0).max(70.0);
        let mut chosen = None;
        let mut hovered = None;
        ui.horizontal_top(|ui| {
            egui::ScrollArea::vertical()
                .id_source("emoji-category-rail")
                .max_height(body_height)
                .show(ui, |ui| {
                    ui.vertical(|ui| {
                        ui.set_width(32.0);
                        if icon_button(
                            ui,
                            Icon::Smile,
                            self.admin_ui.expression_category.is_none(),
                            &format!("All emoji and recently used · Unicode {}", data.version),
                        )
                        .clicked()
                        {
                            self.admin_ui.expression_category = None;
                        }
                        for (category, name) in data.categories.iter().enumerate() {
                            let alias = [
                                "grinning",
                                "wave",
                                "skin-tone-3",
                                "dog",
                                "pizza",
                                "car",
                                "soccer",
                                "bulb",
                                "heart",
                                "flag-dk",
                            ]
                            .get(category)
                            .copied()
                            .unwrap_or("grinning");
                            let index = data.shortcode(alias).unwrap_or_else(|| {
                                data.entries
                                    .iter()
                                    .position(|entry| entry.category == category)
                                    .unwrap_or(0)
                            });
                            let button = egui::ImageButton::new(
                                egui::Image::new((texture.id(), Vec2::splat(23.0)))
                                    .uv(data.uv(index)),
                            )
                            .selected(self.admin_ui.expression_category == Some(category));
                            if ui.add(button).on_hover_text(name).clicked() {
                                self.admin_ui.expression_category = Some(category);
                            }
                        }
                    });
                });
            ui.vertical(|ui| {
                ui.set_width(ui.available_width().max(1.0));
                if self.admin_ui.expression_query.trim().is_empty()
                    && self.admin_ui.expression_category.is_none()
                    && !self.prefs.emoji_recent.is_empty()
                {
                    ui.label(
                        RichText::new("Recently used")
                            .size(font_size(TEXT_CAPTION))
                            .color(c_muted()),
                    );
                    egui::ScrollArea::horizontal()
                        .id_source("recent-emoji-row")
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                for value in &self.prefs.emoji_recent {
                                    if let Some((index, _)) = data.at_start(value) {
                                        let response = crate::emoji::button(ui, index, 28.0);
                                        if response.hovered() {
                                            hovered = Some(index);
                                        }
                                        if response.clicked() {
                                            chosen = Some(index);
                                        }
                                    }
                                }
                            });
                        });
                }
                ui.label(
                    RichText::new(if !self.admin_ui.expression_query.trim().is_empty() {
                        "Search results"
                    } else {
                        self.admin_ui
                            .expression_category
                            .map_or("All emoji", |category| data.categories[category].as_str())
                    })
                    .size(font_size(TEXT_CAPTION))
                    .color(c_muted()),
                );
                if matches.is_empty() {
                    ui.label("No emoji found. Try another name.");
                }
                let columns = (ui.available_width() / 40.0).floor().max(1.0) as usize;
                let grid_height = (body_height - ui.min_rect().height()).max(50.0);
                egui::ScrollArea::vertical()
                    .id_source((
                        "standard-emoji-grid",
                        self.admin_ui.expression_category,
                        &self.admin_ui.expression_query,
                    ))
                    .max_height(grid_height)
                    .show_rows(ui, 38.0, matches.len().div_ceil(columns), |ui, rows| {
                        for row in rows {
                            ui.horizontal(|ui| {
                                for &index in matches.iter().skip(row * columns).take(columns) {
                                    let index = data.with_tone(index, self.prefs.emoji_tone);
                                    ui.push_id(index, |ui| {
                                        let response = crate::emoji::button(ui, index, 28.0);
                                        if response.hovered() {
                                            hovered = Some(index);
                                        }
                                        if response.clicked() {
                                            chosen = Some(index);
                                        }
                                    });
                                }
                            });
                        }
                    });
            });
        });
        ui.add_space(6.0);
        ui.separator();
        let footer_id = ui.id().with("emoji-hover-footer");
        if let Some(index) = hovered {
            ui.data_mut(|data| data.insert_temp(footer_id, index));
        }
        let index = ui
            .data(|memory| memory.get_temp::<usize>(footer_id))
            .unwrap_or_else(|| data.shortcode("grinning").unwrap());
        let entry = &data.entries[index];
        ui.horizontal(|ui| {
            ui.add(egui::Image::new((texture.id(), Vec2::splat(30.0))).uv(data.uv(index)));
            ui.vertical(|ui| {
                ui.add(
                    egui::Label::new(
                        RichText::new(format!(":{}:", entry.aliases[0])).size(font_size(12.0)),
                    )
                    .truncate(true),
                );
                ui.add(
                    egui::Label::new(
                        RichText::new(&entry.name)
                            .size(font_size(TEXT_CAPTION))
                            .color(c_muted()),
                    )
                    .truncate(true),
                );
            });
        });
        chosen
    }
}

#[cfg(test)]
mod panel_tests {
    use super::*;

    #[test]
    fn expression_panel_stays_above_composer_and_within_resized_viewport() {
        for size in [
            Vec2::new(1320.0, 850.0),
            Vec2::new(940.0, 650.0),
            Vec2::new(671.0, 464.0),
        ] {
            let screen = Rect::from_min_size(Pos2::ZERO, size);
            let composer = Rect::from_min_size(
                Pos2::new(100.0, size.y - 80.0),
                Vec2::new(size.x - 125.0, 50.0),
            );
            let rect = expression_panel_rect(screen, Some(composer));
            assert!(screen.contains_rect(rect));
            assert!(rect.top() >= 40.0);
            assert!(rect.bottom() <= composer.top() - 8.0);
            assert_eq!(rect.right(), composer.right());
        }
    }
}
