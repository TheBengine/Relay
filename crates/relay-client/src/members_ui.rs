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
        let group_members = self.current.as_ref().and_then(|target| {
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
            (group.kind == "group").then(|| group.member_ids.clone())
        });
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
                        let response = user_row(ui, user, false, 0, 0, texture.as_ref());
                        self.paint_presence_badge(ui, &user.id, &response);
                        self.user_interactions(ui, user, &response);
                        if response.clicked() {
                            self.open_profile(user.clone());
                        }
                    }
                }
            });
    }
}
