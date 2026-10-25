fn welcome_banner_size(source: Vec2, width: f32, viewport_height: f32) -> Vec2 {
    let max_height = (viewport_height * 0.22).clamp(1.0, 160.0);
    let scale = (width.max(1.0) / source.x.max(1.0)).min(max_height / source.y.max(1.0));
    source * scale
}

/// The community welcome window: same id, width and fixed height as before,
/// with the shared dialog header instead of a title bar.
fn welcome_window<'a>(ctx: &egui::Context, title: &str, open: &'a mut bool) -> egui::Window<'a> {
    desktop_ui::dialog(ctx, title)
        .id(egui::Id::new("community-welcome"))
        .open(open)
        .title_bar(false)
        .default_width(500.0)
        .vscroll(false)
        .collapsible(false)
}

fn welcome_content(
    ui: &mut egui::Ui,
    name: &str,
    description: &str,
    banner: Option<egui::TextureHandle>,
) {
    ui.spacing_mut().scroll = egui::style::ScrollStyle::solid();
    // Reserve the footer for actions, even with a large banner or description.
    egui::ScrollArea::vertical()
        .id_source("welcome-content")
        .max_height(
            (ui.available_height() - 80.0)
                .max(60.0)
                .min((ui.ctx().screen_rect().height() - 240.0).max(60.0)),
        )
        .auto_shrink([false, true])
        .show(ui, |ui| {
            if let Some(texture) = banner {
                let size = welcome_banner_size(
                    texture.size_vec2(),
                    ui.available_width(),
                    ui.ctx().screen_rect().height(),
                );
                ui.vertical_centered(|ui| {
                    ui.add(egui::Image::new((texture.id(), size)).rounding(8.0));
                });
            }
            ui.label(RichText::new(name).size(font_size(25.0)).strong());
            if !description.is_empty() {
                ui.add(egui::Label::new(description).wrap(true));
            }
            ui.add_space(14.0);
            ui.add(
                egui::Label::new("Say hello, find a conversation, and make yourself at home.")
                    .wrap(true),
            );
        });
    ui.add_space(10.0);
}

#[cfg(test)]
mod welcome_layout_tests {
    use super::*;

    #[test]
    fn welcome_actions_stay_visible_with_tall_banners_and_long_descriptions() {
        let ctx = egui::Context::default();
        configure(&ctx);
        let banner = ctx.load_texture(
            "tall-banner",
            egui::ColorImage::new([10, 1000], Color32::BLUE),
            Default::default(),
        );
        let description = "A very long community description. ".repeat(300);
        for size in [
            Vec2::new(1320.0, 850.0),
            Vec2::new(940.0, 650.0),
            Vec2::new(670.0, 464.0),
            Vec2::new(360.0, 420.0),
        ] {
            let mut action = Rect::NOTHING;
            let mut clip = Rect::NOTHING;
            for _ in 0..4 {
                let _ = ctx.run(
                    egui::RawInput {
                        screen_rect: Some(Rect::from_min_size(Pos2::ZERO, size)),
                        ..Default::default()
                    },
                    |ctx| {
                        desktop_ui::dialog(ctx, "Welcome regression")
                            .default_width(500.0)
                            .vscroll(false)
                            .collapsible(false)
                            .show(ctx, |ui| {
                                welcome_content(
                                    ui,
                                    "Test community",
                                    &description,
                                    Some(banner.clone()),
                                );
                                ui.horizontal_wrapped(|ui| {
                                    ui.add(primary("Open #general"));
                                    action = ui.add(secondary("Continue")).rect;
                                    clip = ui.clip_rect();
                                });
                            });
                    },
                );
            }
            assert!(
                clip.contains_rect(action),
                "{size:?}: action {action:?}, clip {clip:?}"
            );
            assert!(
                action.top() >= 32.0 && action.bottom() <= size.y,
                "{size:?}: {action:?}"
            );
        }
    }
}

