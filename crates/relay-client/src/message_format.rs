//! Small Discord-style message markup parser for display and literal-aware mentions.
//!
//! Message contents remain plain text on the wire and in storage, so this does
//! not change encryption, search, editing, or server validation.

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Style {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    pub code: bool,
    pub spoiler: bool,
    pub heading: u8,
    pub small: bool,
    pub quote: bool,
}

impl Style {
    pub fn text_size(self, base: f32) -> f32 {
        match self.heading {
            1 => base * 1.6,
            2 => base * 1.35,
            3 => base * 1.15,
            _ if self.small => base * 0.8,
            _ => base,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub style: Style,
    pub link: Option<String>,
}

pub fn parse(input: &str) -> Vec<Span> {
    let mut spans = Vec::new();
    let mut cursor = 0;
    let mut multi_quote = false;

    while let Some((open, code_start, close, after_close)) = find_fenced_block(input, cursor) {
        parse_blocks(&input[cursor..open], &mut spans, &mut multi_quote);
        push(
            &mut spans,
            &input[code_start..close],
            Style {
                code: true,
                quote: multi_quote,
                ..Style::default()
            },
        );
        cursor = after_close;
    }

    parse_blocks(&input[cursor..], &mut spans, &mut multi_quote);
    autolink(spans)
}

pub fn plain_preview(input: &str, limit: usize) -> String {
    let mut text = String::new();
    let mut hidden = false;
    for span in parse(input) {
        if span.style.spoiler {
            if !hidden {
                text.push_str("[spoiler]");
            }
            hidden = true;
            continue;
        }
        hidden = false;
        let body = if span.style.quote {
            span.text.strip_prefix("│ ").unwrap_or(&span.text)
        } else {
            &span.text
        };
        let mut cursor = 0;
        while cursor < body.len() {
            if !span.style.code {
                if let Some(length) = expression_token_length(&body[cursor..]) {
                    text.push(':');
                    text.push_str(body[cursor..cursor + length].split(':').nth(1).unwrap());
                    text.push(':');
                    cursor += length;
                    continue;
                }
            }
            let ch = body[cursor..].chars().next().unwrap();
            text.push(ch);
            cursor += ch.len_utf8();
        }
    }
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() <= limit {
        text
    } else if limit == 0 {
        String::new()
    } else {
        text.chars()
            .take(limit - 1)
            .chain(std::iter::once('…'))
            .collect()
    }
}

fn parse_blocks(input: &str, output: &mut Vec<Span>, multi_quote: &mut bool) {
    let mut plain = String::new();
    for line in input.split_inclusive('\n') {
        let mut style = Style::default();
        let mut body = line;
        if let Some(rest) = body.strip_prefix(">>> ") {
            *multi_quote = true;
            body = rest;
        }
        if *multi_quote {
            style.quote = true;
        } else if let Some(rest) = body.strip_prefix("> ") {
            style.quote = true;
            body = rest;
        }
        if let Some(rest) = body.strip_prefix("### ") {
            style.heading = 3;
            style.bold = true;
            body = rest;
        } else if let Some(rest) = body.strip_prefix("## ") {
            style.heading = 2;
            style.bold = true;
            body = rest;
        } else if let Some(rest) = body.strip_prefix("# ") {
            style.heading = 1;
            style.bold = true;
            body = rest;
        } else if let Some(rest) = body.strip_prefix("-# ") {
            style.small = true;
            body = rest;
        }
        let indentation = body.bytes().take_while(|ch| *ch == b' ').count();
        let list = body[indentation..]
            .strip_prefix("- ")
            .or_else(|| body[indentation..].strip_prefix("* "));
        if style != Style::default() || list.is_some() {
            parse_inline(&plain, Style::default(), output);
            plain.clear();
            if style.quote {
                push(output, "│ ", style);
            }
            if let Some(rest) = list {
                push(output, &format!("{}• ", " ".repeat(indentation)), style);
                body = rest;
            }
            parse_inline(body, style, output);
        } else {
            plain.push_str(line);
        }
    }
    parse_inline(&plain, Style::default(), output);
}

fn autolink(spans: Vec<Span>) -> Vec<Span> {
    let spans = markdown_links(spans);
    let mut output = Vec::new();
    for span in spans {
        if span.style.code || span.link.is_some() {
            output.push(span);
            continue;
        }

        let mut cursor = 0;
        while cursor < span.text.len() {
            let Some(start) = next_url_start(&span.text, cursor) else {
                push(&mut output, &span.text[cursor..], span.style);
                break;
            };
            if start > cursor {
                push(&mut output, &span.text[cursor..start], span.style);
            }

            let end = url_end(&span.text, start);
            let trimmed_end = trim_url_punctuation(&span.text, start, end);
            let candidate = &span.text[start..trimmed_end];
            if valid_web_url(candidate) {
                push_link(&mut output, candidate, span.style, candidate.to_owned());
                cursor = trimmed_end;
            } else {
                let fallback_end = (start + 4).min(span.text.len());
                push(&mut output, &span.text[start..fallback_end], span.style);
                cursor = fallback_end;
            }
        }
    }
    output
}

fn markdown_links(spans: Vec<Span>) -> Vec<Span> {
    let mut output = Vec::new();
    for span in spans {
        if span.style.code || span.link.is_some() {
            output.push(span);
            continue;
        }

        let mut cursor = 0;
        while let Some((start, label_start, label_end, after, url)) =
            next_markdown_link(&span.text, cursor)
        {
            push(&mut output, &span.text[cursor..start], span.style);
            push_link(
                &mut output,
                &span.text[label_start..label_end],
                span.style,
                url,
            );
            cursor = after;
        }
        push(&mut output, &span.text[cursor..], span.style);
    }
    output
}

fn next_markdown_link(input: &str, from: usize) -> Option<(usize, usize, usize, usize, String)> {
    let mut search_from = from;
    while search_from < input.len() {
        let open = search_from + input[search_from..].find('[')?;
        let label_start = open + 1;
        let Some(close_label) = input[label_start..]
            .find("](")
            .map(|index| label_start + index)
        else {
            search_from = open + 1;
            continue;
        };
        let url_start = close_label + 2;
        if let Some(relative_close_url) = input[url_start..].find(')') {
            let close_url = url_start + relative_close_url;
            let url = input[url_start..close_url].trim();
            let label_end = close_label;
            if label_end > label_start && valid_web_url(url) {
                return Some((open, label_start, label_end, close_url + 1, url.to_owned()));
            }
        }
        search_from = open + 1;
    }
    None
}

fn next_url_start(input: &str, from: usize) -> Option<usize> {
    let mut search_from = from;
    loop {
        let http = input[search_from..]
            .find("http://")
            .map(|index| search_from + index);
        let https = input[search_from..]
            .find("https://")
            .map(|index| search_from + index);
        let start = [http, https].into_iter().flatten().min()?;
        if start == 0
            || input[..start]
                .chars()
                .next_back()
                .is_some_and(|previous| previous.is_whitespace() || "(<[{\"'".contains(previous))
        {
            return Some(start);
        }
        search_from = start + 1;
    }
}

fn url_end(input: &str, start: usize) -> usize {
    input[start..]
        .char_indices()
        .find_map(|(offset, ch)| {
            (ch.is_whitespace() || matches!(ch, '<' | '>' | '"' | '\'' | '|'))
                .then_some(start + offset)
        })
        .unwrap_or(input.len())
}

fn trim_url_punctuation(input: &str, start: usize, mut end: usize) -> usize {
    while end > start {
        let ch = input[start..end]
            .chars()
            .next_back()
            .expect("non-empty URL candidate");
        if !matches!(ch, '.' | ',' | '!' | '?' | ';' | ':' | ')' | ']' | '}') {
            break;
        }
        end -= ch.len_utf8();
    }
    end
}

fn valid_web_url(candidate: &str) -> bool {
    url::Url::parse(candidate).is_ok_and(|url| {
        matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
    })
}

fn find_fenced_block(input: &str, from: usize) -> Option<(usize, usize, usize, usize)> {
    let mut line_start = from;
    while line_start < input.len() {
        if let Some((fence_start, line_after)) = fence_on_line(input, line_start) {
            let mut closing_line = line_after;
            while closing_line < input.len() {
                if let Some((close_start, after_close)) = closing_fence_on_line(input, closing_line)
                {
                    return Some((fence_start, line_after, close_start, after_close));
                }
                closing_line = next_line(input, closing_line);
            }
            // Discord treats an unclosed fenced block as code through end of message.
            return Some((fence_start, line_after, input.len(), input.len()));
        }
        line_start = next_line(input, line_start);
    }
    None
}

fn fence_on_line(input: &str, start: usize) -> Option<(usize, usize)> {
    let end = input[start..]
        .find('\n')
        .map_or(input.len(), |newline| start + newline);
    let line = &input[start..end];
    let spaces = line.bytes().take_while(|byte| *byte == b' ').count();
    if spaces <= 3 && line[spaces..].starts_with("```") {
        Some((
            start + spaces,
            if end < input.len() { end + 1 } else { end },
        ))
    } else {
        None
    }
}

fn closing_fence_on_line(input: &str, start: usize) -> Option<(usize, usize)> {
    let end = input[start..]
        .find('\n')
        .map_or(input.len(), |newline| start + newline);
    let line = &input[start..end];
    let spaces = line.bytes().take_while(|byte| *byte == b' ').count();
    let tail = line.get(spaces..)?;
    if spaces <= 3 && tail.starts_with("```") && tail[3..].trim().is_empty() {
        Some((
            start + spaces,
            if end < input.len() { end + 1 } else { end },
        ))
    } else {
        None
    }
}

fn next_line(input: &str, start: usize) -> usize {
    input[start..]
        .find('\n')
        .map_or(input.len(), |newline| start + newline + 1)
}

fn parse_inline(input: &str, style: Style, output: &mut Vec<Span>) {
    let mut cursor = 0;
    parse_until(input, &mut cursor, None, style, output);
}

fn parse_until(
    input: &str,
    cursor: &mut usize,
    closing: Option<&str>,
    style: Style,
    output: &mut Vec<Span>,
) {
    let mut plain_start = *cursor;

    while *cursor < input.len() {
        let remaining = &input[*cursor..];
        if closing.is_some_and(|delimiter| remaining.starts_with(delimiter)) {
            push(output, &input[plain_start..*cursor], style);
            *cursor += closing.unwrap().len();
            return;
        }

        if !style.code && remaining.starts_with('\\') {
            let escaped = &remaining[1..];
            if let Some(ch) = escaped.chars().next().filter(|ch| {
                matches!(
                    ch,
                    '*' | '_' | '~' | '`' | '\\' | '|' | ':' | '#' | '>' | '-'
                )
            }) {
                push(output, &input[plain_start..*cursor], style);
                push(output, &ch.to_string(), style);
                *cursor += 1 + ch.len_utf8();
                plain_start = *cursor;
                continue;
            }
        }

        if !style.code {
            if let Some(length) = expression_token_length(remaining) {
                *cursor += length;
                continue;
            }
            // URL punctuation and underscores are literal, including markdown targets.
            if remaining.starts_with("https://") || remaining.starts_with("http://") {
                let mut end = url_end(remaining, 0);
                if let Some(delimiter) =
                    closing.filter(|delimiter| remaining[..end].ends_with(delimiter))
                {
                    end -= delimiter.len();
                }
                if end > 0 && valid_web_url(&remaining[..trim_url_punctuation(remaining, 0, end)]) {
                    *cursor += end;
                    continue;
                }
            }
            // Usernames may contain underscores; keep the whole mention literal
            // while allowing an enclosing underline/italic delimiter to close.
            if let Some(username) = remaining.strip_prefix('@') {
                let mut end = 1 + username
                    .find(|ch: char| !ch.is_alphanumeric() && !matches!(ch, '_' | '-' | '.'))
                    .unwrap_or(username.len());
                if let Some(delimiter) = closing.filter(|value| value.starts_with('_')) {
                    let name_end = remaining[..end].trim_end_matches('.').len();
                    if remaining[..name_end].ends_with(delimiter) {
                        end = name_end - delimiter.len();
                    }
                }
                if end > 1 {
                    *cursor += end;
                    continue;
                }
            }
            if let Some((delimiter, next_style)) = delimiter_at(remaining, style) {
                if has_closer(&remaining[delimiter.len()..], delimiter) {
                    push(output, &input[plain_start..*cursor], style);
                    *cursor += delimiter.len();
                    parse_until(input, cursor, Some(delimiter), next_style, output);
                    plain_start = *cursor;
                    continue;
                }
            }
        }

        let ch = remaining.chars().next().expect("cursor is within input");
        *cursor += ch.len_utf8();
    }

    push(output, &input[plain_start..*cursor], style);
}

pub(crate) fn expression_token_length(input: &str) -> Option<usize> {
    if !input.starts_with("<emoji:") && !input.starts_with("<sticker:") {
        return None;
    }
    let end = input.find('>')?;
    let mut fields = input.get(1..end)?.split(':');
    if !input.starts_with('<') || !matches!(fields.next()?, "emoji" | "sticker") {
        return None;
    }
    let name = fields.next()?;
    let id = fields.next()?;
    ((2..=32).contains(&name.len())
        && name
            .bytes()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == b'_')
        && !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == b'-')
        && fields.next().is_none())
    .then_some(end + 1)
}

