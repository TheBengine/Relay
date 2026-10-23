//! Offline Unicode 17 emoji, shortcodes and licensed Twemoji artwork.
use eframe::egui::{self, Color32, FontId, Pos2, Rect, Stroke, Vec2};
use serde::Deserialize;
use std::{collections::HashMap, sync::OnceLock};

#[derive(Deserialize)]
pub struct Entry {
    pub unicode: String,
    pub name: String,
    pub aliases: Vec<String>,
    forms: Vec<String>,
    pub category: usize,
    pub base: String,
    pub tone: String,
}

#[derive(Deserialize)]
pub struct Catalog {
    pub version: String,
    columns: usize,
    cell: usize,
    pub categories: Vec<String>,
    pub entries: Vec<Entry>,
    #[serde(skip)]
    aliases: HashMap<String, usize>,
    #[serde(skip)]
    starts: HashMap<char, Vec<(String, usize)>>,
}

pub fn catalog() -> &'static Catalog {
    static DATA: OnceLock<Catalog> = OnceLock::new();
    DATA.get_or_init(|| {
        let mut data: Catalog =
            serde_json::from_slice(include_bytes!("../assets/emoji/catalog.json"))
                .expect("bundled emoji catalog");
        for (index, entry) in data.entries.iter().enumerate() {
            for alias in &entry.aliases {
                data.aliases.entry(alias.clone()).or_insert(index);
            }
            for form in std::iter::once(&entry.unicode).chain(&entry.forms) {
                data.starts
                    .entry(form.chars().next().unwrap())
                    .or_default()
                    .push((form.clone(), index));
            }
        }
        for values in data.starts.values_mut() {
            values.sort_by_key(|(value, _)| std::cmp::Reverse(value.len()));
        }
        data
    })
}

impl Catalog {
    pub fn shortcode(&self, name: &str) -> Option<usize> {
        self.aliases.get(name).copied()
    }

    pub fn at_start(&self, text: &str) -> Option<(usize, usize)> {
        self.starts
            .get(&text.chars().next()?)?
            .iter()
            .find(|(form, _)| text.starts_with(form) && !text[form.len()..].starts_with('\u{FE0E}'))
            .map(|(form, index)| (*index, form.len()))
    }

    pub fn search(&self, query: &str, category: Option<usize>, include_tones: bool) -> Vec<usize> {
        let query = query
            .trim()
            .trim_matches(':')
            .to_lowercase()
            .replace('_', " ");
        let words: Vec<_> = query.split_whitespace().collect();
        let mut matches: Vec<_> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| category.is_none_or(|value| entry.category == value))
            .filter(|(_, entry)| include_tones || entry.tone.is_empty())
            .filter(|(_, entry)| {
                words.iter().all(|word| {
                    entry.name.to_lowercase().contains(word)
                        || entry
                            .aliases
                            .iter()
                            .any(|alias| alias.replace('_', " ").contains(word))
                        || entry.unicode == *word
                })
            })
            .map(|(index, _)| index)
            .collect();
        if let Some(exact) = self.shortcode(query.replace(' ', "_").as_str()) {
            if let Some(position) = matches.iter().position(|index| *index == exact) {
                matches.swap(0, position);
            }
        }
        matches
    }

    pub fn with_tone(&self, index: usize, tone: u8) -> usize {
        let entry = &self.entries[index];
        if tone == 0 || !entry.tone.is_empty() {
            return index;
        }
        let wanted = format!("{:X}", 0x1F3FA + tone as u32);
        self.entries
            .iter()
            .position(|variant| {
                variant.base == entry.base
                    && !variant.tone.is_empty()
                    && variant.tone.split('-').all(|part| part == wanted)
            })
            .unwrap_or(index)
    }

    pub fn uv(&self, index: usize) -> Rect {
        let rows = self.entries.len().div_ceil(self.columns);
        let size = Vec2::new((self.columns * self.cell) as f32, (rows * self.cell) as f32);
        let min = Vec2::new(
            (index % self.columns * self.cell + 1) as f32,
            (index / self.columns * self.cell + 1) as f32,
        );
        Rect::from_min_max(
            (min / size).to_pos2(),
            ((min + Vec2::splat((self.cell - 2) as f32)) / size).to_pos2(),
        )
    }
}

/// Code and escaped text are literal. URLs are never rewritten.
pub fn expand_shortcodes(input: &str) -> String {
    expand_shortcodes_with(input, |_| None)
}

