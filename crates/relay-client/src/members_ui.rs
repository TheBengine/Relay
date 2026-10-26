//! A narrow viewport keeps the conversation readable and opens members on demand.
use super::*;

impl RelayApp {
    pub(super) fn members(&mut self, ctx: &egui::Context) {
        self.members_compact = ctx.available_rect().width() < 560.0
            || (self.current_voice_text_parent().is_some() && ctx.screen_rect().width() < 1100.0);
        if self.home_page.is_some()
            || matches!(self.current, Some(Target::Direct(_)))
            || self.members_compact
        {
            return;
        }
        if !(if self.current_name().2 {
            self.show_voice_members
        } else {
            self.show_members
        }) {
            return;
        }
        egui::SidePanel::right("members")
            .exact_width(208.0)
            .resizable(false)
            .frame(
                egui::Frame::none()
                    .fill(c_panel())
                    .inner_margin(Margin::symmetric(12.0, 0.0)),
            )
            .show(ctx, |ui| {
                ui.set_enabled(!self.modal_open());
                ui.add_space(50.0);
                self.members_list(ui);
            });
    }

    pub(super) fn compact_members_dialog(&mut self, ctx: &egui::Context) {
        if !self.compact_members_open || self.home_page.is_some() || self.modal_open() {
            return;
        }
        let mut open = true;
        egui::Window::new("Members")
            .id(egui::Id::new("compact-members"))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(240.0)
            .anchor(Align2::RIGHT_TOP, Vec2::new(-16.0, 80.0))
            .max_height((ctx.screen_rect().height() - 120.0).max(100.0))
            .show(ctx, |ui| self.members_list(ui));
        self.compact_members_open &= open;
    }

    fn members_list(&mut self, ui: &mut egui::Ui) {
        let Some(b) = self.bootstrap.clone() else {
            return;
        };
        let group_scope = self.current.as_ref().and_then(|target| {
            let Target::Channel(id) = target else {
                return None;
            };
            let channel = b.channels.iter().find(|c| c.id == *id)?;
            let effective = channel.voice_parent_id.as_deref().unwrap_or(id);
            let info = b.conversations.iter().find(|c| c.channel_id == effective)?;
            let group = if info.kind == "group_voice" {
                b.conversations
                    .iter()
                    .find(|c| Some(c.channel_id.as_str()) == info.parent_id.as_deref())?
            } else {
                info
            };
            (group.kind == "group").then(|| (group.channel_id.clone(), group.member_ids.clone()))
        });
        let group_channel = group_scope.as_ref().map(|(channel, _)| channel.clone());
        let group_members = group_scope.map(|(_, ids)| ids);
        let friends_server = self.in_people();
        egui::ScrollArea::vertical()
            .id_source("member-list")
            .show(ui, |ui| {
                for admin in [true, false] {
                    let count = b
                        .members
                        .iter()
                        .filter(|m| {
                            (m.role == "admin") == admin
                                && group_members.as_ref().is_none_or(|ids| ids.contains(&m.id))
                        })
                        .count();
                    if count == 0 {
                        continue;
                    }
                    ui.add_space(18.0);
                    ui.label(
                        RichText::new(format!(
                            "{} — {count}",
                            if admin { "SERVER OWNER" } else { "MEMBERS" }
                        ))
                        .size(font_size(TEXT_CAPTION))
                        .family(egui::FontFamily::Name("Relay Semibold".into()))
                        .strong()
                        .color(c_muted()),
                    );
                    ui.add_space(6.0);
                    for user in b.members.iter().filter(|m| {
                        (m.role == "admin") == admin
                            && group_members.as_ref().is_none_or(|ids| ids.contains(&m.id))
                    }) {
                        let texture = self.avatar_texture(user);
                        let response = user_row(ui, user, false, 0, 0, false, texture.as_ref());
                        match &group_channel {
                            // In a Friends group, people who are not friends show the
                            // group's coarse state; friends and you keep your own list.
                            Some(channel)
                                if friends_server
                                    && user.id != b.user.id
                                    && !b.social.friends.contains(&user.id) =>
                            {
                                self.paint_group_presence_badge(ui, channel, &user.id, &response)
                            }
                            _ => self.paint_presence_badge(ui, &user.id, &response),
                        }
                        self.user_interactions(ui, user, &response);
                        if response.clicked() {
                            self.open_profile(user.clone());
                        }
                    }
                }
            });
    }
}
