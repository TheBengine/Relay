//! The one rule for what a member may do in a community channel.
//!
//! The server decides channel access with [`resolve`], and the "why" the app
//! shows is the [`Access`] value from that same call. There is no second copy
//! of the rule for the explanation, so the two cannot drift apart.
use serde::{Deserialize, Serialize};

use crate::{PERMISSION_ADMIN, PERMISSION_JOIN, PERMISSION_READ, PERMISSION_SEND, PERMISSION_STREAM};

/// The channel permissions a rule entry can grant.
pub const CHANNEL_BITS: u32 = PERMISSION_READ | PERMISSION_SEND | PERMISSION_JOIN | PERMISSION_STREAM;

/// Which rule decided the result. Exactly one rule applies.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Rule {
    /// The member is banned or has left. Nothing is allowed.
    Inactive,
    /// A community-wide Administrator grant bypasses channel rules.
    Administrator,
    /// A rule for this person on this channel. It is final.
    Person,
    /// Rules for custom roles the person has. Any of them can grant a permission.
    Roles { role_ids: Vec<String> },
    /// A rule for the person's built-in role (member, moderator or admin).
    BaseRole { role: String },
    /// A public channel with no rule for this person: their community-wide grants apply.
    PublicDefault,
    /// A private channel with no rule for this person: nothing is granted.
    PrivateDefault,
}

/// What the server knows when it decides. Built from current database rows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Facts {
    pub active: bool,
    /// The built-in role: "admin", "moderator" or "member".
    pub base_role: String,
    /// Community-wide grants from the built-in role and every custom role.
    pub community_bits: u32,
    pub public: bool,
    /// This channel's rule for the person, if any.
    pub person_rule: Option<u32>,
    /// This channel's rule for the person's built-in role, if any.
    pub base_role_rule: Option<u32>,
    /// This channel's rules for custom roles the person has, by role id.
    pub role_rules: Vec<(String, u32)>,
}

/// The decision and its reason.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Access {
    pub rule: Rule,
    /// The channel bits the deciding rule grants, before the read requirement.
    pub granted: u32,
}

impl Access {
    /// The same test the server applies to every channel request.
    pub fn allows(&self, permission: u32) -> bool {
        match self.rule {
            Rule::Inactive => false,
            Rule::Administrator => true,
            _ => self.granted & PERMISSION_READ != 0 && self.granted & permission == permission,
        }
    }

    /// The channel permissions that are actually usable.
    pub fn effective(&self) -> u32 {
        [PERMISSION_READ, PERMISSION_SEND, PERMISSION_JOIN, PERMISSION_STREAM]
            .into_iter()
            .filter(|bit| self.allows(*bit))
            .fold(0, |bits, bit| bits | bit)
    }

    /// True when the rule grants a permission that cannot be used because the
    /// same rule does not grant Read.
    pub fn blocked_by_missing_read(&self, permission: u32) -> bool {
        !matches!(self.rule, Rule::Inactive | Rule::Administrator)
            && self.granted & PERMISSION_READ == 0
            && self.granted & permission == permission
            && permission != PERMISSION_READ
    }
}

/// Order matters and matches the server's historical behaviour: an inactive
/// member gets nothing; Administrator bypasses channel rules; a rule for the
/// person is final; otherwise custom-role rules replace the default and are
/// combined; otherwise the built-in role's rule, the public default or nothing.
pub fn resolve(facts: &Facts) -> Access {
    if !facts.active {
        return Access { rule: Rule::Inactive, granted: 0 };
    }
    if facts.community_bits & PERMISSION_ADMIN != 0 {
        return Access { rule: Rule::Administrator, granted: CHANNEL_BITS };
    }
    if let Some(bits) = facts.person_rule {
        return Access { rule: Rule::Person, granted: bits & CHANNEL_BITS };
    }
    if !facts.role_rules.is_empty() {
        let mut role_ids: Vec<String> = facts.role_rules.iter().map(|(id, _)| id.clone()).collect();
        role_ids.sort();
        role_ids.dedup();
        let granted = facts.role_rules.iter().fold(0, |bits, (_, rule)| bits | rule) & CHANNEL_BITS;
        return Access { rule: Rule::Roles { role_ids }, granted };
    }
    if let Some(bits) = facts.base_role_rule {
        return Access {
            rule: Rule::BaseRole { role: facts.base_role.clone() },
            granted: bits & CHANNEL_BITS,
        };
    }
    if facts.public {
        Access { rule: Rule::PublicDefault, granted: facts.community_bits & CHANNEL_BITS }
    } else {
        Access { rule: Rule::PrivateDefault, granted: 0 }
    }
}

/// The answer to "what can this person do in this channel, and why".
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectiveChannelAccess {
    pub channel_id: String,
    pub user_id: String,
    /// Set when the channel is a voice channel's text chat, which uses the
    /// voice channel's rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rules_from_channel_id: Option<String>,
    pub public: bool,
    pub base_role: String,
    pub community_bits: u32,
    pub access: Access,
}

