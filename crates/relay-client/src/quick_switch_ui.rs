//! Conversation navigation never starts a microphone or joins a call.
use super::*;

pub(super) struct Selection {
    query: String,
    index: usize,
    focus_input: bool,
}

impl Default for Selection {
    fn default() -> Self {
        Self {
            query: String::new(),
            index: 0,
            focus_input: true,
        }
    }
}

/// What kind of place a result is, drawn as its icon.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Glyph {
    Channel,
    Voice,
    Person,
}

struct Entry {
    target: Target,
    name: String,
    alias: String,
    kind: &'static str,
    prefix: Glyph,
}

/// Relay's semibold face, or the regular one where it is not installed (tests).
/// One result row, Discord's way: icon, name, the username in grey, and the
/// kind of place on the right. The selected row is filled.
fn result_row(ui: &mut egui::Ui, entry: &Entry, active: bool) -> egui::Response {
    let height = 36.0_f32.max(font_size(16.0) + 16.0);
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::click());
    response.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::WidgetType::SelectableLabel,
            active,
            format!("{} · {}", entry.name, entry.kind),
        )
    });
    let palette = UI_PALETTE.with(|palette| palette.get());
    if active {
        ui.painter().rect_filled(rect, 4.0, palette.selection);
    } else if response.hovered() {
        ui.painter().rect_filled(rect, 4.0, palette.hover);
    }
    let painter = ui.painter().with_clip_rect(rect);
    let icon_rect = Rect::from_center_size(
        Pos2::new(rect.left() + 20.0, rect.center().y),
        Vec2::splat(18.0),
    );
    match entry.prefix {
        Glyph::Voice => draw_icon(&painter, icon_rect, Icon::Headphones, c_muted()),
        Glyph::Channel | Glyph::Person => {
            painter.text(
                icon_rect.center(),
                Align2::CENTER_CENTER,
                if entry.prefix == Glyph::Channel {
                    "#"
                } else {
                    "@"
                },
                FontId::proportional(font_size(18.0)),
                c_muted(),
            );
        }
    }
    let kind = painter.layout_no_wrap(
        entry.kind.to_owned(),
        FontId::proportional(font_size(12.0)),
        c_muted(),
    );
    let kind_left = rect.right() - 12.0 - kind.size().x;
    let name = painter.layout_no_wrap(
        entry.name.clone(),
        semibold(ui, font_size(15.0)),
        if active {
            c_text()
        } else {
            c_text().gamma_multiply(0.9)
        },
    );
    let name_pos = Pos2::new(rect.left() + 40.0, rect.center().y - name.size().y * 0.5);
    let name_width = name.size().x;
    let name_clip = Rect::from_min_max(
        Pos2::new(rect.left(), rect.top()),
        Pos2::new(kind_left - 12.0, rect.bottom()),
    );
    let name_painter = painter.with_clip_rect(name_clip);
    name_painter.galley(name_pos, name, c_text());
    if !entry.alias.is_empty() && entry.alias != entry.name {
        name_painter.text(
            Pos2::new(name_pos.x + name_width + 8.0, rect.center().y),
            Align2::LEFT_CENTER,
            &entry.alias,
            FontId::proportional(font_size(13.0)),
            c_muted(),
        );
    }
    painter.galley(
        Pos2::new(kind_left, rect.center().y - kind.size().y * 0.5),
        kind,
        c_muted(),
    );
    response
}

fn matches<'a>(entries: &'a [Entry], query: &str) -> Vec<&'a Entry> {
    let query = query.trim().to_lowercase();
    let mut matches: Vec<_> = entries
        .iter()
        .filter_map(|entry| {
            let name = entry.name.to_lowercase();
            let alias = entry.alias.to_lowercase();
            let rank = if query.is_empty() || name == query || alias == query {
                0
            } else if name.starts_with(&query) || alias.starts_with(&query) {
                1
            } else if name.contains(&query) || alias.contains(&query) {
                2
            } else {
                return None;
            };
            Some((rank, entry))
        })
        .collect();
    // Stable sorting preserves the familiar channel/people order for empty queries.
    matches.sort_by_key(|(rank, _)| *rank);
    matches.into_iter().map(|(_, entry)| entry).collect()
}