impl RelayApp {
    pub(super) fn maybe_open_welcome(&mut self) {
        if self.capture_path.is_some() || !self.has_chat_capability("welcome-v1") {
            return;
        }
        let Some(b) = self.bootstrap.as_ref() else {
            return;
        };
        let scope = format!("{}|{}", b.server_id, b.user.id);
        if !self.prefs.welcome_seen.contains(&scope) {
            if !fresh_host(b) && !self.in_people() {
                self.prefs.host_checklist.begin_joined(b);
            }
            // A fresh host already gets the sharing guide and the first-steps
            // checklist; a second welcome dialog would compete with both.
            self.admin_ui.welcome_open = !fresh_host(b);
            self.prefs.welcome_seen.push(scope);
            if self.prefs.welcome_seen.len() > 64 {
                self.prefs.welcome_seen.remove(0);
            }
            self.defer_preferences();
        }
    }
    pub(super) fn open_welcome(&mut self) {
        self.admin_ui.welcome_open = true;
    }
    pub(super) fn open_system_notices(&mut self) {
        self.admin_ui.system_open = true;
    }
    pub(super) fn open_community_search(&mut self) {
        self.admin_ui.search_open = true;
        self.admin_ui.search_focus = true;
    }
    pub(super) fn open_filtered_history_search(&mut self) {
        let Some(target) = self.current.clone() else {
            return;
        };
        self.admin_ui.search_filters = Default::default();
        match target {
            Target::Channel(id) => self.admin_ui.search_filters.channel_id = Some(id),
            Target::Direct(id) => self.admin_ui.search_filters.peer_id = Some(id),
        }
        self.admin_ui.search_period = 0;
        self.admin_ui.search_query = self.search.clone();
        self.admin_ui.search_applied.clear();
        self.admin_ui.search_applied_filters = Default::default();
        self.admin_ui.search_messages.clear();
        self.admin_ui.search_remaining.clear();
        self.admin_ui.search_errors.clear();
        // A late result from a previous scope must not populate the newly opened search.
        self.admin_requests
            .retain(|_, key| key != "community-search");
        self.open_community_search();
    }
    pub(super) fn community_notice_bar(&mut self, ui: &mut egui::Ui) {
        let Some(channel) = self
            .bootstrap
            .as_ref()
            .and_then(|b| b.server_presentation.system_channel_id.as_ref())
            .filter(|id| self.current == Some(Target::Channel((*id).clone())))
            .cloned()
        else {
            return;
        };
        let key = format!("system-notices:{channel}");
        self.ensure_admin_data(&key, "/api/system-events");
        let events: Vec<CommunitySystemEvent> = self
            .admin_data
            .get(&key)
            .cloned()
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default();
        egui::CollapsingHeader::new("Community notices")
            .id_source("community-notice-bar")
            .default_open(true)
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .id_source("community-notice-scroll")
                    .max_height(100.0)
                    .show(ui, |ui| {
                        for event in events
                            .iter()
                            .filter(|event| !self.is_blocked(&event.user.id))
                        {
                            ui.label(format!(
                                "{} {} the community",
                                profile_ui::display_name(&event.user),
                                if event.kind == "joined" {
                                    "joined"
                                } else {
                                    "left"
                                }
                            ));
                        }
                        if self.admin_requests.values().any(|pending| pending == &key) {
                            ui.spinner();
                        } else if events.is_empty() {
                            ui.label(RichText::new("No recent notices.").color(c_muted()));
                        }
                    });
                if ui.small_button("View all notices").clicked() {
                    self.open_system_notices();
                }
            });
        ui.separator();
    }
    fn community_search_filters(&mut self, ui: &mut egui::Ui, b: &Bootstrap, pending: bool) {
        let width = ui.available_width().min(150.0);
        ui.add_enabled_ui(!pending, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.vertical(|ui| {
                    ui.set_width(width);
                    ui.label(
                        RichText::new("FROM")
                            .size(font_size(TEXT_CAPTION))
                            .color(c_muted()),
                    );
                    let selected = self
                        .admin_ui
                        .search_filters
                        .author_id
                        .as_ref()
                        .and_then(|id| b.members.iter().find(|member| member.id == *id))
                        .map(profile_ui::display_name)
                        .unwrap_or("Anyone");
                    let text =
                        crate::settings_ui::single_line(ui, selected, 14.0, c_text(), width - 36.0);
                    egui::ComboBox::from_id_source("search-from")
                        .width(width)
                        .selected_text(text)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut self.admin_ui.search_filters.author_id,
                                None,
                                "Anyone",
                            );
                            for member in b
                                .members
                                .iter()
                                .filter(|member| !b.social.blocked.contains(&member.id))
                            {
                                let name = profile_ui::display_name(member);
                                let text = crate::settings_ui::single_line(
                                    ui,
                                    name,
                                    14.0,
                                    c_text(),
                                    width - 20.0,
                                );
                                ui.selectable_value(
                                    &mut self.admin_ui.search_filters.author_id,
                                    Some(member.id.clone()),
                                    text,
                                )
                                .on_hover_text(name);
                            }
                        });
                });
                ui.vertical(|ui| {
                    ui.set_width(width);
                    ui.label(
                        RichText::new("IN")
                            .size(font_size(TEXT_CAPTION))
                            .color(c_muted()),
                    );
                    let mut selected_target = self
                        .admin_ui
                        .search_filters
                        .channel_id
                        .clone()
                        .map(Target::Channel)
                        .or_else(|| {
                            self.admin_ui
                                .search_filters
                                .peer_id
                                .clone()
                                .map(Target::Direct)
                        });
                    let selected = match &selected_target {
                        Some(Target::Channel(id)) => b
                            .channels
                            .iter()
                            .find(|channel| channel.id == *id)
                            .map(|channel| format!("#{}", channel.name))
                            .unwrap_or_else(|| "Unavailable channel".into()),
                        Some(Target::Direct(id)) => b
                            .members
                            .iter()
                            .find(|member| member.id == *id)
                            .map(|member| format!("DM · {}", profile_ui::display_name(member)))
                            .unwrap_or_else(|| "Unavailable conversation".into()),
                        None => "Everywhere".into(),
                    };
                    let text = crate::settings_ui::single_line(
                        ui,
                        &selected,
                        14.0,
                        c_text(),
                        width - 36.0,
                    );
                    let response = egui::ComboBox::from_id_source("search-in")
                        .width(width)
                        .selected_text(text)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut selected_target, None, "Everywhere");
                            for channel in b.channels.iter().filter(|channel| {
                                channel.kind == "text"
                                    && b.read_states.iter().any(|state| {
                                        state.channel_id.as_deref() == Some(&channel.id)
                                    })
                            }) {
                                let name = format!("#{}", channel.name);
                                let text = crate::settings_ui::single_line(
                                    ui,
                                    &name,
                                    14.0,
                                    c_text(),
                                    width - 20.0,
                                );
                                ui.selectable_value(
                                    &mut selected_target,
                                    Some(Target::Channel(channel.id.clone())),
                                    text,
                                )
                                .on_hover_text(name);
                            }
                            for member in b.members.iter().filter(|member| {
                                !b.social.blocked.contains(&member.id)
                                    && b.read_states
                                        .iter()
                                        .any(|state| state.peer_id.as_deref() == Some(&member.id))
                            }) {
                                let name = format!("DM · {}", profile_ui::display_name(member));
                                let text = crate::settings_ui::single_line(
                                    ui,
                                    &name,
                                    14.0,
                                    c_text(),
                                    width - 20.0,
                                );
                                ui.selectable_value(
                                    &mut selected_target,
                                    Some(Target::Direct(member.id.clone())),
                                    text,
                                )
                                .on_hover_text(name);
                            }
                        });
                    response.response.widget_info(|| {
                        egui::WidgetInfo::labeled(
                            egui::WidgetType::ComboBox,
                            format!("In: {selected}"),
                        )
                    });
                    match selected_target {
                        Some(Target::Channel(id)) => {
                            self.admin_ui.search_filters.channel_id = Some(id);
                            self.admin_ui.search_filters.peer_id = None;
                        }
                        Some(Target::Direct(id)) => {
                            self.admin_ui.search_filters.channel_id = None;
                            self.admin_ui.search_filters.peer_id = Some(id);
                        }
                        None => {
                            self.admin_ui.search_filters.channel_id = None;
                            self.admin_ui.search_filters.peer_id = None;
                        }
                    }
                });
                ui.vertical(|ui| {
                    ui.set_width(width);
                    ui.label(
                        RichText::new("HAS")
                            .size(font_size(TEXT_CAPTION))
                            .color(c_muted()),
                    );
                    egui::ComboBox::from_id_source("search-has")
                        .width(width)
                        .selected_text(self.admin_ui.search_filters.content.label())
                        .show_ui(ui, |ui| {
                            for kind in crate::search_filters::ContentKind::ALL {
                                ui.selectable_value(
                                    &mut self.admin_ui.search_filters.content,
                                    kind,
                                    kind.label(),
                                );
                            }
                        });
                });
                ui.vertical(|ui| {
                    ui.set_width(width);
                    ui.label(
                        RichText::new("WHEN")
                            .size(font_size(TEXT_CAPTION))
                            .color(c_muted()),
                    );
                    egui::ComboBox::from_id_source("search-when")
                        .width(width)
                        .selected_text(match self.admin_ui.search_period {
                            1 => "Last 24 hours",
                            2 => "Last 7 days",
                            3 => "Last 30 days",
                            _ => "Any time",
                        })
                        .show_ui(ui, |ui| {
                            for (period, name) in [
                                (0, "Any time"),
                                (1, "Last 24 hours"),
                                (2, "Last 7 days"),
                                (3, "Last 30 days"),
                            ] {
                                ui.selectable_value(&mut self.admin_ui.search_period, period, name);
                            }
                        });
                });
            });
            if (self.admin_ui.search_filters.active() || self.admin_ui.search_period != 0)
                && ui.small_button("Clear filters").clicked()
            {
                self.admin_ui.search_filters = Default::default();
                self.admin_ui.search_period = 0;
            }
        });
        ui.add_space(8.0);
    }

    pub(super) fn load_community_search(&mut self, refresh: bool) {
        if self
            .admin_requests
            .values()
            .any(|key| key == "community-search")
        {
            return;
        }
        let Some(origin) = self.command_origin() else {
            return;
        };
        let query = if refresh {
            self.admin_ui.search_query.trim().to_owned()
        } else {
            self.admin_ui.search_applied.clone()
        };
        let mut filters = if refresh {
            self.admin_ui.search_filters.clone()
        } else {
            self.admin_ui.search_applied_filters.clone()
        };
        if refresh {
            filters.since = match self.admin_ui.search_period {
                1 => Some(chrono::Utc::now().timestamp() - 86_400),
                2 => Some(chrono::Utc::now().timestamp() - 7 * 86_400),
                3 => Some(chrono::Utc::now().timestamp() - 30 * 86_400),
                _ => None,
            };
        }
        if (query.is_empty() && !filters.active()) || query.chars().count() > 300 {
            return;
        }
        let remaining = if refresh {
            self.admin_ui.search_messages.clear();
            self.admin_ui.search_errors.clear();
            self.admin_ui.search_remaining.clear();
            None
        } else {
            Some(self.admin_ui.search_remaining.clone())
        };
        let request_id = self.next_request();
        if let Err(error) = self.network.send(Command::CommunitySearch {
            origin,
            query,
            filters: filters.clone(),
            remaining,
            request_id,
        }) {
            self.error = Some(error);
        } else {
            self.admin_ui.search_applied_filters = filters;
            self.admin_requests
                .insert(request_id, "community-search".into());
        }
    }
    pub(super) fn apply_community_search(
        &mut self,
        request_id: u64,
        query: String,
        messages: Vec<(Target, Message)>,
        remaining: Vec<(Target, Option<u64>)>,
        errors: Vec<String>,
    ) {
        if self.admin_requests.remove(&request_id).as_deref() != Some("community-search") {
            return;
        }
        self.admin_ui.search_applied = query;
        for (target, message) in messages {
            if let Some(old) = self
                .admin_ui
                .search_messages
                .iter_mut()
                .find(|(t, m)| t == &target && m.id == message.id)
            {
                *old = (target, message);
            } else {
                self.admin_ui.search_messages.push((target, message));
            }
        }
        self.admin_ui
            .search_messages
            .sort_by_key(|(_, m)| std::cmp::Reverse(m.sequence));
        self.admin_ui.search_remaining = remaining;
        self.admin_ui.search_errors.extend(errors);
    }
    /// True while `community_tools_dialogs` draws the welcome (its header then
    /// owns Escape).
    pub(super) fn welcome_dialog_shown(&self) -> bool {
        self.bootstrap.is_some() && self.admin_ui.welcome_open
    }
    pub(super) fn community_tools_dialogs(&mut self, ctx: &egui::Context) {
        let Some(b) = self.bootstrap.clone() else {
            return;
        };
        if self.admin_ui.welcome_open {
            let mut open = true;
            let title = format!("Welcome to {}", b.server_name);
            welcome_window(ctx, &title, &mut open).show(ctx, |ui| {
                // The shared header stays above the welcome's own
                // scrolling content; Close and Escape only close it.
                if profile_ui::dialog_header_ui::dialog_header(ui, &title) {
                    self.admin_ui.welcome_open = false;
                }
                welcome_content(
                    ui,
                    &b.server_name,
                    &b.server_presentation.description,
                    self.asset_texture("server_banner", "community"),
                );
                ui.horizontal_wrapped(|ui| {
                    if let Some(channel) = b
                        .server_presentation
                        .welcome_channel_id
                        .as_ref()
                        .and_then(|id| b.channels.iter().find(|c| c.id == *id))
                    {
                        if ui.add(primary("Continue")).clicked() {
                            self.admin_ui.welcome_open = false;
                        }
                        if ui
                            .add(secondary(&format!("Open #{}", channel.name)))
                            .clicked()
                        {
                            self.select(Target::Channel(channel.id.clone()));
                            self.admin_ui.welcome_open = false;
                        }
                    } else if ui.add(primary("Continue")).clicked() {
                        self.admin_ui.welcome_open = false;
                    }
                });
            });
            self.admin_ui.welcome_open &= open;
        }
        if self.admin_ui.system_open {
            if let Some(channel) = b
                .server_presentation
                .system_channel_id
                .as_ref()
                .and_then(|id| b.channels.iter().find(|c| c.id == *id))
            {
                let key = format!("system-notices:{}", channel.id);
                self.ensure_admin_data(&key, "/api/system-events");
                let pending = self.admin_requests.values().any(|value| value == &key);
                let events: Vec<CommunitySystemEvent> = self
                    .admin_data
                    .get(&key)
                    .cloned()
                    .and_then(|v| serde_json::from_value(v).ok())
                    .unwrap_or_default();
                let mut open = true;
                let window =
                    desktop_ui::dialog(ctx, format!("Community notices · #{}", channel.name))
                        .id(egui::Id::new("system-notices"))
                        .open(&mut open)
                        .default_width(500.0)
                        .max_height((ctx.screen_rect().height() - 60.0).max(200.0))
                        .collapsible(false);
                // The shared header (title, Close, Escape) stays above the scrolling body.
                if host_setup_ui::show_with_header(
                    ctx,
                    window,
                    &format!("Community notices · #{}", channel.name),
                    |ui| {
                        ui.label("People joining and leaving this community.");
                        ui.horizontal_wrapped(|ui| {
                            if ui.button("Refresh").clicked() {
                                self.refresh_admin_data(&key, "/api/system-events");
                            }
                            if ui.button("Open Channel").clicked() {
                                self.select(Target::Channel(channel.id.clone()));
                                self.admin_ui.system_open = false;
                            }
                        });
                        for event in &events {
                            if self.is_blocked(&event.user.id) {
                                continue;
                            }
                            egui::Frame::none()
                                .fill(c_card())
                                .rounding(6.0)
                                .inner_margin(Margin::same(12.0))
                                .show(ui, |ui| {
                                    ui.label(format!(
                                        "{} {} the community",
                                        profile_ui::display_name(&event.user),
                                        if event.kind == "joined" {
                                            "joined"
                                        } else {
                                            "left"
                                        }
                                    ));
                                    ui.label(
                                        RichText::new(
                                            chrono::DateTime::parse_from_rfc3339(&event.created_at)
                                                .map(|time| {
                                                    time.with_timezone(&chrono::Local)
                                                        .format("%d %b %Y · %H:%M")
                                                        .to_string()
                                                })
                                                .unwrap_or_else(|_| event.created_at.clone()),
                                        )
                                        .size(font_size(12.0))
                                        .color(c_muted()),
                                    )
                                    .on_hover_text(&event.created_at);
                                });
                            ui.add_space(6.0);
                        }
                        if pending {
                            ui.spinner();
                            ui.label("Loading community notices…");
                        } else if events.is_empty() {
                            ui.label("No recent notices.");
                        }
                    },
                ) {
                    open = false;
                }
                self.admin_ui.system_open &= open;
            } else {
                self.admin_ui.system_open = false;
            }
        }
        if self.admin_ui.search_open {
            let pending = self
                .admin_requests
                .values()
                .any(|key| key == "community-search");
            let mut open = true;
            desktop_ui::dialog(ctx, "Search history")
                .id(egui::Id::new("community-search"))
                .open(&mut open)
                .title_bar(false)
                .default_width(580.0)
                .max_height((ctx.screen_rect().height() - 60.0).max(200.0))
                .vscroll(false)
                .collapsible(false)
                .show(ctx, |ui| {
                    // The shared header (title, Close, Escape) stays above the
                    // filters and the search's own scrolling results.
                    if profile_ui::dialog_header_ui::dialog_header(ui, "Search history") {
                        self.admin_ui.search_open = false;
                    }
                    ui.label("Find messages across the conversations you can access.");
                    self.community_search_filters(ui, &b, pending);
                    let mut search = false;
                    ui.horizontal_wrapped(|ui| {
                        let edit = ui.add(
                            egui::TextEdit::singleline(&mut self.admin_ui.search_query)
                                .char_limit(300)
                                .hint_text("Search messages or file names")
                                .desired_width((ui.available_width() - 100.0).max(100.0)),
                        );
                        if std::mem::take(&mut self.admin_ui.search_focus) {
                            edit.request_focus();
                        }
                        search = ui
                            .add_enabled(
                                !pending
                                    && (!self.admin_ui.search_query.trim().is_empty()
                                        || self.admin_ui.search_filters.active()
                                        || self.admin_ui.search_period != 0),
                                primary("Search"),
                            )
                            .clicked()
                            || (!pending
                                && edit.lost_focus()
                                && ui.input(|i| i.key_pressed(egui::Key::Enter)));
                    });
                    if search {
                        self.load_community_search(true);
                    }
                    if pending {
                        ui.spinner();
                        ui.label("Searching conversation history on this device…");
                    }
                    let applied = !self.admin_ui.search_applied.is_empty()
                        || self.admin_ui.search_applied_filters.active();
                    if applied {
                        let title = if self.admin_ui.search_applied.is_empty() {
                            "Filtered history".into()
                        } else {
                            format!("Results for “{}”", self.admin_ui.search_applied)
                        };
                        ui.label(format!(
                            "{} · {} matches",
                            title,
                            self.admin_ui.search_messages.len()
                        ));
                    }
                    let result_height = (ctx.screen_rect().height()
                        - 100.0
                        - (ui.cursor().top() - ui.min_rect().top()))
                    .clamp(70.0, 320.0);
                    egui::ScrollArea::vertical()
                        .id_source("community-search-results")
                        .max_height(result_height)
                        .show(ui, |ui| {
                            for (target, message) in self.admin_ui.search_messages.clone() {
                                if self.is_blocked(&message.author.id) {
                                    continue;
                                }
                                let name = match &target {
                                    Target::Channel(id) => b
                                        .channels
                                        .iter()
                                        .find(|c| c.id == *id)
                                        .map(|c| format!("#{}", c.name)),
                                    Target::Direct(id) => {
                                        b.members.iter().find(|u| u.id == *id).map(|u| {
                                            format!(
                                                "Direct message · {}",
                                                profile_ui::display_name(u)
                                            )
                                        })
                                    }
                                };
                                let Some(name) = name else {
                                    continue;
                                };
                                egui::Frame::none()
                                    .fill(c_card())
                                    .rounding(6.0)
                                    .inner_margin(Margin::same(12.0))
                                    .show(ui, |ui| {
                                        ui.label(
                                            RichText::new(format!(
                                                "{} · {}",
                                                name,
                                                profile_ui::display_name(&message.author)
                                            ))
                                            .strong(),
                                        );
                                        if ui.button("Jump to Message").clicked() {
                                            self.select(target.clone());
                                            self.reveal_message(message.id.clone());
                                            self.admin_ui.search_open = false;
                                        }
                                        self.message_preview(ui, &message);
                                        for file in &message.attachments {
                                            ui.label(&file.name);
                                        }
                                    });
                                ui.add_space(8.0);
                            }
                            if !pending && applied && self.admin_ui.search_messages.is_empty() {
                                ui.label("No matches in the history checked so far.");
                            }
                            if !self.admin_ui.search_remaining.is_empty()
                                && ui
                                    .add_enabled(!pending, secondary("Search More History"))
                                    .clicked()
                            {
                                self.load_community_search(false);
                            }
                            if !self.admin_ui.search_errors.is_empty() {
                                ui.collapsing("Some conversations need attention", |ui| {
                                    for error in &self.admin_ui.search_errors {
                                        ui.label(error);
                                    }
                                });
                            }
                        });
                });
            self.admin_ui.search_open &= open;
        }
    }
}