fn delimiter_at(remaining: &str, style: Style) -> Option<(&str, Style)> {
    let variants = [
        (
            "***",
            Style {
                bold: true,
                italic: true,
                ..style
            },
        ),
        (
            "___",
            Style {
                underline: true,
                italic: true,
                ..style
            },
        ),
        (
            "**",
            Style {
                bold: true,
                ..style
            },
        ),
        (
            "__",
            Style {
                underline: true,
                ..style
            },
        ),
        (
            "~~",
            Style {
                strike: true,
                ..style
            },
        ),
        (
            "||",
            Style {
                spoiler: true,
                ..style
            },
        ),
        (
            "*",
            Style {
                italic: true,
                ..style
            },
        ),
        (
            "_",
            Style {
                italic: true,
                ..style
            },
        ),
        (
            "`",
            Style {
                code: true,
                ..style
            },
        ),
    ];
    variants
        .into_iter()
        .find(|(delimiter, _)| remaining.starts_with(delimiter))
}

fn has_closer(input: &str, delimiter: &str) -> bool {
    input.find(delimiter).is_some_and(|index| index > 0)
}

fn push(output: &mut Vec<Span>, text: &str, style: Style) {
    if text.is_empty() {
        return;
    }
    if let Some(previous) = output.last_mut().filter(|previous| {
        previous.style == style
            && previous.link.is_none()
            && !(style.quote && previous.text.ends_with('\n'))
    }) {
        previous.text.push_str(text);
    } else {
        output.push(Span {
            text: text.to_owned(),
            style,
            link: None,
        });
    }
}

