use crate::network::Target;
use relay_protocol::User;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct SavedCommunity {
    /// Conversation state stays at its original private account scope through
    /// an authenticated host move. This is never selected just by server UUID.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conversation_homes: Vec<ConversationHome>,
    pub url: String,
    #[serde(default)]
    pub server_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub last_target: Option<Target>,
    /// How to reach a community which was joined without an account-free address.
    /// Holds a secret; kept with the other preferences and never printed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iroh: Option<crate::iroh_route::SavedIroh>,
    /// The protection revision this person accepted for sending without
    /// end-to-end encryption. It applies to that one revision only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_protection_accepted: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConversationHome {
    pub server_id: String,
    pub user_id: String,
    pub original_url: String,
}

pub fn conversation_scope(
    saved: &[SavedCommunity],
    url: &str,
    server_id: &str,
    user_id: &str,
) -> String {
    let home = saved
        .iter()
        .find(|v| v.url == url && v.server_id == server_id)
        .and_then(|v| {
            v.conversation_homes
                .iter()
                .find(|v| v.server_id == server_id && v.user_id == user_id)
        });
    crate::conversation_state::ConversationMemory::scope(
        home.map_or(url, |v| &v.original_url),
        user_id,
    )
}

/// Called only for the worker's verified move, after the source editor was saved.
/// Reuse the old account's scope rather than merging/overwriting drafts that may
/// already belong to an unrelated saved login at the new address.
pub fn moved(saved: &mut Vec<SavedCommunity>, from: &crate::network::CommandOrigin, to: &str) {
    let Some(index) = saved
        .iter()
        .position(|v| v.url == from.url && v.server_id == from.server_id)
    else {
        return;
    };
    let mut entry = saved.remove(index);
    let original = entry
        .conversation_homes
        .iter()
        .find(|v| v.server_id == from.server_id && v.user_id == from.user_id)
        .map_or_else(|| from.url.clone(), |v| v.original_url.clone());
    if let Some(destination) = saved.iter().find(|v| v.url == to) {
        for home in &destination.conversation_homes {
            if !entry
                .conversation_homes
                .iter()
                .any(|v| v.server_id == home.server_id && v.user_id == home.user_id)
            {
                entry.conversation_homes.push(home.clone());
            }
        }
    }
    entry
        .conversation_homes
        .retain(|v| v.server_id != from.server_id || v.user_id != from.user_id);
    entry.conversation_homes.insert(
        0,
        ConversationHome {
            server_id: from.server_id.clone(),
            user_id: from.user_id.clone(),
            original_url: original,
        },
    );
    entry.conversation_homes.truncate(12);
    entry.url = to.into();
    // Iroh capabilities belong to one endpoint, and never follow another host.
    entry.iroh = crate::iroh_route::saved_for(to);
    saved.retain(|v| v.url != to);
    saved.insert(index.min(saved.len()), entry);
}

#[derive(Clone, Debug)]
pub struct CommunityInfo {
    pub url: String,
    pub server_id: String,
    pub name: String,
    pub user: User,
    pub current: bool,
}

pub fn normalize_url(input: &str) -> Result<String, String> {
    if input.len() > 2048 {
        return Err("The server address is too long.".into());
    }
    let mut url = url::Url::parse(input.trim())
        .map_err(|_| "Enter a complete HTTP or HTTPS server address.".to_string())?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(
            "Use an HTTP or HTTPS address without credentials, queries or fragments.".into(),
        );
    }
    let path = url.path().trim_end_matches('/').to_owned();
    url.set_path(&path);
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

