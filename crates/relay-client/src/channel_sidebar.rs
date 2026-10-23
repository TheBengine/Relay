//! R8.2: the channel list and user panel (the sidebar), moved unchanged out of
//! `app.rs`; only the method's visibility became `pub(super)`.
use super::*;

/// Who can join your own server, as one clickable line under its name. A
/// green dot when friends can join; the click opens Invite friends.
fn sharing_status_row(
    ui: &mut egui::Ui,
    sharing: Option<host_setup_ui::Sharing>,
) -> egui::Response {
    let (text, shared) = host_setup_ui::sharing_status(sharing);
    let font = FontId::proportional(font_size(12.0));
    let height = ui.fonts(|f| f.row_height(&font)) + 10.0;
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::click());
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Button, format!("{text}. Invite friends"))
    });
    let response = response.on_hover_text("Invite friends");
    let palette = UI_PALETTE.with(|palette| palette.get());
    let painter = ui.painter().with_clip_rect(rect.intersect(ui.clip_rect()));
    if response.hovered() || response.has_focus() {
        painter.rect_filled(rect, 4.0, palette.hover);
    }
    let dot = Pos2::new(rect.left() + 10.0, rect.center().y);
    painter.circle_filled(dot, 4.0, if shared { c_good() } else { c_muted() });
    painter.text(
        Pos2::new(rect.left() + 20.0, rect.center().y),
        Align2::LEFT_CENTER,
        text,
        font,
        if shared { c_text() } else { c_muted() },
    );
    response
}

