//! Local link cards only. Rendering never fetches a page, image or provider API.
use super::*;

#[derive(Debug, PartialEq, Eq)]
struct Card {
    provider: &'static str,
    title: String,
    display: String,
    href: String,
}
fn identifier(value: &str, min: usize, max: usize, extra: &[u8]) -> bool {
    (min..=max).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || extra.contains(&b))
}
fn card(raw: &str) -> Option<Card> {
    if raw.len() > 2048 {
        return None;
    }
    let url = url::Url::parse(raw).ok()?;
    if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() {
        return None;
    }
    let host = url.host_str()?;
    let segments: Vec<_> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    if host == "discord.gg" || segments.iter().any(|s| matches!(*s, "invite" | "join")) {
        return None;
    }
    if url.query_pairs().any(|(key, _)| {
        let key = key.to_ascii_lowercase();
        matches!(key.as_str(), "code" | "state" | "key" | "invite" | "auth")
            || [
                "token",
                "password",
                "secret",
                "credential",
                "session",
                "nonce",
                "proof",
            ]
            .iter()
            .any(|part| key.contains(part))
    }) || url.fragment().is_some_and(|fragment| {
        fragment.contains('=') || fragment.to_ascii_lowercase().contains("token")
    }) {
        return None;
    }
    let mut authority = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    if let Some(port) = url.port() {
        authority.push_str(&format!(":{port}"));
    }
    let mut provider = "Link";
    let mut title = authority.clone();
    match if url.port().is_none() { host } else { "" } {
        "youtube.com" | "www.youtube.com" | "m.youtube.com" => {
            let video = if segments.first() == Some(&"watch") {
                url.query_pairs()
                    .find(|(key, _)| key == "v")
                    .map(|(_, value)| value.into_owned())
            } else if segments.len() == 2 && matches!(segments[0], "shorts" | "embed") {
                Some(segments[1].into())
            } else {
                None
            };
            if video
                .as_deref()
                .is_some_and(|id| identifier(id, 11, 11, b"_-"))
            {
                provider = "YouTube";
                title = "YouTube video".into();
            }
        }
        "youtu.be" if segments.len() == 1 && identifier(segments[0], 11, 11, b"_-") => {
            provider = "YouTube";
            title = "YouTube video".into();
        }
        "twitch.tv" | "www.twitch.tv" => {
            if segments.len() == 1 && identifier(segments[0], 1, 25, b"_") {
                provider = "Twitch";
                title = "Twitch channel".into();
            } else if segments.len() == 2
                && segments[0] == "videos"
                && segments[1].bytes().all(|b| b.is_ascii_digit())
                && segments[1].len() <= 20
            {
                provider = "Twitch";
                title = "Twitch video".into();
            }
        }
        "clips.twitch.tv" if segments.len() == 1 && identifier(segments[0], 1, 100, b"_-") => {
            provider = "Twitch";
            title = "Twitch clip".into();
        }
        "open.spotify.com"
            if segments.len() == 2
                && matches!(
                    segments[0],
                    "track" | "album" | "playlist" | "artist" | "episode" | "show"
                )
                && identifier(segments[1], 22, 22, b"") =>
        {
            provider = "Spotify";
            title = format!("Spotify {}", segments[0]);
        }
        "github.com" | "www.github.com"
            if segments.len() >= 2
                && identifier(segments[0], 1, 100, b"-_")
                && identifier(segments[1], 1, 100, b"-_.") =>
        {
            provider = "GitHub";
            title = format!("{} / {}", segments[0], segments[1]);
            if segments.len() == 4
                && matches!(segments[2], "issues" | "pull")
                && segments[3].bytes().all(|b| b.is_ascii_digit())
            {
                title = format!(
                    "{} · {} #{}",
                    title,
                    if segments[2] == "pull" {
                        "Pull request"
                    } else {
                        "Issue"
                    },
                    segments[3]
                );
            }
        }
        _ => {}
    }
    let display: String = format!("{authority}{}", url.path())
        .chars()
        .take(200)
        .collect();
    Some(Card {
        provider,
        title,
        display,
        href: url.to_string(),
    })
}
fn cards(content: &str, spoilers_revealed: bool) -> Vec<Card> {
    let bounded: String = content.chars().take(10_000).collect();
    let mut seen = std::collections::HashSet::new();
    let mut output = Vec::new();
    for span in crate::message_format::parse(&bounded) {
        if span.style.code || (span.style.spoiler && !spoilers_revealed) {
            continue;
        }
        let Some(card) = span.link.as_deref().and_then(card) else {
            continue;
        };
        if seen.insert(card.href.clone()) {
            output.push(card);
        }
        if output.len() == 3 {
            break;
        }
    }
    output
}
pub(super) fn render(ui: &mut egui::Ui, content: &str, spoilers_revealed: bool) {
    for card in cards(content, spoilers_revealed) {
        ui.add_space(6.0);
        egui::Frame::none()
            .fill(c_card())
            .stroke(egui::Stroke::new(1.0_f32, c_line()))
            .rounding(6.0)
            .inner_margin(egui::Margin::same(10.0))
            .show(ui, |ui| {
                ui.set_max_width(ui.available_width().min(420.0));
                ui.label(egui::RichText::new(card.provider).small().color(c_accent()));
                ui.add(egui::Label::new(egui::RichText::new(card.title).strong()).wrap(true));
                ui.add(
                    egui::Label::new(egui::RichText::new(card.display).small().color(c_muted()))
                        .wrap(true),
                );
                ui.hyperlink_to("Open link", card.href);
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn known_provider_cards_validate_host_and_provider_identifiers() {
        for (url, provider) in [
            ("https://www.youtube.com/watch?v=dQw4w9WgXcQ", "YouTube"),
            ("https://youtu.be/dQw4w9WgXcQ?t=12", "YouTube"),
            ("https://www.twitch.tv/owned_channel", "Twitch"),
            ("https://clips.twitch.tv/Owned-Clip", "Twitch"),
            (
                "https://open.spotify.com/track/0123456789012345678901?si=share",
                "Spotify",
            ),
            ("https://github.com/TheBengine/Relay/pull/12", "GitHub"),
        ] {
            assert_eq!(card(url).unwrap().provider, provider);
        }
        assert_eq!(
            card("https://youtube.com.evil.invalid/watch?v=dQw4w9WgXcQ")
                .unwrap()
                .provider,
            "Link"
        );
        assert_eq!(
            card("https://www.youtube.com/watch?v=short")
                .unwrap()
                .provider,
            "Link"
        );
        let custom = card("https://www.youtube.com:444/watch?v=dQw4w9WgXcQ").unwrap();
        assert_eq!(custom.provider, "Link");
        assert!(custom.display.contains(":444/"));
    }
    #[test]
    fn cards_exclude_code_spoilers_credentials_invitations_and_sensitive_queries() {
        assert!(cards(
            "```\nhttps://example.org/hidden\n``` `https://example.org/also-hidden`",
            false
        )
        .is_empty());
        assert!(cards("||https://example.org/spoiler||", false).is_empty());
        assert_eq!(cards("||https://example.org/spoiler||", true).len(), 1);
        for url in [
            "http://example.org",
            "mailto:person@example.org",
            "https://user:password@example.org",
            "https://example.org/?access_token=secret",
            "https://example.org/#join=secret",
            "https://discord.gg/privateinvite",
        ] {
            assert!(card(url).is_none());
        }
    }
    #[test]
    fn generic_cards_are_capped_deduplicated_and_hide_query_without_changing_click_destination() {
        let cards = cards("https://example.org/a?q=search https://example.org/a?q=search https://example.org/b https://example.org/c https://example.org/d", false);
        assert_eq!(cards.len(), 3);
        assert_eq!(cards[0].display, "example.org/a");
        assert_eq!(cards[0].href, "https://example.org/a?q=search");
    }
    #[test]
    fn rendering_cards_never_connects_or_opens_a_browser_without_a_click() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let link = format!("https://{}/owned?q=hidden", listener.local_addr().unwrap());
        let ctx = egui::Context::default();
        let output = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| render(ui, &link, false));
        });
        assert!(output.platform_output.open_url.is_none());
        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
        );
    }
}