fn picker(
    ui: &mut egui::Ui,
    query: &mut String,
    selection: &mut Selection,
    entries: &[Entry],
) -> Option<Target> {
    let input_id = egui::Id::new("quick-switch-input");
    // Consume before TextEdit, so arrows navigate results rather than its cursor.
    let (up, down, enter) = if ui.memory(|memory| memory.has_focus(input_id)) {
        ui.input_mut(|input| {
            (
                input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp),
                input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown),
                input.consume_key(egui::Modifiers::NONE, egui::Key::Enter),
            )
        })
    } else {
        (false, false, false)
    };
    let input = ui.add(
        egui::TextEdit::singleline(query)
            .id(input_id)
            .hint_text("Where would you like to go?")
            .font(FontId::proportional(font_size(18.0)))
            .margin(Vec2::new(12.0, 10.0))
            .desired_width(f32::INFINITY),
    );
    if selection.focus_input {
        input.request_focus();
        selection.focus_input = false;
    }
    let targets = matches(entries, query);
    let changed = selection.query != *query;
    if changed {
        selection.query.clone_from(query);
        selection.index = 0;
    }
    selection.index = selection.index.min(targets.len().saturating_sub(1));
    if !targets.is_empty() {
        if down {
            selection.index = (selection.index + 1) % targets.len();
        } else if up {
            selection.index = (selection.index + targets.len() - 1) % targets.len();
        }
    }
    ui.add_space(8.0);
    let mut selected = enter
        .then(|| {
            targets
                .get(selection.index)
                .map(|entry| entry.target.clone())
        })
        .flatten();
    if targets.is_empty() {
        ui.label(RichText::new("No conversations found").strong());
        ui.label(
            RichText::new("Try part of a channel name, display name or username.").color(c_muted()),
        );
    } else {
        egui::ScrollArea::vertical()
            .id_source("quick-switch-results")
            .max_height((ui.ctx().screen_rect().height() - 230.0).clamp(100.0, 420.0))
            .auto_shrink([false, true])
            .show(ui, |ui| {
                for (index, entry) in targets.iter().enumerate() {
                    let active = index == selection.index;
                    let response = result_row(ui, entry, active);
                    if active && (up || down || changed) {
                        response.scroll_to_me(Some(Align::Center));
                    }
                    if response.clicked() {
                        selected = Some(entry.target.clone());
                    }
                }
            });
    }
    ui.add_space(8.0);
    ui.label(
        RichText::new("↑ ↓ Choose · Enter Open · Esc Close")
            .size(font_size(12.0))
            .color(c_muted()),
    );
    ui.label(
        RichText::new("Opening a voice channel does not join the call.")
            .size(font_size(12.0))
            .color(c_muted()),
    );
    selected
}

impl RelayApp {
    pub(super) fn quick_switch_dialog(&mut self, ctx: &egui::Context) {
        let Some(bootstrap) = self.bootstrap.as_ref() else {
            return;
        };
        let mut entries: Vec<_> = bootstrap
            .channels
            .iter()
            .map(|channel| {
                let voice = channel.kind == "voice";
                Entry {
                    target: Target::Channel(channel.id.clone()),
                    name: channel.name.clone(),
                    alias: String::new(),
                    kind: self
                        .conversation_info(&channel.id)
                        .map(|info| match info.kind.as_str() {
                            "group" => "Group message",
                            "group_voice" => "Group voice",
                            "thread" => "Thread",
                            "forum" => "Forum",
                            "stage" => "Stage",
                            "announcement" => "Announcement",
                            _ => {
                                if voice {
                                    "Voice channel"
                                } else {
                                    "Text channel"
                                }
                            }
                        })
                        .unwrap_or(if voice {
                            "Voice channel"
                        } else {
                            "Text channel"
                        }),
                    prefix: if voice { Glyph::Voice } else { Glyph::Channel },
                }
            })
            .collect();
        entries.extend(
            bootstrap
                .members
                .iter()
                .filter(|user| user.id != bootstrap.user.id)
                .map(|user| Entry {
                    target: Target::Direct(user.id.clone()),
                    name: profile_ui::display_name(user).to_owned(),
                    alias: user.username.clone(),
                    kind: "Direct message",
                    prefix: Glyph::Person,
                }),
        );
        let mut open = true;
        let mut selected = None;
        // Discord's switcher has no title: the question is in the search field.
        desktop_ui::dialog(ctx, "Go to a conversation")
            .title_bar(false)
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width((ctx.screen_rect().width() - 64.0).clamp(260.0, 520.0))
            .show(ctx, |ui| {
                selected = picker(
                    ui,
                    &mut self.quick_query,
                    &mut self.quick_selection,
                    &entries,
                );
            });
        if let Some(target) = selected {
            self.selected_nav = !matches!(target, Target::Direct(_));
            self.select(target);
            open = false;
        }
        self.quick_switch &= open;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Event, Key, Modifiers, RawInput};