fn push_link(output: &mut Vec<Span>, text: &str, style: Style, url: String) {
    if !text.is_empty() {
        output.push(Span {
            text: text.to_owned(),
            style,
            link: Some(url),
        });
    }
}

/// A link whose visible words are not its own address, like `[Relay](https://…)`.
/// Such a link is shown before it opens, because the words can say anything.
pub fn masked_link(text: &str, url: &str) -> bool {
    let shown = text.trim();
    shown != url && shown != url.trim_end_matches('/')
}

/// The host part of a web address, for "This link goes to …".
pub fn link_host(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .unwrap_or_else(|| url.to_owned())
}

#[cfg(test)]
mod tests {
    #[test]
    fn masked_links_are_the_ones_whose_words_hide_the_address() {
        assert!(!super::masked_link(
            "https://example.com/a",
            "https://example.com/a"
        ));
        assert!(!super::masked_link(
            "https://example.com",
            "https://example.com/"
        ));
        assert!(super::masked_link("Relay", "https://evil.example/login"));
        assert!(super::masked_link(
            "https://relay.example",
            "https://evil.example/login"
        ));
        assert_eq!(
            super::link_host("https://evil.example/login?x=1"),
            "evil.example"
        );
    }

    use super::*;

    #[test]
    fn usernames_with_underscores_render_as_complete_names() {
        let spans = parse("@bob_one_two and @jane.doe.");
        assert_eq!(
            spans
                .iter()
                .map(|span| span.text.as_str())
                .collect::<String>(),
            "@bob_one_two and @jane.doe."
        );
        assert!(spans
            .iter()
            .all(|span| !span.style.italic && !span.style.underline));
    }

