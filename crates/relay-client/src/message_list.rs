//! R8.2: the chat view and its message rows and actions, moved unchanged out
//! of `app.rs`; only the methods' visibility became `pub(super)`.
use super::*;

impl RelayApp {
    pub(super) fn chat_view(&mut self, ui: &mut egui::Ui, name: &str) {
        let can_send = self.current.as_ref().is_some_and(|target| match target {
            Target::Direct(id) => !self.is_blocked(id) && self.can_message(id),
            Target::Channel(id) => self.bootstrap.as_ref().is_some_and(|bootstrap| {
                bootstrap
                    .channels
                    .iter()
                    .any(|channel| channel.id == *id && channel.can_send)
            }),
        });
        egui::TopBottomPanel::bottom("composer")
            .frame(egui::Frame::none().inner_margin(Margin::symmetric(16.0, 10.0)))
            .show_inside(ui, |ui| {
                self.composer_activity(ui);
                if !can_send {
                    ui.label(
                        RichText::new(match &self.current {
                            Some(Target::Direct(id)) if self.is_blocked(id) => {
                                "You blocked this person. Unblock them from their profile to send a message."
                            }
                            Some(Target::Direct(_)) => {
                                "Only friends can send private messages. Add them as a friend to message them."
                            }
                            _ => "Read-only channel · You do not have permission to send messages.",
                        })
                        .color(c_muted())
                        .size(font_size(12.0)),
                    );
                }
                ui.set_enabled(can_send);
                if let Some(reply) = self.reply.clone() {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(
                            RichText::new(format!("Replying to {}", reply.author))
                                .size(font_size(12.0))
                                .color(c_accent()),
                        );
                        ui.add(
                            egui::Label::new(
                                RichText::new(crate::message_format::plain_preview(&reply.excerpt, 160))
                                    .size(font_size(12.0))
                                    .color(c_muted()),
                            )
                            .truncate(true),
                        );
                        if ui.small_button("×").on_hover_text("Cancel reply").clicked() {
                            self.reply = None;
                            self.defer_preferences();
                        }
                    });
                    if let Some(reply) = &mut self.reply {
                        let available = reply.author_id.as_ref().is_some_and(|author| {
                            self.session.as_ref().is_some_and(|session| session.user.id != *author)
                        });
                        if ui.add_enabled(available, egui::Checkbox::new(&mut reply.notify_author, "Notify original author")).on_hover_text("Include the original author in this reply’s mention notifications. Explicit @mentions still notify when this is off.").changed() {
                            self.defer_preferences();
                        }
                    }
                    ui.add_space(5.0);
                }
                let operations: Vec<_> = self
                    .uploads
                    .iter()
                    .filter(|(_, upload)| {
                        matches!(upload.kind, TransferKind::Upload)
                            && self.current.as_ref() == upload.target.as_ref()
                    })
                    .map(|(id, _)| id.clone())
                    .collect();
                for id in operations {
                    let Some(upload) = self.uploads.get(&id) else {
                        continue;
                    };
                    let name = upload
                        .path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned();
                    let error = upload.error.clone();
                    let cancelled = upload.cancelled;
                    let progress = if upload.total == 0 {
                        0.0
                    } else {
                        (upload.transferred as f32 / upload.total as f32).clamp(0.0, 1.0)
                    };
                    ui.horizontal(|ui| {
                        ui.add_sized(
                            [ui.available_width() - 140.0, 20.0],
                            egui::ProgressBar::new(progress).text(if error.is_some() {
                                name.clone()
                            } else {
                                format!("{name} · {:.0}%", progress * 100.0)
                            }),
                        );
                        if error.is_some() {
                            if ui.small_button("Retry").clicked() {
                                self.retry_transfer(&id);
                            }
                            if ui.small_button("Dismiss").clicked() {
                                self.uploads.remove(&id);
                            }
                        } else if ui.small_button("Cancel").clicked() {
                            self.send(Command::CancelUpload {
                                operation_id: id.clone(),
                            });
                        }
                    });
                    if let Some(error) = error {
                        ui.label(
                            RichText::new(if cancelled {
                                "Upload cancelled".into()
                            } else {
                                error
                            })
                            .size(font_size(TEXT_CAPTION))
                            .color(if cancelled {
                                c_muted()
                            } else {
                                c_bad()
                            }),
                        );
                    }
                }
                if !self.attachments.is_empty() {
                    ui.horizontal_wrapped(|ui| {
                        let mut remove = None;
                        for (i, attachment) in self.attachments.iter().enumerate() {
                            if ui.button(format!("{}  ×", attachment.name)).clicked() {
                                remove = Some(i);
                            }
                        }
                        if let Some(i) = remove {
                            self.attachments.remove(i);
                            self.defer_preferences();
                        }
                    });
                    ui.add_space(8.0);
                }
                self.composer_readiness(ui);
                self.protection_notice(ui);
                let pending = self.current.as_ref().is_some_and(|target| self.pending_sends.contains(target));
                let cooling_down = self.current.as_ref().is_some_and(|target| self.cooldowns.remaining(target, Instant::now()).is_some());
                let submit = self.composer_bar(ui, name, pending, cooling_down);
                if submit
                    && can_send
                    && !pending
                    && !cooling_down
                    && (!self.draft.trim().is_empty() || !self.attachments.is_empty())
                {
                    self.submit_composer();
                }
            });
        let searching = !self.search.trim().is_empty();
        if searching
            || self.history_before.is_some()
            || self.history_pending.is_some()
            || self.history_has_newer
        {
            egui::TopBottomPanel::top("history-actions")
                .frame(
                    egui::Frame::none()
                        .fill(c_bg())
                        .inner_margin(Margin::symmetric(16.0, 5.0)),
                )
                .show_inside(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        if searching {
                            ui.label(
                                RichText::new(format!("Results for “{}”", self.search_applied))
                                    .size(font_size(12.0))
                                    .color(c_muted()),
                            );
                            if self.search_pending.is_some() {
                                ui.spinner();
                            }
                            if ui
                                .add_enabled(
                                    self.search_pending.is_none() && self.search_before.is_some(),
                                    secondary("More results"),
                                )
                                .clicked()
                            {
                                self.search_history(true);
                            }
                            if ui.small_button("Clear search").clicked() {
                                self.search.clear();
                                self.search_history(false);
                            }
                        } else {
                            if ui
                                .add_enabled(
                                    self.history_pending.is_none() && self.history_before.is_some(),
                                    secondary("Load older messages"),
                                )
                                .clicked()
                            {
                                self.load_history(true);
                            }
                            if self.history_pending.is_some() {
                                ui.spinner();
                            }
                            if self.history_has_newer
                                && ui.add(secondary("Jump to latest")).clicked()
                            {
                                self.open_latest();
                            }
                        }
                    });
                });
        }
        // The empty conversation has the full first-steps card below. Once a
        // message arrives, keep the compact next action outside virtualized
        // history so sending a hello does not hide the route into voice.
        if !searching && self.messages_ready && !self.messages.is_empty() {
            let mut checklist = std::mem::take(&mut self.prefs.host_checklist);
            let panel = self.host_next_steps_panel(ui, &mut checklist);
            self.prefs.host_checklist = checklist;
            if panel.changed { self.defer_preferences(); }
        }
        let chat_rect = ui.available_rect_before_wrap();
        let interacted = ui.ctx().input(|input| chat_scroll_intent(input, chat_rect));
        if interacted
            || searching
            || self.restore_scroll.is_some()
            || self.restore_anchor.is_some()
            || self.jump_message.is_some()
        {
            self.bottom_follow.cancel();
        }
        let intro = self.direct_intro_user(searching);
        let intro_height = if intro.is_some() {
            direct_intro_height()
        } else {
            0.0
        };
        let mut scroll = egui::ScrollArea::vertical()
            .id_source((
                "message-list",
                self.conversation_scope(),
                self.current.clone(),
                searching,
            ))
            .stick_to_bottom(crate::unread_navigation::follow_latest(
                searching,
                self.unread_navigation.is_some() || self.unread_history_start.is_some(),
            ))
            .animated(!(self.bottom_follow.enabled() || self.scroll_to_end))
            .auto_shrink([false, false]);
        if self.messages_ready && !searching {
            if let Some(anchor) = &self.restore_anchor {
                self.restore_scroll = None;
                self.row_heights
                    .prepare(ui.available_width(), &self.messages);
                if let Some(index) = self
                    .messages
                    .iter()
                    .position(|message| message.id == anchor.message_id)
                {
                    scroll = scroll.vertical_scroll_offset(
                        intro_height
                            + self.messages[..index]
                                .iter()
                                .map(|message| self.row_heights.estimate(message))
                                .sum::<f32>()
                            + anchor.offset,
                    );
                } else {
                    self.restore_anchor = None;
                }
            } else if let Some(offset) = self.restore_scroll.take() {
                scroll = scroll.vertical_scroll_offset(offset);
            }
            if let Some(index) = self
                .jump_message
                .as_ref()
                .and_then(|id| self.messages.iter().position(|message| &message.id == id))
            {
                self.row_heights
                    .prepare(ui.available_width(), &self.messages);
                let offset = intro_height
                    + self.messages[..index]
                        .iter()
                        .map(|message| self.row_heights.estimate(message))
                        .sum::<f32>();
                scroll = scroll.vertical_scroll_offset(offset);
            }
        }
        let read_sequence = self
            .current
            .as_ref()
            .and_then(|target| self.read_state(target))
            .map(|read| read.last_read_sequence)
            .unwrap_or_default();
        let mut highest_seen = 0;
        let mut first_visible = None;
        let output = scroll.show_viewport(ui, |ui, viewport| {
            if !self.messages_ready && !searching {
                if let Some(navigation) = &self.unread_navigation {
                    let error = navigation.error.clone();
                    ui.horizontal_wrapped(|ui| {
                        if let Some(error) = error {
                            ui.label(RichText::new(error).color(c_bad()));
                            if ui.add(secondary("Retry")).clicked() {
                                self.load_unread_page();
                            }
                        } else {
                            ui.spinner();
                            ui.label(
                                RichText::new("Finding the first unread message…").color(c_muted()),
                            );
                        }
                        if ui.add(secondary("Open latest messages")).clicked() {
                            self.open_latest();
                        }
                    });
                    return;
                }
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new("Loading conversation…").color(c_muted()));
                });
                return;
            }
            if let Some(user) = &intro {
                self.direct_intro(ui, user);
            }
            self.outgoing_messages.retain(|(_, local, _, _)| {
                !self.messages.iter().any(|remote| {
                    remote.client_id == local.client_id && remote.author.id == local.author.id
                })
            });
            // Borrow all rows; own only the visible rows before mutating UI state.
            let source: Vec<&Message> = if searching {
                self.search_messages.iter().collect()
            } else {
                self.messages
                    .iter()
                    .chain(
                        self.outgoing_messages
                            .iter()
                            .filter(|(target, _, _, _)| Some(target) == self.current.as_ref())
                            .map(|(_, message, _, _)| message),
                    )
                    .collect()
            };
            if source.is_empty() {
                if intro.is_some() {
                    return;
                }
                if !searching {
                    let mut checklist = std::mem::take(&mut self.prefs.host_checklist);
                    let panel = self.host_next_steps_panel(ui, &mut checklist);
                    self.prefs.host_checklist = checklist;
                    if panel.changed {
                        self.defer_preferences();
                    }
                    if panel.shown {
                        return;
                    }
                }
                ui.add_space(24.0);
                ui.label(
                    RichText::new(if searching {
                        if self.search_pending.is_some() {
                            "Searching history…"
                        } else {
                            "No matching messages."
                        }
                    } else {
                        "Your conversation starts here."
                    })
                    .size(font_size(14.0))
                    .color(c_muted()),
                );
                return;
            }
            let unread_index = if searching {
                None
            } else {
                self.session.as_ref().and_then(|session| {
                    timeline::first_unread_index(
                        source.iter().copied(),
                        &session.user.id,
                        read_sequence,
                    )
                })
            };
            self.row_heights
                .prepare_iter(ui.available_width(), source.iter().copied());
            let heights: Vec<f32> = source
                .iter()
                .map(|message| self.row_heights.estimate(message))
                .collect();
            let range = timeline::visible_range(
                &heights,
                viewport.top() - intro_height,
                viewport.bottom() - intro_height,
            );
            let preceding: f32 = heights[..range.start].iter().sum();
            let following: f32 = heights[range.end..].iter().sum();
            let rows: Vec<_> = range
                .clone()
                .map(|index| {
                    (
                        index,
                        (*source[index]).clone(),
                        index
                            .checked_sub(1)
                            .map(|previous| (*source[previous]).clone()),
                    )
                })
                .collect();
            ui.add_space(preceding);
            let mut height_changed = false;
            for (index, message, previous) in rows {
                let start = ui.cursor().top();
                if previous.as_ref().is_none_or(|previous| {
                    message_day(&previous.created_at) != message_day(&message.created_at)
                }) {
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        ui.add_space(16.0);
                        ui.add_sized(
                            [ui.available_width() * 0.33, 1.0],
                            egui::Separator::default().horizontal(),
                        );
                        ui.label(
                            RichText::new(day_label(&message.created_at))
                                .size(font_size(11.0))
                                .color(c_muted()),
                        );
                        ui.add_sized(
                            [ui.available_width() - 16.0, 1.0],
                            egui::Separator::default().horizontal(),
                        );
                    });
                    ui.add_space(8.0);
                }
                if unread_index == Some(index) {
                    ui.horizontal(|ui| {
                        ui.add_space(16.0);
                        ui.label(
                            RichText::new("NEW MESSAGES")
                                .size(font_size(TEXT_CAPTION))
                                .strong()
                                .color(c_accent()),
                        );
                        let (line, _) = ui.allocate_exact_size(
                            Vec2::new((ui.available_width() - 16.0).max(0.0), 1.0),
                            Sense::hover(),
                        );
                        ui.painter().hline(
                            line.x_range(),
                            line.center().y,
                            Stroke::new(1.0_f32, c_accent()),
                        );
                    });
                }
                let grouped = unread_index != Some(index)
                    && previous
                        .as_ref()
                        .is_some_and(|previous| group_messages(previous, &message));
                self.message_row(ui, &message, grouped);
                let end = ui.cursor().top();
                let rect = Rect::from_min_max(
                    Pos2::new(ui.max_rect().left(), start),
                    Pos2::new(ui.max_rect().right(), end),
                );
                if self.jump_message.as_ref() == Some(&message.id) {
                    ui.scroll_to_rect(rect, Some(Align::Center));
                    self.jump_message = None;
                }
                if self
                    .restore_anchor
                    .as_ref()
                    .is_some_and(|anchor| anchor.message_id == message.id)
                {
                    let anchor = self.restore_anchor.take().unwrap();
                    ui.scroll_to_rect(
                        Rect::from_min_size(
                            Pos2::new(rect.left(), rect.top() + anchor.offset),
                            Vec2::splat(1.0),
                        ),
                        Some(Align::TOP),
                    );
                }
                if ui.is_rect_visible(rect) {
                    highest_seen = highest_seen.max(message.sequence);
                    if first_visible.is_none() {
                        first_visible = Some(ScrollAnchor {
                            message_id: message.id.clone(),
                            offset: (ui.clip_rect().top() - rect.top()).max(0.0),
                        });
                    }
                }
                height_changed |= self.row_heights.record(&message.id, end - start);
            }
            ui.add_space(following + 10.0);
            if height_changed {
                ui.ctx().request_repaint();
            }
            if !searching && self.bottom_follow.align(height_changed, self.scroll_to_end) {
                ui.scroll_to_cursor(Some(Align::BOTTOM));
                self.scroll_to_end = false;
            }
        });
        if interacted
            && !searching
            && output.state.offset.y <= 1.0
            && self.history_pending.is_none()
            && self.history_before.is_some()
        {
            self.load_history(true);
        }
        if self.messages_ready && !searching {
            if let (Some(scope), Some(target)) = (self.conversation_scope(), self.current.clone()) {
                let changed = self
                    .prefs
                    .conversations
                    .scroll(&scope, &target)
                    .is_none_or(|offset| (offset - output.state.offset.y).abs() > 1.0);
                self.prefs
                    .conversations
                    .set_scroll(&scope, &target, output.state.offset.y);
                if let Some(anchor) = first_visible {
                    self.prefs.conversations.set_anchor(&scope, &target, anchor);
                }
                if changed {
                    self.defer_preferences();
                }
                let focused = ui.ctx().input(|input| input.focused);
                if focused
                    && !self.modal_open()
                    && !self.add_community_open
                    && highest_seen
                        > read_sequence
                            .max(self.marked_read.get(&target).copied().unwrap_or_default())
                    && self
                        .network
                        .send(Command::MarkRead {
                            target: target.clone(),
                            last_read_sequence: highest_seen,
                        })
                        .is_ok()
                {
                    self.marked_read.insert(target, highest_seen);
                }
            }
        }
    }

    /// The other person in a direct message, once the whole history is loaded.
    fn direct_intro_user(&self, searching: bool) -> Option<User> {
        if searching || self.history_before.is_some() || !self.messages_ready {
            return None;
        }
        let Some(Target::Direct(id)) = &self.current else {
            return None;
        };
        self.bootstrap
            .as_ref()?
            .members
            .iter()
            .find(|user| &user.id == id)
            .cloned()
    }

    /// Big avatar, name, username and one line, above the first message. Its
    /// height is fixed so the virtual message rows below keep their offsets.
    fn direct_intro(&mut self, ui: &mut egui::Ui, user: &User) {
        let (rect, _) = ui.allocate_exact_size(
            Vec2::new(ui.available_width(), direct_intro_height()),
            Sense::hover(),
        );
        let mut intro = ui.child_ui(
            rect.shrink2(Vec2::new(16.0, 0.0)),
            Layout::top_down(Align::Min),
        );
        intro.set_clip_rect(rect.intersect(ui.clip_rect()));
        intro.add_space(20.0);
        self.user_avatar(&mut intro, user, 80.0, false);
        intro.add_space(10.0);
        let name = profile_ui::display_name(user).to_owned();
        intro.label(
            RichText::new(&name)
                .font(semibold(&intro, font_size(28.0)))
                .color(c_text()),
        );
        if !user.username.eq_ignore_ascii_case(&name) {
            intro.label(
                RichText::new(&user.username)
                    .size(font_size(16.0))
                    .color(c_text()),
            );
        }
        intro.add_space(10.0);
        intro.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            ui.label(
                RichText::new("This is the beginning of your direct message history with ")
                    .color(c_muted()),
            );
            ui.label(RichText::new(format!("@{name}")).strong().color(c_text()));
            ui.label(RichText::new(".").color(c_muted()));
        });
    }

    pub(super) fn message_row(&mut self, ui: &mut egui::Ui, message: &Message, grouped: bool) {
        let delivery = self
            .outgoing_messages
            .iter()
            .find(|(_, local, _, _)| local.id == message.id)
            .map(|(_, _, failed, confirmed)| (*failed, *confirmed));
        if let Some((failed, confirmed)) = delivery {
            if !grouped {
                ui.add_space(
                    self.prefs.appearance.message_spacing * if self.compact { 0.5 } else { 1.0 },
                );
            }
            egui::Frame::none()
                .inner_margin(Margin::symmetric(
                    16.0,
                    if grouped || self.compact { 2.0 } else { 4.0 },
                ))
                .show(ui, |ui| {
                    ui.set_min_width(ui.available_width());
                    ui.horizontal_top(|ui| {
                        if self.compact {
                            ui.label(
                                RichText::new(display_time(&message.created_at))
                                    .size(font_size(11.0))
                                    .color(c_muted()),
                            );
                        } else if grouped {
                            ui.allocate_exact_size(Vec2::new(36.0, 18.0), Sense::hover());
                        } else {
                            self.user_avatar(ui, &message.author, 36.0, false);
                        }
                        ui.add_space(if self.compact { 2.0 } else { 8.0 });
                        ui.vertical(|ui| {
                            ui.set_min_width(ui.available_width());
                            let color = if failed {
                                c_bad()
                            } else if confirmed {
                                c_text()
                            } else {
                                c_muted()
                            };
                            if !grouped || self.compact {
                                ui.horizontal(|ui| {
                                    ui.label(
                                        RichText::new(profile_ui::display_name(&message.author))
                                            .size(font_size(15.0))
                                            .family(egui::FontFamily::Name("Relay Semibold".into()))
                                            .color(self.member_role_color(&message.author))
                                            .strong(),
                                    );
                                    if !self.compact {
                                        ui.label(
                                            RichText::new(display_time(&message.created_at))
                                                .size(font_size(11.0))
                                                .color(c_muted()),
                                        );
                                    }
                                });
                            }
                            self.delivery_preview(ui, message, color, confirmed);
                            for attachment in &message.attachments {
                                ui.label(RichText::new(&attachment.name).color(color));
                            }
                            if !failed && !confirmed {
                                ui.label(
                                    RichText::new("Sending…")
                                        .size(font_size(TEXT_CAPTION))
                                        .color(c_muted()),
                                );
                            }
                            if failed {
                                ui.label(RichText::new("Message could not be sent").color(c_bad()));
                                let available = self
                                    .current
                                    .as_ref()
                                    .is_some_and(|target| !self.pending_sends.contains(target))
                                    && self.draft.is_empty()
                                    && self.attachments.is_empty();
                                if ui.add_enabled(available, secondary("Retry")).clicked() {
                                    if let Some(target) = self.current.clone() {
                                        self.send_ids.insert(
                                            target,
                                            SendAttempt {
                                                client_id: message.client_id.clone().unwrap(),
                                                content: message.content.clone(),
                                                attachments: message
                                                    .attachments
                                                    .iter()
                                                    .map(|a| a.id.clone())
                                                    .collect(),
                                                reply_to_id: message.reply_to_id.clone(),
                                                mentions: message.mentions.clone(),
                                            },
                                        );
                                        self.draft = message.content.clone();
                                        self.attachments = message.attachments.clone();
                                        self.reply =
                                            message.reply_to_id.as_ref().map(|id| ReplyDraft {
                                                message_id: id.clone(),
                                                ..Default::default()
                                            });
                                        self.submit_composer();
                                    }
                                }
                            }
                        });
                    });
                });
            return;
        }
        if self.is_blocked(&message.author.id) {
            self.online_gifs
                .draw_blocked_message(ui, &message.id, &message.content, &self.prefs);
            return;
        }
        let gif_only = crate::online_gifs::message_id(&message.content).is_some();
        let mut media_click_regions = Vec::new();
        if !grouped {
            ui.add_space(if self.compact {
                self.prefs.appearance.message_spacing * 0.5
            } else {
                self.prefs.appearance.message_spacing
            });
        }
        let background = ui.painter().add(egui::Shape::Noop);
        let response = egui::Frame::none()
            .inner_margin(Margin::symmetric(
                16.0,
                if grouped || self.compact { 2.0 } else { 4.0 },
            ))
            .show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                ui.horizontal_top(|ui| {
                    if self.compact {
                        ui.label(
                            RichText::new(display_time(&message.created_at))
                                .size(font_size(11.0))
                                .color(c_muted()),
                        );
                    } else if grouped {
                        ui.allocate_exact_size(Vec2::new(36.0, 18.0), Sense::hover());
                    } else {
                        self.user_avatar(ui, &message.author, 36.0, false);
                    }
                    ui.add_space(if self.compact { 2.0 } else { 8.0 });
                    ui.vertical(|ui| {
                        ui.set_min_width(ui.available_width());
                        if message.pinned_at.is_some() {
                            ui.label(
                                RichText::new("Pinned message")
                                    .size(font_size(TEXT_CAPTION))
                                    .color(c_accent()),
                            );
                        }
                        self.message_protection_label(ui, &message.id);
                        if let Some(reply_id) = &message.reply_to_id {
                            let parent = self
                                .messages
                                .iter()
                                .find(|parent| &parent.id == reply_id)
                                .map(|parent| {
                                    (
                                        profile_ui::display_name(&parent.author).to_owned(),
                                        self.member_role_color(&parent.author),
                                        crate::message_format::plain_preview(&parent.content, 160),
                                    )
                                });
                            let clicked = ui
                                .horizontal(|ui| {
                                    ui.spacing_mut().item_spacing.x = 4.0;
                                    icon(ui, Icon::Reply, 14.0, c_muted());
                                    let mut clicked = false;
                                    match &parent {
                                        Some((name, color, preview)) => {
                                            clicked |= ui
                                                .add(
                                                    egui::Label::new(
                                                        RichText::new(name)
                                                            .size(font_size(12.0))
                                                            .family(egui::FontFamily::Name(
                                                                "Relay Semibold".into(),
                                                            ))
                                                            .strong()
                                                            .color(*color),
                                                    )
                                                    .sense(Sense::click()),
                                                )
                                                .clicked();
                                            clicked |= ui
                                                .add(
                                                    egui::Label::new(
                                                        RichText::new(preview)
                                                            .size(font_size(12.0))
                                                            .color(c_muted()),
                                                    )
                                                    .truncate(true)
                                                    .sense(Sense::click()),
                                                )
                                                .on_hover_text("Open the original message")
                                                .clicked();
                                        }
                                        None => {
                                            clicked |= ui
                                                .add(
                                                    egui::Label::new(
                                                        RichText::new(
                                                            "Reply to an earlier message",
                                                        )
                                                        .size(font_size(12.0))
                                                        .italics()
                                                        .color(c_muted()),
                                                    )
                                                    .sense(Sense::click()),
                                                )
                                                .clicked();
                                        }
                                    }
                                    clicked
                                })
                                .inner;
                            if clicked {
                                self.reveal_message(reply_id.clone());
                            }
                        }
                        if !self.search.trim().is_empty()
                            && ui.small_button("View in conversation").clicked()
                        {
                            self.reveal_message(message.id.clone());
                        }
                        if !grouped && !self.compact {
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new(profile_ui::display_name(&message.author))
                                        .size(font_size(15.0))
                                        .family(egui::FontFamily::Name("Relay Semibold".into()))
                                        .strong()
                                        .color(self.member_role_color(&message.author)),
                                );
                                if !self.compact {
                                    ui.label(
                                        RichText::new(display_time(&message.created_at))
                                            .size(font_size(11.0))
                                            .color(c_muted()),
                                    )
                                    .on_hover_text(&message.created_at);
                                }
                            });
                        }
                        let editing = self
                            .editing
                            .as_ref()
                            .is_some_and(|(id, _)| *id == message.id);
                        if editing {
                            let enabled = !self.modal_open() && ui.is_enabled();
                            let pending = self.pending_edits.contains(&message.id);
                            let (_, value) = self.editing.as_mut().unwrap();
                            let (save, cancel) = message_edit_controls(
                                ui,
                                egui::Id::new(("message-editor", &message.id)),
                                value,
                                !message.attachments.is_empty(),
                                pending,
                                enabled,
                            );
                            if save {
                                let value = value.trim().to_owned();
                                let content = self.expand_message_shortcodes(&value);
                                if let Some(origin) = self.command_origin() {
                                    match self.network.send(Command::EditMessage {
                                        origin,
                                        message_id: message.id.clone(),
                                        content,
                                    }) {
                                        Ok(()) => {
                                            self.pending_edits.insert(message.id.clone());
                                        }
                                        Err(error) => self.error = Some(error),
                                    }
                                }
                            }
                            if cancel {
                                self.editing = None;
                            }
                        } else if self.poll_view(ui, message) {
                        } else if (!message.content.is_empty() && !gif_only)
                            || self.compact
                            || message.edited_at.is_some()
                        {
                            let mut text = egui::text::LayoutJob::default();
                            let mut inline = crate::emoji::Inline::default();
                            let mut visible_char_count = 0;
                            if self.compact {
                                let author =
                                    format!("{}  ", profile_ui::display_name(&message.author));
                                visible_char_count = author.chars().count();
                                inline.accessible.push_str(&author);
                                text.append(
                                    &author,
                                    0.0,
                                    egui::TextFormat {
                                        font_id: FontId::new(
                                            font_size(14.0),
                                            egui::FontFamily::Name("Relay Semibold".into()),
                                        ),
                                        color: c_text(),
                                        ..Default::default()
                                    },
                                );
                            }
                            let spoilers_revealed = self.revealed_spoiler_message.as_deref()
                                == Some(message.id.as_str());
                            let custom_emoji =
                                self.inline_community_emoji(&message.content, spoilers_revealed);
                            let emoji_only = if custom_emoji.is_empty() {
                                crate::emoji::emoji_only(&message.content)
                            } else {
                                crate::emoji::emoji_only_with_custom(
                                    &message.content,
                                    &custom_emoji,
                                )
                            };
                            let message_size = font_size(if !self.compact && emoji_only {
                                30.0
                            } else if self.compact {
                                14.0
                            } else {
                                15.0
                            });
                            let spans = crate::message_format::parse(if gif_only {
                                ""
                            } else {
                                &message.content
                            });
                            let has_spoilers = spans.iter().any(|span| span.style.spoiler);
                            let mut message_links = Vec::new();
                            for span in spans {
                                let span_start = visible_char_count;
                                let hidden = span.style.spoiler && !spoilers_revealed;
                                let mut content = if hidden { "[spoiler]" } else { &span.text };
                                let message_size = span.style.text_size(message_size);
                                if span.style.quote && !hidden {
                                    if let Some(rest) = content.strip_prefix("│ ") {
                                        inline.quote_bar(&mut text, message_size, c_muted());
                                        content = rest;
                                    }
                                }
                                let font_id = if span.style.code {
                                    FontId::monospace(message_size)
                                } else if span.style.bold {
                                    FontId::new(
                                        message_size,
                                        egui::FontFamily::Name("Relay Semibold".into()),
                                    )
                                } else {
                                    FontId::proportional(message_size)
                                };
                                inline.append_with_custom(
                                    &mut text,
                                    content,
                                    egui::TextFormat {
                                        font_id,
                                        color: if span.style.spoiler && !spoilers_revealed {
                                            c_muted()
                                        } else if span.link.is_some() {
                                            c_accent()
                                        } else if span.style.small || span.style.quote {
                                            c_muted()
                                        } else {
                                            c_text()
                                        },
                                        background: if span.style.spoiler && !spoilers_revealed {
                                            c_line()
                                        } else if span.style.code {
                                            ui.visuals().code_bg_color
                                        } else {
                                            Color32::TRANSPARENT
                                        },
                                        italics: span.style.italic,
                                        underline: if (span.link.is_some() || span.style.underline)
                                            && (spoilers_revealed || !span.style.spoiler)
                                        {
                                            Stroke::new(
                                                1.0_f32,
                                                if span.link.is_some() {
                                                    c_accent()
                                                } else {
                                                    c_text()
                                                },
                                            )
                                        } else {
                                            Stroke::NONE
                                        },
                                        strikethrough: if span.style.strike {
                                            Stroke::new(1.0_f32, c_text())
                                        } else {
                                            Stroke::NONE
                                        },
                                        ..Default::default()
                                    },
                                    !hidden && !span.style.code,
                                    &custom_emoji,
                                );
                                visible_char_count = text.text.chars().count();
                                if let Some(url) = span.link.as_ref().filter(|_| !hidden) {
                                    message_links.push((
                                        span_start..visible_char_count,
                                        url.clone(),
                                        crate::message_format::masked_link(&span.text, url),
                                    ));
                                }
                            }
                            if message.edited_at.is_some() {
                                text.append(
                                    "  (edited)",
                                    0.0,
                                    egui::TextFormat {
                                        font_id: FontId::proportional(font_size(TEXT_CAPTION)),
                                        color: c_muted(),
                                        ..Default::default()
                                    },
                                );
                            }
                            text.wrap.max_width = ui.available_width().max(1.0);
                            let galley = ui.painter().layout_job(text);
                            let response = ui.add(
                                egui::Label::new(egui::WidgetText::Galley(galley.clone()))
                                    .sense(Sense::click()),
                            );
                            response.widget_info(|| {
                                egui::WidgetInfo::labeled(
                                    egui::WidgetType::Label,
                                    &inline.accessible,
                                )
                            });
                            inline.paint(ui, &galley, response.rect.min);
                            let hovered_link = response.hover_pos().and_then(|position| {
                                let index = galley
                                    .cursor_from_pos(position - response.rect.min)
                                    .ccursor
                                    .index;
                                message_links
                                    .iter()
                                    .find(|(range, _, _)| range.contains(&index))
                            });
                            if response.clicked() {
                                if let Some(position) = response.interact_pointer_pos() {
                                    let index = galley
                                        .cursor_from_pos(position - response.rect.min)
                                        .ccursor
                                        .index;
                                    if let Some((_, url, masked)) = message_links
                                        .iter()
                                        .find(|(range, _, _)| range.contains(&index))
                                    {
                                        if *masked {
                                            // The words can say anything: show where it goes first.
                                            self.pending_link = Some(url.clone());
                                        } else {
                                            ui.ctx().open_url(egui::OpenUrl::new_tab(url));
                                        }
                                    } else if has_spoilers {
                                        self.revealed_spoiler_message = if spoilers_revealed {
                                            None
                                        } else {
                                            Some(message.id.clone())
                                        };
                                    }
                                }
                            }
                            if has_spoilers {
                                response.on_hover_text(if spoilers_revealed {
                                    "Click to hide spoiler"
                                } else {
                                    "Click to reveal spoiler"
                                });
                            } else if let Some((_, url, _)) = hovered_link {
                                response
                                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                                    .on_hover_text(url);
                            }
                        }
                        for attachment in &message.attachments {
                            ui.add_space(6.0);
                            if ui
                                .add(secondary(&format!(
                                    "{}  ·  {}",
                                    attachment.name,
                                    size_label(attachment.size)
                                )))
                                .on_hover_text("Download file")
                                .clicked()
                            {
                                if let Some(path) = rfd::FileDialog::new()
                                    .set_file_name(&attachment.name)
                                    .save_file()
                                {
                                    self.start_download(attachment.clone(), path);
                                }
                            }
                        }
                        for attachment in &message.attachments {
                            let preview =
                                ui.scope(|ui| self.attachment_preview(ui, attachment, message));
                            if preview.response.rect.is_positive() {
                                media_click_regions.push(preview.response.rect);
                            }
                        }
                        if gif_only {
                            let preview = ui.scope(|ui| {
                                self.online_gif_message(ui, &message.content);
                            });
                            media_click_regions.push(preview.response.rect);
                        } else {
                            self.expression_message_view(
                                ui,
                                &message.content,
                                self.revealed_spoiler_message.as_deref()
                                    == Some(message.id.as_str()),
                            );
                        }
                        if message
                            .reactions
                            .iter()
                            .any(|r| !Self::is_poll_vote(&r.emoji))
                        {
                            ui.add_space(4.0);
                            ui.horizontal_wrapped(|ui| {
                                for reaction in message
                                    .reactions
                                    .iter()
                                    .filter(|r| !Self::is_poll_vote(&r.emoji))
                                {
                                    let selected = self
                                        .session
                                        .as_ref()
                                        .is_some_and(|s| reaction.user_ids.contains(&s.user.id));
                                    let reaction_response = crate::emoji::reaction_button(
                                        ui,
                                        &reaction.emoji,
                                        reaction.user_ids.len(),
                                        if selected {
                                            UI_PALETTE.with(|palette| palette.get().selection)
                                        } else {
                                            c_panel()
                                        },
                                        Stroke::new(
                                            1.0_f32,
                                            if selected { c_accent() } else { c_line() },
                                        ),
                                    );
                                    media_click_regions.push(reaction_response.rect);
                                    if reaction_response.clicked() {
                                        self.react_to_message(message, &reaction.emoji);
                                    }
                                }
                            });
                        }
                    });
                });
            })
            .response;
        let owner = self
            .session
            .as_ref()
            .is_some_and(|s| s.user.id == message.author.id);
        let action_width = if owner { 140.0 } else { 106.0 };
        let quick_count = ((response.rect.width() - action_width - 28.0) / 28.0)
            .floor()
            .clamp(0.0, 3.0) as usize;
        let width = action_width + quick_count as f32 * 28.0;
        let toolbar_pos = message_toolbar_position(response.rect, ui.clip_rect(), width);
        let toolbar_rect = Rect::from_min_size(toolbar_pos, Vec2::new(width + 8.0, 40.0));
        let actions_enabled = !self.modal_open() && ui.is_enabled();
        let hovered = actions_enabled
            && !self.expressions_open()
            && ui.input(|i| {
                i.pointer.hover_pos().is_some_and(|p| {
                    response.contains_pointer()
                        || (self.message_hover.as_ref() == Some(&message.id)
                            && toolbar_rect.contains(p))
                })
            });
        if hovered {
            self.message_hover = Some(message.id.clone());
        } else if self.message_hover.as_ref() == Some(&message.id) {
            self.message_hover = None;
        }
        let mentioned = self
            .session
            .as_ref()
            .is_some_and(|session| message.mentions.contains(&session.user.id));
        if hovered || mentioned {
            ui.painter().set(
                background,
                egui::Shape::rect_filled(
                    response.rect,
                    0.0,
                    if mentioned {
                        UI_PALETTE.with(|palette| palette.get().mention_message)
                    } else {
                        UI_PALETTE.with(|palette| palette.get().hover_message)
                    },
                ),
            );
            if mentioned {
                ui.painter().rect_filled(
                    Rect::from_min_max(
                        response.rect.left_top(),
                        Pos2::new(response.rect.left() + 3.0, response.rect.bottom()),
                    ),
                    0.0,
                    MENTION_BAR,
                );
            }
        }
        if actions_enabled {
            online_gif_ui::message_context_menu(ui, &response, &media_click_regions, |ui| {
                self.message_actions(ui, message)
            });
        }
        if actions_enabled
            && !self.expressions_open()
            && (hovered || self.message_menu.as_ref() == Some(&message.id))
        {
            chat_ui::message_toolbar_area(&message.id, toolbar_pos).show(ui.ctx(), |ui| {
                egui::Frame::none()
                    .fill(c_panel())
                    .stroke(Stroke::new(1.0_f32, c_line()))
                    .rounding(6.0)
                    .inner_margin(Margin::same(2.0))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 0.0;
                            if quick_count > 0 {
                                ui.scope(|ui| {
                                    ui.spacing_mut().button_padding = Vec2::splat(4.0);
                                    for index in crate::emoji::quick_reactions(
                                        &self.prefs.emoji_recent,
                                        self.prefs.emoji_tone,
                                    )
                                    .into_iter()
                                    .take(quick_count)
                                    {
                                        let entry = &crate::emoji::catalog().entries[index];
                                        let response = crate::emoji::action_button(ui, index);
                                        response.widget_info(|| {
                                            egui::WidgetInfo::labeled(
                                                egui::WidgetType::Button,
                                                format!("React with :{}:", entry.aliases[0]),
                                            )
                                        });
                                        if response.clicked() {
                                            self.remember_emoji(index);
                                            self.react_to_message(message, &entry.unicode);
                                        }
                                    }
                                });
                            }
                            if icon_button(ui, Icon::Reply, false, "Reply to message").clicked() {
                                self.begin_reply(ui, message);
                            }
                            if icon_button(ui, Icon::Smile, false, "Add a reaction").clicked() {
                                if let (Some(origin), Some(target)) =
                                    (self.command_origin(), self.current.clone())
                                {
                                    self.open_expression_picker();
                                    self.admin_ui.expression_reaction =
                                        Some((origin, target, message.clone()));
                                }
                            }
                            if owner && icon_button(ui, Icon::Edit, false, "Edit message").clicked()
                            {
                                self.begin_message_edit(ui, message);
                            }
                            let menu =
                                ui.menu_button("•••", |ui| self.message_actions(ui, message));
                            if menu.inner.is_some() {
                                self.message_menu = Some(message.id.clone());
                            } else if self.message_menu.as_ref() == Some(&message.id)
                                && self.capture_path.is_none()
                            {
                                self.message_menu = None;
                            }
                        });
                    });
            });
        }
    }

    pub(super) fn message_actions(&mut self, ui: &mut egui::Ui, message: &Message) {
        if self.can_pin(message)
            && ui
                .add_enabled(
                    self.pins.action.is_none(),
                    egui::Button::new(if message.pinned_at.is_some() {
                        "Unpin message"
                    } else {
                        "Pin message"
                    }),
                )
                .clicked()
        {
            self.set_message_pin(message, message.pinned_at.is_none());
            ui.close_menu();
        }
        if ui.button("Reply").clicked() {
            self.begin_reply(ui, message);
            ui.close_menu();
        }
        for (label, emoji) in [("Like", "👍"), ("Love", "❤️"), ("Celebrate", "🎉")] {
            if ui.button(label).clicked() {
                self.react_to_message(message, emoji);
                ui.close_menu();
            }
        }
        if ui.button("Choose a reaction…").clicked() {
            if let (Some(origin), Some(target)) = (self.command_origin(), self.current.clone()) {
                self.open_expression_picker();
                self.admin_ui.expression_reaction = Some((origin, target, message.clone()));
                ui.close_menu();
            }
        }
        ui.separator();
        if ui.button("Copy text").clicked() {
            ui.output_mut(|o| o.copied_text = message.content.clone());
            ui.close_menu();
        }
        if self
            .session
            .as_ref()
            .is_some_and(|s| s.user.id != message.author.id)
            && ui.button("Report Message").clicked()
        {
            self.begin_report(message.author.clone(), message.content.clone());
            ui.close_menu();
        }
        if self
            .session
            .as_ref()
            .is_some_and(|s| s.user.id == message.author.id)
            && ui.button("Edit message").clicked()
        {
            ui.close_menu();
            self.begin_message_edit(ui, message);
        }
        if self.session.as_ref().is_some_and(|s| {
            s.user.id == message.author.id
                || self
                    .bootstrap
                    .as_ref()
                    .is_some_and(|b| b.has_permission(PERMISSION_MESSAGES))
        }) && ui
            .button(RichText::new("Delete message").color(c_bad()))
            .clicked()
        {
            self.delete_confirm = Some(message.clone());
            ui.close_menu();
        }
    }

    fn begin_message_edit(&mut self, ui: &egui::Ui, message: &Message) {
        self.editing = Some((message.id.clone(), message.content.clone()));
        ui.ctx().memory_mut(|memory| {
            memory.request_focus(egui::Id::new(("message-editor", &message.id)));
        });
    }
}

