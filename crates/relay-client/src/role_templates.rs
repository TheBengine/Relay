//! Role presets and a plain preview for the existing Roles editor.
//!
//! This adds no roles system. A preset only edits the permission bits of the
//! unsaved draft the editor already holds; the normal Save Changes button still
//! sends the existing `PUT /api/roles/{id}` request, and the server still decides
//! what the person may grant. The preview shows which draft permissions would
//! change and which the actor is not allowed to grant (the same per-permission
//! test the checkboxes use). It is a summary of role grants, not a decision about
//! any member's access to any channel: channel rules are not loaded here.
use super::*;

/// Every role permission, in the order the editor lists them. The Roles editor
/// draws its checkboxes from this table so the labels exist only once.
pub(super) const PERMISSION_LABELS: [(u32, &str); 10] = [
    (PERMISSION_READ, "Read channels"),
    (PERMISSION_SEND, "Send messages"),
    (PERMISSION_JOIN, "Connect to voice"),
    (PERMISSION_STREAM, "Share camera and screen"),
    (PERMISSION_CHANNELS, "Manage channels and permissions"),
    (PERMISSION_MESSAGES, "Manage messages and pins"),
    (
        PERMISSION_MEMBERS,
        "Moderate lower members / view audit log",
    ),
    (PERMISSION_INVITES, "Manage invites"),
    (PERMISSION_ROLES, "Manage lower roles"),
    (
        PERMISSION_ADMIN,
        "Administrator (bypasses channel restrictions)",
    ),
];

pub(super) const OVERRIDE_NOTE: &str = "A role grants community-wide permissions. What a person can actually do in a channel can differ: private channels, per-channel rules for roles or people, and administrator access are decided by the server. This editor does not load those channel rules, so this preview is not a complete answer for any member or channel.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Template {
    ReadOnly,
    Member,
    Moderator,
}

impl Template {
    /// There is deliberately no Administrator preset.
    pub(super) const ALL: [Self; 3] = [Self::ReadOnly, Self::Member, Self::Moderator];

    pub(super) fn name(self) -> &'static str {
        match self {
            Self::ReadOnly => "Read only",
            Self::Member => "Member",
            Self::Moderator => "Moderator",
        }
    }

    pub(super) fn bits(self) -> u32 {
        match self {
            Self::ReadOnly => PERMISSION_READ,
            Self::Member => PERMISSION_READ | PERMISSION_SEND | PERMISSION_JOIN | PERMISSION_STREAM,
            // The same set the built-in Moderator has.
            Self::Moderator => {
                Self::Member.bits() | PERMISSION_MESSAGES | PERMISSION_MEMBERS | PERMISSION_INVITES
            }
        }
    }

    fn summary(self) -> &'static str {
        match self {
            Self::ReadOnly => "Can see channels and read. Cannot send messages, join voice or share.",
            Self::Member => "Everyday use: read, send messages, join voice, share camera and screen.",
            Self::Moderator => {
                "Member powers plus managing messages and pins, moderating lower members and managing invites. Same as the built-in Moderator."
            }
        }
    }
}

pub(super) fn names(bits: u32) -> String {
    let list: Vec<&str> = PERMISSION_LABELS
        .iter()
        .filter(|(bit, _)| bits & bit != 0)
        .map(|(_, label)| *label)
        .collect();
    list.join(", ")
}

/// The permissions the signed-in person may put on a role: exactly those for
/// which the editor already enables a checkbox (`Bootstrap::has_permission`).
pub(super) fn grantable(has_permission: impl Fn(u32) -> bool) -> u32 {
    PERMISSION_LABELS
        .iter()
        .filter(|(bit, _)| has_permission(*bit))
        .fold(0, |all, (bit, _)| all | bit)
}

