//! R8.2: the server rail and the main view, moved unchanged out of `app.rs`;
//! only the methods' visibility became `pub(super)`.
use super::*;

impl RelayApp {
    pub(super) fn rail(&mut self, ctx: &egui::Context) {
        egui::SidePanel::left("server-rail")
            .exact_width(56.0)
            .resizable(false)
            .frame(
                egui::Frame::none()
                    .fill(c_rail())
                    .inner_margin(Margin::symmetric(8.0, 12.0)),
            )
            .show(ctx, |ui| {
                ui.set_enabled(!self.modal_open());
                ui.vertical_centered(|ui| {
                    let (rect, response) =
                        ui.allocate_exact_size(Vec2::splat(48.0), Sense::click());
                    ui.painter().rect_filled(
                        rect,
                        if response.hovered() || !self.selected_nav {
                            16.0
                        } else {
                            24.0
                        },
                        if !self.selected_nav {
                            c_accent()
                        } else {
                            c_card()
                        },
                    );
                    draw_icon(ui.painter(), rect.shrink(12.0), Icon::Chat, c_text());
                    response.widget_info(|| {
                        egui::WidgetInfo::labeled(egui::WidgetType::Button, "Direct messages")
                    });
                    if response.on_hover_text("Direct messages").clicked() {
                        if self.central_account.account.is_some() {
                            self.open_people();
                        }
                        self.selected_nav = false;
                        self.home_page = Some(SettingsTab::Friends);
                    }
                    ui.add_space(10.0);
                    ui.separator();
                    ui.add_space(10.0);
                    egui::ScrollArea::vertical()
                        .id_source("community-rail")
                        .max_height((ui.available_height() - 50.0).max(80.0))
                        .show(ui, |ui| {
                            let saved = self.prefs.communities.clone();
                            for community in saved {
                                if self.people_url().as_deref() == Some(community.url.as_str()) {
                                    continue;
                                }
                                let current = community.url == self.login.url && self.selected_nav;
                                let unread: u64 = if community.url == self.login.url {
                                    self.bootstrap
                                        .as_ref()
                                        .map(|bootstrap| {
                                            bootstrap
                                                .read_states
                                                .iter()
                                                .map(|read| read.unread_count)
                                                .sum()
                                        })
                                        .unwrap_or_default()
                                } else {
                                    self.community_reads
                                        .get(&community.url)
                                        .map(|reads| {
                                            reads.iter().map(|read| read.unread_count).sum()
                                        })
                                        .unwrap_or_default()
                                };
                                let name = if community.name.is_empty() {
                                    community.url.as_str()
                                } else {
                                    community.name.as_str()
                                };
                                let (rect, response) =
                                    ui.allocate_exact_size(Vec2::splat(48.0), Sense::click());
                                if current {
                                    ui.painter().rect_filled(
                                        Rect::from_center_size(
                                            Pos2::new(rect.left() - 6.0, rect.center().y),
                                            Vec2::new(4.0, 32.0),
                                        ),
                                        2.0,
                                        c_text(),
                                    );
                                }
                                ui.painter().rect_filled(
                                    rect,
                                    if current || response.hovered() {
                                        16.0
                                    } else {
                                        24.0
                                    },
                                    if current { c_accent() } else { c_card() },
                                );
                                let initials: String = name
                                    .split_whitespace()
                                    .take(2)
                                    .filter_map(|word| word.chars().next())
                                    .collect();
                                ui.painter().text(
                                    rect.center(),
                                    Align2::CENTER_CENTER,
                                    initials,
                                    FontId::proportional(font_size(17.0)),
                                    c_text(),
                                );
                                if community.url == self.login.url {
                                    if let Some(texture) =
                                        self.asset_texture("server_icon", "community")
                                    {
                                        egui::Image::new((texture.id(), rect.size()))
                                            .rounding(if current { 16.0 } else { 24.0 })
                                            .paint_at(ui, rect);
                                    }
                                }
                                if unread > 0 {
                                    unread_badge(
                                        ui,
                                        Pos2::new(rect.right() - 5.0, rect.bottom() - 5.0),
                                        unread,
                                        false,
                                    );
                                }
                                let status = self.community_status(&community.url);
                                let status_text = match status {
                                    ConnectionStatus::SignInRequired => "Sign in to reconnect",
                                    ConnectionStatus::Reconnecting => "Reconnecting…",
                                    ConnectionStatus::Offline => "Offline",
                                    ConnectionStatus::Connected
                                        if self.communities.iter().any(|live| {
                                            live.url == community.url && live.current
                                        }) =>
                                    {
                                        "Active community"
                                    }
                                    ConnectionStatus::Connected => "Signed in",
                                };
                                if status != ConnectionStatus::Connected {
                                    let badge = Pos2::new(rect.right() - 5.0, rect.top() + 5.0);
                                    ui.painter().circle_filled(badge, 8.0, c_rail());
                                    ui.painter().circle_filled(
                                        badge,
                                        6.0,
                                        if status == ConnectionStatus::SignInRequired {
                                            c_bad()
                                        } else {
                                            Color32::from_rgb(240, 178, 50)
                                        },
                                    );
                                    ui.painter().text(
                                        badge,
                                        egui::Align2::CENTER_CENTER,
                                        if status == ConnectionStatus::SignInRequired {
                                            "!"
                                        } else {
                                            "…"
                                        },
                                        egui::FontId::proportional(11.0),
                                        Color32::BLACK,
                                    );
                                }
                                response.widget_info(|| {
                                    egui::WidgetInfo::selected(
                                        egui::WidgetType::Button,
                                        current,
                                        format!("{name} · {status_text}"),
                                    )
                                });
                                let response = response.on_hover_text(format!(
                                    "{name}\n{}\n{status_text}",
                                    community.url,
                                ));
                                if response.clicked() {
                                    self.switch_community(community.url.clone());
                                }
                                response.context_menu(|ui| {
                                    self.saved_server_context_menu(ui, &community)
                                });
                                ui.add_space(10.0);
                            }
                        });
                    ui.add_space(12.0);
                    if icon_button(ui, Icon::Plus, false, "Connect to another server").clicked() {
                        self.open_community_dialog();
                    }
                });
            });
    }