/// Shared account proofs need an authenticated route (or this PC's loopback).
/// Keep the form and the network worker on the same admission rule.
pub(crate) fn account_destination(raw: &str) -> Result<String, String> {
    let normalized = normalize_url(raw)?;
    let url = url::Url::parse(&normalized).map_err(|_| "Invalid server address.")?;
    let host = url.host_str().unwrap_or_default();
    let iroh = host
        .strip_suffix(".iroh")
        .is_some_and(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()));
    if url.scheme() == "https"
        || (url.scheme() == "http" && (host == "127.0.0.1" || host == "[::1]" || iroh))
    {
        Ok(normalized)
    } else {
        Err("This server needs an HTTPS or iroh address to use your Relay account. You can still use its existing server account below.".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn addresses_are_canonical_and_never_embed_credentials() {
        assert_eq!(
            normalize_url(" HTTP://EXAMPLE.COM:80/// ").unwrap(),
            "http://example.com"
        );
        assert!(normalize_url("https://name:secret@example.com").is_err());
        assert!(normalize_url("https://example.com?token=secret").is_err());
        assert!(normalize_url("file:///private").is_err());
        assert_ne!(
            normalize_url("https://example.com/a").unwrap(),
            normalize_url("https://example.com/b").unwrap()
        );
    }
}

#[cfg(test)]
mod handover_memory_tests {
    use super::*;
    use crate::{
        chat_state::{Draft, ScopedDrafts},
        conversation_state::ConversationMemory,
    };
    #[test]
    fn repeated_verified_moves_keep_drafts_and_navigation_in_the_original_account_scope() {
        let id = uuid::Uuid::new_v4().to_string();
        let mut saved = vec![SavedCommunity {
            url: "https://old.example".into(),
            server_id: id.clone(),
            ..Default::default()
        }];
        let original = conversation_scope(&saved, "https://old.example", &id, "alice");
        let target = Target::Channel("private".into());
        let mut memory = ConversationMemory::default();
        memory.remember(&original, &target);
        memory.set_scroll(&original, &target, 245.0);
        memory.save_drafts(
            &original,
            vec![(
                target.clone(),
                Draft {
                    content: "unsent before the move".into(),
                    ..Default::default()
                },
            )],
        );
        let mut ram = ScopedDrafts::default();
        ram.store(&original, memory.drafts(&original));
        for (from, to) in [
            ("https://old.example", "https://next.example"),
            ("https://next.example", "https://last.example"),
        ] {
            moved(
                &mut saved,
                &crate::network::CommandOrigin {
                    url: from.into(),
                    server_id: id.clone(),
                    user_id: "alice".into(),
                },
                to,
            );
            let scope = conversation_scope(&saved, to, &id, "alice");
            assert_eq!(scope, original);
            assert_eq!(memory.last(&scope, None), Some(target.clone()));
            assert_eq!(memory.scroll(&scope, &target), Some(245.0));
            assert_eq!(memory.drafts(&scope)[0].1.content, "unsent before the move");
            assert_eq!(
                ram.load(&scope).unwrap()[0].1.content,
                "unsent before the move"
            );
            assert!(memory
                .drafts(&conversation_scope(&saved, to, &id, "bob"))
                .is_empty());
            assert!(memory
                .drafts(&conversation_scope(&saved, to, "other-community", "alice"))
                .is_empty());
        }
        let roundtrip: Vec<SavedCommunity> =
            serde_json::from_slice(&serde_json::to_vec(&saved).unwrap()).unwrap();
        assert_eq!(
            conversation_scope(&roundtrip, "https://last.example", &id, "alice"),
            original
        );
    }
    #[test]
    fn a_saved_destination_login_keeps_its_other_accounts_conversation_binding() {
        let id = uuid::Uuid::new_v4().to_string();
        let mut saved = vec![
            SavedCommunity {
                url: "https://old.example".into(),
                server_id: id.clone(),
                ..Default::default()
            },
            SavedCommunity {
                url: "https://new.example".into(),
                server_id: id.clone(),
                conversation_homes: vec![ConversationHome {
                    server_id: id.clone(),
                    user_id: "bob".into(),
                    original_url: "https://bobs-original.example".into(),
                }],
                ..Default::default()
            },
        ];
        moved(
            &mut saved,
            &crate::network::CommandOrigin {
                url: "https://old.example".into(),
                server_id: id.clone(),
                user_id: "alice".into(),
            },
            "https://new.example",
        );
        assert_eq!(
            conversation_scope(&saved, "https://new.example", &id, "alice"),
            ConversationMemory::scope("https://old.example", "alice")
        );
        assert_eq!(
            conversation_scope(&saved, "https://new.example", &id, "bob"),
            ConversationMemory::scope("https://bobs-original.example", "bob")
        );
    }
}
