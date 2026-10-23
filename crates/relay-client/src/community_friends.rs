//! A RAM-only view of friendships on communities this account already joined.
//! Rows retain their server/account identity; equal names never merge people.
use crate::network::CommandOrigin;
use relay_protocol::{Bootstrap, User};
use std::collections::HashMap;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Relationship {
    Friend,
    Incoming,
    Outgoing,
    Blocked,
}

#[derive(Clone)]
pub(crate) struct Row {
    pub origin: CommandOrigin,
    pub server_name: String,
    pub user: User,
    pub relationship: Relationship,
    pub online: bool,
}

#[derive(Default)]
pub(crate) struct Directory {
    servers: HashMap<String, (CommandOrigin, Vec<Row>)>,
}

impl Directory {
    pub(crate) fn remember(&mut self, url: &str, bootstrap: &Bootstrap) {
        if bootstrap.capabilities.iter().any(|c| c == "people-v1") {
            self.remove(url);
            return;
        }
        let origin = CommandOrigin {
            url: url.into(),
            server_id: bootstrap.server_id.clone(),
            user_id: bootstrap.user.id.clone(),
        };
        if origin.url.is_empty() || origin.server_id.is_empty() || origin.user_id.is_empty() {
            self.remove(url);
            return;
        }
        let rows = if bootstrap
            .capabilities
            .iter()
            .any(|capability| capability == "social-v1")
        {
            bootstrap
                .members
                .iter()
                .filter(|user| user.id != origin.user_id)
                .filter_map(|user| {
                    let social = &bootstrap.social;
                    let relationship = if social.blocked.contains(&user.id) {
                        Relationship::Blocked
                    } else if social.friends.contains(&user.id) {
                        Relationship::Friend
                    } else if social.incoming.contains(&user.id) {
                        Relationship::Incoming
                    } else if social.outgoing.contains(&user.id) {
                        Relationship::Outgoing
                    } else {
                        return None;
                    };
                    Some(Row {
                        origin: origin.clone(),
                        server_name: bootstrap.server_name.clone(),
                        user: user.clone(),
                        relationship,
                        online: bootstrap.presence.iter().any(|presence| {
                            presence.user_id == user.id
                                && matches!(presence.status.as_str(), "online" | "away" | "dnd")
                        }),
                    })
                })
                .collect()
        } else {
            Vec::new()
        };
        self.servers.insert(url.into(), (origin, rows));
    }
    pub(crate) fn retain(&mut self, owners: &[CommandOrigin]) {
        self.servers.retain(|_, (owner, _)| owners.contains(owner));
    }
    pub(crate) fn remove(&mut self, url: &str) {
        self.servers.remove(url);
    }
    pub(crate) fn clear(&mut self) {
        self.servers.clear();
    }
    pub(crate) fn rows(&self, owners: &[CommandOrigin], query: &str, tab: u8) -> Vec<Row> {
        let query = query.trim().to_lowercase();
        let mut rows: Vec<_> = self
            .servers
            .values()
            .filter(|(owner, _)| owners.contains(owner))
            .flat_map(|(_, rows)| rows)
            .filter(|row| {
                let kind = match tab {
                    0 | 1 => row.relationship == Relationship::Friend,
                    2 => matches!(
                        row.relationship,
                        Relationship::Incoming | Relationship::Outgoing
                    ),
                    3 => row.relationship == Relationship::Blocked,
                    _ => false,
                };
                kind && (query.is_empty()
                    || [
                        &row.user.username,
                        &row.user.profile.display_name,
                        &row.server_name,
                    ]
                    .iter()
                    .any(|value| value.to_lowercase().contains(&query)))
            })
            .cloned()
            .collect();
        rows.sort_by_cached_key(|row| {
            (
                row.user.username.to_lowercase(),
                row.server_name.to_lowercase(),
                row.origin.url.clone(),
                row.user.id.clone(),
            )
        });
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn snapshot(server: &str, owner: &str) -> Bootstrap {
        let mut b = Bootstrap {
            server_id: server.into(),
            server_name: server.into(),
            ..Default::default()
        };
        b.user.id = owner.into();
        b.capabilities.push("social-v1".into());
        b.members.push(User {
            id: "friend-id".into(),
            username: "same_name".into(),
            ..Default::default()
        });
        b.social.friends.push("friend-id".into());
        b
    }
    fn owner(url: &str, b: &Bootstrap) -> CommandOrigin {
        CommandOrigin {
            url: url.into(),
            server_id: b.server_id.clone(),
            user_id: b.user.id.clone(),
        }
    }
    #[test]
    fn same_names_and_ids_on_two_servers_remain_two_scoped_rows() {
        let a = snapshot("server-a", "alice");
        let b = snapshot("server-b", "bob");
        let mut d = Directory::default();
        d.remember("https://a", &a);
        d.remember("https://b", &b);
        let owners = [owner("https://a", &a), owner("https://b", &b)];
        let rows = d.rows(&owners, "same_name", 1);
        assert_eq!(rows.len(), 2);
        assert_ne!(rows[0].origin, rows[1].origin);
        assert_eq!(d.rows(&owners, "server-b", 1).len(), 1);
    }
    #[test]
    fn changed_account_or_server_and_removed_members_cannot_show_old_friends() {
        let a = snapshot("server-a", "alice");
        let mut d = Directory::default();
        d.remember("https://a", &a);
        let replacement = snapshot("server-a", "other");
        assert!(d
            .rows(&[owner("https://a", &replacement)], "", 1)
            .is_empty());
        d.retain(&[owner("https://a", &replacement)]);
        assert!(d.rows(&[owner("https://a", &a)], "", 1).is_empty());
        let mut changed = a.clone();
        changed.members.clear();
        d.remember("https://a", &changed);
        assert!(d.rows(&[owner("https://a", &a)], "", 1).is_empty());
        d.remember("https://a", &a);
        let changed = snapshot("different-server", "alice");
        assert!(d.rows(&[owner("https://a", &changed)], "", 1).is_empty());
    }
    #[test]
    fn block_overrides_friendship_and_pending_requests_stay_distinct() {
        let mut a = snapshot("server-a", "alice");
        a.social.blocked.push("friend-id".into());
        let mut d = Directory::default();
        d.remember("https://a", &a);
        let owners = [owner("https://a", &a)];
        assert!(d.rows(&owners, "", 1).is_empty());
        assert_eq!(d.rows(&owners, "", 3).len(), 1);
        a.social.blocked.clear();
        a.social.friends.clear();
        a.social.incoming.push("friend-id".into());
        d.remember("https://a", &a);
        assert_eq!(
            d.rows(&owners, "", 2)[0].relationship as u8,
            Relationship::Incoming as u8
        );
        assert!(d.rows(&owners, "", 1).is_empty());
        d.clear();
        assert!(d.rows(&owners, "", 2).is_empty());
    }
}
