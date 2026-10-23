//! Search predicates run only on locally decrypted history, before results leave the worker.
use crate::network::Target;
use relay_protocol::Message;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ContentKind {
    #[default]
    Any,
    File,
    Image,
    Video,
    Gif,
    Link,
}

impl ContentKind {
    pub const ALL: [Self; 6] = [
        Self::Any,
        Self::File,
        Self::Image,
        Self::Video,
        Self::Gif,
        Self::Link,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::Any => "Any content",
            Self::File => "Files",
            Self::Image => "Images",
            Self::Video => "Videos",
            Self::Gif => "GIFs",
            Self::Link => "Links",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchFilters {
    pub author_id: Option<String>,
    pub channel_id: Option<String>,
    pub peer_id: Option<String>,
    pub content: ContentKind,
    // UTC boundaries are captured once per search and reused for all later history pages.
    pub since: Option<i64>,
}

impl SearchFilters {
    pub fn active(&self) -> bool {
        self.author_id.is_some()
            || self.channel_id.is_some()
            || self.peer_id.is_some()
            || self.content != ContentKind::Any
            || self.since.is_some()
    }

    pub fn includes_target(&self, target: &Target) -> bool {
        match (&self.channel_id, &self.peer_id, target) {
            (None, None, _) => true,
            (Some(selected), None, Target::Channel(id)) => selected == id,
            (None, Some(selected), Target::Direct(id)) => selected == id,
            _ => false,
        }
    }

    pub fn matches(&self, target: &Target, message: &Message, needle: &str) -> bool {
        if !self.includes_target(target)
            || self
                .author_id
                .as_ref()
                .is_some_and(|id| *id != message.author.id)
            || self.since.is_some_and(|since| {
                chrono::DateTime::parse_from_rfc3339(&message.created_at)
                    .map_or(true, |date| date.timestamp() < since)
            })
        {
            return false;
        }
        if !needle.is_empty()
            && !message.content.to_lowercase().contains(needle)
            && !message
                .attachments
                .iter()
                .any(|file| file.name.to_lowercase().contains(needle))
        {
            return false;
        }
        match self.content {
            ContentKind::Any => true,
            ContentKind::File => !message.attachments.is_empty(),
            ContentKind::Image => message
                .attachments
                .iter()
                .any(|file| file.mime.starts_with("image/")),
            ContentKind::Video => message
                .attachments
                .iter()
                .any(|file| file.mime.starts_with("video/")),
            ContentKind::Gif => {
                message
                    .attachments
                    .iter()
                    .any(|file| file.mime == "image/gif")
                    || crate::online_gifs::message_id(&message.content).is_some()
            }
            ContentKind::Link => crate::message_format::parse(&message.content)
                .iter()
                .any(|span| span.link.is_some()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_scope_does_not_search_a_channel_with_the_same_id() {
        let mut filters = SearchFilters {
            peer_id: Some("mira-id".into()),
            ..Default::default()
        };
        assert!(filters.active());
        assert!(filters.includes_target(&Target::Direct("mira-id".into())));
        assert!(!filters.includes_target(&Target::Channel("mira-id".into())));
        assert!(!filters.includes_target(&Target::Direct("jonas-id".into())));
        // Invalid combined scopes must not silently broaden access.
        filters.channel_id = Some("general-id".into());
        assert!(!filters.includes_target(&Target::Direct("mira-id".into())));
        assert!(!filters.includes_target(&Target::Channel("general-id".into())));
    }

    #[test]
    fn combined_filters_use_stable_author_and_channel_ids() {
        let filters = SearchFilters {
            author_id: Some("mira-id".into()),
            channel_id: Some("general-id".into()),
            peer_id: None,
            content: ContentKind::Gif,
            since: None,
        };
        let mut message = Message {
            author: relay_protocol::User {
                id: "mira-id".into(),
                username: "Renamed user".into(),
                ..Default::default()
            },
            attachments: vec![relay_protocol::Attachment {
                id: "file-id".into(),
                name: "Victory.GIF".into(),
                mime: "image/gif".into(),
                size: 100,
                url: "opaque".into(),
            }],
            ..Default::default()
        };
        assert!(filters.matches(&Target::Channel("general-id".into()), &message, "victory"));
        assert!(!filters.matches(&Target::Direct("general-id".into()), &message, "victory"));
        assert!(!filters.matches(&Target::Channel("other".into()), &message, "victory"));
        message.author.id = "someone-else".into();
        assert!(!filters.matches(&Target::Channel("general-id".into()), &message, ""));
    }

    #[test]
    fn fixed_time_boundary_handles_offsets_and_malformed_dates() {
        let filters = SearchFilters {
            since: Some(1_728_000_000),
            ..Default::default()
        };
        let target = Target::Channel("general".into());
        let mut message = Message {
            created_at: "2024-10-04T00:00:00Z".into(),
            ..Default::default()
        };
        assert!(filters.matches(&target, &message, ""));
        message.created_at = "2024-10-04T02:00:00+02:00".into();
        assert!(filters.matches(&target, &message, ""));
        message.created_at = "2024-10-03T23:59:59Z".into();
        assert!(!filters.matches(&target, &message, ""));
        message.created_at = "invalid".into();
        assert!(!filters.matches(&target, &message, ""));
        assert!(SearchFilters::default().matches(&target, &message, ""));
    }

    #[test]
    fn content_filters_keep_literal_code_out_of_link_results() {
        let target = Target::Channel("general".into());
        let filters = SearchFilters {
            content: ContentKind::Link,
            ..Default::default()
        };
        let mut message = Message {
            content: "[Guide 😭](https://example.com/guide)".into(),
            ..Default::default()
        };
        assert!(filters.matches(&target, &message, "guide"));
        message.content = "`https://example.com/guide`".into();
        assert!(!filters.matches(&target, &message, "guide"));
        let gif = SearchFilters {
            content: ContentKind::Gif,
            ..Default::default()
        };
        message.content = "https://giphy.com/gifs/owned123".into();
        assert!(gif.matches(&target, &message, ""));
        message.content = "https://giphy.com.evil.test/gifs/owned123".into();
        assert!(!gif.matches(&target, &message, ""));
    }
}
