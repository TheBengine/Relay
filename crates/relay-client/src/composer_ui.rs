//! Keep writing central; secondary actions move into the add menu in narrow chats.
use super::*;

fn lock_completion_focus(ctx: &egui::Context, editor_id: egui::Id) {
    // Focus navigation reads raw Tab before completion consumes InputState events.
    // Keep it with the editor while a popup offers Tab as a selection shortcut.
    ctx.memory_mut(|memory| {
        memory.set_focus_lock_filter(
            editor_id,
            egui::EventFilter {
                tab: true,
                horizontal_arrows: true,
                vertical_arrows: true,
                ..Default::default()
            },
        );
    });
}

fn username_character(ch: char) -> bool {
    ch.is_alphanumeric() || matches!(ch, '_' | '-' | '.')
}

fn mention_start(input: &str, at: usize) -> bool {
    if input[..at]
        .chars()
        .next_back()
        .is_some_and(|ch| !ch.is_whitespace() && !",.!?():;[]{}*~|>".contains(ch))
    {
        return false;
    }
    let mut cursor = 0;
    let mut fence = 0;
    while cursor < at {
        let tail = &input[cursor..];
        if tail.starts_with('\\') {
            cursor += 1;
            if let Some(ch) = input[cursor..].chars().next() {
                cursor += ch.len_utf8();
            }
        } else if tail.starts_with('`') {
            let count = tail.bytes().take_while(|ch| *ch == b'`').count();
            if fence == 0 {
                fence = count;
            } else if fence == count {
                fence = 0;
            }
            cursor += count;
        } else if fence == 0 && (tail.starts_with("https://") || tail.starts_with("http://")) {
            cursor += tail.find(char::is_whitespace).unwrap_or(tail.len());
        } else {
            cursor += tail.chars().next().unwrap().len_utf8();
        }
        if cursor > at {
            return false;
        }
    }
    fence == 0
}

fn mention_query(input: &str, character_cursor: usize) -> Option<(std::ops::Range<usize>, String)> {
    let end = input
        .char_indices()
        .nth(character_cursor)
        .map_or(input.len(), |(byte, _)| byte);
    let start = input[..end].rfind('@')?;
    let query = &input[start + 1..end];
    if query.chars().count() > 32
        || !query.chars().all(username_character)
        || !mention_start(input, start)
    {
        return None;
    }
    let token_end = end
        + input[end..]
            .find(|ch: char| !username_character(ch))
            .unwrap_or(input.len() - end);
    Some((start..token_end, query.to_owned()))
}

fn replace_mention(
    draft: &mut String,
    mut range: std::ops::Range<usize>,
    username: &str,
) -> Option<usize> {
    if !draft.is_char_boundary(range.start)
        || !draft.is_char_boundary(range.end)
        || range.start > range.end
    {
        return None;
    }
    if !username.ends_with('.') {
        while range.end > range.start && draft[..range.end].ends_with('.') {
            range.end -= 1;
        }
    }
    let next = draft[range.end..].chars().next();
    let space = next.is_none_or(|ch| !ch.is_whitespace() && !",.!?():;[]{}*~|>".contains(ch));
    let value = format!("@{username}{}", if space { " " } else { "" });
    if draft.chars().count() - draft[range.clone()].chars().count() + value.chars().count() > 4000 {
        return None;
    }
    let cursor = draft[..range.start].chars().count() + value.chars().count();
    draft.replace_range(range, &value);
    Some(cursor)
}

// The transport still uses the existing encrypted mention-ID field. Exact names
// win before case folding, so Unicode case collisions cannot ping both people.
pub(super) fn mentioned_users(input: &str, members: &[User]) -> Vec<String> {
    let mut ids = HashSet::new();
    for span in crate::message_format::parse(input) {
        if span.style.code || span.link.is_some() {
            continue;
        }
        let input = span.text.as_str();
        for (start, ch) in input.char_indices() {
            if ch != '@' || !mention_start(input, start) {
                continue;
            }
            let tail = &input[start + 1..];
            let length = tail
                .find(|ch: char| !username_character(ch))
                .unwrap_or(tail.len());
            let name = &tail[..length];
            let select = |name: &str| {
                if name.is_empty() {
                    return None;
                }
                let exact: Vec<_> = members.iter().filter(|m| m.username == name).collect();
                if exact.len() == 1 {
                    return Some(exact[0]);
                }
                let lower = name.to_lowercase();
                let folded: Vec<_> = members
                    .iter()
                    .filter(|m| m.username.to_lowercase() == lower)
                    .collect();
                (folded.len() == 1).then(|| folded[0])
            };
            if !name.is_empty() {
                if let Some(member) = select(name).or_else(|| select(name.trim_end_matches('.'))) {
                    ids.insert(member.id.as_str());
                }
            }
        }
    }
    members
        .iter()
        .filter(|m| ids.remove(m.id.as_str()))
        .map(|m| m.id.clone())
        .collect()
}

fn manual_mention_text(input: &str, range: std::ops::Range<usize>, username: &str) -> String {
    let cursor = input
        .char_indices()
        .nth(range.start)
        .map_or(input.len(), |(byte, _)| byte);
    let separator = if mention_start(input, cursor) {
        ""
    } else {
        " "
    };
    let next = input.chars().nth(range.end);
    let space = next.is_none_or(|ch| !ch.is_whitespace() && !",.!?():;[]{}*~|>".contains(ch));
    format!("{separator}@{username}{}", if space { " " } else { "" })
}

fn mention_choices(bootstrap: &Bootstrap, target: &Target, query: &str) -> Vec<User> {
    if !target_available(target, bootstrap) {
        return Vec::new();
    }
    let query = query.to_lowercase();
    let group = match target {
        Target::Channel(id) => bootstrap
            .conversations
            .iter()
            .find(|c| c.channel_id == *id && c.kind == "group"),
        Target::Direct(_) => None,
    };
    let mut members: Vec<_> = bootstrap
        .members
        .iter()
        .filter(|m| {
            m.id != bootstrap.user.id
                && !bootstrap.social.blocked.contains(&m.id)
                && !m.username.is_empty()
                && m.username.chars().all(username_character)
                && match target {
                    Target::Direct(id) => m.id == *id,
                    Target::Channel(_) => group.is_none_or(|g| {
                        g.member_ids.contains(&bootstrap.user.id) && g.member_ids.contains(&m.id)
                    }),
                }
                && (m.username.to_lowercase().contains(&query)
                    || profile_ui::display_name(m).to_lowercase().contains(&query))
        })
        .collect();
    members.sort_by_cached_key(|m| {
        let name = m.username.to_lowercase();
        let display = profile_ui::display_name(m).to_lowercase();
        (
            name != query,
            !name.starts_with(&query),
            !display.starts_with(&query),
            name,
            m.id.clone(),
        )
    });
    let mut ids = HashSet::new();
    members
        .into_iter()
        .filter(|m| ids.insert(&m.id))
        .take(6)
        .cloned()
        .collect()
}

fn consume_plain_key(input: &mut egui::InputState, key: egui::Key) -> bool {
    let mut consumed = false;
    input.events.retain(|event| {
        let matches = matches!(event, egui::Event::Key { key: event_key, pressed: true, modifiers, .. }
            if *event_key == key && *modifiers == egui::Modifiers::NONE);
        consumed |= matches && (matches!(event, egui::Event::Key { repeat: false, .. })
            || matches!(key, egui::Key::ArrowUp | egui::Key::ArrowDown));
        !matches
    });
    consumed
}