/// Standard aliases take precedence; caller-owned aliases use the same literal exclusions.
pub fn expand_shortcodes_with(input: &str, resolve: impl Fn(&str) -> Option<String>) -> String {
    let mut output = String::with_capacity(input.len());
    let mut cursor = 0;
    let mut fence = 0;
    while cursor < input.len() {
        let tail = &input[cursor..];
        if let Some(length) = crate::message_format::expression_token_length(tail) {
            output.push_str(&tail[..length]);
            cursor += length;
            continue;
        }
        if let Some(escaped) = tail.strip_prefix('\\') {
            let length = 1 + escaped.chars().next().map_or(0, char::len_utf8);
            output.push_str(&tail[..length]);
            cursor += length;
            continue;
        }
        if tail.starts_with('`') {
            let count = tail.bytes().take_while(|value| *value == b'`').count();
            if fence == 0 {
                fence = count;
            } else if fence == count {
                fence = 0;
            }
            output.push_str(&tail[..count]);
            cursor += count;
            continue;
        }
        if fence == 0 && (tail.starts_with("https://") || tail.starts_with("http://")) {
            let end = tail.find(char::is_whitespace).unwrap_or(tail.len());
            output.push_str(&tail[..end]);
            cursor += end;
            continue;
        }
        if fence == 0 && tail.starts_with(':') {
            if let Some(end) = tail[1..].find(':').filter(|end| *end > 0 && *end <= 100) {
                let name = &tail[1..end + 1];
                if let Some(index) = catalog().shortcode(name) {
                    output.push_str(&catalog().entries[index].unicode);
                    cursor += end + 2;
                    continue;
                }
                if let Some(value) = resolve(name) {
                    output.push_str(&value);
                    cursor += end + 2;
                    continue;
                }
            }
        }
        let ch = tail.chars().next().unwrap();
        output.push(ch);
        cursor += ch.len_utf8();
    }
    output
}

/// Byte range before an unselected character cursor; never cross words or code.
pub fn completion(
    input: &str,
    character_cursor: usize,
) -> Option<(std::ops::Range<usize>, String)> {
    let end = input
        .char_indices()
        .nth(character_cursor)
        .map_or(input.len(), |(byte, _)| byte);
    let prefix = &input[..end];
    let start = prefix.rfind(':')?;
    let query = &prefix[start + 1..];
    if query.len() < 2
        || query.len() > 80
        || !query
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '+'))
    {
        return None;
    }
    if start > 0 && !prefix[..start].ends_with(char::is_whitespace) {
        return None;
    }
    let before = &prefix[..start];
    let mut fence = 0;
    let mut chars = before.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            chars.next();
        } else if ch == '`' {
            let mut count = 1;
            while chars.peek() == Some(&'`') {
                chars.next();
                count += 1;
            }
            if fence == 0 {
                fence = count;
            } else if fence == count {
                fence = 0;
            }
        }
    }
    (fence == 0).then(|| (start..end, query.to_owned()))
}

pub fn texture(ctx: &egui::Context) -> egui::TextureHandle {
    let id = egui::Id::new("relay-emoji-atlas");
    if let Some(texture) = ctx.data(|data| data.get_temp::<egui::TextureHandle>(id)) {
        return texture;
    }
    let image = image::load_from_memory(include_bytes!("../assets/emoji/atlas.png"))
        .expect("bundled emoji atlas")
        .into_rgba8();
    let color = egui::ColorImage::from_rgba_unmultiplied(
        [image.width() as usize, image.height() as usize],
        image.as_raw(),
    );
    let texture = ctx.load_texture("Twemoji Unicode 17", color, egui::TextureOptions::LINEAR);
    ctx.data_mut(|data| data.insert_temp(id, texture.clone()));
    texture
}

pub fn install_fonts(fonts: &mut egui::FontDefinitions) {
    for (name, bytes) in [
        (
            "Relay Emoji Space",
            include_bytes!("../assets/emoji/space.ttf").as_slice(),
        ),
        (
            "Relay Emoji Zero",
            include_bytes!("../assets/emoji/zero.ttf").as_slice(),
        ),
    ] {
        fonts
            .font_data
            .insert(name.into(), egui::FontData::from_static(bytes));
        fonts
            .families
            .insert(egui::FontFamily::Name(name.into()), vec![name.into()]);
    }
}

pub fn editor_job(
    text: &str,
    size: f32,
    color: Color32,
    width: f32,
) -> (egui::text::LayoutJob, Inline) {
    editor_job_with_custom(text, size, color, width, &[])
}

