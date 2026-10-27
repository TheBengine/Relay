//! Squads: named whisper groups on top of the existing private whisper (F8).
//!
//! A squad is a saved list of people in one community, kept on this PC only.
//! Choosing a squad points the existing whisper at the squad members who are
//! in voice right now; holding F8 then talks only to them, exactly like
//! whispering to one person. Nothing new is sent to the server: the server
//! still checks each recipient, and members who are not in voice are named as
//! not reachable instead of being silently dropped.
use super::*;
use std::collections::{BTreeMap, BTreeSet};

pub(super) const MAX_SQUADS: usize = 8;
pub(super) const MAX_MEMBERS: usize = 16;
pub(super) const MAX_NAME: usize = 32;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Squad {
    pub name: String,
    /// User ids in this community.
    pub members: BTreeSet<String>,
}

/// Squads per community scope (the same scope that keeps per-person volume).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Squads {
    #[serde(default)]
    scopes: BTreeMap<String, Vec<Squad>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum SquadError {
    EmptyName,
    NameTooLong,
    NameTaken,
    TooManySquads,
    TooManyMembers,
    Missing,
}

impl SquadError {
    pub(super) fn text(&self) -> &'static str {
        match self {
            Self::EmptyName => "Give the squad a name.",
            Self::NameTooLong => "Use a name of 32 characters or fewer.",
            Self::NameTaken => "A squad with that name already exists here.",
            Self::TooManySquads => "This community already has 8 squads. Delete one first.",
            Self::TooManyMembers => "A squad can have up to 16 people.",
            Self::Missing => "That squad no longer exists.",
        }
    }
}

fn clean_name(name: &str) -> Result<String, SquadError> {
    let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
    if name.is_empty() {
        Err(SquadError::EmptyName)
    } else if name.chars().count() > MAX_NAME {
        Err(SquadError::NameTooLong)
    } else {
        Ok(name)
    }
}

impl Squads {
    pub(super) fn list(&self, scope: &str) -> &[Squad] {
        self.scopes.get(scope).map_or(&[], Vec::as_slice)
    }

    fn find_mut(&mut self, scope: &str, name: &str) -> Result<&mut Squad, SquadError> {
        self.scopes
            .get_mut(scope)
            .and_then(|list| list.iter_mut().find(|s| s.name == name))
            .ok_or(SquadError::Missing)
    }

    fn taken(&self, scope: &str, name: &str, except: Option<&str>) -> bool {
        self.list(scope)
            .iter()
            .any(|s| Some(s.name.as_str()) != except && s.name.to_lowercase() == name.to_lowercase())
    }

    pub(super) fn create(&mut self, scope: &str, name: &str) -> Result<String, SquadError> {
        let name = clean_name(name)?;
        if self.taken(scope, &name, None) {
            return Err(SquadError::NameTaken);
        }
        let list = self.scopes.entry(scope.to_owned()).or_default();
        if list.len() >= MAX_SQUADS {
            return Err(SquadError::TooManySquads);
        }
        list.push(Squad { name: name.clone(), members: BTreeSet::new() });
        Ok(name)
    }

    pub(super) fn rename(&mut self, scope: &str, from: &str, to: &str) -> Result<String, SquadError> {
        let to = clean_name(to)?;
        if self.taken(scope, &to, Some(from)) {
            return Err(SquadError::NameTaken);
        }
        self.find_mut(scope, from)?.name = to.clone();
        Ok(to)
    }

    pub(super) fn delete(&mut self, scope: &str, name: &str) -> Result<(), SquadError> {
        let list = self.scopes.get_mut(scope).ok_or(SquadError::Missing)?;
        let before = list.len();
        list.retain(|s| s.name != name);
        if list.len() == before {
            return Err(SquadError::Missing);
        }
        if list.is_empty() {
            self.scopes.remove(scope);
        }
        Ok(())
    }