fn completion_action(
    input: &mut egui::InputState,
    selected: &mut usize,
    count: usize,
) -> Option<Option<usize>> {
    if count == 0 {
        return None;
    }
    if consume_plain_key(input, egui::Key::Escape) {
        return Some(None);
    }
    if consume_plain_key(input, egui::Key::ArrowDown) {
        *selected = (*selected + 1) % count;
    }
    if consume_plain_key(input, egui::Key::ArrowUp) {
        *selected = (*selected + count - 1) % count;
    }
    if consume_plain_key(input, egui::Key::Tab) || consume_plain_key(input, egui::Key::Enter) {
        return Some(Some(*selected));
    }
    None
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum EmojiCompletion {
    Unicode(usize),
    Community { id: String, name: String },
}

fn expand_message_shortcodes(input: &str, assets: &[MediaAsset]) -> String {
    if assets.is_empty() {
        return crate::emoji::expand_shortcodes(input);
    }
    crate::emoji::expand_shortcodes_with(input, |name| {
        let mut matches = assets.iter().filter_map(|asset| {
            if asset.kind != "emoji" || !asset.name.eq_ignore_ascii_case(name) {
                return None;
            }
            let token = format!("<emoji:{}:{}>", asset.name, asset.id);
            (crate::message_format::expression_token_length(&token) == Some(token.len()))
                .then_some(token)
        });
        let token = matches.next()?;
        // Never choose an arbitrary image for an ambiguous community name.
        matches.all(|other| other == token).then_some(token)
    })
}

fn emoji_completion_choices(query: &str, assets: &[MediaAsset]) -> Vec<EmojiCompletion> {
    let query = query.to_ascii_lowercase();
    let catalog = crate::emoji::catalog();
    let mut matches = Vec::with_capacity(6);
    if let Some(index) = catalog.shortcode(&query) {
        matches.push(EmojiCompletion::Unicode(index));
    }
    let mut custom: Vec<_> = assets
        .iter()
        .filter(|asset| {
            asset.kind == "emoji"
                && asset.name.to_ascii_lowercase().contains(&query)
                && crate::message_format::expression_token_length(&format!(
                    "<emoji:{}:{}>",
                    asset.name, asset.id
                ))
                .is_some()
        })
        .collect();
    custom.sort_by_key(|asset| {
        let name = asset.name.to_ascii_lowercase();
        (name != query, !name.starts_with(&query), name)
    });
    for asset in custom {
        let choice = EmojiCompletion::Community {
            id: asset.id.clone(),
            name: asset.name.clone(),
        };
        if !matches.contains(&choice) {
            matches.push(choice);
        }
        if matches.len() == 6 {
            return matches;
        }
    }
    for index in catalog.search(&query, None, true) {
        let choice = EmojiCompletion::Unicode(index);
        if !matches.contains(&choice) {
            matches.push(choice);
        }
        if matches.len() == 6 {
            break;
        }
    }
    matches
}

fn replace_composer_selection(
    draft: &mut String,
    range: Option<std::ops::Range<usize>>,
    value: &str,
) -> usize {
    let count = draft.chars().count();
    let range = range.unwrap_or(count..count);
    let start = range.start.min(count);
    let end = range.end.max(start).min(count);
    let byte = |index| {
        draft
            .char_indices()
            .nth(index)
            .map_or(draft.len(), |(byte, _)| byte)
    };
    draft.replace_range(byte(start)..byte(end), value);
    start + value.chars().count()
}

impl RelayApp {
    pub(super) fn expand_message_shortcodes(&self, input: &str) -> String {
        if !self.prefs.appearance.convert_emoji_shortcodes {
            return input.to_owned();
        }
        let assets = self
            .bootstrap
            .as_ref()
            .map(|bootstrap| bootstrap.assets.as_slice())
            .unwrap_or_default();
        expand_message_shortcodes(input, assets)
    }

    pub(super) fn insert_composer_text(&mut self, ctx: &egui::Context, value: &str) {
        let editor_id = egui::Id::new(("composer-editor", self.current.clone()));
        let mut state = egui::TextEdit::load_state(ctx, editor_id).unwrap_or_default();
        let range = state.cursor.char_range().map(|range| {
            range.primary.index.min(range.secondary.index)
                ..range.primary.index.max(range.secondary.index)
        });
        let cursor = replace_composer_selection(&mut self.draft, range, value);
        state
            .cursor
            .set_char_range(Some(egui::text::CCursorRange::one(
                egui::text::CCursor::new(cursor),
            )));
        state.store(ctx, editor_id);
        ctx.memory_mut(|memory| memory.request_focus(editor_id));
        self.defer_preferences();
    }

    fn composer_emoji_keys(&mut self, ui: &mut egui::Ui, editor_id: egui::Id) {
        if !ui.memory(|memory| memory.has_focus(editor_id)) {
            return;
        }
        let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), editor_id) else {
            return;
        };
        let Some(range) = state.cursor.char_range() else {
            return;
        };
        if range.primary.index != range.secondary.index {
            return;
        }
        let custom = if self.draft.contains("<emoji:") {
            self.inline_community_emoji(&self.draft.clone(), true)
        } else {
            Vec::new()
        };
        for (key, previous, remove) in [
            (egui::Key::Backspace, true, true),
            (egui::Key::Delete, false, true),
            (egui::Key::ArrowLeft, true, false),
            (egui::Key::ArrowRight, false, false),
        ] {
            if let Some((bytes, chars)) = crate::emoji::edit_cluster_with_custom(
                &self.draft,
                range.primary.index,
                previous,
                &custom,
            ) {
                if ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, key)) {
                    let cursor = if remove || previous {
                        chars.start
                    } else {
                        chars.end
                    };
                    if remove {
                        self.draft.replace_range(bytes, "");
                        self.defer_preferences();
                    }
                    state
                        .cursor
                        .set_char_range(Some(egui::text::CCursorRange::one(
                            egui::text::CCursor::new(cursor),
                        )));
                    state.store(ui.ctx(), editor_id);
                    break;
                }
            }
        }
    }

    fn format_composer(
        &mut self,
        ctx: &egui::Context,
        editor_id: egui::Id,
        prefix: &str,
        suffix: &str,
    ) {
        let mut state = egui::TextEdit::load_state(ctx, editor_id).unwrap_or_default();
        let cursor = state
            .cursor
            .char_range()
            .map(|range| {
                (
                    range.primary.index.min(range.secondary.index),
                    range.primary.index.max(range.secondary.index),
                )
            })
            .unwrap_or_else(|| {
                let end = self.draft.chars().count();
                (end, end)
            });
        let byte = |index| {
            self.draft
                .char_indices()
                .nth(index)
                .map_or(self.draft.len(), |(byte, _)| byte)
        };
        let (start, end) = (byte(cursor.0), byte(cursor.1));
        let selected = self.draft[start..end].to_owned();
        self.draft
            .replace_range(start..end, &format!("{prefix}{selected}{suffix}"));
        let first = cursor.0 + prefix.chars().count();
        let last = first + selected.chars().count();
        state
            .cursor
            .set_char_range(Some(egui::text::CCursorRange::two(
                egui::text::CCursor::new(first),
                egui::text::CCursor::new(last),
            )));
        state.store(ctx, editor_id);
        ctx.memory_mut(|memory| memory.request_focus(editor_id));
        self.defer_preferences();
    }

    fn composer_format_keys(&mut self, ui: &mut egui::Ui, editor_id: egui::Id) {
        if !ui.memory(|memory| memory.has_focus(editor_id)) {
            return;
        }
        for (key, marker) in [
            (egui::Key::B, "**"),
            (egui::Key::I, "*"),
            (egui::Key::U, "__"),
        ] {
            if ui.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, key)) {
                self.format_composer(ui.ctx(), editor_id, marker, marker);
            }
        }
    }

    fn formatting_menu(&mut self, ui: &mut egui::Ui, editor_id: egui::Id) {
        for (name, prefix, suffix) in [
            ("Bold · Ctrl+B", "**", "**"),
            ("Italic · Ctrl+I", "*", "*"),
            ("Underline · Ctrl+U", "__", "__"),
            ("Strikethrough", "~~", "~~"),
            ("Spoiler", "||", "||"),
            ("Inline code", "`", "`"),
            ("Code block", "\n```\n", "\n```\n"),
            ("Heading", "\n# ", "\n"),
            ("Quote", "\n> ", "\n"),
            ("List", "\n- ", "\n"),
            ("Small text", "\n-# ", "\n"),
        ] {
            if ui.button(name).clicked() {
                self.format_composer(ui.ctx(), editor_id, prefix, suffix);
                ui.close_menu();
            }
        }
    }

    pub(super) fn dismiss_emoji_completion(&mut self, ctx: &egui::Context) {
        let editor_id = egui::Id::new(("composer-editor", self.current.clone()));
        let picker_id = egui::Id::new("manual-mention-editor");
        if let Some(active) = ctx.data(|data| data.get_temp::<egui::Id>(picker_id)) {
            if active != editor_id
                || ctx.input_mut(|input| consume_plain_key(input, egui::Key::Escape))
            {
                ctx.data_mut(|data| {
                    data.remove::<egui::Id>(picker_id);
                    data.remove::<String>(active.with("mention-menu-query"));
                });
                return;
            }
        }
        if !ctx.memory(|memory| memory.has_focus(editor_id)) {
            return;
        }
        if let Some((range, query, _)) = self.mention_completion(ctx, editor_id) {
            let id = editor_id
                .with(("mention-completion", range.start, query))
                .with("dismissed");
            if !ctx.data(|data| data.get_temp::<bool>(id).unwrap_or(false))
                && ctx
                    .input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
            {
                ctx.data_mut(|data| data.insert_temp(id, true));
            }
        }
        if let Some((_, query, _)) = self.emoji_completion(ctx, editor_id) {
            let id = editor_id
                .with(("emoji-completion", query))
                .with("dismissed");
            if !ctx.data(|data| data.get_temp::<bool>(id).unwrap_or(false))
                && ctx
                    .input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
            {
                ctx.data_mut(|data| data.insert_temp(id, true));
            }
        }
    }

    fn emoji_completion(
        &self,
        ctx: &egui::Context,
        editor_id: egui::Id,
    ) -> Option<(std::ops::Range<usize>, String, Vec<EmojiCompletion>)> {
        if !self.prefs.appearance.emoji_suggestions {
            return None;
        }
        let state = egui::TextEdit::load_state(ctx, editor_id)?;
        let cursor = state.cursor.char_range()?;
        if cursor.primary.index != cursor.secondary.index {
            return None;
        }
        let (range, query) = crate::emoji::completion(&self.draft, cursor.primary.index)?;
        let assets = self
            .bootstrap
            .as_ref()
            .map_or(&[][..], |b| b.assets.as_slice());
        let matches = emoji_completion_choices(&query, assets);
        (!matches.is_empty()).then_some((range, query, matches))
    }

    fn insert_completion(
        &mut self,
        ctx: &egui::Context,
        editor_id: egui::Id,
        range: std::ops::Range<usize>,
        choice: EmojiCompletion,
    ) {
        let value = match &choice {
            EmojiCompletion::Unicode(index) => {
                crate::emoji::catalog().entries[*index].unicode.clone()
            }
            EmojiCompletion::Community { id, name } => {
                if !self.bootstrap.as_ref().is_some_and(|b| {
                    b.assets.iter().any(|asset| {
                        asset.kind == "emoji" && asset.id == *id && asset.name == *name
                    })
                }) {
                    return;
                }
                format!("<emoji:{name}:{id}>")
            }
        };
        let cursor = self.draft[..range.start].chars().count() + value.chars().count();
        self.draft.replace_range(range, &value);
        if let Some(mut state) = egui::TextEdit::load_state(ctx, editor_id) {
            state
                .cursor
                .set_char_range(Some(egui::text::CCursorRange::one(
                    egui::text::CCursor::new(cursor),
                )));
            state.store(ctx, editor_id);
        }
        if let EmojiCompletion::Unicode(index) = choice {
            self.remember_emoji(index);
        }
        self.defer_preferences();
        ctx.memory_mut(|memory| memory.request_focus(editor_id));
    }

    fn emoji_completion_keys(&mut self, ui: &mut egui::Ui, editor_id: egui::Id) {
        if !ui.memory(|memory| memory.has_focus(editor_id)) {
            return;
        }
        let Some((range, query, matches)) = self.emoji_completion(ui.ctx(), editor_id) else {
            return;
        };
        let id = editor_id.with(("emoji-completion", &query));
        let mut selected = ui
            .data(|data| data.get_temp::<usize>(id).unwrap_or(0))
            .min(matches.len() - 1);
        if ui.data(|data| data.get_temp::<bool>(id.with("dismissed")).unwrap_or(false)) {
            return;
        }
        let chosen = ui.input_mut(|input| completion_action(input, &mut selected, matches.len()));
        ui.data_mut(|data| data.insert_temp(id, selected));
        match chosen {
            Some(Some(index)) => {
                self.insert_completion(ui.ctx(), editor_id, range, matches[index].clone())
            }
            Some(None) => ui.data_mut(|data| data.insert_temp(id.with("dismissed"), true)),
            None => {}
        }
    }

    fn mention_completion(
        &self,
        ctx: &egui::Context,
        editor_id: egui::Id,
    ) -> Option<(std::ops::Range<usize>, String, Vec<User>)> {
        let completion = (|| {
            if !self.prefs.appearance.mention_suggestions {
                return None;
            }
            let state = egui::TextEdit::load_state(ctx, editor_id)?;
            let cursor = state.cursor.char_range()?;
            if cursor.primary.index != cursor.secondary.index {
                return None;
            }
            let (range, query) = mention_query(&self.draft, cursor.primary.index)?;
            let choices = mention_choices(self.bootstrap.as_ref()?, self.current.as_ref()?, &query);
            (!choices.is_empty()).then_some((range, query, choices))
        })();
        let identity = completion
            .as_ref()
            .map(|(range, query, _)| (range.start, query.clone()));
        ctx.data_mut(|data| {
            let last = editor_id.with("mention-last-query");
            if data.get_temp::<Option<(usize, String)>>(last) != Some(identity.clone()) {
                if let Some((start, query)) = &identity {
                    data.remove::<bool>(
                        editor_id
                            .with(("mention-completion", start, query))
                            .with("dismissed"),
                    );
                }
                data.insert_temp(last, identity);
            }
        });
        completion
    }

    fn insert_mention_completion(
        &mut self,
        ctx: &egui::Context,
        editor_id: egui::Id,
        range: std::ops::Range<usize>,
        choice: &User,
    ) {
        if !self
            .bootstrap
            .as_ref()
            .zip(self.current.as_ref())
            .is_some_and(|(b, target)| {
                mention_choices(b, target, &choice.username)
                    .iter()
                    .any(|m| m.id == choice.id && m.username == choice.username)
            })
        {
            return;
        }
        let Some(cursor) = replace_mention(&mut self.draft, range, &choice.username) else {
            return;
        };
        if let Some(mut state) = egui::TextEdit::load_state(ctx, editor_id) {
            state
                .cursor
                .set_char_range(Some(egui::text::CCursorRange::one(
                    egui::text::CCursor::new(cursor),
                )));
            state.store(ctx, editor_id);
        }
        self.defer_preferences();
        ctx.memory_mut(|memory| memory.request_focus(editor_id));
    }

    fn mention_completion_keys(&mut self, ui: &mut egui::Ui, editor_id: egui::Id) {
        if !ui.memory(|memory| memory.has_focus(editor_id)) {
            return;
        }
        let Some((range, query, choices)) = self.mention_completion(ui.ctx(), editor_id) else {
            return;
        };
        let id = editor_id.with(("mention-completion", range.start, &query));
        if ui.data(|data| data.get_temp::<bool>(id.with("dismissed")).unwrap_or(false)) {
            return;
        }
        let mut selected = ui
            .data(|data| data.get_temp::<usize>(id).unwrap_or(0))
            .min(choices.len() - 1);
        let before = selected;
        let action = ui.input_mut(|input| completion_action(input, &mut selected, choices.len()));
        ui.data_mut(|data| {
            data.insert_temp(id, selected);
            if before != selected {
                data.insert_temp(id.with("reveal-selection"), true);
            }
        });
        match action {
            Some(Some(index)) => {
                self.insert_mention_completion(ui.ctx(), editor_id, range, &choices[index])
            }
            Some(None) => ui.data_mut(|data| data.insert_temp(id.with("dismissed"), true)),
            None => {}
        }
    }

    fn mention_completion_popup(&mut self, ui: &mut egui::Ui, editor_id: egui::Id, rect: Rect) {
        if !ui.memory(|memory| memory.has_focus(editor_id)) {
            return;
        }
        let Some((range, query, choices)) = self.mention_completion(ui.ctx(), editor_id) else {
            return;
        };
        let id = editor_id.with(("mention-completion", range.start, &query));
        if ui.data(|data| data.get_temp::<bool>(id.with("dismissed")).unwrap_or(false)) {
            return;
        }
        lock_completion_focus(ui.ctx(), editor_id);
        let selected = ui
            .data(|data| data.get_temp::<usize>(id).unwrap_or(0))
            .min(choices.len() - 1);
        let reveal = ui.data_mut(|data| {
            data.remove_temp::<bool>(id.with("reveal-selection"))
                .unwrap_or(false)
        });
        let screen = ui.ctx().screen_rect();
        let width = rect
            .width()
            .clamp(180.0, 340.0)
            .min((screen.width() - 24.0).max(1.0));
        let height =
            (rect.top() - screen.top() - font_size(TEXT_CAPTION) * 3.0 - 40.0).clamp(32.0, 240.0);
        let anchor = Pos2::new(
            rect.left().clamp(
                screen.left() + 8.0,
                (screen.right() - width - 16.0).max(screen.left() + 8.0),
            ),
            rect.top() - 6.0,
        );
        let mut chosen = None;
        egui::Area::new(editor_id.with("mention-popup"))
            .order(egui::Order::Foreground)
            .fixed_pos(anchor)
            .pivot(Align2::LEFT_BOTTOM)
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_width(width);
                    ui.label(
                        RichText::new("People · ↑ ↓ · Tab / Enter · Esc")
                            .size(font_size(TEXT_CAPTION))
                            .color(c_muted()),
                    );
                    egui::ScrollArea::vertical()
                        .id_source(id.with("results"))
                        .max_height(height)
                        .show(ui, |ui| {
                            for (row, person) in choices.iter().enumerate() {
                                let response = self.mention_person_row(ui, person, row == selected);
                                if reveal && row == selected {
                                    response.scroll_to_me(Some(Align::Center));
                                }
                                if response.clicked() {
                                    chosen = Some(person.clone());
                                }
                            }
                        });
                });
            });
        if let Some(person) = chosen {
            self.insert_mention_completion(ui.ctx(), editor_id, range, &person);
        }
    }

    fn mention_person_row(
        &mut self,
        ui: &mut egui::Ui,
        person: &User,
        selected: bool,
    ) -> egui::Response {
        let width = ui.available_width();
        let text_width = (width - 48.0).max(1.0);
        let name = crate::settings_ui::single_line(
            ui,
            profile_ui::display_name(person),
            14.0,
            c_text(),
            text_width,
        );
        let username = crate::settings_ui::single_line(
            ui,
            &format!("@{}", person.username),
            11.0,
            c_muted(),
            text_width,
        );
        let height = (name.size().y + username.size().y + 8.0).max(40.0);
        let (rect, response) = ui.allocate_exact_size(Vec2::new(width, height), Sense::click());
        if selected || response.hovered() || response.has_focus() {
            ui.painter().rect_filled(
                rect,
                6.0,
                if selected {
                    c_accent().gamma_multiply(0.22)
                } else {
                    c_card()
                },
            );
        }
        let texture = self.avatar_texture(person);
        profile_ui::paint_user_avatar(
            ui,
            Rect::from_center_size(
                Pos2::new(rect.left() + 20.0, rect.center().y),
                Vec2::splat(28.0),
            ),
            person,
            texture.as_ref(),
            false,
        );
        ui.painter().galley(
            Pos2::new(rect.left() + 40.0, rect.top() + 4.0),
            name.clone(),
            c_text(),
        );
        ui.painter().galley(
            Pos2::new(rect.left() + 40.0, rect.top() + 4.0 + name.size().y),
            username,
            c_muted(),
        );
        let label = format!(
            "{} (@{})",
            profile_ui::display_name(person),
            person.username
        );
        response.widget_info(|| {
            egui::WidgetInfo::selected(
                egui::WidgetType::SelectableLabel,
                selected,
                format!("Mention {label}"),
            )
        });
        response.on_hover_text(label)
    }

    fn emoji_completion_popup(&mut self, ui: &mut egui::Ui, editor_id: egui::Id, rect: Rect) {
        if !ui.memory(|memory| memory.has_focus(editor_id)) {
            return;
        }
        let Some((range, query, matches)) = self.emoji_completion(ui.ctx(), editor_id) else {
            return;
        };
        let id = editor_id.with(("emoji-completion", &query));
        if ui.data(|data| data.get_temp::<bool>(id.with("dismissed")).unwrap_or(false)) {
            return;
        }
        lock_completion_focus(ui.ctx(), editor_id);
        let selected = ui
            .data(|data| data.get_temp::<usize>(id).unwrap_or(0))
            .min(matches.len() - 1);
        let mut chosen = None;
        egui::Area::new(editor_id.with("emoji-popup"))
            .order(egui::Order::Foreground)
            .fixed_pos(rect.left_top() - Vec2::new(0.0, 8.0))
            .pivot(Align2::LEFT_BOTTOM)
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_width(rect.width().clamp(200.0, 340.0));
                    ui.label(
                        RichText::new("Emoji · ↑ ↓ to choose · Tab / Enter to insert")
                            .size(font_size(TEXT_CAPTION))
                            .color(c_muted()),
                    );
                    for (row, choice) in matches.iter().enumerate() {
                        ui.horizontal(|ui| {
                            let (label, clicked) = match choice {
                                EmojiCompletion::Unicode(index) => (
                                    format!(
                                        ":{}:",
                                        crate::emoji::catalog().entries[*index].aliases[0]
                                    ),
                                    crate::emoji::button(ui, *index, 24.0).clicked(),
                                ),
                                EmojiCompletion::Community { name, .. } => {
                                    let (rect, response) = ui.allocate_exact_size(
                                        Vec2::splat(24.0),
                                        egui::Sense::click(),
                                    );
                                    response.widget_info(|| {
                                        egui::WidgetInfo::labeled(
                                            egui::WidgetType::Button,
                                            format!("Insert community emoji :{name}:"),
                                        )
                                    });
                                    if let Some(texture) = self.asset_texture("emoji", name) {
                                        ui.painter().image(
                                            texture.id(),
                                            rect,
                                            Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                                            Color32::WHITE,
                                        );
                                    } else {
                                        ui.painter().text(
                                            rect.center(),
                                            Align2::CENTER_CENTER,
                                            ":",
                                            FontId::proportional(font_size(18.0)),
                                            c_muted(),
                                        );
                                    }
                                    (format!(":{name}: · Server emoji"), response.clicked())
                                }
                            };
                            if clicked {
                                chosen = Some(choice.clone());
                            }
                            let text = crate::settings_ui::single_line(
                                ui,
                                &label,
                                14.0,
                                c_text(),
                                ui.available_width(),
                            );
                            let response = ui
                                .selectable_label(row == selected, text)
                                .on_hover_text(&label);
                            response.widget_info(|| {
                                egui::WidgetInfo::selected(
                                    egui::WidgetType::SelectableLabel,
                                    row == selected,
                                    &label,
                                )
                            });
                            if response.clicked() {
                                chosen = Some(choice.clone());
                            }
                        });
                    }
                });
            });
        if let Some(index) = chosen {
            self.insert_completion(ui.ctx(), editor_id, range, index);
        }
    }

    fn manual_mention_popup(&mut self, ui: &mut egui::Ui, editor_id: egui::Id, rect: Rect) {
        let picker_id = egui::Id::new("manual-mention-editor");
        if ui.data(|data| data.get_temp::<egui::Id>(picker_id)) != Some(editor_id) {
            return;
        }
        let opened = ui.data_mut(|data| {
            data.remove_temp::<bool>(editor_id.with("mention-menu-focus"))
                .unwrap_or(false)
        });
        let screen = ui.ctx().screen_rect();
        let width = rect
            .width()
            .clamp(180.0, 340.0)
            .min((screen.width() - 24.0).max(1.0));
        let height = (rect.top() - screen.top() - font_size(14.0) * 4.0 - 48.0).clamp(32.0, 200.0);
        let anchor = Pos2::new(
            rect.left().clamp(
                screen.left() + 8.0,
                (screen.right() - width - 16.0).max(screen.left() + 8.0),
            ),
            rect.top() - 6.0,
        );
        let response = egui::Area::new(editor_id.with("manual-mention-popup"))
            .order(egui::Order::Foreground)
            .fixed_pos(anchor)
            .pivot(Align2::LEFT_BOTTOM)
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_width(width);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Mention").color(c_text()));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("Close").clicked() {
                                ui.data_mut(|data| data.remove::<egui::Id>(picker_id));
                            }
                        });
                    });
                    self.mention_menu(ui, editor_id, height, opened);
                });
            });
        if !opened
            && ui.input(|input| {
                input.pointer.any_pressed()
                    && input
                        .pointer
                        .interact_pos()
                        .is_some_and(|pos| !response.response.rect.contains(pos))
            })
        {
            ui.data_mut(|data| data.remove::<egui::Id>(picker_id));
        }
        if ui.data(|data| data.get_temp::<egui::Id>(picker_id)) != Some(editor_id) {
            ui.data_mut(|data| data.remove::<String>(editor_id.with("mention-menu-query")));
        }
    }

    fn mention_menu(&mut self, ui: &mut egui::Ui, editor_id: egui::Id, height: f32, focus: bool) {
        let query_id = editor_id.with("mention-menu-query");
        let mut query = ui.data(|data| data.get_temp::<String>(query_id).unwrap_or_default());
        let response = ui.add(
            egui::TextEdit::singleline(&mut query)
                .hint_text("Find a person")
                .char_limit(32)
                .desired_width(260.0),
        );
        ui.ctx().accesskit_node_builder(response.id, |builder| {
            builder.set_name("Find people to mention")
        });
        if focus {
            response.request_focus();
        }
        ui.data_mut(|data| data.insert_temp(query_id, query.clone()));
        let choices = self
            .bootstrap
            .as_ref()
            .zip(self.current.as_ref())
            .map(|(b, target)| mention_choices(b, target, &query))
            .unwrap_or_default();
        if choices.is_empty() {
            ui.label("No people match this name.");
        }
        egui::ScrollArea::vertical()
            .max_height(height)
            .show(ui, |ui| {
                for member in choices {
                    let response = self.mention_person_row(ui, &member, false);
                    if response.clicked() {
                        let range = egui::TextEdit::load_state(ui.ctx(), editor_id)
                            .and_then(|state| state.cursor.char_range())
                            .map_or(
                                self.draft.chars().count()..self.draft.chars().count(),
                                |range| {
                                    range.primary.index.min(range.secondary.index)
                                        ..range.primary.index.max(range.secondary.index)
                                },
                            );
                        let value = manual_mention_text(&self.draft, range, &member.username);
                        self.insert_composer_text(ui.ctx(), &value);
                        ui.data_mut(|data| data.remove::<String>(query_id));
                        ui.data_mut(|data| {
                            data.remove::<egui::Id>(egui::Id::new("manual-mention-editor"))
                        });
                        ui.close_menu();
                    }
                }
            });
    }

    pub(super) fn composer_bar(
        &mut self,
        ui: &mut egui::Ui,
        name: &str,
        pending: bool,
        cooling_down: bool,
    ) -> bool {
        let editor_id = egui::Id::new(("composer-editor", self.current.clone()));
        self.composer_format_keys(ui, editor_id);
        self.composer_emoji_keys(ui, editor_id);
        self.mention_completion_keys(ui, editor_id);
        self.emoji_completion_keys(ui, editor_id);
        let mut submit =
            ui.memory(|memory| memory.has_focus(editor_id)) && ui.input_mut(consume_send_key);
        let mut editor_rect = Rect::NOTHING;
        let composer = egui::Frame::none()
            .fill(c_card())
            .rounding(8.0)
            .inner_margin(Margin::symmetric(8.0, 6.0))
            .show(ui, |ui| {
                let expanded = ui.available_width() >= 360.0;
                let show_emoji = ui.available_width() >= 300.0;
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    ui.menu_button("+", |ui| {
                        if ui.button("Upload a file (up to 10 MB)").clicked() {
                            if let Some(target) = self.current.clone() {
                                if let Some(path) = rfd::FileDialog::new().pick_file() {
                                    self.start_upload(target, path);
                                }
                            }
                            ui.close_menu();
                        }
                        if ui.button("Create Poll").clicked() {
                            self.open_poll();
                            ui.close_menu();
                        }
                        if ui.button("Mention someone").clicked() {
                            ui.data_mut(|data| {
                                data.insert_temp(egui::Id::new("manual-mention-editor"), editor_id);
                                data.insert_temp(editor_id.with("mention-menu-focus"), true);
                            });
                            ui.close_menu();
                        }
                        ui.menu_button("Text formatting", |ui| self.formatting_menu(ui, editor_id));
                        if !expanded {
                            ui.separator();
                            if ui.button("GIF favorites & recent files").clicked() {
                                self.open_gif_picker();
                                ui.close_menu();
                            }
                            if ui.button("Stickers").clicked() {
                                self.open_sticker_picker();
                                ui.close_menu();
                            }
                        }
                        if !show_emoji && ui.button("Emoji").clicked() {
                            self.open_expression_picker();
                            ui.close_menu();
                        }
                    })
                    .response
                    .on_hover_text("Add files, polls, mentions and expressions");
                    // All trailing controls have fixed widths. Reserve them before the editor.
                    let trailing =
                        if expanded {
                            3
                        } else if show_emoji {
                            1
                        } else {
                            0
                        } + usize::from(pending || self.prefs.appearance.show_send_button);
                    let reserved = trailing as f32 * 34.0 + if expanded { 2.0 } else { 0.0 };
                    let height =
                        (self.draft.lines().count().clamp(1, 5) as f32 * font_size(21.0)).max(32.0);
                    let editor_inline = std::cell::RefCell::new(crate::emoji::Inline::default());
                    let custom = self.inline_community_emoji(&self.draft.clone(), true);
                    let mut layouter = |ui: &egui::Ui, text: &str, width: f32| {
                        let (job, inline) = if custom.is_empty() {
                            crate::emoji::editor_job(text, font_size(15.0), c_text(), width)
                        } else {
                            crate::emoji::editor_job_with_custom(
                                text,
                                font_size(15.0),
                                c_text(),
                                width,
                                &custom,
                            )
                        };
                        *editor_inline.borrow_mut() = inline;
                        ui.fonts(|fonts| fonts.layout_job(job))
                    };
                    let editor = ui.add_sized(
                        [(ui.available_width() - reserved).max(1.0), height],
                        |ui: &mut egui::Ui| {
                            let output = egui::TextEdit::multiline(&mut self.draft)
                                .id(editor_id)
                                .hint_text(format!(
                                    "Message {}{name}",
                                    if matches!(self.current, Some(Target::Direct(_))) {
                                        "@"
                                    } else {
                                        "#"
                                    }
                                ))
                                .frame(false)
                                .font(FontId::proportional(font_size(15.0)))
                                .desired_rows(1)
                                .char_limit(4000)
                                .margin(Vec2::new(5.0, 5.0))
                                .layouter(&mut layouter)
                                .show(ui);
                            editor_inline.borrow().paint_clipped(
                                ui,
                                &output.galley,
                                output.galley_pos,
                                output.text_clip_rect,
                            );
                            output.response
                        },
                    );
                    if editor.changed() {
                        self.defer_preferences();
                        if let Some(target) = self.current.clone() {
                            self.typing_activity = Some((target, Instant::now()));
                        }
                    }
                    editor_rect = editor.rect;
                    if expanded {
                        if ui
                            .add_sized(
                                [32.0, 30.0],
                                egui::Button::new(
                                    RichText::new("GIF")
                                        .color(c_muted())
                                        .size(font_size(TEXT_CAPTION)),
                                )
                                .fill(Color32::TRANSPARENT)
                                .stroke(Stroke::NONE),
                            )
                            .on_hover_text("GIF favorites and recent files")
                            .clicked()
                        {
                            self.open_gif_picker();
                        }
                        if icon_button(ui, Icon::Sticker, false, "Stickers").clicked() {
                            self.open_sticker_picker();
                        }
                    }
                    if show_emoji && icon_button(ui, Icon::Smile, false, "Emoji").clicked() {
                        self.open_expression_picker();
                    }
                    let ready = !pending
                        && !cooling_down
                        && (!self.draft.trim().is_empty() || !self.attachments.is_empty());
                    if pending {
                        ui.add_sized([30.0, 30.0], egui::Spinner::new())
                            .on_hover_text("Sending message…");
                    } else if self.prefs.appearance.show_send_button {
                        submit |= ui
                            .add_enabled_ui(ready, |ui| {
                                icon_button(
                                    ui,
                                    Icon::Send,
                                    false,
                                    "Send · Enter (Shift+Enter for a new line)",
                                )
                                .clicked()
                            })
                            .inner;
                    }
                });
            });
        self.composer_anchor = Some(composer.response.rect);
        self.emoji_completion_popup(ui, editor_id, editor_rect);
        self.mention_completion_popup(ui, editor_id, editor_rect);
        self.manual_mention_popup(ui, editor_id, editor_rect);
        submit
    }
}