pub fn editor_job_with_custom(
    text: &str,
    size: f32,
    color: Color32,
    width: f32,
    custom: &[CustomEmoji],
) -> (egui::text::LayoutJob, Inline) {
    let mut job = egui::text::LayoutJob::default();
    let mut inline = Inline {
        accessible: text.to_owned(),
        ..Default::default()
    };
    let format = egui::TextFormat {
        font_id: FontId::proportional(size),
        color,
        ..Default::default()
    };
    let mut cursor = 0;
    let mut plain = 0;
    let ranges = custom_editor_ranges(text, custom);
    let mut ranges = ranges.into_iter().peekable();
    while cursor < text.len() {
        if ranges
            .peek()
            .is_some_and(|(range, _)| range.start == cursor)
        {
            let (range, index) = ranges.next().unwrap();
            job.append(&text[plain..cursor], 0.0, format.clone());
            let token = &text[range.clone()];
            let first = token.chars().next().unwrap().len_utf8();
            let image_size = size * 1.25;
            inline
                .custom
                .push((job.text.chars().count(), custom[index].clone(), image_size));
            for (piece, font) in [
                (&token[..first], "Relay Emoji Space"),
                (&token[first..], "Relay Emoji Zero"),
            ] {
                job.append(
                    piece,
                    0.0,
                    egui::TextFormat {
                        font_id: FontId::new(image_size, egui::FontFamily::Name(font.into())),
                        color: Color32::TRANSPARENT,
                        line_height: Some(image_size + 2.0),
                        ..Default::default()
                    },
                );
            }
            cursor = range.end;
            plain = cursor;
        } else if let Some((index, length)) = catalog().at_start(&text[cursor..]) {
            job.append(&text[plain..cursor], 0.0, format.clone());
            let emoji = &text[cursor..cursor + length];
            let first = emoji.chars().next().unwrap().len_utf8();
            let image_size = size * 1.25;
            inline
                .images
                .push((job.text.chars().count(), index, image_size));
            for (piece, font) in [
                (&emoji[..first], "Relay Emoji Space"),
                (&emoji[first..], "Relay Emoji Zero"),
            ] {
                job.append(
                    piece,
                    0.0,
                    egui::TextFormat {
                        font_id: FontId::new(image_size, egui::FontFamily::Name(font.into())),
                        color: Color32::TRANSPARENT,
                        line_height: Some(image_size + 2.0),
                        ..Default::default()
                    },
                );
            }
            cursor += length;
            plain = cursor;
        } else {
            cursor += text[cursor..].chars().next().unwrap().len_utf8();
        }
    }
    job.append(&text[plain..], 0.0, format);
    job.wrap.max_width = width;
    (job, inline)
}

fn custom_editor_ranges(
    text: &str,
    custom: &[CustomEmoji],
) -> Vec<(std::ops::Range<usize>, usize)> {
    let mut result = Vec::new();
    if custom.is_empty() {
        return result;
    }
    let mut cursor = 0;
    let mut fence = 0;
    while cursor < text.len() {
        let tail = &text[cursor..];
        if let Some(escaped) = tail.strip_prefix('\\') {
            cursor += 1 + escaped.chars().next().map_or(0, char::len_utf8);
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
        } else if let Some((index, emoji)) = custom.iter().enumerate().find(|(_, emoji)| {
            fence == 0 && !emoji.token.is_empty() && tail.starts_with(&emoji.token)
        }) {
            let end = cursor + emoji.token.len();
            result.push((cursor..end, index));
            cursor = end;
        } else {
            cursor += tail.chars().next().unwrap().len_utf8();
        }
    }
    result
}

pub fn edit_cluster_with_custom(
    input: &str,
    cursor: usize,
    previous: bool,
    custom: &[CustomEmoji],
) -> Option<(std::ops::Range<usize>, std::ops::Range<usize>)> {
    for (bytes, _) in custom_editor_ranges(input, custom) {
        let first = input[..bytes.start].chars().count();
        let end = first + input[bytes.clone()].chars().count();
        if if previous {
            first < cursor && cursor <= end
        } else {
            first <= cursor && cursor < end
        } {
            return Some((bytes, first..end));
        }
    }
    edit_cluster(input, cursor, previous)
}

pub fn edit_cluster(
    input: &str,
    cursor: usize,
    previous: bool,
) -> Option<(std::ops::Range<usize>, std::ops::Range<usize>)> {
    let mut byte = 0;
    let mut character = 0;
    while byte < input.len() {
        if let Some((_, length)) = catalog().at_start(&input[byte..]) {
            let count = input[byte..byte + length].chars().count();
            if count > 1
                && if previous {
                    character < cursor && cursor <= character + count
                } else {
                    character <= cursor && cursor < character + count
                }
            {
                return Some((byte..byte + length, character..character + count));
            }
            byte += length;
            character += count;
        } else {
            byte += input[byte..].chars().next().unwrap().len_utf8();
            character += 1;
        }
    }
    None
}