    pub(super) fn add(&mut self, scope: &str, name: &str, user: &str) -> Result<(), SquadError> {
        let squad = self.find_mut(scope, name)?;
        if !squad.members.contains(user) && squad.members.len() >= MAX_MEMBERS {
            return Err(SquadError::TooManyMembers);
        }
        squad.members.insert(user.to_owned());
        Ok(())
    }

    pub(super) fn remove(&mut self, scope: &str, name: &str, user: &str) -> Result<(), SquadError> {
        self.find_mut(scope, name)?.members.remove(user);
        Ok(())
    }

    /// The squad after `current` in this scope, wrapping around.
    pub(super) fn next_after(&self, scope: &str, current: Option<&str>) -> Option<String> {
        let list = self.list(scope);
        let index = current
            .and_then(|name| list.iter().position(|s| s.name == name))
            .map_or(0, |i| (i + 1) % list.len().max(1));
        list.get(index).map(|s| s.name.clone())
    }
}

/// Who a whisper to this squad reaches right now.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Reach {
    /// Members in voice in this community, other than you.
    pub reachable: Vec<String>,
    /// Members not in voice: a whisper does not reach them.
    pub absent: Vec<String>,
}

pub(super) fn reach(squad: &Squad, in_voice: &BTreeSet<String>, me: &str) -> Reach {
    let mut result = Reach::default();
    for member in squad.members.iter().filter(|m| m.as_str() != me) {
        if in_voice.contains(member) {
            result.reachable.push(member.clone());
        } else {
            result.absent.push(member.clone());
        }
    }
    result
}

/// What the squad bar says. Counts only what the voice list shows.
pub(super) fn reach_text(name: &str, reach: &Reach) -> String {
    let total = reach.reachable.len() + reach.absent.len();
    match (reach.reachable.len(), total) {
        (_, 0) => format!("{name}: no members yet"),
        (0, _) => format!("{name}: nobody is in voice"),
        (1, 1) => format!("{name}: 1 in voice"),
        (n, t) if n == t => format!("{name}: all {t} in voice"),
        (n, t) => format!("{name}: {n} of {t} in voice"),
    }
}

#[derive(Default)]
pub(super) struct State {
    /// The squad the whisper currently points at, in this session only.
    pub active: Option<String>,
    pub window_open: bool,
    pub new_name: String,
    pub renaming: Option<(String, String)>,
    pub error: Option<&'static str>,
}

impl RelayApp {
    fn squad_scope(&self) -> String {
        self.conversation_scope()
            .unwrap_or_else(|| self.login.url.clone())
    }

    fn voice_user_ids(&self) -> BTreeSet<String> {
        self.bootstrap
            .as_ref()
            .map(|b| b.voice_members.iter().map(|m| m.user.id.clone()).collect())
            .unwrap_or_default()
    }

    fn my_user_id(&self) -> String {
        self.session.as_ref().map(|s| s.user.id.clone()).unwrap_or_default()
    }

    fn member_name(&self, id: &str) -> String {
        self.bootstrap
            .as_ref()
            .and_then(|b| b.members.iter().find(|m| m.id == id))
            .map_or_else(|| "Former member".to_owned(), |m| profile_ui::display_name(m).to_owned())
    }

    /// Point the existing whisper at the squad's members who are in voice.
    pub(super) fn whisper_to_squad(&mut self, name: &str) {
        let scope = self.squad_scope();
        let Some(squad) = self.prefs.squads.list(&scope).iter().find(|s| s.name == name).cloned() else {
            self.squads.error = Some(SquadError::Missing.text());
            return;
        };
        let whisper_on = self
            .media
            .as_ref()
            .is_some_and(|media| media.active_settings().whisper_enabled);
        if self.voice_channel.is_none() || !whisper_on {
            self.notice = Some((
                if self.voice_channel.is_none() {
                    "Join voice to whisper to a squad"
                } else {
                    "Turn on Private whisper in Voice settings to use squads"
                }
                .into(),
                Instant::now(),
            ));
            return;
        }
        let reach = reach(&squad, &self.voice_user_ids(), &self.my_user_id());
        if reach.reachable.is_empty() {
            self.notice = Some((reach_text(&squad.name, &reach), Instant::now()));
            return;
        }
        if let Some(media) = &self.media {
            media.select_whisper_group(reach.reachable.clone());
        }
        self.squads.active = Some(squad.name.clone());
        self.notice = Some((
            format!("Hold F8 to whisper. {}", reach_text(&squad.name, &reach)),
            Instant::now(),
        ));
    }