#[cfg(test)]
mod mention_tests {
    use super::*;

    #[test]
    fn manual_picker_inserts_at_the_selection_with_a_valid_mention_boundary() {
        let people = [user("mira", "Mira", "")];
        for (initial, range, expected) in [
            ("😭", 1..1, "😭 @Mira "),
            ("hello world", 6..11, "hello @Mira "),
            ("hello", 5..5, "hello @Mira "),
            ("( tomorrow", 1..1, "(@Mira tomorrow"),
            ("()", 1..1, "(@Mira)"),
            ("æø", 2..2, "æø @Mira "),
        ] {
            let mut draft = initial.to_owned();
            let value = manual_mention_text(&draft, range.clone(), "Mira");
            replace_composer_selection(&mut draft, Some(range), &value);
            assert_eq!(draft, expected);
            assert_eq!(mentioned_users(&draft, &people), ["mira"]);
        }
    }

    fn user(id: &str, username: &str, display: &str) -> User {
        User {
            id: id.into(),
            username: username.into(),
            profile: UserProfile {
                display_name: display.into(),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn mention_query_uses_unicode_cursor_and_excludes_email_links_escapes_and_code() {
        let input = "😭 hi @Ås!";
        let (range, query) = mention_query(input, 8).unwrap();
        assert_eq!(&input[range], "@Ås");
        assert_eq!(query, "Ås");
        for input in [
            "mail@Mira",
            "https://example.com/?@Mira",
            "[link](https://example.com/@Mira",
            "` @Mira",
            "```rust\n@Mira",
            "\\@Mira",
            "hi @Mira hello",
        ] {
            assert!(
                mention_query(input, input.chars().count()).is_none(),
                "{input}"
            );
        }
        assert_eq!(mention_query("`code` (@Mi", 11).unwrap().1, "Mi");
        assert_eq!(mention_query("hi @", 4).unwrap().1, "");
        assert!(crate::emoji::completion("@Mira", 5).is_none());
        assert!(mention_query(":sob", 4).is_none());
    }

    #[test]
    fn mention_selection_replaces_whole_word_preserves_punctuation_and_bounds_length() {
        let mut input = "😭 hi @MiRa. next".to_owned();
        let (range, query) = mention_query(&input, 8).unwrap();
        assert_eq!(query, "Mi");
        let cursor = replace_mention(&mut input, range, "Mira").unwrap();
        assert_eq!(input, "😭 hi @Mira. next");
        assert_eq!(cursor, 10);
        let mut input = "@Mi".to_owned();
        assert_eq!(replace_mention(&mut input, 0..3, "Mira"), Some(6));
        assert_eq!(input, "@Mira ");
        let mut full = format!("{} @", "x".repeat(3998));
        let before = full.clone();
        assert!(replace_mention(&mut full, 3999..4000, "Mira").is_none());
        assert_eq!(full, before);
    }

    #[test]
    fn mention_candidates_respect_current_recipient_scope_blocks_and_display_name_search() {
        let mut b = Bootstrap {
            user: user("me", "Alex", "Alex"),
            members: vec![
                user("me", "Alex", "Alex"),
                user("mira", "Mira", "Friday Captain"),
                user("jonas", "Jonas", "Jonas"),
                user("blocked", "Miriam", "Miriam"),
            ],
            channels: vec![
                Channel {
                    id: "general".into(),
                    kind: "text".into(),
                    ..Default::default()
                },
                Channel {
                    id: "group".into(),
                    kind: "text".into(),
                    ..Default::default()
                },
            ],
            social: SocialState {
                blocked: vec!["blocked".into()],
                ..Default::default()
            },
            conversations: vec![ConversationInfo {
                channel_id: "group".into(),
                kind: "group".into(),
                member_ids: vec!["me".into(), "mira".into()],
                ..Default::default()
            }],
            ..Default::default()
        };
        let ids = |people: Vec<User>| people.into_iter().map(|m| m.id).collect::<Vec<_>>();
        assert_eq!(
            ids(mention_choices(
                &b,
                &Target::Channel("general".into()),
                "CAPTAIN"
            )),
            ["mira"]
        );
        assert_eq!(
            ids(mention_choices(
                &b,
                &Target::Channel("general".into()),
                "mi"
            )),
            ["mira"]
        );
        assert_eq!(
            ids(mention_choices(&b, &Target::Direct("jonas".into()), "")),
            ["jonas"]
        );
        assert_eq!(
            ids(mention_choices(&b, &Target::Channel("group".into()), "")),
            ["mira"]
        );
        assert!(mention_choices(&b, &Target::Channel("gone".into()), "").is_empty());
        b.conversations[0].member_ids.retain(|id| id != "me");
        assert!(mention_choices(&b, &Target::Channel("group".into()), "").is_empty());
        b.members
            .extend((0..20).map(|i| user(&format!("person{i}"), &format!("person{i}"), "")));
        assert_eq!(
            mention_choices(&b, &Target::Channel("general".into()), "").len(),
            6
        );
        assert_eq!(
            ids(mention_choices(
                &b,
                &Target::Channel("general".into()),
                "person19"
            ))[0],
            "person19"
        );
    }

    #[test]
    fn notification_ids_accept_dotted_names_without_code_url_or_case_collision_pings() {
        let people = [
            user("mira", "Mira", ""),
            user("dotted", "jane.doe", ""),
            user("upper", "Åse", ""),
            user("lower", "åse", ""),
        ];
        assert_eq!(
            mentioned_users("@MIRA, @jane.doe. @Mira @Miriam email@Mira", &people),
            ["mira", "dotted"]
        );
        assert!(mentioned_users(
            "` @Mira ` ```\n@jane.doe\n``` https://example.com/?@Mira \\@Mira @jane.doe.other",
            &people
        )
        .is_empty());
        assert_eq!(mentioned_users("@Åse", &people), ["upper"]);
        assert_eq!(mentioned_users("@åse", &people), ["lower"]);
        assert!(mentioned_users("@ÅSE", &people).is_empty());
    }

    #[test]
    fn formatted_mentions_keep_notification_ids_while_link_labels_stay_literal() {
        let people = [
            user("mira", "Mira", ""),
            user("dotted", "jane.doe", ""),
            user("underscores", "bob_one_two", ""),
        ];
        assert_eq!(
            mentioned_users("__@Mira__ **@jane.doe**", &people),
            ["mira", "dotted"]
        );
        assert!(mentioned_users("[@Mira](https://example.com) `@jane.doe`", &people).is_empty());
        assert_eq!(
            mentioned_users("@bob_one_two __@bob_one_two__ _@bob_one_two_", &people),
            ["underscores"]
        );
    }

    #[test]
    fn completion_consumes_enter_before_send_and_preserves_shift_enter() {
        for (key, modifiers, expected) in [
            (egui::Key::Enter, egui::Modifiers::NONE, Some(Some(0))),
            (egui::Key::Tab, egui::Modifiers::NONE, Some(Some(0))),
            (egui::Key::Escape, egui::Modifiers::NONE, Some(None)),
            (egui::Key::Enter, egui::Modifiers::SHIFT, None),
        ] {
            let ctx = egui::Context::default();
            let _ = ctx.run(
                egui::RawInput {
                    events: vec![egui::Event::Key {
                        key,
                        physical_key: None,
                        pressed: true,
                        repeat: false,
                        modifiers,
                    }],
                    ..Default::default()
                },
                |ctx| {
                    ctx.input_mut(|input| {
                        assert_eq!(completion_action(input, &mut 0, 2), expected);
                        assert!(!consume_send_key(input));
                        if modifiers.shift {
                            assert!(input.key_pressed(egui::Key::Enter));
                        }
                    });
                },
            );
        }
        let ctx = egui::Context::default();
        let _ = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::Key {
                    key: egui::Key::ArrowUp,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }],
                ..Default::default()
            },
            |ctx| {
                ctx.input_mut(|input| {
                    let mut selected = 0;
                    assert_eq!(completion_action(input, &mut selected, 2), None);
                    assert_eq!(selected, 1);
                });
            },
        );
    }