    fn entries() -> Vec<Entry> {
        ["general", "game-night", "games"]
            .into_iter()
            .enumerate()
            .map(|(index, name)| Entry {
                target: Target::Channel(index.to_string()),
                name: name.into(),
                alias: String::new(),
                kind: "Text channel",
                prefix: Glyph::Channel,
            })
            .collect()
    }

    fn frame(
        ctx: &egui::Context,
        query: &mut String,
        selection: &mut Selection,
        entries: &[Entry],
        key: Option<Key>,
    ) -> Option<Target> {
        let mut selected = None;
        let events = key
            .map(|key| {
                vec![Event::Key {
                    key,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: Modifiers::NONE,
                }]
            })
            .unwrap_or_default();
        let _ = ctx.run(
            RawInput {
                events,
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(940.0, 650.0))),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.label("Background conversation");
                });
                desktop_ui::dialog(ctx, "Go to a conversation").show(ctx, |ui| {
                    selected = picker(ui, query, selection, entries);
                });
            },
        );
        selected
    }

    #[test]
    fn opening_the_actual_dialog_focuses_search_without_a_mouse_click() {
        let ctx = egui::Context::default();
        let (mut query, mut selection) = (String::new(), Selection::default());
        let entries = entries();
        frame(&ctx, &mut query, &mut selection, &entries, None);
        frame(&ctx, &mut query, &mut selection, &entries, None);
        assert!(ctx.memory(|memory| memory.has_focus(egui::Id::new("quick-switch-input"))));
        frame(
            &ctx,
            &mut query,
            &mut selection,
            &entries,
            Some(Key::ArrowDown),
        );
        assert_eq!(selection.index, 1);
    }

    #[test]
    fn actual_arrow_keys_choose_second_result_and_enter_opens_it() {
        let ctx = egui::Context::default();
        let (mut query, mut selection) = (String::new(), Selection::default());
        let entries = entries();
        frame(&ctx, &mut query, &mut selection, &entries, None);
        ctx.memory_mut(|memory| memory.request_focus(egui::Id::new("quick-switch-input")));
        frame(&ctx, &mut query, &mut selection, &entries, None);
        frame(
            &ctx,
            &mut query,
            &mut selection,
            &entries,
            Some(Key::ArrowDown),
        );
        assert_eq!(selection.index, 1);
        assert!(ctx.memory(|memory| memory.has_focus(egui::Id::new("quick-switch-input"))));
        assert_eq!(
            frame(&ctx, &mut query, &mut selection, &entries, Some(Key::Enter)),
            Some(Target::Channel("1".into()))
        );
    }

    #[test]
    fn query_changes_reset_selection_and_empty_results_do_not_open_anything() {
        let ctx = egui::Context::default();
        let (mut query, mut selection) = (String::new(), Selection::default());
        let entries = entries();
        frame(&ctx, &mut query, &mut selection, &entries, None);
        ctx.memory_mut(|memory| memory.request_focus(egui::Id::new("quick-switch-input")));
        frame(&ctx, &mut query, &mut selection, &entries, None);
        frame(
            &ctx,
            &mut query,
            &mut selection,
            &entries,
            Some(Key::ArrowUp),
        );
        assert_eq!(selection.index, 2);
        query = "general".into();
        assert_eq!(
            frame(&ctx, &mut query, &mut selection, &entries, Some(Key::Enter)),
            Some(Target::Channel("0".into()))
        );
        query = "missing".into();
        assert_eq!(
            frame(&ctx, &mut query, &mut selection, &entries, Some(Key::Enter)),
            None
        );
        assert_eq!(selection.index, 0);
    }

    #[test]
    fn exact_and_prefix_matches_rank_first_and_usernames_remain_searchable() {
        let mut entries = entries();
        entries[0].name = "old-games".into();
        entries[1].name = "Games for Friday".into();
        assert_eq!(
            matches(&entries, "games")
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            ["games", "Games for Friday", "old-games"]
        );
        entries[0].name = "Display Name".into();
        entries[0].alias = "alex".into();
        assert_eq!(matches(&entries, "ALEX")[0].name, "Display Name");
    }
}