#[cfg(test)]
mod tests {
    use super::*;

    const MEMBER_BITS: u32 = 15;

    fn member() -> Facts {
        Facts {
            active: true,
            base_role: "member".into(),
            community_bits: MEMBER_BITS,
            public: true,
            ..Facts::default()
        }
    }

    #[test]
    fn a_public_channel_without_rules_uses_community_grants() {
        let access = resolve(&member());
        assert_eq!(access.rule, Rule::PublicDefault);
        assert_eq!(access.effective(), MEMBER_BITS);
        let mut read_only = member();
        read_only.community_bits = PERMISSION_READ;
        assert_eq!(resolve(&read_only).effective(), PERMISSION_READ);
    }

    #[test]
    fn a_private_channel_without_rules_grants_nothing() {
        let mut facts = member();
        facts.public = false;
        let access = resolve(&facts);
        assert_eq!(access.rule, Rule::PrivateDefault);
        assert_eq!(access.effective(), 0);
    }

    #[test]
    fn administrator_bypasses_even_a_denying_person_rule() {
        let mut facts = member();
        facts.community_bits |= PERMISSION_ADMIN;
        facts.public = false;
        facts.person_rule = Some(0);
        let access = resolve(&facts);
        assert_eq!(access.rule, Rule::Administrator);
        assert_eq!(access.effective(), CHANNEL_BITS);
    }

    #[test]
    fn an_inactive_member_gets_nothing_even_as_administrator() {
        let mut facts = member();
        facts.active = false;
        facts.community_bits = PERMISSION_ADMIN | MEMBER_BITS;
        let access = resolve(&facts);
        assert_eq!(access.rule, Rule::Inactive);
        assert_eq!(access.effective(), 0);
    }

    #[test]
    fn a_person_rule_is_final_over_roles_and_defaults() {
        let mut facts = member();
        facts.person_rule = Some(PERMISSION_READ);
        facts.role_rules = vec![("raid".into(), MEMBER_BITS)];
        facts.base_role_rule = Some(MEMBER_BITS);
        let access = resolve(&facts);
        assert_eq!(access.rule, Rule::Person);
        assert_eq!(access.effective(), PERMISSION_READ);
    }

    #[test]
    fn custom_role_rules_combine_and_replace_the_base_role_rule() {
        let mut facts = member();
        facts.base_role_rule = Some(0);
        facts.role_rules = vec![
            ("raid".into(), PERMISSION_READ | PERMISSION_JOIN),
            ("casters".into(), PERMISSION_READ | PERMISSION_STREAM),
            ("raid".into(), PERMISSION_READ),
        ];
        let access = resolve(&facts);
        assert_eq!(
            access.rule,
            Rule::Roles { role_ids: vec!["casters".into(), "raid".into()] }
        );
        assert_eq!(access.effective(), PERMISSION_READ | PERMISSION_JOIN | PERMISSION_STREAM);
        assert!(!access.allows(PERMISSION_SEND));
    }

    #[test]
    fn the_base_role_rule_replaces_the_public_default() {
        let mut facts = member();
        facts.base_role_rule = Some(PERMISSION_READ);
        let access = resolve(&facts);
        assert_eq!(access.rule, Rule::BaseRole { role: "member".into() });
        assert_eq!(access.effective(), PERMISSION_READ);
    }

    #[test]
    fn without_read_nothing_else_is_usable_and_that_is_reported() {
        let mut facts = member();
        facts.person_rule = Some(PERMISSION_SEND | PERMISSION_JOIN);
        let access = resolve(&facts);
        assert_eq!(access.effective(), 0);
        assert!(access.blocked_by_missing_read(PERMISSION_SEND));
        assert!(access.blocked_by_missing_read(PERMISSION_JOIN));
        assert!(!access.blocked_by_missing_read(PERMISSION_STREAM));
        assert!(!access.blocked_by_missing_read(PERMISSION_READ));
    }

    #[test]
    fn rule_bits_outside_channel_permissions_are_ignored() {
        let mut facts = member();
        facts.person_rule = Some(PERMISSION_READ | PERMISSION_ADMIN);
        let access = resolve(&facts);
        assert_eq!(access.granted, PERMISSION_READ);
        assert_eq!(access.rule, Rule::Person);
    }

    #[test]
    fn the_answer_round_trips_and_refuses_unknown_fields() {
        let answer = EffectiveChannelAccess {
            channel_id: "c".into(),
            user_id: "u".into(),
            rules_from_channel_id: None,
            public: true,
            base_role: "member".into(),
            community_bits: MEMBER_BITS,
            access: resolve(&member()),
        };
        let text = serde_json::to_string(&answer).unwrap();
        assert_eq!(serde_json::from_str::<EffectiveChannelAccess>(&text).unwrap(), answer);
        let extra = text.replacen('{', "{\"extra\":1,", 1);
        assert!(serde_json::from_str::<EffectiveChannelAccess>(&extra).is_err());
    }
}