/// First steps for the host and a newly joined member. Progress comes from
/// server-confirmed messages and a connected shared room, never from clicks.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct HostChecklistMemory {
    /// Communities where this account could invite and was alone when first seen.
    #[serde(default)]
    started: Vec<String>,
    #[serde(default)]
    joined: Vec<String>,
    #[serde(default)]
    said_hello: Vec<String>,
    #[serde(default)]
    connected_together: Vec<String>,
    #[serde(default)]
    dismissed: Vec<String>,
}

const HOST_CHECKLIST_SCOPES: usize = 64;

/// Returns true when the list changed, so the caller saves preferences only then.
fn remember_scope(list: &mut Vec<String>, scope: &str) -> bool {
    if list.iter().any(|entry| entry == scope) {
        return false;
    }
    list.push(scope.to_owned());
    if list.len() > HOST_CHECKLIST_SCOPES {
        list.remove(0);
    }
    true
}

/// Alone in the community and allowed to invite: someone who just created it.
fn fresh_host(b: &Bootstrap) -> bool {
    // Bootstrap members include the account itself.
    b.members.len() <= 1 && b.has_permission(PERMISSION_INVITES)
}

fn community_scope(b: &Bootstrap) -> String {
    format!("{}|{}", b.server_id, b.user.id)
}