impl RelayApp {
    pub(super) fn sidebar(&mut self, ctx: &egui::Context) {
        egui::SidePanel::left("channels")
            .default_width(240.0)
            .width_range(180.0..=360.0)
            .resizable(true)
            // The panel colours already separate channels from chat; egui still
            // draws its resize line on hover and while dragging.
            .show_separator_line(false)
            .frame(egui::Frame::none().fill(c_panel()))
            .show(ctx, |ui| {
                ui.set_enabled(!self.modal_open());
                let Some(b) = self.bootstrap.clone() else {
                    return;
                };
                if self.selected_nav {
                    if let Some(texture) = self.asset_texture("server_banner", "community") {
                        egui::TopBottomPanel::top("server-image-banner")
                            .exact_height(96.0)
                            .frame(egui::Frame::none())
                            .show_inside(ui, |ui| {
                                ui.add(egui::Image::new((
                                    texture.id(),
                                    Vec2::new(ui.available_width(), 96.0),
                                )));
                            });
                    }
                }
                egui::TopBottomPanel::top("server-heading")
                    .exact_height(32.0)
                    .frame(egui::Frame::none().inner_margin(Margin::symmetric(16.0, 9.0)))
                    .show_inside(ui, |ui| {
                        ui.horizontal(|ui| {
                            let heading = ui.add_sized(
                                [ui.available_width() - 40.0, 30.0],
                                egui::Label::new(
                                    RichText::new(if self.selected_nav {
                                        &b.server_name
                                    } else {
                                        "Direct messages"
                                    })
                                    .size(font_size(16.0))
                                    .family(egui::FontFamily::Name("Relay Semibold".into()))
                                    .strong(),
                                )
                                .truncate(true)
                                .sense(Sense::click()),
                            );
                            if self.selected_nav && heading.clicked() {
                                self.server_menu = !self.server_menu;
                            }
                            if self.selected_nav {
                                let response = icon_button(
                                    ui,
                                    Icon::Chevron,
                                    self.server_menu,
                                    "Server options",
                                );
                                self.server_menu_anchor = Pos2::new(
                                    heading.rect.left() - 4.0,
                                    response.rect.bottom() + 12.0,
                                );
                                if response.clicked() {
                                    self.server_menu = !self.server_menu;
                                }
                            }
                        });
                    });
                if let Some(sharing) = self.own_server_sharing().filter(|_| self.selected_nav) {
                    egui::TopBottomPanel::top("sharing-status")
                        .frame(egui::Frame::none().inner_margin(Margin::symmetric(10.0, 2.0)))
                        .show_inside(ui, |ui| {
                            if sharing_status_row(ui, sharing).clicked() {
                                self.open_friend_sharing();
                            }
                        });
                }
                egui::TopBottomPanel::top("conversation-finder")
                    .frame(egui::Frame::none().inner_margin(Margin::symmetric(10.0, 6.0)))
                    .show_inside(ui, |ui| {
                        if ui
                            .add_sized(
                                [ui.available_width(), 28.0],
                                secondary("Find a conversation · Ctrl+K"),
                            )
                            .clicked()
                        {
                            self.quick_switch = true;
                            self.quick_query.clear();
                            self.quick_selection = quick_switch_ui::Selection::default();
                            ui.memory_mut(|memory| {
                                memory.request_focus(egui::Id::new("quick-switch-input"))
                            });
                        }
                    });
                egui::TopBottomPanel::bottom("profile-footer")
                    .frame(
                        egui::Frame::none()
                            .fill(c_card())
                            .rounding(6.0)
                            .inner_margin(Margin::same(8.0)),
                    )
                    .show_inside(ui, |ui| {
                        if !self.uploads.is_empty() {
                            if ui
                                .add(secondary(&format!("Transfers · {}", self.uploads.len())))
                                .clicked()
                            {
                                self.transfers_open = true;
                            }
                            ui.add_space(6.0);
                        }
                        if let Some(channel) =
                            self.voice_channel.clone().or(self.joining_voice.clone())
                        {
                            let name = b
                                .channels
                                .iter()
                                .find(|c| c.id == channel)
                                .map(|c| c.name.as_str())
                                .unwrap_or("Voice room");
                            ui.horizontal(|ui| {
                                let text_width = (ui.available_width() - 36.0).max(80.0);
                                ui.vertical(|ui| {
                                    ui.set_width(text_width);
                                    ui.label(
                                        RichText::new(if self.voice_reconnecting() {
                                            "Reconnecting voice…"
                                        } else if self.voice_encryption_error.is_some() {
                                            "Voice paused"
                                        } else if self.joining_voice.is_some() {
                                            "Joining voice…"
                                        } else if self
                                            .media
                                            .as_ref()
                                            .is_some_and(MediaEngine::is_changing_devices)
                                        {
                                            "Changing audio devices…"
                                        } else if self.audio_connecting && !self.voice_ready() {
                                            "Opening audio…"
                                        } else if self.voice_connected() {
                                            "Voice connected"
                                        } else {
                                            "Connecting voice…"
                                        })
                                        .size(font_size(13.0))
                                        .family(egui::FontFamily::Name("Relay Semibold".into()))
                                        .strong()
                                        .color(
                                            if self.voice_connected() {
                                                c_good()
                                            } else {
                                                c_accent()
                                            },
                                        ),
                                    );
                                    let room = ui
                                        .add_sized(
                                            [text_width, 18.0],
                                            egui::Label::new(
                                                RichText::new(name)
                                                    .size(font_size(TEXT_CAPTION))
                                                    .color(c_muted()),
                                            )
                                            .truncate(true)
                                            .sense(Sense::click()),
                                        )
                                        .on_hover_text("Open voice room");
                                    if room.clicked() {
                                        self.select(Target::Channel(channel.clone()));
                                    }
                                    if self.voice_connected() {
                                        self.connection_path_caption(ui, text_width);
                                    }
                                    if let Some(media) = &self.media {
                                        if media.active_settings().whisper_enabled {
                                            let targets = media.whisper_targets();
                                            if !targets.is_empty() {
                                                let names: Vec<_> = targets
                                                    .iter()
                                                    .filter_map(|target| {
                                                        b.voice_members.iter().find(|member| {
                                                            &member.user.id == target
                                                        })
                                                    })
                                                    .map(|member| {
                                                        profile_ui::display_name(&member.user)
                                                            .to_owned()
                                                    })
                                                    .collect();
                                                let label = format!(
                                                    "Private to {} - F8 ({})",
                                                    names.join(", "),
                                                    if targets
                                                        .iter()
                                                        .all(|target| media.whisper_ready(target))
                                                    {
                                                        "ready"
                                                    } else {
                                                        "paused"
                                                    }
                                                );
                                                ui.add_sized(
                                                    [text_width, 18.0],
                                                    egui::Label::new(
                                                        RichText::new(&label)
                                                            .size(font_size(TEXT_CAPTION))
                                                            .color(c_accent()),
                                                    )
                                                    .truncate(true),
                                                )
                                                .on_hover_text(label);
                                            }
                                        }
                                    }
                                });
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    if icon_button(
                                        ui,
                                        Icon::HangUp,
                                        false,
                                        if self.voice_ready() {
                                            "Disconnect voice"
                                        } else {
                                            "Cancel voice join"
                                        },
                                    )
                                    .clicked()
                                    {
                                        self.leave_voice();
                                    }
                                });
                            });
                            if self.source_active(false) || self.source_starting(false) {
                                ui.horizontal(|ui| {
                                    dot(ui, c_good(), 3.0);
                                    ui.label(
                                        RichText::new(if self.source_starting(false) {
                                            "Opening source…"
                                        } else {
                                            "Sharing screen"
                                        })
                                        .size(font_size(TEXT_CAPTION))
                                        .color(c_good()),
                                    );
                                    if ui
                                        .small_button("Stop")
                                        .on_hover_text(
                                            "Stop sharing your screen, from any conversation",
                                        )
                                        .clicked()
                                    {
                                        self.stop_capture(false);
                                    }
                                });
                            }
                            if self.source_active(true) || self.source_starting(true) {
                                ui.horizontal(|ui| {
                                    dot(ui, c_good(), 3.0);
                                    ui.label(
                                        RichText::new(if self.source_starting(true) {
                                            "Opening camera…"
                                        } else {
                                            "Camera on"
                                        })
                                        .size(font_size(TEXT_CAPTION))
                                        .color(c_good()),
                                    );
                                    if ui
                                        .small_button("Stop")
                                        .on_hover_text(
                                            "Turn your camera off, from any conversation",
                                        )
                                        .clicked()
                                    {
                                        self.stop_capture(true);
                                    }
                                });
                            }
                            ui.add_space(6.0);
                            ui.separator();
                            ui.add_space(6.0);
                        }
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 2.0;
                            self.self_profile_button(ui, &b.user);
                            if icon_button_sized(
                                ui,
                                24.0,
                                Icon::Mic,
                                self.muted,
                                if self.muted {
                                    "Unmute microphone"
                                } else {
                                    "Mute microphone"
                                },
                            )
                            .clicked()
                            {
                                self.toggle_mute();
                            }
                            self.audio_device_menu(ui, true);
                            if icon_button_sized(
                                ui,
                                24.0,
                                Icon::Headphones,
                                self.deafened,
                                if self.deafened { "Undeafen" } else { "Deafen" },
                            )
                            .clicked()
                            {
                                self.toggle_deafen();
                            }
                            self.audio_device_menu(ui, false);
                            if icon_button_sized(ui, 24.0, Icon::Settings, false, "User settings")
                                .clicked()
                            {
                                self.settings_open = true;
                            }
                        });
                    });
                egui::ScrollArea::vertical()
                    .id_source("channel-navigation")
                    .show(ui, |ui| {
                        ui.add_space(16.0);
                        if self.selected_nav {
                            self.threads_navigation(ui);
                            if icon_nav_row(
                                ui,
                                "server-events",
                                Icon::Calendar,
                                "Events",
                                self.home_page == Some(SettingsTab::Events),
                            )
                            .clicked()
                            {
                                self.home_page = Some(SettingsTab::Events);
                            }
                            for kind in ["text", "voice"] {
                                ui.horizontal(|ui| {
                                    ui.add_space(14.0);
                                    ui.label(
                                        RichText::new(if kind == "text" {
                                            "TEXT CHANNELS"
                                        } else {
                                            "VOICE CHANNELS"
                                        })
                                        .size(font_size(TEXT_CAPTION))
                                        .family(egui::FontFamily::Name("Relay Semibold".into()))
                                        .strong()
                                        .color(c_muted()),
                                    );
                                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                        if b.has_permission(PERMISSION_CHANNELS)
                                            && ui
                                                .small_button("+")
                                                .on_hover_text("Create channel")
                                                .clicked()
                                        {
                                            self.channel_kind =
                                                if kind == "voice" { "voice" } else { "text" };
                                            self.create_channel_open = true;
                                        }
                                    });
                                });
                                ui.add_space(4.0);
                                let mut last_category: Option<Option<String>> = None;
                                let mut channels: Vec<_> = b
                                    .channels
                                    .iter()
                                    .filter(|channel| {
                                        channel.kind == kind
                                            && self.community_channel(channel)
                                            && !self.channel_hidden_by_server_policy(channel)
                                    })
                                    .collect();
                                channels.sort_by_key(|channel| {
                                    (
                                        channel
                                            .category_id
                                            .as_deref()
                                            .and_then(|id| {
                                                self.admin_data
                                                    .get("categories")
                                                    .and_then(serde_json::Value::as_array)
                                                    .and_then(|categories| {
                                                        categories.iter().find(|category| {
                                                            category["id"].as_str() == Some(id)
                                                        })
                                                    })
                                                    .and_then(|category| {
                                                        category["position"].as_i64()
                                                    })
                                            })
                                            .unwrap_or(i64::MIN),
                                        channel.category_id.clone(),
                                        channel.position,
                                    )
                                });
                                for channel in channels {
                                    if last_category.as_ref() != Some(&channel.category_id) {
                                        if let Some(id) = &channel.category_id {
                                            let category = self
                                                .admin_data
                                                .get("categories")
                                                .and_then(serde_json::Value::as_array)
                                                .and_then(|categories| {
                                                    categories.iter().find(|category| {
                                                        category["id"].as_str() == Some(id)
                                                    })
                                                })
                                                .and_then(|category| category["name"].as_str())
                                                .unwrap_or("Category");
                                            ui.add_space(6.0);
                                            let category = category.to_owned();
                                            self.category_interactions(ui, id, &category);
                                        }
                                        last_category = Some(channel.category_id.clone());
                                    }
                                    let unread = self
                                        .read_state(&Target::Channel(channel.id.clone()))
                                        .map(|read| (read.unread_count, read.mention_count))
                                        .unwrap_or_default();
                                    let channel_response = channel_row(
                                        ui,
                                        &channel.name,
                                        channel_tooltip_identity(&b.server_id, &channel.id),
                                        kind == "voice",
                                        self.current.as_ref()
                                            == Some(&Target::Channel(channel.id.clone())),
                                        self.voice_channel.as_ref() == Some(&channel.id),
                                        unread,
                                    );
                                    self.channel_interactions(ui, channel, &channel_response);
                                    let chat = (kind == "voice")
                                        .then(|| {
                                            b.channels.iter().find(|chat| {
                                                chat.voice_parent_id.as_ref() == Some(&channel.id)
                                            })
                                        })
                                        .flatten();
                                    let chat_clicked = chat.is_some_and(|chat| {
                                        desktop_ui::voice_chat_action(
                                            ui,
                                            &channel_response,
                                            self.current.as_ref()
                                                == Some(&Target::Channel(chat.id.clone())),
                                        )
                                        .clicked()
                                    });
                                    if chat_clicked {
                                        if let Some(chat) = chat {
                                            self.select(Target::Channel(chat.id.clone()));
                                        }
                                    } else if channel_response.clicked() {
                                        self.select(Target::Channel(channel.id.clone()));
                                        if kind == "voice" {
                                            self.join_voice(channel.id.clone());
                                        }
                                    }
                                    if kind == "voice" {
                                        for member in b
                                            .voice_members
                                            .iter()
                                            .filter(|m| m.channel_id == channel.id)
                                        {
                                            ui.horizontal(|ui| {
                                                ui.add_space(38.0);
                                                self.user_avatar(
                                                    ui,
                                                    &member.user,
                                                    22.0,
                                                    self.member_speaking(member),
                                                );
                                                let name_width = (ui.available_width()
                                                    - if member.muted || member.deafened {
                                                        22.0
                                                    } else {
                                                        0.0
                                                    })
                                                .max(20.0);
                                                // Left-aligned beside the avatar; `add_sized`
                                                // would centre the name in the remaining width.
                                                ui.allocate_ui_with_layout(
                                                    Vec2::new(name_width, 22.0),
                                                    Layout::left_to_right(Align::Center),
                                                    |ui| {
                                                        ui.add(
                                                            egui::Label::new(
                                                                RichText::new(
                                                                    profile_ui::display_name(
                                                                        &member.user,
                                                                    ),
                                                                )
                                                                .size(font_size(13.0))
                                                                .color(c_muted()),
                                                            )
                                                            .truncate(true),
                                                        )
                                                        .on_hover_text(&member.user.username);
                                                    },
                                                );
                                                if member.muted || member.deafened {
                                                    icon(
                                                        ui,
                                                        if member.deafened {
                                                            Icon::Headphones
                                                        } else {
                                                            Icon::Mic
                                                        },
                                                        14.0,
                                                        c_muted(),
                                                    );
                                                }
                                            });
                                        }
                                    }
                                }
                                ui.add_space(18.0);
                            }
                        } else {
                            self.groups_navigation(ui);
                            let recent = self
                                .conversation_scope()
                                .map(|scope| self.prefs.conversations.recent_directs(&scope))
                                .unwrap_or_default();
                            let query = self.people_search.to_lowercase();
                            let mut people: Vec<_> = b
                                .members
                                .iter()
                                .filter(|member| {
                                    member.id != b.user.id
                                        && (!self.in_people()
                                            || b.social.friends.contains(&member.id))
                                        && (member.username.to_lowercase().contains(&query)
                                            || profile_ui::display_name(member)
                                                .to_lowercase()
                                                .contains(&query))
                                })
                                .collect();
                            people.sort_by_key(|member| {
                                recent
                                    .iter()
                                    .position(|target| target == &Target::Direct(member.id.clone()))
                                    .unwrap_or(usize::MAX)
                            });
                            for member in people {
                                let (unread, mentions) = self
                                    .read_state(&Target::Direct(member.id.clone()))
                                    .map(|read| (read.unread_count, read.mention_count))
                                    .unwrap_or_default();
                                let texture = self.avatar_texture(member);
                                let response = user_row(
                                    ui,
                                    member,
                                    self.current.as_ref()
                                        == Some(&Target::Direct(member.id.clone())),
                                    unread,
                                    mentions,
                                    texture.as_ref(),
                                );
                                self.paint_presence_badge(ui, &member.id, &response);
                                self.user_interactions(ui, member, &response);
                                if response.clicked() {
                                    self.select(Target::Direct(member.id.clone()));
                                }
                            }
                            if b.members.len() <= 1 {
                                ui.add_space(10.0);
                                ui.label(
                                    RichText::new("Invite someone to start a conversation.")
                                        .size(font_size(13.0))
                                        .color(c_muted()),
                                );
                            }
                        }
                    });
            });
    }
}