/// Consume the save shortcut before the multiline editor can insert a newline.
/// Escape continues through the app's existing modal-aware dismiss handling.
fn message_edit_controls(
    ui: &mut egui::Ui,
    id: egui::Id,
    value: &mut String,
    has_files: bool,
    pending: bool,
    enabled: bool,
) -> (bool, bool) {
    let shortcut = enabled
        && !pending
        && ui.memory(|memory| memory.has_focus(id))
        && ui.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, egui::Key::Enter));
    ui.add_enabled(
        enabled && !pending,
        egui::TextEdit::multiline(value)
            .id(id)
            .desired_width(f32::INFINITY)
            .desired_rows(2)
            .char_limit(4000)
            .font(FontId::proportional(font_size(15.0))),
    );
    let valid = has_files || !value.trim().is_empty();
    let (save, cancel) = ui.horizontal_wrapped(|ui| {
        let save = ui.add_enabled(enabled && !pending && valid, primary(if pending { "Saving…" } else { "Save" })).clicked();
        let cancel = ui.add_enabled(enabled, secondary("Cancel")).clicked();
        (save, cancel)
    }).inner;
    ui.add(egui::Label::new(RichText::new(if valid {
        "Ctrl+Enter to save · Esc to cancel · Enter for a new line"
    } else {
        "Add text to save, or use Delete message to remove it."
    }).size(font_size(TEXT_CAPTION)).color(c_muted())).wrap(true));
    (save || (shortcut && valid), cancel)
}