/// The one text channel the "Say hello" step opens and checks: #general, or
/// the first text channel when the community has no #general.
fn host_hello_channel(b: &Bootstrap) -> Option<&Channel> {
    let text = |channel: &&Channel| channel.kind == "text" && channel.can_send;
    b.channels
        .iter()
        .filter(text)
        .find(|channel| channel.name == "general")
        .or_else(|| b.channels.iter().find(text))
}

/// Lead both PCs to the same room when someone has already joined voice.
fn host_voice_channel(b: &Bootstrap) -> Option<&Channel> {
    let joinable = |channel: &&Channel| channel.kind == "voice" && channel.can_join;
    b.channels.iter().filter(joinable).find(|channel| {
        b.voice_members.iter().any(|member| {
            member.channel_id == channel.id && member.user.id != b.user.id
        })
    }).or_else(|| b.channels.iter().find(joinable))
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct HostSteps {
    pub joining: bool,
    pub invited: bool,
    pub said_hello: bool,
    pub tried_voice: bool,
}

impl HostSteps {
    fn complete(self) -> bool {
        self.invited && self.said_hello && self.tried_voice
    }
}

impl HostChecklistMemory {
    /// Called on the account's first community welcome, never for Friends.
    fn begin_joined(&mut self, b: &Bootstrap) -> bool {
        if host_hello_channel(b).is_none() && host_voice_channel(b).is_none() {
            return false;
        }
        let scope = community_scope(b);
        let changed = remember_scope(&mut self.started, &scope);
        remember_scope(&mut self.joined, &scope) || changed
    }

    /// `delivered` are server-confirmed messages (never pending sends), and
    /// `connected_voice` is the channel of a working voice connection.
    /// Returns true when something new was remembered.
    pub(super) fn observe(
        &mut self,
        b: &Bootstrap,
        delivered: &[Message],
        connected_voice: Option<&str>,
    ) -> bool {
        let scope = community_scope(b);
        let mut changed = fresh_host(b) && remember_scope(&mut self.started, &scope);
        if !self.started.contains(&scope) {
            return changed;
        }
        let hello = host_hello_channel(b).map(|channel| channel.id.as_str());
        let said_hello = hello.is_some_and(|hello| {
            delivered.iter().any(|message| {
                message.author.id == b.user.id
                    && message.recipient_id.is_none()
                    && message.channel_id.as_deref() == Some(hello)
            })
        });
        if said_hello {
            changed |= remember_scope(&mut self.said_hello, &scope);
        }
        let in_voice = connected_voice.is_some_and(|voice| {
            b.channels.iter().any(|channel| channel.id == voice && channel.kind == "voice")
                && b.voice_members.iter().any(|member| {
                    member.channel_id == voice && member.user.id != b.user.id
                })
        });
        if in_voice {
            changed |= remember_scope(&mut self.connected_together, &scope);
        }
        changed
    }

    /// Missing permissions/channels are not tasks the member can complete.
    pub(super) fn steps(&self, b: &Bootstrap) -> Option<HostSteps> {
        let scope = community_scope(b);
        if !self.started.contains(&scope) || self.dismissed.contains(&scope) {
            return None;
        }
        let steps = HostSteps {
            joining: self.joined.contains(&scope),
            invited: self.joined.contains(&scope) || b.members.len() > 1,
            said_hello: host_hello_channel(b).is_none() || self.said_hello.contains(&scope),
            tried_voice: host_voice_channel(b).is_none() || self.connected_together.contains(&scope),
        };
        (!steps.complete()).then_some(steps)
    }

    /// Returns true when the community was not dismissed before.
    pub(super) fn dismiss(&mut self, b: &Bootstrap) -> bool {
        remember_scope(&mut self.dismissed, &community_scope(b))
    }
}

/// What the empty-channel hook did this frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct HostPanel {
    /// The checklist replaced the empty-state line.
    pub shown: bool,
    /// The checklist memory changed and preferences should be saved.
    pub changed: bool,
}