pub fn button(ui: &mut egui::Ui, index: usize, size: f32) -> egui::Response {
    let data = catalog();
    let entry = &data.entries[index];
    let texture = texture(ui.ctx());
    let response = ui
        .add(
            egui::ImageButton::new(
                egui::Image::new((texture.id(), Vec2::splat(size))).uv(data.uv(index)),
            )
            .frame(false),
        )
        .on_hover_text(format!("{} · :{}:", entry.name, entry.aliases[0]));
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, &entry.name));
    response
}

pub fn action_button(ui: &mut egui::Ui, index: usize) -> egui::Response {
    let data = catalog();
    let entry = &data.entries[index];
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(28.0), egui::Sense::click());
    if response.hovered() || response.has_focus() {
        let visuals = ui.style().interact(&response);
        ui.painter().rect_filled(rect, 4.0, visuals.bg_fill);
        if response.has_focus() {
            ui.painter().rect_stroke(rect, 4.0, visuals.bg_stroke);
        }
    }
    ui.painter().image(
        texture(ui.ctx()).id(),
        rect.shrink(4.0),
        data.uv(index),
        Color32::WHITE,
    );
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, &entry.name));
    response.on_hover_text(format!("{} · :{}:", entry.name, entry.aliases[0]))
}

pub fn quick_reactions(recent: &[String], tone: u8) -> Vec<usize> {
    let data = catalog();
    let mut result = Vec::with_capacity(3);
    for value in recent.iter().map(String::as_str).chain(["👍", "❤️", "😂"]) {
        if let Some((index, length)) = data.at_start(value) {
            if length == value.len() {
                let index = data.with_tone(index, tone);
                if !result.contains(&index) {
                    result.push(index);
                    if result.len() == 3 {
                        break;
                    }
                }
            }
        }
    }
    result
}

pub fn emoji_only(input: &str) -> bool {
    emoji_only_with_custom(input, &[])
}

pub fn emoji_only_with_custom(input: &str, custom: &[CustomEmoji]) -> bool {
    let mut tail = input.trim();
    let mut count = 0;
    while !tail.is_empty() {
        let Some(length) = catalog()
            .at_start(tail)
            .map(|(_, length)| length)
            .or_else(|| {
                custom
                    .iter()
                    .find(|emoji| !emoji.token.is_empty() && tail.starts_with(&emoji.token))
                    .map(|emoji| emoji.token.len())
            })
        else {
            return false;
        };
        count += 1;
        if count > 3 {
            return false;
        }
        tail = tail[length..].trim_start();
    }
    count > 0
}

pub fn reaction_button(
    ui: &mut egui::Ui,
    emoji: &str,
    count: usize,
    fill: Color32,
    stroke: Stroke,
) -> egui::Response {
    let display = match emoji {
        "+1" => "👍",
        "<3" => "❤️",
        "GG" => "🎉",
        value => value,
    };
    let mut job = egui::text::LayoutJob::default();
    let mut inline = Inline::default();
    let format = egui::TextFormat {
        font_id: FontId::proportional(14.0),
        color: ui.visuals().text_color(),
        ..Default::default()
    };
    inline.append(&mut job, display, format.clone(), true);
    inline.append(&mut job, &format!("  {count}"), format, false);
    let galley = ui.painter().layout_job(job);
    let response = ui.add(
        egui::Button::new(egui::WidgetText::Galley(galley.clone()))
            .fill(fill)
            .stroke(stroke)
            .rounding(5.0),
    );
    inline.paint(ui, &galley, response.rect.center() - galley.size() * 0.5);
    response.widget_info(|| {
        egui::WidgetInfo::labeled(
            egui::WidgetType::Button,
            format!("{display}, {count} reactions"),
        )
    });
    response
}

#[derive(Default)]
pub struct Inline {
    images: Vec<(usize, usize, f32)>,
    custom: Vec<(usize, CustomEmoji, f32)>,
    image_tint: Option<Color32>,
    quotes: Vec<(usize, Color32)>,
    pub accessible: String,
}

#[derive(Clone)]
pub struct CustomEmoji {
    pub token: String,
    pub name: String,
    pub texture: Option<egui::TextureHandle>,
}