    pub(super) fn main_view(&mut self, ctx: &egui::Context) {
        self.people_call_notice(ctx);
        self.private_call_panel(ctx);
        if self.home_page.is_some() {
            self.desktop_home(ctx);
            return;
        }
        let (name, topic, is_voice) = self.current_name();
        let voice_parent = self.current_voice_text_parent();
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(c_bg()))
            .show(ctx, |ui| {
                ui.set_enabled(!self.modal_open());
                egui::TopBottomPanel::top("channel-header")
                    .exact_height(32.0)
                    .frame(egui::Frame::none().inner_margin(Margin::symmetric(16.0, 9.0)))
                    .show_inside(ui, |ui| {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            let create_in_channel = self.current.as_ref().and_then(|target| {
                                let Target::Channel(id) = target else {
                                    return None;
                                };
                                self.bootstrap.as_ref().and_then(|b| {
                                    b.channels
                                        .iter()
                                        .find(|channel| channel.id == *id)
                                        .filter(|channel| {
                                            channel.kind == "text" && channel.can_send
                                        })
                                        .filter(|_| {
                                            self.conversation_info(id).is_none_or(|info| {
                                                !matches!(info.kind.as_str(), "group" | "thread")
                                            })
                                        })
                                        .map(|_| id.clone())
                                })
                            });
                            if let Some(parent) = create_in_channel {
                                if icon_button(ui, Icon::Plus, false, "Create thread or post")
                                    .clicked()
                                {
                                    self.open_conversation_create("thread", Some(parent));
                                }
                            }
                            if self.bootstrap.as_ref().is_some_and(|b| {
                                b.server_presentation
                                    .system_channel_id
                                    .as_ref()
                                    .is_some_and(|id| {
                                        self.current == Some(Target::Channel(id.clone()))
                                    })
                            }) && ui.small_button("Community notices").clicked()
                            {
                                self.open_system_notices();
                            }
                            if let Some(parent) = &voice_parent {
                                if icon_button(ui, Icon::Close, false, "Close voice text chat")
                                    .clicked()
                                {
                                    self.select(Target::Channel(parent.id.clone()));
                                }
                            }
                            if self
                                .bootstrap
                                .as_ref()
                                .is_some_and(|b| b.has_permission(PERMISSION_CHANNELS))
                                && matches!(self.current, Some(Target::Channel(_)))
                                && icon_button(ui, Icon::More, false, "Edit Channel").clicked()
                            {
                                if let Some(Target::Channel(id)) = self.current.clone() {
                                    if let Some(channel) = self
                                        .bootstrap
                                        .as_ref()
                                        .and_then(|b| b.channels.iter().find(|c| c.id == id))
                                        .cloned()
                                    {
                                        self.open_channel_settings(channel);
                                    }
                                }
                            }
                            if !matches!(self.current, Some(Target::Direct(_)))
                                && icon_button(
                                    ui,
                                    Icon::Users,
                                    if self.members_compact {
                                        self.compact_members_open
                                    } else if is_voice {
                                        self.show_voice_members
                                    } else {
                                        self.show_members
                                    },
                                    "Show members",
                                )
                                .clicked()
                            {
                                if self.members_compact {
                                    self.compact_members_open = !self.compact_members_open;
                                } else if is_voice {
                                    self.show_voice_members = !self.show_voice_members;
                                } else {
                                    self.show_members = !self.show_members;
                                }
                            }
                            if matches!(self.current, Some(Target::Channel(_)))
                                && self.has_chat_capability("pins-v1")
                                && icon_button(ui, Icon::Pin, self.pins.open, "Pinned messages")
                                    .clicked()
                            {
                                self.pins.open = true;
                                self.request_pins();
                            }
                            if !is_voice
                                && icon_button(
                                    ui,
                                    Icon::Search,
                                    self.history_search_open || !self.search.is_empty(),
                                    "Search history · Ctrl+F",
                                )
                                .clicked()
                            {
                                self.history_search_open = true;
                                ctx.memory_mut(|memory| {
                                    memory.request_focus(egui::Id::new("history-search"))
                                });
                            }
                            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                                if is_voice || voice_parent.is_some() {
                                    icon(ui, Icon::Headphones, 22.0, c_muted());
                                } else if matches!(self.current, Some(Target::Direct(_))) {
                                    icon(ui, Icon::Chat, 22.0, c_muted());
                                } else {
                                    ui.label(
                                        RichText::new("#").size(font_size(25.0)).color(c_muted()),
                                    );
                                }
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(&name)
                                            .size(font_size(16.0))
                                            .family(egui::FontFamily::Name("Relay Semibold".into()))
                                            .strong(),
                                    )
                                    .truncate(true),
                                );
                                if !topic.is_empty() && ui.available_width() > 480.0 {
                                    ui.separator();
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(&topic)
                                                .size(font_size(12.0))
                                                .color(c_muted()),
                                        )
                                        .truncate(true),
                                    );
                                }
                            });
                        });
                    });
                if !is_voice && (self.history_search_open || !self.search.is_empty()) {
                    self.history_search_panel(ui);
                }
                if self.error.is_some()
                    || self.notice.is_some()
                    || self.connection_status != ConnectionStatus::Connected
                {
                    egui::TopBottomPanel::bottom("connection-status")
                        .frame(
                            egui::Frame::none()
                                .fill(c_panel())
                                .inner_margin(Margin::symmetric(16.0, 8.0)),
                        )
                        .show_inside(ui, |ui| {
                            ui.horizontal_wrapped(|ui| {
                                if let Some(error) = self.error.clone() {
                                    ui.label(
                                        RichText::new(&error).size(font_size(12.0)).color(c_bad()),
                                    );
                                    if self.encryption_ui.status.status == error
                                        && ui.small_button("Set Up Private Chat").clicked()
                                    {
                                        self.settings_tab = SettingsTab::Encryption;
                                        self.settings_open = true;
                                        self.send(Command::CryptoRefresh);
                                    }
                                    if let Some(channel) = self.voice_retry_target() {
                                        if ui.small_button("Try call again")
                                            .on_hover_text("Join the same voice room again. Your microphone opens when you join.")
                                            .clicked()
                                        {
                                            self.join_voice(channel);
                                        }
                                    }
                                    if ui.small_button("Dismiss").clicked() {
                                        self.error = None;
                                        self.voice_retry = None;
                                    }
                                } else if self.connection_status != ConnectionStatus::Connected {
                                    ui.label(
                                        RichText::new(match self.connection_status {
                                            ConnectionStatus::Reconnecting => {
                                                "Reconnecting… Your draft is safe."
                                            }
                                            ConnectionStatus::Offline => {
                                                "Server unavailable. Retrying…"
                                            }
                                            ConnectionStatus::SignInRequired => {
                                                "Sign in again to reconnect."
                                            }
                                            ConnectionStatus::Connected => "Connected",
                                        })
                                        .size(font_size(12.0))
                                        .color(c_accent()),
                                    );
                                } else if let Some((notice, _)) = &self.notice {
                                    ui.label(
                                        RichText::new(notice)
                                            .size(font_size(12.0))
                                            .color(c_muted()),
                                    );
                                }
                            });
                        });
                }
                if is_voice {
                    if let Some(Target::Channel(id)) = self.current.clone() {
                        self.voice_view(ui, &name, &id);
                    }
                } else if let Some(parent) = voice_parent {
                    if ui.available_width() < 600.0 {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(RichText::new(&parent.name).strong());
                            if ui.button("Open voice room").clicked() {
                                self.select(Target::Channel(parent.id.clone()));
                            }
                        });
                        self.chat_view(ui, &name);
                    } else {
                        let width = (ui.available_width() * 0.5).clamp(260.0, 440.0);
                        egui::SidePanel::right("voice-text-chat")
                            .resizable(true)
                            .default_width(width)
                            .width_range(240.0..=600.0)
                            .show_inside(ui, |ui| {
                                self.chat_view(ui, &name);
                            });
                        self.voice_view(ui, &parent.name, &parent.id);
                    }
                } else {
                    self.conversation_toolbar(ui);
                    self.community_notice_bar(ui);
                    let forum = self.current.clone().and_then(|target| {
                        if let Target::Channel(id) = target {
                            self.conversation_info(&id)
                                .filter(|c| c.kind == "forum")
                                .map(|_| id)
                        } else {
                            None
                        }
                    });
                    if let Some(forum) = forum {
                        self.forum_view(ui, &forum);
                    } else {
                        self.chat_view(ui, &name);
                    }
                }
            });
    }
}