impl RelayApp {
    /// Hook, once per frame: record only confirmed progress. Returns true when
    /// the memory changed, so preferences are saved only on real changes.
    pub(super) fn observe_host_progress(&self, memory: &mut HostChecklistMemory) -> bool {
        let Some(b) = self.bootstrap.as_ref() else {
            return false;
        };
        // `messages` holds delivered messages of the open conversation; pending
        // sends live in `outgoing_messages` and do not count. `voice_channel`
        // is set by the join ack, before media runs, so require a connection.
        let connected_voice = self
            .voice_channel
            .as_deref()
            .filter(|_| self.voice_connected());
        memory.observe(b, &self.messages, connected_voice)
    }

    /// Remain available after the first message so the voice step is not lost.
    pub(super) fn host_next_steps_panel(
        &mut self,
        ui: &mut egui::Ui,
        memory: &mut HostChecklistMemory,
    ) -> HostPanel {
        // Community channels only; an empty direct conversation keeps its own text.
        if !matches!(self.current, Some(Target::Channel(_))) {
            return HostPanel::default();
        }
        let Some(b) = self.bootstrap.as_ref() else {
            return HostPanel::default();
        };
        let Some(steps) = memory.steps(b) else {
            return HostPanel::default();
        };
        let hello = host_hello_channel(b).map(|channel| (channel.id.clone(), channel.name.clone()));
        let voice = host_voice_channel(b).map(|channel| channel.id.clone());
        let click = host_checklist_card(
            ui,
            steps,
            hello.as_ref().map(|(_, name)| name.as_str()),
            voice.is_some(),
            !self.messages.is_empty(),
        );
        let mut changed = false;
        match click {
            Some(HostClick::Invite) => self.open_friend_sharing(),
            Some(HostClick::Hello) => {
                if let Some((id, _)) = hello {
                    self.select(Target::Channel(id));
                    let composer = egui::Id::new(("composer-editor", self.current.clone()));
                    ui.ctx().memory_mut(|memory| memory.request_focus(composer));
                }
            }
            Some(HostClick::Voice) => {
                if let Some(id) = voice {
                    if self.voice_channel.as_ref() == Some(&id) {
                        self.select(Target::Channel(id));
                    } else {
                        self.join_voice(id);
                    }
                }
            }
            Some(HostClick::Dismiss) => {
                if let Some(b) = self.bootstrap.as_ref() {
                    changed = memory.dismiss(b);
                }
            }
            None => {}
        }
        HostPanel {
            shown: true,
            changed,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HostClick {
    Invite,
    Hello,
    Voice,
    Dismiss,
}

/// The card itself, without app state, so its layout can be tested.
fn host_checklist_card(
    ui: &mut egui::Ui,
    steps: HostSteps,
    hello: Option<&str>,
    voice: bool,
    compact: bool,
) -> Option<HostClick> {
    let mut click = None;
    if compact {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("Next step").strong());
            if !steps.invited {
                if ui.add(primary("Invite a friend")).clicked() { click = Some(HostClick::Invite); }
            } else if !steps.said_hello {
                if let Some(name) = hello {
                    if ui.add(secondary(&format!("Say hello in #{name}"))).clicked() { click = Some(HostClick::Hello); }
                }
            } else if !steps.tried_voice && voice {
                if ui.add(primary("Join voice together")).clicked() { click = Some(HostClick::Voice); }
            }
            if ui.small_button("Dismiss").clicked() { click = Some(HostClick::Dismiss); }
        });
        ui.add_space(6.0);
        return click;
    }
    ui.add_space(24.0);
    egui::Frame::none()
        .fill(c_panel())
        .rounding(8.0)
        .inner_margin(Margin::same(20.0))
        .show(ui, |ui| {
            ui.set_max_width(ui.available_width().min(460.0));
            ui.add(
                egui::Label::new(
                    RichText::new(if steps.joining { "You joined the community" } else { "Your community is ready" })
                        .size(font_size(18.0))
                        .strong(),
                )
                .wrap(true),
            );
            ui.add_space(4.0);
            ui.add(
                egui::Label::new(
                    RichText::new(if steps.joining {
                        "Say hello, then join the same voice room as your friend. Once connected, check that you can hear each other."
                    } else {
                        "Send an invitation to the other PC, then say hello and join voice together. Keep this host running while friends use it."
                    })
                        .color(c_muted()),
                )
                .wrap(true),
            );
            ui.add_space(14.0);
            if steps.invited {
                host_step_done(ui, if steps.joining { "Joined with your account" } else { "A friend has joined" });
            } else if ui
                .add(primary("Invite a friend").min_size(Vec2::new(ui.available_width(), 40.0)))
                .clicked()
            {
                click = Some(HostClick::Invite);
            }
            ui.add_space(8.0);
            if let Some(name) = hello {
                if steps.said_hello {
                    host_step_done(ui, &format!("You said hello in #{name}"));
                } else if ui
                    .add(secondary(&format!("Say hello in #{name}")))
                    .clicked()
                {
                    click = Some(HostClick::Hello);
                }
            }
            ui.add_space(8.0);
            if voice {
                if steps.tried_voice {
                    host_step_done(ui, "Joined the same voice room");
                } else if ui.add(secondary("Join voice together")).clicked() {
                    click = Some(HostClick::Voice);
                }
            }
            ui.add_space(12.0);
            if ui
                .add(egui::Button::new(RichText::new("Dismiss").color(c_muted())).frame(false))
                .clicked()
            {
                click = Some(HostClick::Dismiss);
            }
        });
    click
}