impl Inline {
    pub fn tint_images(&mut self, color: Color32) {
        self.image_tint = Some(color);
    }
    pub fn quote_bar(&mut self, job: &mut egui::text::LayoutJob, size: f32, color: Color32) {
        self.quotes.push((job.text.chars().count(), color));
        self.accessible.push_str("Quote: ");
        job.append(
            "\u{2003}",
            0.0,
            egui::TextFormat {
                font_id: FontId::new(size, egui::FontFamily::Name("Relay Emoji Zero".into())),
                color: Color32::TRANSPARENT,
                ..Default::default()
            },
        );
    }
    pub fn append_with_custom(
        &mut self,
        job: &mut egui::text::LayoutJob,
        text: &str,
        format: egui::TextFormat,
        allow_emoji: bool,
        custom: &[CustomEmoji],
    ) {
        if !allow_emoji {
            self.append(job, text, format, allow_emoji);
            return;
        }
        let mut cursor = 0;
        let mut plain = 0;
        while cursor < text.len() {
            if let Some(emoji) = custom
                .iter()
                .find(|emoji| !emoji.token.is_empty() && text[cursor..].starts_with(&emoji.token))
            {
                self.append(job, &text[plain..cursor], format.clone(), true);
                let size = format.font_id.size * 1.25;
                self.custom
                    .push((job.text.chars().count(), emoji.clone(), size));
                self.accessible.push_str(&format!(":{}:", emoji.name));
                job.append(
                    "\u{2003}",
                    0.0,
                    egui::TextFormat {
                        font_id: FontId::new(
                            size,
                            egui::FontFamily::Name("Relay Emoji Space".into()),
                        ),
                        color: Color32::TRANSPARENT,
                        underline: Stroke::NONE,
                        strikethrough: Stroke::NONE,
                        line_height: Some(size + 2.0),
                        ..format.clone()
                    },
                );
                cursor += emoji.token.len();
                plain = cursor;
            } else if let Some(length) =
                crate::message_format::expression_token_length(&text[cursor..])
            {
                self.append(job, &text[plain..cursor], format.clone(), true);
                let token = &text[cursor..cursor + length];
                let name = token.split(':').nth(1).unwrap();
                self.append(job, &format!(":{name}:"), format.clone(), true);
                cursor += length;
                plain = cursor;
            } else {
                cursor += text[cursor..].chars().next().unwrap().len_utf8();
            }
        }
        self.append(job, &text[plain..], format, true);
    }
    pub fn append(
        &mut self,
        job: &mut egui::text::LayoutJob,
        text: &str,
        format: egui::TextFormat,
        allow_emoji: bool,
    ) {
        self.accessible.push_str(text);
        if !allow_emoji {
            job.append(text, 0.0, format);
            return;
        }
        let mut cursor = 0;
        let mut plain = 0;
        while cursor < text.len() {
            if let Some((index, length)) = catalog().at_start(&text[cursor..]) {
                job.append(&text[plain..cursor], 0.0, format.clone());
                let size = format.font_id.size * 1.25;
                self.images.push((job.text.chars().count(), index, size));
                job.append(
                    "\u{2003}",
                    0.0,
                    egui::TextFormat {
                        font_id: FontId::new(
                            size,
                            egui::FontFamily::Name("Relay Emoji Space".into()),
                        ),
                        color: Color32::TRANSPARENT,
                        underline: Stroke::NONE,
                        strikethrough: Stroke::NONE,
                        line_height: Some(size + 2.0),
                        ..format.clone()
                    },
                );
                cursor += length;
                plain = cursor;
            } else {
                cursor += text[cursor..].chars().next().unwrap().len_utf8();
            }
        }
        job.append(&text[plain..], 0.0, format);
    }

    pub fn paint(&self, ui: &egui::Ui, galley: &egui::Galley, origin: Pos2) {
        self.paint_clipped(ui, galley, origin, ui.clip_rect());
    }

