//! "Check access": what one member can do in one channel, and why.
//!
//! The answer comes from the server's `GET /api/channels/{id}/access/{user}`,
//! which runs the exact rule the server uses to allow or refuse requests
//! (`relay_protocol::channel_access::resolve`). This file only turns that
//! answer into sentences. It does not guess from the editor's unsaved rows.
use super::*;
use relay_protocol::channel_access::{EffectiveChannelAccess, Rule};

/// The four channel permissions, in the order the channel editor shows them.
pub(super) const CHANNEL_PERMISSIONS: [(u32, &str); 4] = [
    (PERMISSION_READ, "Read and see the channel"),
    (PERMISSION_SEND, "Send messages"),
    (PERMISSION_JOIN, "Join voice"),
    (PERMISSION_STREAM, "Share camera and screen"),
];

pub(super) const LIMITS_NOTE: &str = "Checked by the server with its current rules. Slowmode and moderation actions such as a voice mute can still limit a person at a given moment.";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Line {
    pub bit: u32,
    pub label: &'static str,
    pub allowed: bool,
    pub why: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Explanation {
    pub headline: String,
    pub lines: Vec<Line>,
    pub note: Option<&'static str>,
}

fn role_list(ids: &[String], role_name: &dyn Fn(&str) -> Option<String>) -> String {
    let names: Vec<String> = ids
        .iter()
        .map(|id| role_name(id).unwrap_or_else(|| "a removed role".into()))
        .collect();
    match names.len() {
        0 => "no role".into(),
        1 => names[0].clone(),
        _ => {
            let (last, rest) = names.split_last().unwrap();
            format!("{} and {last}", rest.join(", "))
        }
    }
}

fn base_role_name(role: &str) -> &'static str {
    match role {
        "admin" => "Owner",
        "moderator" => "Moderator",
        _ => "Member",
    }
}

/// Sentences for one answer. `role_name` maps a custom role id to its name.
pub(super) fn explain(
    answer: &EffectiveChannelAccess,
    person: &str,
    role_name: &dyn Fn(&str) -> Option<String>,
) -> Explanation {
    let access = &answer.access;
    let headline = match &access.rule {
        Rule::Inactive => format!("{person} is banned or has left this community, so nothing is allowed."),
        Rule::Administrator => format!("{person} has Administrator, which bypasses every channel rule."),
        Rule::Person => format!("A rule for {person} on this channel decides. It overrides their roles."),
        Rule::Roles { role_ids } => format!(
            "The channel's rules for {} decide. Any of these roles can grant a permission.",
            role_list(role_ids, role_name)
        ),
        Rule::BaseRole { role } => format!(
            "The channel's rule for everyone with the built-in {} role decides.",
            base_role_name(role)
        ),
        Rule::PublicDefault => format!(
            "Public channel with no rule for {person} or their roles, so their community-wide role permissions apply."
        ),
        Rule::PrivateDefault => format!(
            "Private channel with no rule for {person} or their roles, so nothing is granted. Add a rule for them or one of their roles."
        ),
    };
    let lines = CHANNEL_PERMISSIONS
        .iter()
        .map(|(bit, label)| {
            let allowed = access.allows(*bit);
            let why = if allowed {
                match access.rule {
                    Rule::Administrator => "Allowed by Administrator.".into(),
                    Rule::PublicDefault => "Allowed by their community-wide roles.".into(),
                    _ => "Allowed by the deciding rule.".into(),
                }
            } else if access.blocked_by_missing_read(*bit) {
                "The rule grants it, but without Read nothing else works.".into()
            } else {
                match access.rule {
                    Rule::Inactive => "Not a current member.".into(),
                    Rule::PublicDefault => "None of their roles grants it community-wide.".into(),
                    Rule::PrivateDefault => "Nothing grants access to this private channel.".into(),
                    _ => "The deciding rule does not grant it.".into(),
                }
            };
            Line { bit: *bit, label, allowed, why }
        })
        .collect();
    Explanation {
        headline,
        lines,
        note: answer
            .rules_from_channel_id
            .is_some()
            .then_some("This is a voice channel's text chat. It uses the voice channel's rules."),
    }
}

pub(super) fn request_key(channel: &str, user: &str) -> String {
    format!("access:{channel}:{user}")
}

pub(super) fn request_path(channel: &str, user: &str) -> String {
    format!("/api/channels/{channel}/access/{user}")
}

/// Channels the check can answer for: regular community channels. A voice
/// channel's text chat is covered by its voice channel.
/// The permissions that mean something for this kind of channel: a text
/// channel has no voice to join or screen to share.
pub(super) fn relevant(kind: &str, bit: u32) -> bool {
    kind == "voice" || matches!(bit, PERMISSION_READ | PERMISSION_SEND)
}

/// "#name" for text channels; voice channels say so instead of a "#".
pub(super) fn channel_label(channel: &Channel) -> String {
    if channel.kind == "voice" {
        format!("{} · voice", channel.name)
    } else {
        format!("#{}", channel.name)
    }
}