    pub(super) fn whisper_next_squad(&mut self) {
        let scope = self.squad_scope();
        match self.prefs.squads.next_after(&scope, self.squads.active.as_deref()) {
            Some(name) => self.whisper_to_squad(&name),
            None => {
                self.notice = Some(("Create a squad first: open Squads in the voice room".into(), Instant::now()));
            }
        }
    }

    /// The overlay and voice room line for the active squad, if any.
    pub(super) fn active_squad_line(&self) -> Option<String> {
        let name = self.squads.active.as_ref()?;
        let squad = self
            .prefs
            .squads
            .list(&self.squad_scope())
            .iter()
            .find(|s| &s.name == name)?;
        // Only while the whisper still points at this squad's reachable members.
        let targets: BTreeSet<String> = self
            .media
            .as_ref()
            .map(|media| media.whisper_targets().into_iter().collect())
            .unwrap_or_default();
        let reach = reach(squad, &self.voice_user_ids(), &self.my_user_id());
        let same = !targets.is_empty() && targets.iter().all(|t| squad.members.contains(t));
        same.then(|| format!("Squad {} · F8", reach_text(&squad.name, &reach)))
    }

    /// Context-menu entries for one person: add to or remove from each squad.
    pub(super) fn squad_menu(&mut self, ui: &mut egui::Ui, user_id: &str) {
        let scope = self.squad_scope();
        let squads = self.prefs.squads.list(&scope).to_vec();
        // Flat entries: a nested menu opens only on hover, which keyboard and
        // screen-reader users cannot reach.
        ui.separator();
        {
            for squad in &squads {
                let inside = squad.members.contains(user_id);
                let label = if inside {
                    format!("Remove from {}", squad.name)
                } else {
                    format!("Add to {}", squad.name)
                };
                if ui.button(label).clicked() {
                    let result = if inside {
                        self.prefs.squads.remove(&scope, &squad.name, user_id)
                    } else {
                        self.prefs.squads.add(&scope, &squad.name, user_id)
                    };
                    match result {
                        Ok(()) => self.defer_preferences(),
                        Err(error) => self.notice = Some((error.text().into(), Instant::now())),
                    }
                    ui.close_menu();
                }
            }
            if ui
                .button(if squads.is_empty() { "Create a squad…" } else { "Manage squads…" })
                .clicked()
            {
                self.squads.window_open = true;
                ui.close_menu();
            }
        }
    }

    /// The voice room's squad row: one button per squad, plus Manage.
    pub(super) fn squad_bar(&mut self, ui: &mut egui::Ui) {
        let scope = self.squad_scope();
        let squads = self.prefs.squads.list(&scope).to_vec();
        let in_voice = self.voice_user_ids();
        let me = self.my_user_id();
        let mut chosen = None;
        ui.horizontal_wrapped(|ui| {
            ui.label(
                RichText::new("SQUADS")
                    .size(font_size(crate::appearance::TEXT_CAPTION))
                    .color(c_muted())
                    .strong(),
            );
            for squad in &squads {
                let text = reach_text(&squad.name, &reach(squad, &in_voice, &me));
                let active = self.squads.active.as_deref() == Some(squad.name.as_str());
                if ui
                    .selectable_label(active, text)
                    .on_hover_text("Point the whisper at this squad, then hold F8. Ctrl + Shift + S picks the next squad.")
                    .clicked()
                {
                    chosen = Some(squad.name.clone());
                }
            }
            if ui.small_button(if squads.is_empty() { "Create a squad" } else { "Manage" }).clicked() {
                self.squads.window_open = true;
            }
        });
        if let Some(name) = chosen {
            self.whisper_to_squad(&name);
        }
    }