    #[test]
    fn usernames_preserve_enclosing_styles_and_punctuation() {
        let spans = parse("__@bob_one_two__. _@jane.doe_!");
        assert_eq!(
            spans
                .iter()
                .map(|span| span.text.as_str())
                .collect::<String>(),
            "@bob_one_two. @jane.doe!"
        );
        assert!(spans
            .iter()
            .any(|span| span.text == "@bob_one_two" && span.style.underline));
        assert!(spans
            .iter()
            .any(|span| span.text == "@jane.doe" && span.style.italic));
    }

    #[test]
    fn compact_previews_hide_spoilers_preserve_literal_code_and_bound_unicode() {
        let preview = plain_preview("**Ready** ||hidden **surprise** https://example.com/private|| <emoji:game_night:owned>\n> Tomorrow 😭", 160);
        assert_eq!(preview, "Ready [spoiler] :game_night: Tomorrow 😭");
        assert!(!preview.contains("surprise") && !preview.contains("example.com"));
        assert_eq!(
            plain_preview("`||literal|| <emoji:game_night:owned>`", 160),
            "||literal|| <emoji:game_night:owned>"
        );
        let bounded = plain_preview("æøå 😭 a longer message", 7);
        assert_eq!(bounded.chars().count(), 7);
        assert!(bounded.ends_with('…'));
        assert_eq!(plain_preview("anything", 0), "");
    }