fn host_step_done(ui: &mut egui::Ui, text: &str) {
    ui.add(egui::Label::new(RichText::new(format!("✓  {text}")).color(c_good())).wrap(true));
}

#[cfg(test)]
mod host_checklist_tests {
    use super::*;

    fn user(id: &str, role: &str) -> serde_json::Value {
        json!({"id": id, "username": id, "role": role})
    }

    fn channel(id: &str, name: &str, kind: &str) -> serde_json::Value {
        json!({"id": id, "name": name, "kind": kind, "topic": ""})
    }

    fn community(
        server: &str,
        account: &str,
        members: usize,
        role: &str,
        channels: Vec<serde_json::Value>,
    ) -> Bootstrap {
        let others = (1..members).map(|n| user(&format!("friend{n}"), "member"));
        let members: Vec<_> = std::iter::once(user(account, role)).chain(others).collect();
        serde_json::from_value(json!({
            "server_name": "Home", "server_id": server, "user": user(account, role),
            "channels": channels, "members": members, "voice_members": []
        }))
        .expect("minimal bootstrap fixture")
    }

    /// #general, #random and a voice channel, with ids unique per server.
    fn bootstrap(server: &str, members: usize, role: &str) -> Bootstrap {
        community(
            server,
            "owner",
            members,
            role,
            vec![
                channel(&format!("{server}-random"), "random", "text"),
                channel(&format!("{server}-general"), "general", "text"),
                channel(&format!("{server}-voice"), "Lounge", "voice"),
            ],
        )
    }