    pub fn paint_clipped(&self, ui: &egui::Ui, galley: &egui::Galley, origin: Pos2, clip: Rect) {
        if self.images.is_empty() && self.custom.is_empty() && self.quotes.is_empty() {
            return;
        }
        let texture = (!self.images.is_empty()).then(|| texture(ui.ctx()));
        let painter = ui.painter().with_clip_rect(clip);
        let mut start = 0;
        for row in &galley.rows {
            for (position, emoji, size) in &self.custom {
                if let Some(glyph) = position
                    .checked_sub(start)
                    .and_then(|offset| row.glyphs.get(offset))
                {
                    let rect = Rect::from_center_size(
                        origin + Vec2::new(glyph.pos.x + glyph.size.x * 0.5, row.rect.center().y),
                        Vec2::splat(*size),
                    );
                    if ui.is_rect_visible(rect) {
                        if let Some(texture) = &emoji.texture {
                            painter.image(
                                texture.id(),
                                rect,
                                Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                                self.image_tint.unwrap_or(Color32::WHITE),
                            );
                        } else {
                            painter.rect_filled(rect.shrink(2.0), 4.0, Color32::from_gray(90));
                        }
                        ui.interact(
                            rect,
                            ui.id().with(("custom-emoji", position)),
                            egui::Sense::hover(),
                        )
                        .on_hover_text(format!(":{}:", emoji.name));
                    }
                }
            }
            for &(position, index, size) in &self.images {
                if let Some(glyph) = position
                    .checked_sub(start)
                    .and_then(|offset| row.glyphs.get(offset))
                {
                    let rect = Rect::from_center_size(
                        origin + Vec2::new(glyph.pos.x + glyph.size.x * 0.5, row.rect.center().y),
                        Vec2::splat(size),
                    );
                    if ui.is_rect_visible(rect) {
                        painter.image(
                            texture.as_ref().unwrap().id(),
                            rect,
                            catalog().uv(index),
                            self.image_tint.unwrap_or(Color32::WHITE),
                        );
                    }
                }
            }
            for &(position, color) in &self.quotes {
                let end = galley
                    .job
                    .text
                    .chars()
                    .enumerate()
                    .skip(position)
                    .find(|(_, ch)| *ch == '\n')
                    .map_or(galley.job.text.chars().count(), |(index, _)| index);
                if position < start + row.glyphs.len() && end >= start {
                    let rect = Rect::from_min_size(
                        origin + Vec2::new(-6.0, row.rect.top() + 1.0),
                        Vec2::new(2.0, (row.rect.height() - 2.0).max(1.0)),
                    );
                    painter.rect_filled(rect, 1.0, color);
                }
            }
            start += row.glyphs.len() + usize::from(row.ends_with_newline);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_editor_keeps_original_cursor_indices_and_one_image_width() {
        let emoji = CustomEmoji {
            token: "<emoji:squad:owned>".into(),
            name: "squad".into(),
            texture: None,
        };
        let text = "æ <emoji:squad:owned> 🇩🇰";
        let (job, inline) = editor_job_with_custom(
            text,
            15.0,
            Color32::WHITE,
            400.0,
            std::slice::from_ref(&emoji),
        );
        assert_eq!(job.text, text);
        assert_eq!(inline.custom[0].0, 2);
        let ctx = egui::Context::default();
        let mut fonts = egui::FontDefinitions::default();
        install_fonts(&mut fonts);
        ctx.set_fonts(fonts);
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let galley = ui.painter().layout_job(job.clone());
                let glyphs = &galley.rows[0].glyphs;
                assert_eq!(glyphs.len(), text.chars().count());
                assert!(glyphs[2].size.x > 15.0);
                assert!(glyphs[3..2 + emoji.token.len()]
                    .iter()
                    .all(|glyph| glyph.size.x == 0.0));
            });
        });
        let (bytes, chars) = edit_cluster_with_custom(
            text,
            2 + emoji.token.len(),
            true,
            std::slice::from_ref(&emoji),
        )
        .unwrap();
        let mut erased = text.to_owned();
        erased.replace_range(bytes, "");
        assert_eq!(erased, "æ  🇩🇰");
        assert_eq!(chars, 2..2 + emoji.token.len());
        for text in [
            "`<emoji:squad:owned>`",
            "\\<emoji:squad:owned>",
            "https://example.com/<emoji:squad:owned>",
        ] {
            let (job, inline) = editor_job_with_custom(
                text,
                15.0,
                Color32::WHITE,
                400.0,
                std::slice::from_ref(&emoji),
            );
            assert_eq!(job.text, text);
            assert!(inline.custom.is_empty());
        }
    }

    #[test]
    fn custom_emoji_only_messages_share_unicode_jumbo_limits() {
        let emoji = CustomEmoji {
            token: "<emoji:squad:owned>".into(),
            name: "squad".into(),
            texture: None,
        };
        let custom = std::slice::from_ref(&emoji);
        assert!(emoji_only_with_custom("<emoji:squad:owned> 😭 🇩🇰", custom));
        assert!(!emoji_only_with_custom(
            "<emoji:squad:owned> 😭 🇩🇰 👍",
            custom
        ));
        assert!(!emoji_only_with_custom("<emoji:removed:other>", custom));
        assert!(!emoji_only_with_custom("`<emoji:squad:owned>`", custom));
        assert!(!emoji_only_with_custom("Hello <emoji:squad:owned>", custom));
    }

    #[test]
    fn community_images_share_text_positions_with_unicode_and_keep_code_literal() {
        let emoji = CustomEmoji {
            token: "<emoji:squad:owned>".into(),
            name: "squad".into(),
            texture: None,
        };
        let mut job = egui::text::LayoutJob::default();
        let mut inline = Inline::default();
        inline.append_with_custom(
            &mut job,
            "æ <emoji:squad:owned> 😭! <emoji:unknown:id>",
            egui::TextFormat::default(),
            true,
            std::slice::from_ref(&emoji),
        );
        assert_eq!(job.text, "æ \u{2003} \u{2003}! :unknown:");
        assert_eq!(inline.accessible, "æ :squad: 😭! :unknown:");
        assert_eq!(inline.custom[0].0, 2);
        assert_eq!(inline.images[0].0, 4);
        inline.append_with_custom(
            &mut job,
            &emoji.token,
            egui::TextFormat::default(),
            false,
            std::slice::from_ref(&emoji),
        );
        assert!(job.text.ends_with("<emoji:squad:owned>"));
        assert_eq!(inline.custom.len(), 1);
    }

    #[test]
    fn wrapped_quote_bar_covers_its_rows_and_stops_before_normal_text() {
        let ctx = egui::Context::default();
        let mut fonts = egui::FontDefinitions::default();
        install_fonts(&mut fonts);
        ctx.set_fonts(fonts);
        let expected_bars = std::cell::Cell::new(0);
        let output = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let mut job = egui::text::LayoutJob::default();
                let mut inline = Inline::default();
                inline.quote_bar(&mut job, 15.0, Color32::GRAY);
                inline.append(
                    &mut job,
                    "This quote wraps across multiple rows of text.\nNormal",
                    egui::TextFormat {
                        font_id: FontId::proportional(15.0),
                        ..Default::default()
                    },
                    true,
                );
                job.wrap.max_width = 100.0;
                let galley = ui.painter().layout_job(job);
                assert!(galley.rows.len() >= 4);
                expected_bars.set(galley.rows.len() - 1);
                assert_eq!(galley.rows[0].glyphs[0].size.x, 0.0);
                inline.paint(ui, &galley, ui.min_rect().min + Vec2::new(10.0, 0.0));
            });
        });
        let bars = output.shapes.iter().filter(|shape| matches!(&shape.shape, egui::epaint::Shape::Rect(rect) if rect.rect.width() == 2.0 && rect.fill == Color32::GRAY)).count();
        assert_eq!(bars, expected_bars.get());
    }

    #[test]
    fn complete_unicode_17_and_common_shortcodes_are_bundled() {
        assert_eq!(catalog().entries.len(), 3953);
        assert_eq!(catalog().version, "17.0");
        assert_eq!(
            expand_shortcodes("😭 :sob: :thumbsup: :+1: :heart:"),
            "😭 😭 👍 👍 ❤️"
        );
        for entry in &catalog().entries {
            assert_eq!(
                catalog().at_start(&entry.unicode).map(|(_, length)| length),
                Some(entry.unicode.len()),
                "{}",
                entry.name
            );
        }
    }

    #[test]
    fn code_escapes_urls_and_unknown_names_stay_literal() {
        let text = r"`:sob:` ```rust :sob: ``` \:sob: https://example.com/:sob: :unknown: :sob:";
        assert_eq!(
            expand_shortcodes(text),
            r"`:sob:` ```rust :sob: ``` \:sob: https://example.com/:sob: :unknown: 😭"
        );
        assert_eq!(expand_shortcodes("```:sob:"), "```:sob:");
    }

    #[test]
    fn community_references_survive_names_that_are_unicode_aliases() {
        let text = "<emoji:sob:owned-id> <sticker:heart:owned-sticker> :sob:";
        assert_eq!(
            expand_shortcodes(text),
            "<emoji:sob:owned-id> <sticker:heart:owned-sticker> 😭"
        );
        assert_eq!(
            expand_shortcodes("[Custom <emoji:sob:owned-id>](https://example.com)"),
            "[Custom <emoji:sob:owned-id>](https://example.com)"
        );
    }

    #[test]
    fn longest_sequence_wins_for_skin_tones_families_and_flags() {
        for emoji in ["👩🏽‍💻", "👨‍👩‍👧‍👦", "🇩🇰", "👍🏿", "🏳️‍🌈", "❤️"]
        {
            let (index, length) = catalog().at_start(emoji).unwrap();
            assert_eq!(length, emoji.len());
            assert_eq!(catalog().entries[index].unicode, emoji);
        }
        let base = catalog().shortcode("thumbsup").unwrap();
        assert_eq!(
            catalog().entries[catalog().with_tone(base, 5)].unicode,
            "👍🏿"
        );
    }

    #[test]
    fn autocomplete_uses_unicode_cursor_and_excludes_code_and_urls() {
        assert_eq!(completion("æ :so", 5), Some((3..6, "so".into())));
        assert!(completion("` :so", 5).is_none());
        assert!(completion("https://x/:so", 13).is_none());
        assert!(completion("\\:so", 4).is_none());
        assert!(completion(":s", 2).is_none());
    }

    #[test]
    fn inline_layout_keeps_cluster_positions_and_accessible_unicode() {
        let mut job = egui::text::LayoutJob::default();
        let mut inline = Inline::default();
        inline.append(&mut job, "Hej 👩🏽‍💻 🇩🇰!", egui::TextFormat::default(), true);
        assert_eq!(job.text, "Hej \u{2003} \u{2003}!");
        assert_eq!(
            inline
                .images
                .iter()
                .map(|(position, _, _)| *position)
                .collect::<Vec<_>>(),
            [4, 6]
        );
        assert_eq!(inline.accessible, "Hej 👩🏽‍💻 🇩🇰!");
        inline.append(&mut job, " literal 👍", egui::TextFormat::default(), false);
        assert_eq!(inline.images.len(), 2);
        assert!(job.text.ends_with(" literal 👍"));
    }

    #[test]
    fn jumbo_emoji_are_limited_and_text_presentation_stays_text() {
        assert!(emoji_only(" 🇩🇰 👩🏽‍💻 ❤️ "));
        assert!(!emoji_only("😀😀😀😀"));
        assert!(!emoji_only("Hello 😀"));
        assert!(!emoji_only(""));
        assert!(catalog().at_start("©\u{FE0E}").is_none());
    }

    #[test]
    fn editor_preserves_unicode_and_reserves_one_slot_per_sequence() {
        let ctx = egui::Context::default();
        let mut fonts = egui::FontDefinitions::default();
        install_fonts(&mut fonts);
        ctx.set_fonts(fonts);
        let text = "æ 👩🏽‍💻 🇩🇰 ❤️";
        let (job, inline) = editor_job(text, 16.0, Color32::WHITE, 1000.0);
        assert_eq!(job.text, text);
        assert_eq!(inline.images.len(), 3);
        let _ = ctx.run(Default::default(), |ctx| {
            let galley = ctx.fonts(|fonts| fonts.layout_job(job.clone()));
            let glyphs = &galley.rows[0].glyphs;
            let start = inline.images[0].0;
            assert!((glyphs[start].size.x - 20.0).abs() < 0.1);
            for glyph in &glyphs[start + 1..start + "👩🏽‍💻".chars().count()] {
                assert_eq!(glyph.size.x, 0.0);
            }
        });
    }

    #[test]
    fn emoji_sequence_edits_never_leave_half_a_flag_or_zwj_sequence() {
        assert_eq!(edit_cluster("æ 🇩🇰!", 4, true), Some((3..11, 2..4)));
        assert_eq!(edit_cluster("æ 🇩🇰!", 2, false), Some((3..11, 2..4)));
        assert!(edit_cluster("abc", 2, true).is_none());
        assert!(edit_cluster("🇩🇰", 0, true).is_none());
        assert!(edit_cluster("🇩🇰", 2, false).is_none());
    }

    #[test]
    fn quick_reactions_keep_distinct_valid_recents_and_apply_selected_skin_tone() {
        let values = |indices: Vec<usize>| {
            indices
                .into_iter()
                .map(|index| catalog().entries[index].unicode.as_str())
                .collect::<Vec<_>>()
        };
        assert_eq!(values(quick_reactions(&[], 4)), ["👍🏾", "❤️", "😂"]);
        assert_eq!(
            values(quick_reactions(
                &["😭".into(), "😭".into(), "text 😀".into(), "🇩🇰".into()],
                0
            )),
            ["😭", "🇩🇰", "👍"]
        );
        assert_eq!(
            values(quick_reactions(&["👍".into(), "👍🏾".into()], 4)),
            ["👍🏾", "❤️", "😂"]
        );
    }
}