pub(super) fn checkable_channels(bootstrap: &Bootstrap) -> Vec<&Channel> {
    bootstrap
        .channels
        .iter()
        .filter(|channel| channel.voice_parent_id.is_none())
        .collect()
}

impl RelayApp {
    /// Pick a person (and a channel, unless one is given) and show the
    /// server's answer. Used in channel settings and in member settings.
    pub(super) fn access_check_ui(
        &mut self,
        ui: &mut egui::Ui,
        bootstrap: &Bootstrap,
        fixed_channel: Option<&str>,
        fixed_member: Option<&str>,
    ) {
        if let Some(channel) = fixed_channel {
            self.admin_ui.access_channel = channel.to_owned();
        }
        if let Some(member) = fixed_member {
            self.admin_ui.access_member = member.to_owned();
        }
        let small = |text: &str| {
            RichText::new(text)
                .size(font_size(TEXT_CAPTION))
                .color(c_muted())
                .strong()
        };
        ui.horizontal_wrapped(|ui| {
            if fixed_member.is_none() {
                let chosen = bootstrap
                    .members
                    .iter()
                    .find(|m| m.id == self.admin_ui.access_member)
                    .map_or("Choose a person", |m| m.username.as_str())
                    .to_owned();
                ui.vertical(|ui| {
                    ui.label(small("PERSON"));
                    let response = egui::ComboBox::from_id_source(("access-member", fixed_channel))
                        .selected_text(&chosen)
                        .show_ui(ui, |ui| {
                            for member in &bootstrap.members {
                                ui.selectable_value(
                                    &mut self.admin_ui.access_member,
                                    member.id.clone(),
                                    &member.username,
                                );
                            }
                        })
                        .response;
                    response.widget_info(|| {
                        egui::WidgetInfo::labeled(egui::WidgetType::ComboBox, format!("Person: {chosen}"))
                    });
                });
            }
            if fixed_channel.is_none() {
                let channels = checkable_channels(bootstrap);
                let chosen = channels
                    .iter()
                    .find(|c| c.id == self.admin_ui.access_channel)
                    .map_or("Choose a channel".to_owned(), |c| channel_label(c));
                ui.vertical(|ui| {
                    ui.label(small("CHANNEL"));
                    let response = egui::ComboBox::from_id_source(("access-channel", fixed_member))
                        .selected_text(&chosen)
                        .show_ui(ui, |ui| {
                            for channel in channels {
                                ui.selectable_value(
                                    &mut self.admin_ui.access_channel,
                                    channel.id.clone(),
                                    channel_label(channel),
                                );
                            }
                        })
                        .response;
                    response.widget_info(|| {
                        egui::WidgetInfo::labeled(egui::WidgetType::ComboBox, format!("Channel: {chosen}"))
                    });
                });
            }
        });
        let (channel, member) = (
            self.admin_ui.access_channel.clone(),
            self.admin_ui.access_member.clone(),
        );
        if channel.is_empty() || member.is_empty() {
            ui.label(
                RichText::new("Choose a person and a channel to see what they can do there and why.")
                    .size(font_size(TEXT_CAPTION))
                    .color(c_muted()),
            );
            return;
        }
        let key = request_key(&channel, &member);
        self.ensure_admin_data(&key, &request_path(&channel, &member));
        if self.admin_requests.values().any(|pending| pending == &key) {
            ui.label("Asking the server…");
            return;
        }
        let Some(answer) = self
            .admin_data
            .get(&key)
            .and_then(|data| serde_json::from_value::<EffectiveChannelAccess>(data.clone()).ok())
        else {
            ui.colored_label(c_bad(), "Relay could not get an answer from the server.");
            if ui.add(secondary("Try again")).clicked() {
                self.admin_data.remove(&key);
            }
            return;
        };
        let person = bootstrap
            .members
            .iter()
            .find(|m| m.id == member)
            .map_or_else(|| "This person".to_owned(), |m| m.username.clone());
        let roles = bootstrap.roles.clone();
        let role_name = move |id: &str| roles.iter().find(|r| r.id == id).map(|r| r.name.clone());
        let text = explain(&answer, &person, &role_name);
        let kind = bootstrap
            .channels
            .iter()
            .find(|c| c.id == channel)
            .map_or("text", |c| c.kind.as_str());
        ui.add_space(6.0);
        ui.add(egui::Label::new(RichText::new(&text.headline).strong()).wrap(true));
        ui.add_space(4.0);
        // Words carry the result; colour only repeats it.
        egui::Grid::new(("access-lines", &key))
            .num_columns(3)
            .spacing([16.0, 6.0])
            .show(ui, |ui| {
                for line in text.lines.iter().filter(|l| relevant(kind, l.bit)) {
                    ui.label(line.label);
                    let (word, color) = if line.allowed {
                        ("Allowed", c_good())
                    } else {
                        ("Not allowed", c_bad())
                    };
                    ui.label(RichText::new(word).color(color).strong());
                    ui.add(egui::Label::new(RichText::new(&line.why).color(c_muted())).wrap(true));
                    ui.end_row();
                }
            });
        ui.add_space(4.0);
        let caption = |text: &str| {
            egui::Label::new(RichText::new(text).size(font_size(TEXT_CAPTION)).color(c_muted())).wrap(true)
        };
        if let Some(note) = text.note {
            ui.add(caption(note));
        }
        ui.add(caption(LIMITS_NOTE));
        if ui.add(secondary("Check again")).clicked() {
            self.admin_data.remove(&key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use relay_protocol::channel_access::{resolve, Facts};

    fn answer(facts: Facts, parent: Option<&str>) -> EffectiveChannelAccess {
        EffectiveChannelAccess {
            channel_id: "c".into(),
            user_id: "u".into(),
            rules_from_channel_id: parent.map(Into::into),
            public: facts.public,
            base_role: facts.base_role.clone(),
            community_bits: facts.community_bits,
            access: resolve(&facts),
        }
    }

    fn member() -> Facts {
        Facts {
            active: true,
            base_role: "member".into(),
            community_bits: 15,
            public: true,
            ..Facts::default()
        }
    }

    fn names(id: &str) -> Option<String> {
        match id {
            "raid" => Some("Raid".into()),
            "casters" => Some("Casters".into()),
            _ => None,
        }
    }

    #[test]
    fn every_rule_gets_its_own_reason_and_names_roles() {
        let mut facts = member();
        facts.role_rules = vec![("raid".into(), 5), ("casters".into(), 9), ("gone".into(), 1)];
        let text = explain(&answer(facts, None), "Mia", &names);
        assert!(text.headline.contains("Casters, a removed role and Raid"), "{}", text.headline);
        assert_eq!(
            text.lines.iter().map(|l| l.allowed).collect::<Vec<_>>(),
            [true, false, true, true]
        );
        let mut private = member();
        private.public = false;
        let text = explain(&answer(private, None), "Mia", &names);
        assert!(text.headline.contains("Private channel"));
        assert!(text.lines.iter().all(|l| !l.allowed && l.why.contains("private")));
        let mut admin = member();
        admin.community_bits |= PERMISSION_ADMIN;
        let text = explain(&answer(admin, None), "Mia", &names);
        assert!(text.headline.contains("Administrator"));
        assert!(text.lines.iter().all(|l| l.allowed));
    }

    #[test]
    fn a_missing_read_is_named_as_the_blocker() {
        let mut facts = member();
        facts.person_rule = Some(PERMISSION_SEND | PERMISSION_JOIN);
        let text = explain(&answer(facts, None), "Mia", &names);
        assert!(text.headline.contains("rule for Mia"));
        assert!(!text.lines[1].allowed);
        assert!(text.lines[1].why.contains("without Read"));
        assert!(text.lines[3].why.contains("does not grant"));
    }

    #[test]
    fn the_explanation_never_disagrees_with_the_decision() {
        for person_rule in [None, Some(0), Some(1), Some(3), Some(14), Some(15)] {
            for public in [false, true] {
                for community_bits in [0, 1, 15, 15 | PERMISSION_ADMIN] {
                    for active in [false, true] {
                        let facts = Facts {
                            active,
                            base_role: "member".into(),
                            community_bits,
                            public,
                            person_rule,
                            ..Facts::default()
                        };
                        let answer = answer(facts, None);
                        let text = explain(&answer, "Mia", &names);
                        for (line, (bit, _)) in text.lines.iter().zip(CHANNEL_PERMISSIONS) {
                            assert_eq!(line.allowed, answer.access.allows(bit));
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn text_channels_show_only_text_permissions_and_voice_channels_say_voice() {
        assert!(relevant("text", PERMISSION_READ) && relevant("text", PERMISSION_SEND));
        assert!(!relevant("text", PERMISSION_JOIN) && !relevant("text", PERMISSION_STREAM));
        assert!(CHANNEL_PERMISSIONS.iter().all(|(bit, _)| relevant("voice", *bit)));
        let voice = Channel { name: "Lounge".into(), kind: "voice".into(), ..Default::default() };
        let text = Channel { name: "general".into(), kind: "text".into(), ..Default::default() };
        assert_eq!(channel_label(&voice), "Lounge · voice");
        assert_eq!(channel_label(&text), "#general");
    }

    #[test]
    fn a_voice_text_chat_says_it_uses_the_voice_rules() {
        let text = explain(&answer(member(), Some("voice")), "Mia", &names);
        assert!(text.note.unwrap().contains("voice channel's rules"));
        assert!(explain(&answer(member(), None), "Mia", &names).note.is_none());
        assert!(LIMITS_NOTE.contains("Slowmode"));
    }
}