    fn message(author: &str, channel: Option<&str>, recipient: Option<&str>) -> Message {
        serde_json::from_value(json!({
            "id": "m1", "channel_id": channel, "recipient_id": recipient,
            "author": user(author, "member"), "content": "hello",
            "created_at": "2026-10-15T20:00:00Z"
        }))
        .expect("minimal message fixture")
    }

    fn hello_in(memory: &HostChecklistMemory, b: &Bootstrap) -> bool {
        memory.steps(b).is_some_and(|steps| steps.said_hello)
    }

    fn add_voice_peer(b: &mut Bootstrap, channel: &str) {
        b.voice_members.push(serde_json::from_value(json!({
            "user": user("friend1", "member"), "channel_id": channel,
            "muted": false, "deafened": false, "sharing": false, "camera": false
        })).unwrap());
    }

    #[test]
    fn fresh_host_sees_steps_and_members_never_do() {
        let mut memory = HostChecklistMemory::default();
        let host = bootstrap("s1", 1, "admin");
        assert!(memory.observe(&host, &[], None), "first sight is saved");
        assert!(
            !memory.observe(&host, &[], None),
            "an unchanged frame is not"
        );
        assert_eq!(memory.steps(&host), Some(HostSteps::default()));
        // An account that cannot invite, and an admin of an existing busy
        // community, are not fresh hosts and keep the ordinary welcome.
        let member = bootstrap("s2", 1, "member");
        let own = [message("owner", Some("s2-general"), None)];
        assert!(!memory.observe(&member, &own, Some("s2-voice")));
        assert!(memory.steps(&member).is_none());
        let busy = bootstrap("s3", 5, "admin");
        assert!(!memory.observe(&busy, &[], None));
        assert!(memory.steps(&busy).is_none());
        assert_eq!(memory.started, ["s1|owner"]);
    }

    #[test]
    fn newly_joined_member_follows_delivered_hello_into_the_friends_voice_room() {
        let mut member = bootstrap("joined", 2, "member");
        member.channels[1].can_send = false;
        member.channels.push(serde_json::from_value(channel("friends-room", "Together", "voice")).unwrap());
        let mut memory = HostChecklistMemory::default();
        assert!(memory.begin_joined(&member));
        let first = memory.steps(&member).unwrap();
        assert!(first.joining && first.invited && !first.said_hello && !first.tried_voice);
        assert_eq!(host_hello_channel(&member).unwrap().id, "joined-random");
        // A read-only #general is never the suggested composer destination.
        memory.observe(&member, &[message("owner", Some("joined-general"), None)], None);
        assert!(!memory.steps(&member).unwrap().said_hello);
        memory.observe(&member, &[message("owner", Some("joined-random"), None)], Some("friends-room"));
        assert!(memory.steps(&member).unwrap().said_hello);
        assert!(!memory.steps(&member).unwrap().tried_voice);
        add_voice_peer(&mut member, "friends-room");
        assert_eq!(host_voice_channel(&member).unwrap().id, "friends-room");
        // A peer in another room is not a shared connection on this PC.
        memory.observe(&member, &[], Some("joined-voice"));
        assert!(memory.steps(&member).is_some());
        memory.observe(&member, &[], Some("friends-room"));
        assert!(memory.steps(&member).is_none());
        let restored: HostChecklistMemory = serde_json::from_str(&serde_json::to_string(&memory).unwrap()).unwrap();
        assert!(restored.steps(&member).is_none());
        let previous: HostChecklistMemory = serde_json::from_value(json!({
            "started": ["joined|owner"], "said_hello": ["joined|owner"],
            "tried_voice": ["joined|owner"]
        })).unwrap();
        assert!(!previous.steps(&member).unwrap().tried_voice,
            "old solo-connection progress does not prove a shared room");
    }

    #[test]
    fn hello_needs_own_delivered_message_in_the_hello_channel() {
        let mut memory = HostChecklistMemory::default();
        let host = bootstrap("s1", 1, "admin");
        memory.observe(&host, &[], None);
        for (wrong, why) in [
            (message("owner", Some("s1-random"), None), "another channel"),
            (message("owner", None, Some("friend1")), "a direct message"),
            (
                message("friend1", Some("s1-general"), None),
                "another author",
            ),
            (
                message("owner", Some("s2-general"), None),
                "another community",
            ),
        ] {
            assert!(!memory.observe(&host, &[wrong], None), "{why}");
            assert!(!hello_in(&memory, &host), "{why}");
        }
        // Pending sends never reach `delivered`; only the server copy counts.
        assert!(memory.observe(&host, &[message("owner", Some("s1-general"), None)], None));
        assert!(hello_in(&memory, &host));
    }