/// Why the presets cannot be used on this role right now, if they cannot.
pub(super) fn lock_reason(pending: bool, editable: bool) -> Option<&'static str> {
    if pending {
        Some("Waiting for the server to finish another role change.")
    } else if !editable {
        Some("You can change only roles ranked below yours that grant nothing you lack.")
    } else {
        None
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Preview {
    pub turn_on: u32,
    pub turn_off: u32,
    /// Part of `turn_on` that the person may not grant.
    pub blocked: u32,
}

impl Preview {
    pub(super) fn changes(&self) -> bool {
        self.turn_on | self.turn_off != 0
    }
    pub(super) fn available(&self) -> bool {
        self.blocked == 0
    }
}

pub(super) fn preview(template: Template, draft_bits: u32, grantable: u32) -> Preview {
    let target = template.bits();
    let turn_on = target & !draft_bits;
    Preview {
        turn_on,
        turn_off: draft_bits & !target,
        blocked: turn_on & !grantable,
    }
}

/// Replace only the draft's permission bits. Name, colour, position and id are
/// not touched, and nothing is sent. Returns the blocked bits if the person may
/// not grant the preset, leaving the draft as it was.
pub(super) fn apply(
    draft: &mut CommunityRole,
    template: Template,
    grantable: u32,
) -> Result<(), u32> {
    let plan = preview(template, draft.permissions, grantable);
    if !plan.available() {
        return Err(plan.blocked);
    }
    draft.permissions = template.bits();
    Ok(())
}

fn memory_id(role_id: &str) -> egui::Id {
    egui::Id::new(("role-template-choice", role_id))
}

/// The preset row, preview and Apply/Cancel for one role's unsaved draft.
/// `assigned` is how many people currently hold the role, as last loaded.
pub(super) fn section(
    ui: &mut egui::Ui,
    role_id: &str,
    assigned: usize,
    draft: &mut CommunityRole,
    grantable: u32,
    locked: Option<&str>,
) {
    let id = memory_id(role_id);
    let mut chosen: Option<Template> = ui.data_mut(|data| data.get_temp(id));
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new("Start from a preset").color(c_muted()));
        for template in Template::ALL {
            if ui
                .selectable_label(chosen == Some(template), template.name())
                .clicked()
            {
                chosen = (chosen != Some(template)).then_some(template);
            }
        }
    });
    if let Some(reason) = locked {
        ui.add(egui::Label::new(RichText::new(reason).color(c_muted())).wrap(true));
    }
    if let Some(template) = chosen {
        let plan = preview(template, draft.permissions, grantable);
        let mut apply_now = false;
        let mut cancel = false;
        egui::Frame::none()
            .fill(c_card())
            .rounding(6.0)
            .inner_margin(Margin::same(10.0))
            .show(ui, |ui| {
                ui.label(RichText::new(template.name()).strong());
                ui.add(egui::Label::new(template.summary()).wrap(true));
                if !plan.changes() {
                    ui.label(RichText::new("This role already has exactly these permissions.").color(c_muted()));
                } else {
                    if plan.turn_on != 0 {
                        ui.add(
                            egui::Label::new(format!("Turns on: {}", names(plan.turn_on)))
                                .wrap(true),
                        );
                    }
                    if plan.turn_off != 0 {
                        ui.add(
                            egui::Label::new(format!("Turns off: {}", names(plan.turn_off)))
                                .wrap(true),
                        );
                    }
                }
                if assigned > 0 {
                    ui.add(
                        egui::Label::new(
                            RichText::new(format!(
                                "{assigned} {} currently hold this role.",
                                if assigned == 1 { "person" } else { "people" }
                            ))
                            .size(font_size(TEXT_CAPTION))
                            .color(c_muted()),
                        )
                        .wrap(true),
                    );
                }
                if !plan.available() {
                    ui.add(
                        egui::Label::new(
                            RichText::new(format!(
                                "You can't apply this preset: it would grant {}, which you don't hold. The server also checks this.",
                                names(plan.blocked)
                            ))
                            .color(c_bad()),
                        )
                        .wrap(true),
                    );
                }
                ui.add(
                    egui::Label::new(
                        RichText::new("Apply changes only the unsaved draft below. Nothing is sent until you press Save Changes.")
                            .size(font_size(TEXT_CAPTION))
                            .color(c_muted()),
                    )
                    .wrap(true),
                );
                ui.horizontal_wrapped(|ui| {
                    apply_now = ui
                        .add_enabled(plan.available() && plan.changes(), primary("Apply to draft"))
                        .clicked();
                    cancel = ui.add(secondary("Cancel")).clicked();
                });
            });
        if apply_now && apply(draft, template, grantable).is_ok() {
            chosen = None;
        }
        if cancel {
            chosen = None;
        }
    }
    ui.collapsing("What role permissions mean", |ui| {
        ui.add(
            egui::Label::new(
                RichText::new(OVERRIDE_NOTE)
                    .size(font_size(TEXT_BODY))
                    .color(c_muted()),
            )
            .wrap(true),
        );
    });
    ui.data_mut(|data| match chosen {
        Some(template) => data.insert_temp(id, template),
        None => {
            data.remove::<Template>(id);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: u32 = PERMISSION_ALL;

    fn role() -> CommunityRole {
        CommunityRole {
            id: "role-1".into(),
            name: "Helpers".into(),
            color: 0x43b581,
            position: 7,
            permissions: PERMISSION_READ | PERMISSION_SEND,
        }
    }

    #[test]
    fn presets_are_small_valid_and_never_administrator() {
        assert_eq!(Template::ALL.len(), 3);
        for template in Template::ALL {
            let bits = template.bits();
            assert_eq!(bits & PERMISSION_ADMIN, 0, "{template:?}");
            assert_eq!(bits & PERMISSION_ROLES, 0, "{template:?}");
            assert_eq!(bits & PERMISSION_CHANNELS, 0, "{template:?}");
            assert_eq!(bits & !PERMISSION_ALL, 0, "{template:?}");
            assert!(bits & PERMISSION_READ != 0, "{template:?}");
            let mut candidate = role();
            candidate.permissions = bits;
            assert!(candidate.valid(), "{template:?}");
        }
        assert_eq!(Template::ReadOnly.bits(), PERMISSION_READ);
        assert_eq!(Template::Member.bits(), 15);
        assert_eq!(
            Template::Moderator.bits(),
            15 | PERMISSION_MESSAGES | PERMISSION_MEMBERS | PERMISSION_INVITES
        );
        // Strictly growing, so the names mean something.
        assert!(Template::ReadOnly.bits() & !Template::Member.bits() == 0);
        assert!(Template::Member.bits() & !Template::Moderator.bits() == 0);
    }

    #[test]
    fn the_label_table_covers_every_permission_once() {
        let mut seen = 0;
        for (bit, label) in PERMISSION_LABELS {
            assert!(bit.is_power_of_two() && !label.is_empty());
            assert_eq!(seen & bit, 0, "{label}");
            seen |= bit;
        }
        assert_eq!(seen, PERMISSION_ALL);
        assert_eq!(
            names(PERMISSION_READ | PERMISSION_SEND),
            "Read channels, Send messages"
        );
        assert_eq!(names(0), "");
    }

    #[test]
    fn preview_names_exactly_what_would_change() {
        // Read + Send now; Member adds voice and sharing and removes nothing.
        let up = preview(Template::Member, PERMISSION_READ | PERMISSION_SEND, ALL);
        assert_eq!(up.turn_on, PERMISSION_JOIN | PERMISSION_STREAM);
        assert_eq!(up.turn_off, 0);
        assert!(up.available() && up.changes());
        // Read only on the same role removes sending.
        let down = preview(Template::ReadOnly, PERMISSION_READ | PERMISSION_SEND, ALL);
        assert_eq!((down.turn_on, down.turn_off), (0, PERMISSION_SEND));
        // Extra powers outside the preset are listed as turned off, not hidden.
        let extra = preview(
            Template::Member,
            15 | PERMISSION_CHANNELS | PERMISSION_ADMIN,
            ALL,
        );
        assert_eq!(extra.turn_off, PERMISSION_CHANNELS | PERMISSION_ADMIN);
        // Same permissions: nothing to apply.
        let same = preview(Template::Member, 15, ALL);
        assert!(!same.changes() && same.available());
    }

    #[test]
    fn a_preset_cannot_grant_what_the_person_does_not_hold() {
        // A delegated role manager who holds read, send and roles only.
        let mine = PERMISSION_READ | PERMISSION_SEND | PERMISSION_ROLES;
        let plan = preview(Template::Moderator, PERMISSION_READ, mine);
        assert_eq!(
            plan.blocked,
            PERMISSION_JOIN
                | PERMISSION_STREAM
                | PERMISSION_MESSAGES
                | PERMISSION_MEMBERS
                | PERMISSION_INVITES
        );
        assert!(!plan.available());
        let mut draft = role();
        let before = draft.clone();
        assert_eq!(
            apply(&mut draft, Template::Moderator, mine),
            Err(plan.blocked)
        );
        assert_eq!(draft, before, "a refused preset leaves the draft alone");
        // Removing powers is always allowed, even ones the person lacks.
        let down = preview(Template::ReadOnly, 15 | PERMISSION_MESSAGES, mine);
        assert!(down.available() && down.turn_off != 0);
        // An administrator holds every permission, so nothing is blocked.
        assert!(preview(Template::Moderator, 0, ALL).available());
    }

    #[test]
    fn grantable_matches_the_checkbox_rule() {
        let holds = PERMISSION_READ | PERMISSION_INVITES;
        assert_eq!(grantable(|bit| holds & bit == bit), holds);
        assert_eq!(grantable(|_| false), 0);
        // The administrator test in Bootstrap::has_permission answers true for everything.
        assert_eq!(grantable(|_| true), PERMISSION_ALL);
    }

    #[test]
    fn apply_changes_only_the_permission_bits_of_the_draft() {
        let baseline = role();
        let mut draft = baseline.clone();
        draft.name = "Helpers (edited)".into();
        draft.color = 0xff0000;
        draft.position = 9;
        assert_eq!(apply(&mut draft, Template::Moderator, ALL), Ok(()));
        assert_eq!(draft.permissions, Template::Moderator.bits());
        assert_eq!(draft.name, "Helpers (edited)");
        assert_eq!(draft.color, 0xff0000);
        assert_eq!(draft.position, 9);
        assert_eq!(draft.id, "role-1");
        // The saved role is untouched, so the normal Save is still what sends it.
        assert_eq!(baseline.permissions, PERMISSION_READ | PERMISSION_SEND);
        assert!(draft != baseline && draft.valid());
    }

    #[test]
    fn choosing_and_cancelling_never_edits_the_draft() {
        // The preview is a pure function of the draft: looking at every preset
        // leaves the draft exactly as it was.
        let draft = role();
        let before = draft.clone();
        for template in Template::ALL {
            let _ = preview(template, draft.permissions, ALL);
        }
        assert_eq!(draft, before);
    }

    #[test]
    fn applying_twice_is_stable() {
        let mut draft = role();
        assert_eq!(apply(&mut draft, Template::Member, ALL), Ok(()));
        let once = draft.clone();
        assert!(!preview(Template::Member, draft.permissions, ALL).changes());
        assert_eq!(apply(&mut draft, Template::Member, ALL), Ok(()));
        assert_eq!(draft, once);
    }

    #[test]
    fn the_lock_reason_explains_why_presets_are_unavailable() {
        assert!(lock_reason(true, true).unwrap().contains("Waiting"));
        assert!(lock_reason(true, false).unwrap().contains("Waiting"));
        assert!(lock_reason(false, false)
            .unwrap()
            .contains("ranked below yours"));
        assert_eq!(lock_reason(false, true), None);
    }

    #[test]
    fn the_explanation_does_not_claim_a_channel_decision() {
        let note = OVERRIDE_NOTE;
        assert!(note.contains("community-wide"));
        assert!(note.contains("private channels"));
        assert!(note.contains("does not load those channel rules"));
        assert!(note.contains("not a complete answer"));
        for claim in [
            "can access",
            "will be able",
            "guaranteed",
            "always has access",
            "effective permissions",
        ] {
            assert!(!note.to_lowercase().contains(claim), "{claim}");
        }
    }
}