    /// Drawn with the app's other dialogs, so it can never stay open unseen.
    pub(super) fn squads_window(&mut self, ctx: &egui::Context) {
        if !self.squads.window_open {
            return;
        }
        let scope = self.squad_scope();
        // Esc closes it, like the app's other dialogs.
        let mut open = !ctx.input(|i| i.key_pressed(egui::Key::Escape));
        let mut changed = false;
        desktop_ui::dialog(ctx, "Squads")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(380.0)
            .show(ctx, |ui| {
                ui.add(
                    egui::Label::new(
                        RichText::new("Saved on this PC for this community. Add people from their voice menu. A whisper reaches only squad members who are in voice.")
                            .size(font_size(crate::appearance::TEXT_CAPTION))
                            .color(c_muted()),
                    )
                    .wrap(true),
                );
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.squads.new_name)
                            .hint_text("New squad name")
                            .char_limit(MAX_NAME),
                    );
                    if ui.add(secondary("Create")).clicked() {
                        match self.prefs.squads.create(&scope, &self.squads.new_name) {
                            Ok(_) => {
                                self.squads.new_name.clear();
                                self.squads.error = None;
                                changed = true;
                            }
                            Err(error) => self.squads.error = Some(error.text()),
                        }
                    }
                });
                if let Some(error) = self.squads.error {
                    ui.colored_label(c_bad(), error);
                }
                for squad in self.prefs.squads.list(&scope).to_vec() {
                    ui.add_space(6.0);
                    ui.separator();
                    ui.add_space(2.0);
                    ui.horizontal(|ui| {
                        if let Some((from, draft)) = &mut self.squads.renaming {
                            if *from == squad.name {
                                ui.add(egui::TextEdit::singleline(draft).char_limit(MAX_NAME));
                                if ui.small_button("Save").clicked() {
                                    let (from, draft) = (from.clone(), draft.clone());
                                    match self.prefs.squads.rename(&scope, &from, &draft) {
                                        Ok(name) => {
                                            if self.squads.active.as_deref() == Some(from.as_str()) {
                                                self.squads.active = Some(name);
                                            }
                                            self.squads.renaming = None;
                                            self.squads.error = None;
                                            changed = true;
                                        }
                                        Err(error) => self.squads.error = Some(error.text()),
                                    }
                                }
                                return;
                            }
                        }
                        ui.label(RichText::new(&squad.name).strong());
                        if ui.small_button("Rename").clicked() {
                            self.squads.renaming = Some((squad.name.clone(), squad.name.clone()));
                        }
                        if ui.small_button("Delete").clicked()
                            && self.prefs.squads.delete(&scope, &squad.name).is_ok()
                        {
                            if self.squads.active.as_deref() == Some(squad.name.as_str()) {
                                self.squads.active = None;
                            }
                            changed = true;
                        }
                    });
                    if squad.members.is_empty() {
                        ui.label(RichText::new("No members yet.").color(c_muted()));
                    }
                    for member in &squad.members {
                        ui.horizontal(|ui| {
                            ui.add_space(12.0);
                            ui.label(self.member_name(member));
                            if ui.small_button("Remove").clicked()
                                && self.prefs.squads.remove(&scope, &squad.name, member).is_ok()
                            {
                                changed = true;
                            }
                        });
                    }
                }
            });
        self.squads.window_open = open;
        if changed {
            self.defer_preferences();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HERE: &str = "https://a.example";

    fn ids(list: &[&str]) -> BTreeSet<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn names_are_cleaned_unique_per_community_and_bounded() {
        let mut squads = Squads::default();
        assert_eq!(squads.create(HERE, "  Alpha   team "), Ok("Alpha team".into()));
        assert_eq!(squads.create(HERE, "alpha TEAM"), Err(SquadError::NameTaken));
        assert_eq!(squads.create("https://b.example", "Alpha team"), Ok("Alpha team".into()));
        assert_eq!(squads.create(HERE, "   "), Err(SquadError::EmptyName));
        assert_eq!(squads.create(HERE, &"x".repeat(33)), Err(SquadError::NameTooLong));
        for i in 1..MAX_SQUADS {
            squads.create(HERE, &format!("S{i}")).unwrap();
        }
        assert_eq!(squads.create(HERE, "One more"), Err(SquadError::TooManySquads));
        assert_eq!(squads.rename(HERE, "S1", "s2"), Err(SquadError::NameTaken));
        assert_eq!(squads.rename(HERE, "S1", "S1 renamed"), Ok("S1 renamed".into()));
        assert_eq!(squads.rename(HERE, "S1", "x"), Err(SquadError::Missing));
        squads.delete(HERE, "S1 renamed").unwrap();
        assert_eq!(squads.delete(HERE, "S1 renamed"), Err(SquadError::Missing));
    }

    #[test]
    fn members_are_bounded_and_adding_twice_is_harmless() {
        let mut squads = Squads::default();
        squads.create(HERE, "Raid").unwrap();
        for i in 0..MAX_MEMBERS {
            squads.add(HERE, "Raid", &format!("u{i}")).unwrap();
        }
        assert_eq!(squads.add(HERE, "Raid", "u0"), Ok(()));
        assert_eq!(squads.add(HERE, "Raid", "extra"), Err(SquadError::TooManyMembers));
        squads.remove(HERE, "Raid", "u0").unwrap();
        assert_eq!(squads.add(HERE, "Raid", "extra"), Ok(()));
        assert_eq!(squads.add(HERE, "Nope", "u1"), Err(SquadError::Missing));
    }

    #[test]
    fn a_whisper_reaches_only_members_in_voice_and_never_yourself() {
        let squad = Squad { name: "Raid".into(), members: ids(&["me", "mia", "noor", "kai"]) };
        let result = reach(&squad, &ids(&["me", "mia", "kai", "stranger"]), "me");
        assert_eq!(result.reachable, vec!["kai".to_owned(), "mia".to_owned()]);
        assert_eq!(result.absent, vec!["noor".to_owned()]);
        assert_eq!(reach_text("Raid", &result), "Raid: 2 of 3 in voice");
        let all = reach(&squad, &ids(&["mia", "noor", "kai"]), "me");
        assert_eq!(reach_text("Raid", &all), "Raid: all 3 in voice");
        let none = reach(&squad, &ids(&[]), "me");
        assert_eq!(reach_text("Raid", &none), "Raid: nobody is in voice");
        let one = reach(&Squad { name: "Duo".into(), members: ids(&["me", "mia"]) }, &ids(&["mia"]), "me");
        assert_eq!(reach_text("Duo", &one), "Duo: 1 in voice");
        let empty = reach(&Squad { name: "New".into(), members: ids(&["me"]) }, &ids(&["me"]), "me");
        assert_eq!(reach_text("New", &empty), "New: no members yet");
    }

    #[test]
    fn next_squad_cycles_in_order_and_starts_at_the_first() {
        let mut squads = Squads::default();
        assert_eq!(squads.next_after(HERE, None), None);
        for name in ["A", "B", "C"] {
            squads.create(HERE, name).unwrap();
        }
        assert_eq!(squads.next_after(HERE, None).as_deref(), Some("A"));
        assert_eq!(squads.next_after(HERE, Some("A")).as_deref(), Some("B"));
        assert_eq!(squads.next_after(HERE, Some("C")).as_deref(), Some("A"));
        assert_eq!(squads.next_after(HERE, Some("gone")).as_deref(), Some("A"));
    }

    #[test]
    fn squads_round_trip_through_preferences_and_old_files_load_empty() {
        let mut squads = Squads::default();
        squads.create(HERE, "Raid").unwrap();
        squads.add(HERE, "Raid", "mia").unwrap();
        let text = serde_json::to_string(&squads).unwrap();
        assert_eq!(serde_json::from_str::<Squads>(&text).unwrap(), squads);
        assert_eq!(serde_json::from_str::<Squads>("{}").unwrap(), Squads::default());
    }
}