    #[test]
    fn custom_expression_names_with_underscores_remain_whole_inside_markup() {
        let first = "<emoji:game_night:owned-1>";
        let second = "<emoji:ready_to_play:owned-2>";
        let input = format!("**{first}** then {second} ||{first}|| `{second}`");
        let spans = parse(&input);
        assert_eq!(
            spans
                .iter()
                .map(|span| span.text.as_str())
                .collect::<String>(),
            format!("{first} then {second} {first} {second}")
        );
        assert!(spans
            .iter()
            .any(|span| span.text == first && span.style.bold));
        assert!(spans
            .iter()
            .any(|span| span.text == first && span.style.spoiler));
        assert!(spans
            .iter()
            .any(|span| span.text == second && span.style.code));
        assert!(!spans.iter().any(|span| span.style.italic));
    }

    #[test]
    fn discord_underline_and_combined_styles_are_distinct_from_bold() {
        let result = parse("__underlined__ ___both___ __**strong**__");
        assert!(result
            .iter()
            .any(|span| span.text == "underlined" && span.style.underline && !span.style.bold));
        assert!(result
            .iter()
            .any(|span| span.text == "both" && span.style.underline && span.style.italic));
        assert!(result
            .iter()
            .any(|span| span.text == "strong" && span.style.underline && span.style.bold));
    }

    #[test]
    fn discord_headings_subtext_quotes_and_lists_respect_line_boundaries() {
        let result = parse("# Heading\n## Second\n### Third\n-# Small\n> Quote\n- First\n  * Nested\n1. Ordered\ninline # unchanged\n\\# literal");
        assert!(result
            .iter()
            .any(|span| span.text == "Heading\n" && span.style.heading == 1));
        assert!(result.iter().any(|span| span.style.heading == 2));
        assert!(result.iter().any(|span| span.style.heading == 3));
        assert!(result
            .iter()
            .any(|span| span.text == "Small\n" && span.style.small));
        assert!(result
            .iter()
            .any(|span| span.text.contains("│ Quote") && span.style.quote));
        let text: String = result.iter().map(|span| span.text.as_str()).collect();
        assert!(text.contains("• First\n  • Nested\n1. Ordered"));
        assert!(text.contains("inline # unchanged\n# literal"));
        let code = parse("```\n# Heading\n__literal__\n```");
        assert!(code
            .iter()
            .all(|span| span.style.code && span.style.heading == 0 && !span.style.underline));
    }

    #[test]
    fn multiline_quote_continues_and_plain_multiline_emphasis_survives() {
        assert!(parse(">>> first\nsecond")
            .iter()
            .all(|span| span.style.quote));
        assert!(parse("**first\nsecond**")
            .iter()
            .all(|span| span.style.bold));
        let result = parse(">>> first\n```\ncode\n```\nlast");
        assert!(result.iter().all(|span| span.style.quote));
    }

    #[test]
    fn formatting_never_rewrites_underscores_in_web_addresses() {
        for text in [
            "https://example.com/one_two_three",
            "**https://example.com/one_two_three**",
            "[link](https://example.com/one_two_three)",
        ] {
            let result = parse(text);
            assert!(result
                .iter()
                .any(|span| span.link.as_deref() == Some("https://example.com/one_two_three")));
        }
    }

    fn spans(input: &str) -> Vec<(String, Style)> {
        parse(input)
            .into_iter()
            .map(|span| (span.text, span.style))
            .collect()
    }