    #[test]
    fn held_enter_cannot_select_a_person_then_send_on_key_repeat() {
        for open in [false, true] {
            let ctx = egui::Context::default();
            // egui derives repeat from keys held across frames, rather than
            // trusting the RawInput repeat flag on a first key press.
            let _ = ctx.run(
                egui::RawInput {
                    events: vec![egui::Event::Key {
                        key: egui::Key::Enter,
                        physical_key: None,
                        pressed: true,
                        repeat: false,
                        modifiers: egui::Modifiers::NONE,
                    }],
                    ..Default::default()
                },
                |ctx| {
                    ctx.input_mut(|input| {
                        if open {
                            assert_eq!(completion_action(input, &mut 0, 2), Some(Some(0)));
                        } else {
                            assert!(consume_send_key(input));
                        }
                    });
                },
            );
            let _ = ctx.run(
                egui::RawInput {
                    events: vec![egui::Event::Key {
                        key: egui::Key::Enter,
                        physical_key: None,
                        pressed: true,
                        repeat: true,
                        modifiers: egui::Modifiers::NONE,
                    }],
                    ..Default::default()
                },
                |ctx| {
                    ctx.input_mut(|input| {
                        if open {
                            assert_eq!(completion_action(input, &mut 0, 2), None);
                        }
                        assert!(!consume_send_key(input));
                        assert!(!input.key_pressed(egui::Key::Enter));
                    });
                },
            );
        }
    }
}