/// The direct message introduction's fixed height: room for an 80 px avatar,
/// the name, the username and two lines of text at the current text size.
fn direct_intro_height() -> f32 {
    20.0 + 80.0
        + 10.0
        + font_size(28.0) * 1.3
        + font_size(16.0) * 1.3
        + 10.0
        + font_size(15.0) * 1.4 * 2.0
        + 20.0
}

#[cfg(test)]
mod edit_tests {
    use super::*;

    fn frame(
        context: &egui::Context,
        text: &mut String,
        modifiers: Option<egui::Modifiers>,
        has_files: bool,
        pending: bool,
        enabled: bool,
    ) -> bool {
        let mut save = false;
        let _ = context.run(
            egui::RawInput {
                focused: true,
                events: modifiers.into_iter().map(|modifiers| egui::Event::Key {
                    key: egui::Key::Enter,
                    physical_key: Some(egui::Key::Enter),
                    pressed: true,
                    repeat: false,
                    modifiers,
                }).collect(),
                modifiers: modifiers.unwrap_or_default(),
                ..Default::default()
            },
            |context| {
                egui::CentralPanel::default().show(context, |ui| {
                    save = message_edit_controls(ui, egui::Id::new("editor"), text, has_files, pending, enabled).0;
                });
            },
        );
        save
    }