    #[test]
    fn hello_follows_the_same_channel_as_the_action_without_general() {
        let mut memory = HostChecklistMemory::default();
        let host = community(
            "s1",
            "owner",
            1,
            "admin",
            vec![
                channel("lobby", "lobby", "text"),
                channel("chat", "chat", "text"),
            ],
        );
        assert_eq!(
            host_hello_channel(&host).map(|c| c.name.as_str()),
            Some("lobby")
        );
        memory.observe(&host, &[message("owner", Some("chat"), None)], None);
        assert!(!hello_in(&memory, &host));
        memory.observe(&host, &[message("owner", Some("lobby"), None)], None);
        assert!(hello_in(&memory, &host));
        // #general wins over an earlier text channel, for action and check alike.
        let with_general = bootstrap("s2", 1, "admin");
        assert_eq!(
            host_hello_channel(&with_general).map(|c| c.id.as_str()),
            Some("s2-general")
        );
    }

    #[test]
    fn voice_needs_a_connection_together_in_this_community() {
        let mut memory = HostChecklistMemory::default();
        let mut host = bootstrap("s1", 1, "admin");
        memory.observe(&host, &[], None);
        // Join ack only, pending or failed media: no connected channel.
        assert!(!memory.observe(&host, &[], None));
        // Connected, but in another community's channel.
        assert!(!memory.observe(&host, &[], Some("s2-voice")));
        assert!(!memory.steps(&host).unwrap().tried_voice);
        // A solo connection is not progress through the shared-call step.
        assert!(!memory.observe(&host, &[], Some("s1-voice")));
        add_voice_peer(&mut host, "s1-voice");
        assert!(memory.observe(&host, &[], Some("s1-voice")));
        // Leaving voice later does not undo a step that really happened.
        assert!(!memory.observe(&host, &[], None));
        assert!(memory.steps(&host).unwrap().tried_voice);
    }

    #[test]
    fn progress_is_per_community_and_account_and_completes() {
        let mut memory = HostChecklistMemory::default();
        let mut alone = bootstrap("s1", 1, "admin");
        let mut other_account = alone.clone();
        other_account.user.id = "second".into();
        memory.observe(&alone, &[], None);
        memory.observe(&other_account, &[], None);
        add_voice_peer(&mut alone, "s1-voice");
        memory.observe(
            &alone,
            &[message("owner", Some("s1-general"), None)],
            Some("s1-voice"),
        );
        let steps = memory.steps(&alone).unwrap();
        assert!(steps.said_hello && steps.tried_voice && !steps.invited);
        assert_eq!(memory.steps(&other_account), Some(HostSteps::default()));
        let joined = bootstrap("s1", 2, "admin");
        assert!(!memory.observe(&joined, &[], None));
        assert_eq!(memory.steps(&joined), None, "all three steps done");
    }

    #[test]
    fn dismiss_is_per_community_and_survives_serialization() {
        let mut memory = HostChecklistMemory::default();
        let first = bootstrap("s1", 1, "admin");
        let second = bootstrap("s2", 1, "admin");
        memory.observe(&first, &[], None);
        memory.observe(&second, &[], None);
        assert!(memory.dismiss(&first), "a new dismissal is saved");
        assert!(!memory.dismiss(&first), "a repeated one is not");
        assert!(memory.steps(&first).is_none());
        assert!(memory.steps(&second).is_some());
        let saved = serde_json::to_string(&memory).unwrap();
        let loaded: HostChecklistMemory = serde_json::from_str(&saved).unwrap();
        assert_eq!(loaded, memory);
        assert!(loaded.steps(&first).is_none());
        // Preferences written before this field existed still load.
        assert_eq!(
            serde_json::from_str::<HostChecklistMemory>("{}").unwrap(),
            HostChecklistMemory::default()
        );
    }

    #[test]
    fn remembered_scopes_are_bounded() {
        let mut memory = HostChecklistMemory::default();
        for n in 0..(HOST_CHECKLIST_SCOPES + 10) {
            let server = format!("s{n}");
            let hello = [message("owner", Some(&format!("{server}-general")), None)];
            let voice = format!("{server}-voice");
            let mut b = bootstrap(&server, 1, "admin");
            add_voice_peer(&mut b, &voice);
            memory.observe(&b, &hello, Some(&voice));
        }
        assert_eq!(memory.started.len(), HOST_CHECKLIST_SCOPES);
        assert_eq!(memory.said_hello.len(), HOST_CHECKLIST_SCOPES);
        assert_eq!(memory.connected_together.len(), HOST_CHECKLIST_SCOPES);
        assert!(memory.steps(&bootstrap("s0", 1, "admin")).is_none());
    }

    #[test]
    fn card_fits_small_windows_zoom_and_large_text() {
        let long = "a-very-long-channel-name-that-has-to-wrap-somewhere";
        for (size, zoom, text) in [
            (Vec2::new(1320.0, 850.0), 1.0, 1.0),
            (Vec2::new(670.0, 464.0), 1.0, 1.0),
            (Vec2::new(360.0, 420.0), 1.0, 1.4),
            (Vec2::new(360.0, 420.0), 1.5, 1.4),
        ] {
            UI_FONT_SCALE.with(|scale| scale.set(text));
            let ctx = egui::Context::default();
            configure(&ctx);
            ctx.set_zoom_factor(zoom);
            let mut screen = Rect::NOTHING;
            let mut card = Rect::NOTHING;
            let mut dismiss_visible = false;
            for _ in 0..3 {
                let _ = ctx.run(
                    egui::RawInput {
                        screen_rect: Some(Rect::from_min_size(Pos2::ZERO, size)),
                        ..Default::default()
                    },
                    |ctx| {
                        screen = ctx.screen_rect();
                        egui::CentralPanel::default().show(ctx, |ui| {
                            egui::ScrollArea::vertical().show(ui, |ui| {
                                host_checklist_card(ui, HostSteps::default(), Some(long), true, false);
                                card = ui.min_rect();
                                dismiss_visible = ui.clip_rect().max.x >= card.right() - 0.5;
                            });
                        });
                    },
                );
            }
            UI_FONT_SCALE.with(|scale| scale.set(1.0));
            assert!(
                card.left() >= screen.left() && card.right() <= screen.right() + 0.5,
                "{size:?} zoom {zoom} text {text}: card {card:?} outside {screen:?}"
            );
            assert!(dismiss_visible, "{size:?} zoom {zoom} text {text}");
            assert!(card.height() > 120.0, "{size:?} zoom {zoom}: {card:?}");
        }
    }
}