#[cfg(test)]
mod insertion_tests {
    use super::{
        emoji_completion_choices, expand_message_shortcodes, replace_composer_selection,
        EmojiCompletion, MediaAsset,
    };

    #[test]
    fn complete_server_aliases_share_unicode_literal_and_reference_exclusions() {
        let assets = [
            MediaAsset {
                id: "owned-id".into(),
                kind: "emoji".into(),
                name: "game_night".into(),
                ..Default::default()
            },
            MediaAsset {
                id: "owned-sob".into(),
                kind: "emoji".into(),
                name: "sob".into(),
                ..Default::default()
            },
        ];
        let input = ":GAME_NIGHT: :sob: `:game_night:` \\:game_night: https://example.com/:game_night: <emoji:sob:owned-sob> [Party :game_night:](https://example.com)";
        assert_eq!(
            expand_message_shortcodes(input, &assets),
            "<emoji:game_night:owned-id> 😭 `:game_night:` \\:game_night: https://example.com/:game_night: <emoji:sob:owned-sob> [Party <emoji:game_night:owned-id>](https://example.com)"
        );
        assert_eq!(
            expand_message_shortcodes("```:game_night:", &assets),
            "```:game_night:"
        );
    }

    #[test]
    fn complete_server_aliases_reject_ambiguous_invalid_and_non_emoji_assets() {
        let asset = |name: &str, id: &str, kind: &str| MediaAsset {
            id: id.into(),
            name: name.into(),
            kind: kind.into(),
            ..Default::default()
        };
        let assets = [
            asset("party", "first", "emoji"),
            asset("PARTY", "second", "emoji"),
            asset("sticker_only", "sticker", "sticker"),
            asset("invalid", "bad:id", "emoji"),
            asset("duplicate", "same", "emoji"),
            asset("duplicate", "same", "emoji"),
        ];
        assert_eq!(
            expand_message_shortcodes(
                ":party: :sticker_only: :invalid: :unknown: :duplicate:",
                &assets
            ),
            ":party: :sticker_only: :invalid: :unknown: <emoji:duplicate:same>"
        );
    }