    #[test]
    fn message_edit_shortcut_is_focused_and_does_not_insert_a_newline() {
        let context = egui::Context::default();
        configure(&context);
        let id = egui::Id::new("editor");
        let command = egui::Modifiers { ctrl: true, command: true, ..Default::default() };
        let mut text = "Existing message".to_owned();
        frame(&context, &mut text, None, false, false, true);
        context.memory_mut(|memory| memory.request_focus(id));
        assert!(frame(&context, &mut text, Some(command), false, false, true));
        assert_eq!(text, "Existing message");
        context.memory_mut(|memory| memory.surrender_focus(id));
        assert!(!frame(&context, &mut text, Some(command), false, false, true));
        context.memory_mut(|memory| memory.request_focus(id));
        assert!(!frame(&context, &mut text, Some(egui::Modifiers::NONE), false, false, true));
        assert!(text.contains('\n'));
        assert_eq!(text.replace('\n', ""), "Existing message");
        context.memory_mut(|memory| memory.request_focus(id));
        assert!(!frame(&context, &mut text, Some(command), false, true, true));
        context.memory_mut(|memory| memory.request_focus(id));
        assert!(!frame(&context, &mut text, Some(command), false, false, false));
        text.clear();
        frame(&context, &mut text, None, false, false, true);
        context.memory_mut(|memory| memory.request_focus(id));
        assert!(!frame(&context, &mut text, Some(command), false, false, true));
        assert!(text.is_empty());
        context.memory_mut(|memory| memory.request_focus(id));
        assert!(frame(&context, &mut text, Some(command), true, false, true));
    }
}