    #[test]
    fn parses_inline_formatting_and_nested_styles() {
        assert_eq!(
            spans("Hi **bold _and italic_** ~~gone~~ `code`"),
            vec![
                ("Hi ".into(), Style::default()),
                (
                    "bold ".into(),
                    Style {
                        bold: true,
                        ..Style::default()
                    }
                ),
                (
                    "and italic".into(),
                    Style {
                        bold: true,
                        italic: true,
                        ..Style::default()
                    }
                ),
                (" ".into(), Style::default()),
                (
                    "gone".into(),
                    Style {
                        strike: true,
                        ..Style::default()
                    }
                ),
                (" ".into(), Style::default()),
                (
                    "code".into(),
                    Style {
                        code: true,
                        ..Style::default()
                    }
                ),
            ]
        );
    }

    #[test]
    fn keeps_unicode_escapes_and_unmatched_markers_safe() {
        assert_eq!(
            spans("Hej 🌍 \\*ord\\* **åben"),
            vec![("Hej 🌍 *ord* **åben".into(), Style::default())]
        );
    }

    #[test]
    fn parses_fenced_code_and_keeps_surrounding_text() {
        assert_eq!(
            spans("før\n```rust\nlet x = 1;\n```\nefter"),
            vec![
                ("før\n".into(), Style::default()),
                (
                    "let x = 1;\n".into(),
                    Style {
                        code: true,
                        ..Style::default()
                    }
                ),
                ("efter".into(), Style::default()),
            ]
        );
    }

    #[test]
    fn unmatched_fence_displays_its_body_as_code() {
        assert_eq!(
            spans("```\nlet value = 3;"),
            vec![(
                "let value = 3;".into(),
                Style {
                    code: true,
                    ..Style::default()
                }
            )]
        );
    }

    #[test]
    fn marks_spoilers_without_interpreting_markup_inside_code() {
        assert_eq!(
            spans("||secret **word**|| `||visible||`"),
            vec![
                (
                    "secret ".into(),
                    Style {
                        spoiler: true,
                        ..Style::default()
                    }
                ),
                (
                    "word".into(),
                    Style {
                        spoiler: true,
                        bold: true,
                        ..Style::default()
                    }
                ),
                (" ".into(), Style::default()),
                (
                    "||visible||".into(),
                    Style {
                        code: true,
                        ..Style::default()
                    }
                ),
            ]
        );
    }

    #[test]
    fn autolinks_only_valid_web_urls_and_leaves_trailing_punctuation_out() {
        let result = parse("See (https://example.com/path?q=one), now `https://code.test`.");
        let links: Vec<_> = result
            .iter()
            .filter_map(|span| span.link.as_deref())
            .collect();
        assert_eq!(links, vec!["https://example.com/path?q=one"]);
        assert!(result
            .iter()
            .any(|span| span.text == "), now " && span.link.is_none()));
        assert!(result.iter().any(|span| {
            span.text == "https://code.test" && span.style.code && span.link.is_none()
        }));
    }

    #[test]
    fn refuses_non_web_schemes_and_urls_with_embedded_credentials() {
        let result = parse("javascript://example.test http://user:pass@example.test");
        assert!(result.iter().all(|span| span.link.is_none()));
    }

    #[test]
    fn finds_links_after_non_link_text_with_url_like_characters() {
        let result = parse("foohttps://invalid.test then https://valid.test");
        assert_eq!(
            result
                .iter()
                .filter_map(|span| span.link.as_deref())
                .collect::<Vec<_>>(),
            vec!["https://valid.test"]
        );
    }

    #[test]
    fn markdown_link_displays_only_its_label_and_keeps_link_inside_bold_text() {
        let result = parse("**[Relay](https://example.com)**");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].text, "Relay");
        assert!(result[0].style.bold);
        assert_eq!(result[0].link.as_deref(), Some("https://example.com"));
    }

    #[test]
    fn invalid_markdown_link_stays_visible_and_does_not_create_a_target() {
        let result = parse("[unsafe](javascript://example.test)");
        assert_eq!(
            result
                .iter()
                .map(|span| span.text.as_str())
                .collect::<String>(),
            "[unsafe](javascript://example.test)"
        );
        assert!(result.iter().all(|span| span.link.is_none()));
    }
}