    #[test]
    fn server_completion_preserves_unicode_aliases_and_excludes_non_emoji_assets() {
        let emoji = MediaAsset {
            id: "owned-id".into(),
            kind: "emoji".into(),
            name: "sob".into(),
            ..Default::default()
        };
        let mut sticker = emoji.clone();
        sticker.kind = "sticker".into();
        sticker.id = "sticker-id".into();
        let mut invalid = emoji.clone();
        invalid.id = "invalid:id".into();
        let choices = emoji_completion_choices("sob", &[sticker, invalid, emoji.clone(), emoji]);
        assert_eq!(
            choices[0],
            EmojiCompletion::Unicode(crate::emoji::catalog().shortcode("sob").unwrap())
        );
        let custom: Vec<_> = choices
            .iter()
            .filter(|choice| matches!(choice, EmojiCompletion::Community { .. }))
            .collect();
        assert_eq!(
            custom,
            vec![&EmojiCompletion::Community {
                id: "owned-id".into(),
                name: "sob".into()
            }]
        );
        assert!(choices.len() <= 6);
    }

    #[test]
    fn server_completion_is_case_insensitive_and_prefers_prefixes() {
        let assets: Vec<_> = ["night_game", "game_night", "game"]
            .into_iter()
            .enumerate()
            .map(|(index, name)| MediaAsset {
                id: format!("owned-{index}"),
                kind: "emoji".into(),
                name: name.into(),
                ..Default::default()
            })
            .collect();
        let choices = emoji_completion_choices("GAME", &assets);
        let names: Vec<_> = choices
            .iter()
            .filter_map(|choice| {
                if let EmojiCompletion::Community { name, .. } = choice {
                    Some(name.as_str())
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(names, ["game", "game_night", "night_game"]);
    }

    #[test]
    fn picker_inserts_at_unicode_cursor_and_replaces_selection() {
        let mut draft = "😀 hello world".to_owned();
        assert_eq!(replace_composer_selection(&mut draft, Some(2..7), "🇩🇰"), 4);
        assert_eq!(draft, "😀 🇩🇰 world");
        assert_eq!(replace_composer_selection(&mut draft, Some(4..4), "!"), 5);
        assert_eq!(draft, "😀 🇩🇰! world");
        let end = draft.chars().count();
        assert_eq!(replace_composer_selection(&mut draft, None, "😭"), end + 1);
        assert!(draft.ends_with("world😭"));
    }
}

#[cfg(test)]
mod completion_focus_tests {
    use super::{completion_action, consume_send_key, egui, lock_completion_focus};
    #[test]
    fn completion_tab_keeps_editor_focus_so_next_enter_can_send() {
        for open in [false, true] {
            let ctx = egui::Context::default();
            let editor_id = egui::Id::new("completion-focus-editor");
            let mut draft = "@Jo".to_owned();
            for frame in 0..3 {
                let _ = ctx.run(
                    egui::RawInput {
                        events: if frame == 2 {
                            vec![egui::Event::Key {
                                key: egui::Key::Tab,
                                physical_key: None,
                                pressed: true,
                                repeat: false,
                                modifiers: egui::Modifiers::NONE,
                            }]
                        } else {
                            vec![]
                        },
                        ..Default::default()
                    },
                    |ctx| {
                        egui::CentralPanel::default().show(ctx, |ui| {
                            if frame == 2 && open {
                                assert_eq!(
                                    ctx.input_mut(|input| completion_action(input, &mut 0, 1)),
                                    Some(Some(0))
                                );
                                ctx.memory_mut(|memory| memory.request_focus(editor_id));
                            }
                            let editor =
                                ui.add(egui::TextEdit::multiline(&mut draft).id(editor_id));
                            if frame == 0 {
                                editor.request_focus();
                            }
                            let _ = ui.button("GIF");
                            if frame == 1 && open {
                                lock_completion_focus(ctx, editor_id);
                            }
                        });
                    },
                );
            }
            assert_eq!(ctx.memory(|memory| memory.has_focus(editor_id)), open);
            if open {
                let _ = ctx.run(
                    egui::RawInput {
                        events: vec![egui::Event::Key {
                            key: egui::Key::Enter,
                            physical_key: None,
                            pressed: true,
                            repeat: false,
                            modifiers: egui::Modifiers::NONE,
                        }],
                        ..Default::default()
                    },
                    |ctx| {
                        assert!(ctx.memory(|memory| memory.has_focus(editor_id)));
                        assert!(ctx.input_mut(consume_send_key));
                    },
                );
            }
        }
    }
}
