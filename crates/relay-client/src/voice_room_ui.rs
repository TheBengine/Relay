//! R6.1: the voice room page. A calm header with the room's real participant
//! count, and a centred grid of participant tiles that fits the space it has.
//! Rendering only; media, devices and the connection are never touched here.
use super::*;

/// What the page knows about the own connection to the shown room.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct RoomConnection {
    pub joined: bool,
    pub encryption_paused: bool,
    pub reconnecting: bool,
    pub connected: bool,
    /// Audio devices are open (`voice_ready`), the network may still be starting.
    pub media_ready: bool,
}

/// The line above the tiles while a call starts or recovers. A working call
/// needs none ("Voice connected" lives in the sidebar's connection panel), and
/// before joining, Join Voice under the tiles says it.
pub(super) fn room_status(state: RoomConnection) -> Option<&'static str> {
    if state.reconnecting {
        Some("Reconnecting voice… Your mute and deafen settings are preserved. You can disconnect below.")
    } else if state.encryption_paused {
        Some("Voice is paused. Check the call message below.")
    } else if !state.joined || state.connected {
        None
    } else if state.media_ready {
        Some("Connecting voice… You can disconnect below.")
    } else {
        Some("Opening your audio devices… You can cancel below.")
    }
}

/// Space between the dimmed tiles and Join Voice.
pub(super) const JOIN_GAP: f32 = 16.0;

/// Join Voice and its caption under the grid, which is centred with them.
pub(super) fn join_block_height() -> f32 {
    JOIN_GAP + 38.0 + 6.0 + font_size(crate::appearance::TEXT_CAPTION) * 1.4 + 4.0
}

/// One other person in the call, for the sound check.
pub(super) struct SoundRow<'a> {
    pub name: &'a str,
    pub muted: bool,
    /// Age of decoded speech received during this stay in the room.
    pub last_received: Option<Duration>,
    /// How long they have been in the call with you.
    pub together: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Hearing {
    /// Non-silent audio reached the decoder recently; physical playback is unknown.
    Heard,
    Quiet,
    Muted,
    NotYet,
}

pub(super) fn hearing(row: &SoundRow) -> Hearing {
    if row.muted {
        Hearing::Muted
    } else if row
        .last_received
        .is_some_and(|age| age <= Duration::from_secs(5))
    {
        Hearing::Heard
    } else if row.last_received.is_some() {
        Hearing::Quiet
    } else {
        Hearing::NotYet
    }
}

/// One sentence when you are deafened, or when someone unmuted has been in the
/// call for a while and nothing of theirs has arrived. It says where to look.
pub(super) fn sound_hint(deafened: bool, rows: &[SoundRow]) -> Option<String> {
    if deafened {
        return Some("You are deafened, so you hear no one. Undeafen to listen.".into());
    }
    let quiet = rows.iter().find(|row| {
        !row.muted
            && row.together >= Duration::from_secs(20)
            && row
                .last_received
                .is_none_or(|age| age >= Duration::from_secs(20))
    })?;
    Some(format!(
        "Can't hear {}? Ask them to say something and watch for 'audio received'. Check their microphone, your output device and volume, and the connection. Receiving audio does not confirm that your speakers are playing it.",
        quiet.name
    ))
}

/// Receiving packets does not mean they can be heard: a saved zero volume can
/// silence an otherwise healthy call. Recovery is always an explicit choice.
pub(super) fn local_volume_help(
    ui: &mut egui::Ui,
    issue: crate::audio_settings::LocalVolumeIssue,
) -> bool {
    use crate::audio_settings::LocalVolumeIssue;
    let (message, action) = match issue {
        LocalVolumeIssue::Output => (
            "Your output volume is 0%, so you cannot hear this call.",
            "Set output volume to 100%",
        ),
        LocalVolumeIssue::Microphone => (
            "Your microphone volume is 0%, so others cannot hear you.",
            "Restore microphone volume",
        ),
    };
    egui::Frame::none()
        .fill(c_rail())
        .rounding(8.0)
        .inner_margin(Margin::same(12.0))
        .show(ui, |ui| {
            ui.add(egui::Label::new(RichText::new(message).strong()).wrap(true));
            ui.button(action).clicked()
        })
        .inner
}

/// The LAN's elected voice gateway in plain words, with `name` for accounts.
pub(super) fn gateway_choice_text(
    choice: &crate::gateway_control::Choice,
    active: bool,
    name: impl Fn(&str) -> String,
) -> String {
    let mut text = format!(
        "{} serves voice for your network: {}.",
        name(&choice.gateway_id),
        choice.reasons
    );
    if choice.others > 0 {
        text.push_str(&format!(
            " It was elected from {} approved gateways here because it measured best or has served longest.",
            choice.others + 1
        ));
    }
    if let Some((next, held)) = &choice.moving_to {
        text.push_str(&format!(
            " {} now measures better ({held} s); everyone here moves to it after {} s, without a gap.",
            name(next),
            relay_protocol::host_choice::SUSTAIN_MS / 1000
        ));
    }
    if !active {
        text.push_str(" Your voice uses the normal route until that path is measured faster.");
    }
    text
}

pub(super) fn local_gateway_controls(
    ui: &mut egui::Ui,
    enabled: bool,
    available: bool,
    active: bool,
    friends_call: bool,
    choice: Option<String>,
) -> Option<bool> {
    egui::Frame::none()
        .fill(c_rail())
        .rounding(8.0)
        .inner_margin(Margin::same(12.0))
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("Local voice gateways").strong());
                ui.label(RichText::new(if !available {
                    "Not available for this connection"
                } else if !enabled {
                    "Off"
                } else if active {
                    "Using a measured gateway route"
                } else {
                    "Enabled · using the normal voice route"
                }).color(if available && enabled && active { c_good() } else { c_muted() }));
            });
            let description = if !available {
                "This call keeps its normal voice route. Its server or connection does not currently support local voice gateways."
            } else if friends_call {
                "Optional: allow this PC to use or offer a gateway to people in this exact call on your local network. No server role is granted."
            } else {
                "Optional: use approved gateways on your local network. This PC can offer one only with an accepted Voice gateway role."
            };
            ui.add(egui::Label::new(RichText::new(description).color(c_muted())).wrap(true));
            if let Some(choice) = choice.filter(|_| available && enabled) {
                ui.add(egui::Label::new(RichText::new(choice)).wrap(true));
            }
            if available {
                ui.add(egui::Label::new(RichText::new("Relay checks the complete voice path and selects a gateway only when it is faster or the normal route fails. Stopping returns to the normal route. Your call's privacy setting stays the same; video uses a separate route.").size(font_size(TEXT_CAPTION)).color(c_muted())).wrap(true));
            }
            ui.add_enabled(available || enabled, secondary(if enabled {
                "Stop local voice gateways"
            } else {
                "Enable local voice gateways"
            })).clicked().then_some(!enabled)
        })
        .inner
}

/// Offer help for a recent pattern of late audio, rather than a single glitch
/// or the expected silence/concealment between words. Counters reset per call.
#[derive(Clone, Default)]
pub(super) struct QualityWatch {
    sample: Option<(Instant, u64, u64)>,
    visible_until: Option<Instant>,
}
impl QualityWatch {
    pub(super) fn observe(&mut self, now: Instant, received: u64, late: u64) -> bool {
        match self.sample {
            Some((_, old_received, old_late)) if received < old_received || late < old_late => {
                *self = Self::default();
            }
            Some((at, old_received, old_late))
                if now.saturating_duration_since(at) >= Duration::from_secs(2) =>
            {
                let arrivals = received.saturating_sub(old_received);
                let missed = late.saturating_sub(old_late);
                if arrivals >= 40 && missed >= 5 && missed.saturating_mul(20) >= arrivals {
                    self.visible_until = Some(now + Duration::from_secs(10));
                }
                self.sample = Some((now, received, late));
            }
            _ => {}
        }
        if self.sample.is_none() {
            self.sample = Some((now, received, late));
        }
        self.visible_until.is_some_and(|until| now < until)
    }
}

/// Rendering returns an explicit choice; it never changes a device or setting.
pub(super) fn steadier_audio_help(ui: &mut egui::Ui) -> bool {
    egui::Frame::none().fill(c_rail()).rounding(8.0).inner_margin(Margin::same(12.0)).show(ui, |ui| {
        ui.add(egui::Label::new(RichText::new("Incoming audio is arriving unevenly.").strong()).wrap(true));
        ui.add(egui::Label::new(RichText::new("Automatic smoothing can reduce breaks in the sound. It may add a short delay when your connection needs it.").size(font_size(13.0)).color(c_muted())).wrap(true));
        ui.button("Make audio steadier").clicked()
    }).inner
}

/// One named cause and its next step, with text (never colour alone).
pub(super) fn voice_problem_help(ui: &mut egui::Ui, problem: crate::voice_problem::VoiceProblem) {
    egui::Frame::none().fill(c_rail()).rounding(8.0).inner_margin(Margin::same(12.0)).show(ui, |ui| {
        ui.add(egui::Label::new(RichText::new(format!("⚠ {}", problem.title())).strong().color(c_bad())).wrap(true));
        ui.add(egui::Label::new(RichText::new(problem.next_step()).size(font_size(13.0)).color(c_text())).wrap(true));
    });
}

/// Per-person receive activity. This is not a physical speaker test and must
/// not blame the sender solely because speech has not arrived here.
pub(super) fn sound_check(ui: &mut egui::Ui, rows: &[SoundRow], hint: Option<&str>) {
    ui.horizontal_wrapped(|ui| {
        ui.add_space(GRID_MARGIN);
        ui.label(
            RichText::new("SOUND CHECK")
                .font(semibold(ui, font_size(crate::appearance::TEXT_CAPTION)))
                .strong()
                .color(c_muted()),
        );
        for row in rows {
            ui.add_space(8.0);
            let (color, words) = match hearing(row) {
                Hearing::Heard => (c_good(), "audio received"),
                Hearing::Quiet => (c_muted(), "no recent audio"),
                Hearing::Muted => (c_muted(), "muted"),
                Hearing::NotYet => (c_muted(), "waiting for audio"),
            };
            let (dot, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
            ui.painter().circle_filled(dot.center(), 4.0, color);
            ui.label(
                RichText::new(format!("{} · {words}", row.name))
                    .size(font_size(13.0))
                    .color(if color == c_good() {
                        c_text()
                    } else {
                        c_muted()
                    }),
            );
        }
    });
    if let Some(hint) = hint {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.add_space(GRID_MARGIN);
            ui.add(egui::Label::new(RichText::new(hint).size(font_size(13.0))).wrap(true));
        });
    }
}

const TILE_GAP: f32 = 8.0;
const GRID_MARGIN: f32 = 24.0;
/// Discord puts at most four tiles in a row.
const MAX_COLUMNS: usize = 4;
const TILE_MIN_WIDTH: f32 = 160.0;
/// Tiles in the strip under a focused stream.
const STRIP_TILE_WIDTH: f32 = 192.0;

fn tile_size(width: f32) -> Vec2 {
    Vec2::new(width, width * 9.0 / 16.0)
}

/// Columns and tile size; each row is centred in the available width.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct RoomGrid {
    pub columns: usize,
    pub tile: Vec2,
}

impl RoomGrid {
    pub(super) fn rows(&self, count: usize) -> usize {
        count.max(1).div_ceil(self.columns)
    }
    /// From the first tile's top to the last tile's bottom.
    pub(super) fn height(&self, count: usize) -> f32 {
        let rows = self.rows(count) as f32;
        self.tile.y * rows + TILE_GAP * (rows - 1.0)
    }
}

/// Discord's fit: 16:9 tiles as large as the space allows, at most four per
/// row. When even the smallest tiles cannot fit the height the page scrolls,
/// with as few rows as the width allows. Never wider than `width`.
pub(super) fn room_grid(width: f32, height: f32, count: usize) -> RoomGrid {
    let width = width.max(1.0);
    let height = height.max(0.0);
    let count = count.max(1);
    let mut best: Option<RoomGrid> = None;
    let mut largest = 0.0_f32;
    for columns in 1..=count.min(MAX_COLUMNS) {
        let rows = count.div_ceil(columns) as f32;
        let by_width = (width - TILE_GAP * (columns - 1) as f32) / columns as f32;
        if by_width <= 0.0 {
            break;
        }
        let by_height = (height - TILE_GAP * (rows - 1.0)) / rows * 16.0 / 9.0;
        let tile = by_width.min(by_height.max(TILE_MIN_WIDTH));
        // A tie (both at the minimum) goes to more columns: fewer rows to
        // scroll. Fewer rows also win when the tiles stay nearly as large, so
        // two people sit side by side in a wide window, as in Discord.
        let fewer_rows = best.is_some_and(|best| best.rows(count) > rows as usize);
        if tile >= largest - 0.5 || (fewer_rows && tile >= largest * 0.8) {
            best = Some(RoomGrid {
                columns,
                tile: tile_size(tile),
            });
        }
        largest = largest.max(tile);
    }
    best.unwrap_or(RoomGrid {
        columns: 1,
        tile: tile_size(width),
    })
}

/// Display data for one participant; built by the app from its voice members.
pub(super) struct ParticipantTile<'a> {
    pub user: &'a User,
    pub avatar: Option<egui::TextureHandle>,
    pub speaking: bool,
    pub muted: bool,
    pub deafened: bool,
    pub sharing: bool,
    pub camera: bool,
}

/// One received camera or screen stream, shown as a tile in the same grid.
pub(super) struct StreamTile<'a> {
    pub user: &'a User,
    pub texture: egui::TextureHandle,
    pub screen: bool,
    /// A still preview from the server rather than the live stream.
    pub preview: bool,
    pub pinned: bool,
}

/// Someone shares their screen but no picture has arrived here yet: Discord's
/// "Watch Stream" tile.
pub(super) struct WaitingTile<'a> {
    pub user: &'a User,
    pub avatar: Option<egui::TextureHandle>,
    pub watch: Watch,
}

/// What a waiting stream tile offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Watch {
    /// Video is not connected: a click connects it.
    Offer,
    /// Video is connecting.
    Connecting,
    /// Video is connected; the first picture is on its way.
    Arriving,
    /// You are not in the call.
    JoinFirst,
}

impl Watch {
    pub(super) fn caption(self) -> &'static str {
        match self {
            Watch::Offer => "Watch Stream",
            Watch::Connecting => "Connecting video…",
            Watch::Arriving => "Waiting for the picture…",
            Watch::JoinFirst => "Join voice to watch",
        }
    }
}

pub(super) enum Tile<'a> {
    Person(ParticipantTile<'a>),
    Stream(StreamTile<'a>),
    Waiting(WaitingTile<'a>),
}

/// Spoken state for assistive technology; the tile itself shows icons only.
pub(super) fn tile_status(tile: &ParticipantTile) -> &'static str {
    if tile.deafened {
        "Deafened"
    } else if tile.muted {
        "Mic off"
    } else if tile.speaking {
        "Speaking"
    } else {
        "Connected"
    }
}

pub(super) fn stream_kind(tile: &StreamTile) -> &'static str {
    match (tile.screen, tile.preview) {
        (true, false) => "Screen share",
        (false, false) => "Camera",
        (true, true) => "Screen preview",
        (false, true) => "Camera preview",
    }
}

fn tile_label(tile: &Tile) -> String {
    match tile {
        Tile::Person(person) => format!(
            "{}, {}",
            profile_ui::display_name(person.user),
            tile_status(person)
        ),
        Tile::Stream(stream) => format!(
            "{}, {}",
            profile_ui::display_name(stream.user),
            stream_kind(stream)
        ),
        Tile::Waiting(waiting) => format!(
            "{} is sharing their screen, {}",
            profile_ui::display_name(waiting.user),
            waiting.watch.caption()
        ),
    }
}

/// Space above the grid that centres it vertically in `space`. `grid` is the
/// height from the first tile's top to the last tile's bottom, `tail` what the
/// grid allocates below that. A grid that does not fit starts at the top
/// (never a negative spacer), so the first row stays visible and the page
/// scrolls; a fitting grid never adds a scrollbar.
pub(super) fn vertical_inset(space: f32, grid: f32, tail: f32) -> f32 {
    let free = space - grid - tail;
    if free <= 0.0 {
        0.0
    } else {
        ((space - grid) * 0.5).min(free)
    }
}

/// Lays out `count` tiles of `grid` from `top`, each row centred in `width`.
fn place_rows(grid: RoomGrid, count: usize, width: f32, top: f32, rects: &mut Vec<Rect>) {
    if count == 0 {
        return;
    }
    for row in 0..grid.rows(count) {
        let first = row * grid.columns;
        let in_row = (count - first).min(grid.columns);
        let row_width = grid.tile.x * in_row as f32 + TILE_GAP * (in_row as f32 - 1.0);
        let left = ((width - row_width) * 0.5).max(0.0);
        let y = top + row as f32 * (grid.tile.y + TILE_GAP);
        for column in 0..in_row {
            rects.push(Rect::from_min_size(
                Pos2::new(left + column as f32 * (grid.tile.x + TILE_GAP), y),
                grid.tile,
            ));
        }
    }
}

/// Where each tile goes, in order, relative to the grid's top-left corner,
/// and the total height. `focus` makes that tile large, with the others in a
/// centred strip of small tiles under it, as in Discord's focused view.
pub(super) fn tile_layout(
    width: f32,
    height: f32,
    count: usize,
    focus: Option<usize>,
) -> (Vec<Rect>, f32) {
    let mut rects = Vec::with_capacity(count);
    if count == 0 {
        return (rects, 0.0);
    }
    let Some(focus) = focus.filter(|focus| *focus < count) else {
        let grid = room_grid(width, height, count);
        place_rows(grid, count, width, 0.0, &mut rects);
        return (rects, grid.height(count));
    };
    let others = count - 1;
    let strip_tile = STRIP_TILE_WIDTH.min(width.max(1.0));
    let strip = RoomGrid {
        columns: (((width + TILE_GAP) / (strip_tile + TILE_GAP)).floor() as usize)
            .clamp(1, others.max(1)),
        tile: tile_size(strip_tile),
    };
    let strip_height = if others > 0 {
        strip.height(others) + TILE_GAP
    } else {
        0.0
    };
    let big = room_grid(width, height - strip_height, 1).tile;
    let big_rect = Rect::from_min_size(Pos2::new(((width - big.x) * 0.5).max(0.0), 0.0), big);
    let mut small = Vec::with_capacity(others);
    place_rows(strip, others, width, big.y + TILE_GAP, &mut small);
    let mut small = small.into_iter();
    for index in 0..count {
        rects.push(if index == focus {
            big_rect
        } else {
            small.next().unwrap_or(big_rect)
        });
    }
    (rects, big.y + strip_height)
}

/// Draws the centred grid and returns one response per tile, in order, so the
/// caller keeps its profile, volume and stream menus. `ids` keep each tile's
/// menu with its person or stream when the order changes. `space` is the
/// height left on the page for the grid and `below`, content that follows the
/// grid and is centred with it; 0 keeps the grid at the top. `dim` shows a
/// room you have not joined.
pub(super) fn participant_grid(
    ui: &mut egui::Ui,
    tiles: &[Tile],
    ids: &[egui::Id],
    focus: Option<usize>,
    space: f32,
    below: f32,
    dim: bool,
) -> Vec<egui::Response> {
    // Keep a margin on both sides, so the tiles never touch the window edge.
    let width = (ui.available_width() - 2.0 * GRID_MARGIN).max(1.0);
    let (layout, height) = tile_layout(width, space - below, tiles.len(), focus);
    ui.add_space(vertical_inset(space, height + below, 0.0));
    let (area, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::hover());
    let origin = area.min + Vec2::new(((area.width() - width) * 0.5).max(0.0), 0.0);
    let mut responses = Vec::with_capacity(tiles.len());
    for (index, (tile, at)) in tiles.iter().zip(layout).enumerate() {
        let rect = at.translate(origin.to_vec2());
        let id = ids
            .get(index)
            .copied()
            .unwrap_or_else(|| ui.id().with(("voice-tile", index)));
        let response = ui.interact(rect, id, Sense::click());
        paint_tile(ui, rect, tile, response.hovered(), dim);
        let label = tile_label(tile);
        response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, &label));
        responses.push(response);
    }
    responses
}

/// Discord's 2 px green ring while speaking, a faint ring on hover.
fn tile_ring(painter: &egui::Painter, rect: Rect, speaking: bool, hovered: bool) {
    if speaking {
        painter.rect_stroke(rect.shrink(1.0), 8.0, Stroke::new(2.0_f32, c_good()));
    } else if hovered {
        painter.rect_stroke(rect.shrink(0.5), 8.0, Stroke::new(1.0_f32, c_line()));
    }
}

fn paint_tile(ui: &egui::Ui, rect: Rect, tile: &Tile, hovered: bool, dim: bool) {
    let painter = ui.painter().with_clip_rect(rect.intersect(ui.clip_rect()));
    match tile {
        Tile::Person(person) => {
            painter.rect_filled(rect, 8.0, tile_fill());
            // About 80 points in a normal tile, as in Discord.
            let size = (rect.width() * 0.17)
                .min(rect.height() * 0.45)
                .clamp(24.0, 112.0);
            let avatar = Rect::from_center_size(rect.center(), Vec2::splat(size));
            profile_ui::paint_user_avatar(ui, avatar, person.user, person.avatar.as_ref(), false);
            let mut marks = Vec::new();
            if person.deafened {
                marks.push(Mark::Off(Icon::Headphones));
            } else if person.muted {
                marks.push(Mark::Off(Icon::Mic));
            }
            if person.camera {
                marks.push(Mark::On(Icon::Camera));
            }
            if person.sharing {
                marks.push(Mark::Live);
            }
            name_pill(ui, &painter, rect, profile_ui::display_name(person.user), &marks);
            tile_ring(&painter, rect, person.speaking, hovered);
        }
        Tile::Stream(stream) => {
            painter.rect_filled(rect, 8.0, Color32::BLACK);
            let size = stream.texture.size_vec2();
            if size.x > 0.0 && size.y > 0.0 {
                // A screen shows whole; a camera fills the tile, as in Discord.
                let scale = if stream.screen {
                    (rect.width() / size.x).min(rect.height() / size.y)
                } else {
                    (rect.width() / size.x).max(rect.height() / size.y)
                };
                let shown = Rect::from_center_size(rect.center(), size * scale);
                let visible = shown.intersect(rect);
                let uv = Rect::from_min_max(
                    Pos2::new(
                        (visible.left() - shown.left()) / shown.width(),
                        (visible.top() - shown.top()) / shown.height(),
                    ),
                    Pos2::new(
                        (visible.right() - shown.left()) / shown.width(),
                        (visible.bottom() - shown.top()) / shown.height(),
                    ),
                );
                painter.add(egui::epaint::RectShape {
                    rect: visible,
                    rounding: egui::Rounding::same(8.0),
                    fill: Color32::WHITE,
                    stroke: Stroke::NONE,
                    fill_texture_id: stream.texture.id(),
                    uv,
                });
            }
            if stream.screen {
                live_badge(ui, &painter, rect.left_top() + Vec2::splat(8.0));
            }
            if stream.pinned {
                let pin = Rect::from_min_size(
                    rect.right_top() + Vec2::new(-30.0, 8.0),
                    Vec2::splat(22.0),
                );
                painter.circle_filled(pin.center(), 11.0, Color32::from_black_alpha(153));
                draw_icon(&painter, pin.shrink(5.0), Icon::Pin, Color32::WHITE);
            }
            name_pill(ui, &painter, rect, profile_ui::display_name(stream.user), &[]);
            tile_ring(&painter, rect, false, hovered);
        }
        Tile::Waiting(waiting) => paint_waiting(ui, &painter, rect, waiting, hovered),
    }
    if dim {
        painter.rect_filled(rect, 8.0, stage_fill().gamma_multiply(0.45));
    }
}

/// A shared screen that has not arrived here: the sharer's avatar, a line
/// saying who shares, and Discord's white "Watch Stream" pill.
fn paint_waiting(
    ui: &egui::Ui,
    painter: &egui::Painter,
    rect: Rect,
    waiting: &WaitingTile,
    hovered: bool,
) {
    painter.rect_filled(rect, 8.0, Color32::from_rgb(17, 18, 20));
    let name = profile_ui::display_name(waiting.user);
    let size = (rect.width() * 0.12)
        .min(rect.height() * 0.28)
        .clamp(20.0, 72.0);
    let line = font_size(14.0) * 1.3;
    let pill_height = 32.0;
    let block = size + 10.0 + line + 10.0 + pill_height;
    let top = rect.center().y - block * 0.5;
    let avatar = Rect::from_center_size(
        Pos2::new(rect.center().x, top + size * 0.5),
        Vec2::splat(size),
    );
    profile_ui::paint_user_avatar(ui, avatar, waiting.user, waiting.avatar.as_ref(), false);
    let mut job = egui::text::LayoutJob::simple(
        format!("{name} is sharing their screen"),
        semibold(ui, font_size(14.0)),
        Color32::WHITE,
        (rect.width() - 24.0).max(16.0),
    );
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    let galley = ui.painter().layout_job(job);
    let text_top = avatar.bottom() + 10.0;
    painter.galley(
        Pos2::new(rect.center().x - galley.size().x * 0.5, text_top),
        galley,
        Color32::WHITE,
    );
    let offer = waiting.watch == Watch::Offer;
    let label = painter.layout_no_wrap(
        waiting.watch.caption().into(),
        semibold(ui, font_size(14.0)),
        if offer {
            Color32::from_rgb(17, 18, 20)
        } else {
            Color32::from_gray(220)
        },
    );
    let pill = Rect::from_center_size(
        Pos2::new(rect.center().x, text_top + line + 10.0 + pill_height * 0.5),
        Vec2::new(label.size().x + 32.0, pill_height),
    );
    if offer {
        let fill = if hovered {
            Color32::from_gray(222)
        } else {
            Color32::WHITE
        };
        painter.rect_filled(pill, 6.0, fill);
    } else {
        painter.rect_filled(pill, 6.0, Color32::from_white_alpha(18));
    }
    painter.galley(pill.center() - label.size() * 0.5, label, Color32::WHITE);
    if rect.height() > 90.0 {
        live_badge(ui, painter, rect.left_top() + Vec2::splat(8.0));
    }
    tile_ring(painter, rect, false, hovered && offer);
}

/// What the name pill shows after the name.
#[derive(Clone, Copy)]
enum Mark {
    /// Muted or deafened: red, with a slash.
    Off(Icon),
    On(Icon),
    Live,
}

const MARK: f32 = 16.0;
const MARK_GAP: f32 = 6.0;

fn live_text(ui: &egui::Ui) -> Arc<egui::Galley> {
    ui.painter()
        .layout_no_wrap("LIVE".into(), semibold(ui, font_size(11.0)), Color32::WHITE)
}

/// Discord's red "LIVE" badge with its top-left corner at `at`; returns its width.
fn live_badge(ui: &egui::Ui, painter: &egui::Painter, at: Pos2) -> f32 {
    let galley = live_text(ui);
    let badge = Rect::from_min_size(at, galley.size() + Vec2::new(10.0, 4.0));
    painter.rect_filled(badge, 4.0, c_bad());
    painter.galley(badge.min + Vec2::new(5.0, 2.0), galley, Color32::WHITE);
    badge.width()
}

/// The name in a translucent black pill at the bottom-left, with the mute
/// and deafen marks after it.
fn name_pill(ui: &egui::Ui, painter: &egui::Painter, tile: Rect, name: &str, marks: &[Mark]) {
    const PAD: f32 = 8.0;
    let live = live_text(ui).size();
    let marks_width: f32 = marks
        .iter()
        .map(|mark| match mark {
            Mark::Live => live.x + 10.0 + MARK_GAP,
            _ => MARK + MARK_GAP,
        })
        .sum();
    let room = (tile.width() - 4.0 * PAD - marks_width).max(16.0);
    let mut job = egui::text::LayoutJob::simple(
        name.to_owned(),
        semibold(ui, font_size(14.0)),
        Color32::WHITE,
        room,
    );
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    let galley = ui.painter().layout_job(job);
    let height = (galley.size().y + 6.0).max(MARK + 8.0);
    let pill = Rect::from_min_size(
        tile.left_bottom() + Vec2::new(PAD, -PAD - height),
        Vec2::new(galley.size().x + marks_width + 2.0 * PAD - if marks.is_empty() { 0.0 } else { MARK_GAP }, height),
    );
    painter.rect_filled(pill, 6.0, Color32::from_black_alpha(153));
    let mut x = pill.left() + PAD;
    painter.galley(
        Pos2::new(x, pill.center().y - galley.size().y * 0.5),
        galley.clone(),
        Color32::WHITE,
    );
    x += galley.size().x + MARK_GAP;
    for mark in marks {
        let glyph = Rect::from_min_size(
            Pos2::new(x, pill.center().y - MARK * 0.5),
            Vec2::splat(MARK),
        );
        match *mark {
            Mark::Off(shape) => {
                draw_icon(painter, glyph.shrink(1.0), shape, c_bad());
                painter.line_segment(
                    [glyph.left_top(), glyph.right_bottom()],
                    Stroke::new(1.6_f32, c_bad()),
                );
                x += MARK + MARK_GAP;
            }
            Mark::On(shape) => {
                draw_icon(painter, glyph.shrink(1.0), shape, Color32::WHITE);
                x += MARK + MARK_GAP;
            }
            Mark::Live => {
                let top = pill.center().y - (live.y + 4.0) * 0.5;
                x += live_badge(ui, painter, Pos2::new(x, top)) + MARK_GAP;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn quality_help_needs_recent_repeated_late_audio_and_expires() {
        let now = Instant::now();
        let mut watch = QualityWatch::default();
        assert!(!watch.observe(now, 0, 0));
        assert!(!watch.observe(now + Duration::from_secs(2), 200, 1));
        assert!(!watch.observe(now + Duration::from_secs(4), 400, 2));
        assert!(watch.observe(now + Duration::from_secs(6), 600, 30));
        assert!(watch.observe(now + Duration::from_secs(8), 800, 30));
        assert!(!watch.observe(now + Duration::from_secs(17), 1600, 30));
    }
    #[test]
    fn a_new_call_and_sparse_idle_traffic_do_not_offer_spurious_quality_help() {
        let now = Instant::now();
        let mut watch = QualityWatch::default();
        assert!(!watch.observe(now, 0, 0));
        assert!(!watch.observe(now + Duration::from_secs(2), 4, 5));
        assert!(watch.observe(now + Duration::from_secs(4), 204, 35));
        assert!(!watch.observe(now + Duration::from_secs(5), 1, 0));
        assert!(!watch.observe(now + Duration::from_secs(8), 201, 0));
    }

    fn row(name: &str, muted: bool, heard: bool, seconds: u64) -> SoundRow<'_> {
        SoundRow {
            name,
            muted,
            last_received: heard.then_some(Duration::ZERO),
            together: Duration::from_secs(seconds),
        }
    }

    #[test]
    fn the_sound_check_says_who_you_hear() {
        assert_eq!(hearing(&row("Mira", false, true, 5)), Hearing::Heard);
        // A current mute takes precedence over previously received speech.
        assert_eq!(hearing(&row("Mira", true, true, 5)), Hearing::Muted);
        assert_eq!(hearing(&row("Jonas", true, false, 60)), Hearing::Muted);
        assert_eq!(hearing(&row("Jonas", false, false, 60)), Hearing::NotYet);
    }

    #[test]
    fn received_speech_expires_and_a_new_pause_can_offer_a_sound_check() {
        let mut speaker = row("Mira", false, true, 60);
        speaker.last_received = Some(Duration::from_secs(5));
        assert_eq!(hearing(&speaker), Hearing::Heard);
        speaker.last_received = Some(Duration::from_secs(6));
        assert_eq!(hearing(&speaker), Hearing::Quiet);
        assert!(sound_hint(false, &[speaker]).is_none());
        let speaker = SoundRow {
            name: "Mira",
            muted: false,
            last_received: Some(Duration::from_secs(20)),
            together: Duration::from_secs(60),
        };
        assert!(sound_hint(false, &[speaker])
            .unwrap()
            .contains("Ask them to say something"));
    }

    #[test]
    fn a_hint_appears_only_when_someone_unmuted_stays_silent_for_a_while() {
        assert!(
            sound_hint(false, &[row("Jonas", false, false, 5)]).is_none(),
            "too early"
        );
        assert!(
            sound_hint(false, &[row("Jonas", true, false, 60)]).is_none(),
            "muted"
        );
        assert!(
            sound_hint(false, &[row("Jonas", false, true, 60)]).is_none(),
            "heard"
        );
        let hint = sound_hint(
            false,
            &[row("Mira", false, true, 60), row("Jonas", false, false, 25)],
        )
        .expect("Jonas has been silent for 25 seconds");
        assert!(hint.contains("Can't hear Jonas"), "{hint}");
        assert!(hint.contains("your output device and volume"), "{hint}");
        assert!(!hint.contains("on their side"), "{hint}");
        // Deafened: that explains it, whoever is talking.
        let deaf = sound_hint(true, &[row("Mira", false, true, 60)]).unwrap();
        assert!(deaf.contains("deafened"), "{deaf}");
    }

    #[test]
    fn the_sound_check_wraps_in_a_narrow_call_window() {
        for (width, font_scale) in [(940.0, 1.0), (420.0, 1.0), (360.0, 1.4)] {
            let ctx = egui::Context::default();
            crate::appearance::AppearanceSettings {
                font_scale,
                ..Default::default()
            }
            .apply(&ctx, None);
            let rows = [
                row("Mira", false, true, 60),
                row("A rather long display name", false, false, 60),
                row("Jonas", true, false, 60),
            ];
            let hint = sound_hint(false, &rows);
            for _ in 0..3 {
                let mut used = egui::Rect::NOTHING;
                let _ = ctx.run(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            Vec2::new(width, 800.0),
                        )),
                        ..Default::default()
                    },
                    |ctx| {
                        egui::CentralPanel::default().show(ctx, |ui| {
                            sound_check(ui, &rows, hint.as_deref());
                            assert!(
                                !steadier_audio_help(ui),
                                "drawing the help must not choose a setting"
                            );
                            used = ui.min_rect();
                        });
                    },
                );
                assert!(
                    used.right() <= width + 0.5,
                    "{width} x{font_scale}: {used:?}"
                );
            }
        }
    }

    fn user(n: usize) -> User {
        serde_json::from_value(serde_json::json!({
            "id": format!("u{n}"),
            "username": format!("participant-with-a-long-name-{n}"),
            "role": "member"
        }))
        .expect("artificial user")
    }

    #[test]
    fn grid_fits_16_9_tiles_with_at_most_four_per_row() {
        for (count, columns) in [(1, 1), (2, 2), (3, 2), (4, 2), (5, 3), (9, 3), (12, 4), (16, 4)] {
            let grid = room_grid(1200.0, 600.0, count);
            assert_eq!(grid.columns, columns, "{count} participants");
        }
        // Two people sit side by side even where stacking would be slightly larger.
        assert_eq!(room_grid(960.0, 570.0, 2).columns, 2);
        // One person fills the height, at 16:9.
        let one = room_grid(1200.0, 600.0, 1);
        assert!((one.tile.y - 600.0).abs() < 0.01, "{one:?}");
        for width in [120.0, 200.0, 219.0, 360.0, 652.0, 940.0, 1400.0] {
            for height in [0.0, 300.0, 650.0] {
                for count in [0, 1, 3, 4, 9, 20] {
                    let grid = room_grid(width, height, count);
                    let case = format!("{count} in {width}x{height}: {grid:?}");
                    let used = grid.tile.x * grid.columns as f32
                        + TILE_GAP * (grid.columns - 1) as f32;
                    assert!(used <= width + 0.01, "{case}");
                    assert!(grid.columns <= MAX_COLUMNS, "{case}");
                    assert!((grid.tile.y - grid.tile.x * 9.0 / 16.0).abs() < 0.01, "{case}");
                    let fits = grid.height(count) <= height + 0.01;
                    assert!(fits || grid.tile.x <= TILE_MIN_WIDTH + 0.01, "{case}");
                }
            }
        }
        // Without height the smallest tiles fill as many columns as fit.
        assert_eq!(room_grid(700.0, 0.0, 9).columns, 4);
        // Below the smallest tile a single shrinking column remains.
        assert_eq!(room_grid(120.0, 0.0, 9).columns, 1);
        assert_eq!(room_grid(120.0, 0.0, 9).tile.x, 120.0);
    }

    #[test]
    fn a_focused_tile_is_large_with_the_others_in_a_strip_below() {
        let (rects, height) = tile_layout(900.0, 600.0, 4, Some(2));
        assert_eq!(rects.len(), 4);
        let big = rects[2];
        assert_eq!(big.top(), 0.0);
        assert!((big.center().x - 450.0).abs() < 0.5, "centred: {big:?}");
        assert!((big.height() - big.width() * 9.0 / 16.0).abs() < 0.01);
        for (index, small) in rects.iter().enumerate().filter(|(index, _)| *index != 2) {
            assert!(small.width() < big.width(), "{index}: {small:?}");
            assert!(small.top() > big.bottom(), "{index}: {small:?}");
            assert!(small.left() >= 0.0 && small.right() <= 900.0 + 0.01);
        }
        assert!(height <= 600.0 + 0.01, "fits: {height}");
        assert!((height - rects[0].bottom()).abs() < 0.01);
        // Without others the focused tile is the whole layout.
        let (alone, height) = tile_layout(900.0, 600.0, 1, Some(0));
        assert_eq!(alone[0].height(), height);
        // An unknown focus falls back to the grid.
        let (grid, _) = tile_layout(900.0, 600.0, 4, Some(9));
        assert_eq!(grid[0].size(), grid[3].size());
    }

    #[test]
    fn status_shows_real_states_and_never_repeats_voice_connected() {
        let joined = RoomConnection {
            joined: true,
            ..Default::default()
        };
        let opening = room_status(joined).unwrap();
        assert!(opening.starts_with("Opening your audio devices"));
        let connecting = room_status(RoomConnection {
            media_ready: true,
            ..joined
        });
        assert!(connecting.unwrap().starts_with("Connecting voice"));
        let connected = RoomConnection {
            media_ready: true,
            connected: true,
            ..joined
        };
        assert_eq!(room_status(connected), None);
        let reconnecting = room_status(RoomConnection {
            reconnecting: true,
            ..connected
        });
        assert!(reconnecting.unwrap().starts_with("Reconnecting voice"));
        let paused = room_status(RoomConnection {
            encryption_paused: true,
            ..connected
        });
        assert_eq!(
            paused,
            Some("Voice is paused. Check the call message below.")
        );
        let recovering = room_status(RoomConnection {
            reconnecting: true,
            encryption_paused: true,
            ..connected
        })
        .unwrap();
        assert!(recovering.starts_with("Reconnecting voice"));
        assert!(!recovering.contains("verified"));
        assert_eq!(room_status(RoomConnection::default()), None);
    }

    #[test]
    fn tile_status_keeps_mute_deafen_and_speaking() {
        let person = user(1);
        let tile = |muted, deafened, speaking| ParticipantTile {
            user: &person,
            avatar: None,
            speaking,
            muted,
            deafened,
            sharing: false,
            camera: false,
        };
        assert_eq!(tile_status(&tile(false, false, false)), "Connected");
        assert_eq!(tile_status(&tile(false, false, true)), "Speaking");
        assert_eq!(tile_status(&tile(true, false, false)), "Mic off");
        assert_eq!(tile_status(&tile(true, true, false)), "Deafened");
    }

    #[test]
    fn stream_tiles_name_their_kind() {
        let person = user(1);
        let ctx = egui::Context::default();
        let texture = ctx.load_texture(
            "stream-test",
            egui::ColorImage::new([16, 9], Color32::BLACK),
            Default::default(),
        );
        let tile = |screen, preview| StreamTile {
            user: &person,
            texture: texture.clone(),
            screen,
            preview,
            pinned: false,
        };
        assert_eq!(stream_kind(&tile(true, false)), "Screen share");
        assert_eq!(stream_kind(&tile(false, false)), "Camera");
        assert_eq!(stream_kind(&tile(true, true)), "Screen preview");
        assert_eq!(stream_kind(&tile(false, true)), "Camera preview");
        assert_eq!(
            tile_label(&Tile::Stream(tile(true, false))),
            "participant-with-a-long-name-1, Screen share"
        );
    }

    /// The real page shape: a bottom control panel and a scrolling centre with
    /// the header, the grid centred in what is left and the page's closing gap.
    /// `scroll` scrolls the page down over later frames. Also returns where the
    /// grid's space starts, below the header, and where the visible page ends.
    /// (While the content fits, egui clips it to the whole panel, so the clip
    /// rectangle can reach behind the controls; the content's own maximum
    /// rectangle is the visible page.)
    fn render(
        size: Vec2,
        text: f32,
        count: usize,
        scroll: f32,
    ) -> (Rect, Vec<Rect>, Rect, Rect, f32, f32) {
        UI_FONT_SCALE.with(|scale| scale.set(text));
        let users: Vec<_> = (0..count).map(user).collect();
        let ctx = egui::Context::default();
        configure(&ctx);
        let mut screen = Rect::NOTHING;
        let mut tiles = Vec::new();
        let mut clip = Rect::NOTHING;
        let mut control = Rect::NOTHING;
        let mut top = 0.0;
        let mut bottom = 0.0;
        let frames = if scroll > 0.0 { 20 } else { 3 };
        for frame in 0..frames {
            let events = if scroll > 0.0 && (3..13).contains(&frame) {
                vec![
                    egui::Event::PointerMoved(Pos2::new(size.x * 0.5, size.y * 0.3)),
                    egui::Event::Scroll(Vec2::new(0.0, -scroll)),
                ]
            } else {
                Vec::new()
            };
            let _ = ctx.run(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, size)),
                    events,
                    ..Default::default()
                },
                |ctx| {
                    screen = ctx.screen_rect();
                    egui::CentralPanel::default().show(ctx, |ui| {
                        egui::TopBottomPanel::bottom("voice-controls-test").show_inside(ui, |ui| {
                            control = ui.add(primary("Join voice")).rect;
                        });
                        egui::ScrollArea::vertical().show(ui, |ui| {
                            ui.label("Connecting voice…");
                            let data: Vec<_> = users
                                .iter()
                                .enumerate()
                                .map(|(n, user)| {
                                    Tile::Person(ParticipantTile {
                                        user,
                                        avatar: None,
                                        speaking: n == 1,
                                        muted: n == 2,
                                        deafened: n == 3,
                                        sharing: n == 0,
                                        camera: n == 0,
                                    })
                                })
                                .collect();
                            ui.add_space(28.0);
                            top = ui.cursor().top();
                            let space = ui.available_height() - 28.0;
                            tiles = participant_grid(ui, &data, &[], None, space, 0.0, false)
                                .into_iter()
                                .map(|response| response.rect)
                                .collect();
                            ui.add_space(28.0);
                            clip = ui.clip_rect();
                            bottom = ui.max_rect().bottom();
                        });
                    });
                },
            );
        }
        UI_FONT_SCALE.with(|scale| scale.set(1.0));
        (screen, tiles, clip, control, top, bottom)
    }

    #[test]
    fn tiles_fit_small_windows_large_text_and_keep_controls_visible() {
        for size in [
            Vec2::new(940.0, 650.0),
            Vec2::new(360.0, 420.0),
            Vec2::new(210.0, 420.0),
        ] {
            for text in [1.0, 1.4] {
                for count in [0, 1, 4, 9] {
                    let (screen, tiles, clip, control, _, _) = render(size, text, count, 0.0);
                    let case = format!("{size:?} text {text} with {count}");
                    assert_eq!(tiles.len(), count, "{case}");
                    for tile in &tiles {
                        assert!(
                            tile.left() >= clip.left() - 0.5 && tile.right() <= clip.right() + 0.5,
                            "{case}: tile {tile:?} outside clip {clip:?}"
                        );
                    }
                    assert!(
                        screen.contains_rect(control),
                        "{case}: controls {control:?} hidden"
                    );
                    if count == 4 && size.x >= 900.0 {
                        let left = tiles[0].left() - clip.left();
                        let right = clip.right() - tiles[1].right();
                        assert!((left - right).abs() < 2.0, "{case}: not centred");
                        assert_eq!(tiles[0].top(), tiles[1].top(), "{case}: 2x2");
                        assert!(tiles[2].top() > tiles[0].bottom(), "{case}: 2x2");
                    }
                }
            }
        }
    }

    #[test]
    fn call_buttons_follow_discord_states() {
        assert_eq!(call_style(Icon::Mic, false, false), CallStyle::Idle);
        assert_eq!(call_style(Icon::Mic, true, false), CallStyle::Off);
        assert_eq!(call_style(Icon::Headphones, true, false), CallStyle::Off);
        assert_eq!(call_style(Icon::Camera, true, false), CallStyle::Live);
        assert_eq!(call_style(Icon::Screen, true, false), CallStyle::Live);
        assert_eq!(call_style(Icon::HangUp, false, true), CallStyle::Leave);
        // Five 56-point buttons, three normal gaps and the wider one before Disconnect.
        assert_eq!(call_bar_width(5), 5.0 * 56.0 + 3.0 * 8.0 + 16.0);
        assert_eq!(call_bar_width(1), 56.0);
    }

    #[test]
    fn vertical_inset_centres_only_what_fits() {
        assert_eq!(vertical_inset(500.0, 200.0, 16.0), 150.0);
        // Close to full: the tail below the grid still fits, no scrollbar.
        assert_eq!(vertical_inset(500.0, 480.0, 16.0), 4.0);
        assert_eq!(vertical_inset(500.0, 490.0, 16.0), 0.0);
        assert_eq!(vertical_inset(500.0, 900.0, 16.0), 0.0);
        assert_eq!(vertical_inset(-40.0, 200.0, 16.0), 0.0);
        assert_eq!(vertical_inset(0.0, 0.0, 16.0), 0.0);
    }

    #[test]
    fn a_fitting_grid_is_centred_below_the_header_and_a_tall_one_scrolls_from_the_top() {
        for (size, text, count) in [
            (Vec2::new(940.0, 650.0), 1.0, 1),
            (Vec2::new(940.0, 650.0), 1.0, 4),
            (Vec2::new(940.0, 650.0), 1.4, 1),
            (Vec2::new(940.0, 650.0), 1.4, 4),
            (Vec2::new(500.0, 900.0), 1.0, 1),
            (Vec2::new(500.0, 900.0), 1.4, 2),
        ] {
            let case = format!("{size:?} text {text} with {count}");
            let (_, tiles, _, _, top, bottom) = render(size, text, count, 0.0);
            let above = tiles[0].top() - top;
            let below = (bottom - 28.0) - tiles[count - 1].bottom();
            // A wide window fills its height; a tall one moves the grid down
            // (top-aligned would be 0).
            if size.y > size.x {
                assert!(above > 0.5, "{case}: the grid is not moved down ({above})");
            }
            // Centred: the grid takes exactly its rows' height.
            assert!(below >= -0.5, "{case}: {below} below the grid");
            assert!(
                (above - below).abs() < 1.0,
                "{case}: {above} above and {below} below the grid"
            );
            assert!(tiles[count - 1].bottom() <= bottom, "{case}: no scrolling");
        }
        // Nine people in a short window with large text do not fit: the grid
        // starts right below the header, the first row is fully visible, and
        // the page still scrolls to the last tile.
        let short = Vec2::new(360.0, 420.0);
        let (_, tiles, clip, _, top, _) = render(short, 1.4, 9, 0.0);
        assert!((tiles[0].top() - top).abs() < 0.5, "starts at the top");
        assert!(tiles[0].top() >= clip.top(), "first row visible");
        assert!(tiles[8].bottom() > clip.bottom(), "needs scrolling");
        let (_, tiles, clip, _, _, _) = render(short, 1.4, 9, 400.0);
        assert!(
            tiles[8].bottom() <= clip.bottom() + 0.5 && tiles[8].top() >= clip.top() - 0.5,
            "scrolled to the last tile: {:?} in {clip:?}",
            tiles[8]
        );
    }
}

/// How a round call button is filled. Discord: grey at rest, white with a
/// dark glyph while your camera or screen runs, red with a slash while you
/// are muted or deafened, red to leave and green to answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CallStyle {
    Idle,
    Live,
    Off,
    Leave,
    Answer,
}

/// The call bar's button diameter, as in Discord.
pub(super) const CALL_BUTTON: f32 = 56.0;
/// Space between call buttons, and the wider gap before Disconnect.
pub(super) const CALL_GAP: f32 = 8.0;
pub(super) const CALL_LEAVE_GAP: f32 = 16.0;

/// The style `call_control` draws for a call-bar state.
pub(super) fn call_style(shape: Icon, selected: bool, disconnect: bool) -> CallStyle {
    if disconnect {
        CallStyle::Leave
    } else if selected && matches!(shape, Icon::Camera | Icon::Screen) {
        CallStyle::Live
    } else if selected {
        CallStyle::Off
    } else {
        CallStyle::Idle
    }
}

/// `a` moved towards `b` by `t` (0..1), per channel.
fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let lerp = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgba_unmultiplied(
        lerp(a.r(), b.r()),
        lerp(a.g(), b.g()),
        lerp(a.b(), b.b()),
        lerp(a.a(), b.a()),
    )
}

/// One round call button. A disabled `ui` fades it and ignores clicks.
pub(super) fn call_button(
    ui: &mut egui::Ui,
    shape: Icon,
    style: CallStyle,
    label: &str,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(CALL_BUTTON), Sense::click());
    let (fill, ink) = match style {
        CallStyle::Idle => (c_hover(), c_text()),
        CallStyle::Live => (c_text(), c_bg()),
        CallStyle::Off | CallStyle::Leave => (c_bad(), Color32::WHITE),
        CallStyle::Answer => (c_good(), Color32::WHITE),
    };
    let fill = match (response.hovered(), style) {
        (false, _) => fill,
        (true, CallStyle::Idle) => mix(fill, c_text(), 0.12),
        (true, _) => mix(fill, Color32::BLACK, 0.14),
    };
    let painter = ui.painter();
    let radius = CALL_BUTTON * 0.5;
    painter.circle_filled(rect.center(), radius, fill);
    if response.has_focus() {
        painter.circle_stroke(rect.center(), radius + 2.0, Stroke::new(2.0_f32, c_accent()));
    }
    let glyph = Rect::from_center_size(rect.center(), Vec2::splat(22.0));
    draw_icon(painter, glyph, shape, ink);
    if style == CallStyle::Off {
        strike(painter, glyph.expand(1.0), fill, ink);
    }
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, label));
    response.on_hover_text(label)
}

/// A diagonal slash with a gap in the background colour, so it reads as "off".
fn strike(painter: &egui::Painter, glyph: Rect, background: Color32, ink: Color32) {
    let line = [glyph.left_top(), glyph.right_bottom()];
    painter.line_segment(line, Stroke::new(5.0_f32, background));
    painter.line_segment(line, Stroke::new(2.0_f32, ink));
}

/// A round call-bar button (signature shared with the icon capture panel):
/// red for disconnect and for an active mute/deafen, white while camera or
/// screen share runs, grey otherwise.
pub(super) fn call_control(
    ui: &mut egui::Ui,
    shape: Icon,
    selected: bool,
    disconnect: bool,
    label: &str,
) -> egui::Response {
    call_button(ui, shape, call_style(shape, selected, disconnect), label)
}

/// The width of a centred call bar with `buttons` buttons, Disconnect last.
pub(super) fn call_bar_width(buttons: usize) -> f32 {
    let buttons = buttons.max(1);
    let leave = if buttons > 1 { CALL_LEAVE_GAP } else { 0.0 };
    CALL_BUTTON * buttons as f32 + CALL_GAP * buttons.saturating_sub(2) as f32 + leave
}

/// True for the dark themes (Dark, Onyx, Ash), where the call gets Discord's
/// black stage. The light theme keeps its page colours, so text drawn in
/// theme colours stays readable there.
fn dark_theme() -> bool {
    let bg = c_bg();
    u16::from(bg.r()) + u16::from(bg.g()) + u16::from(bg.b()) < 384
}

/// The call stage behind the tiles and the call bar: black, as Discord's call view.
pub(super) fn stage_fill() -> Color32 {
    if dark_theme() {
        Color32::BLACK
    } else {
        c_bg()
    }
}

/// A person's tile on the stage: one step up from the stage.
pub(super) fn tile_fill() -> Color32 {
    if dark_theme() {
        Color32::from_rgb(36, 37, 41)
    } else {
        c_card()
    }
}

/// Text drawn straight on the stage.
pub(super) fn stage_ink() -> Color32 {
    if dark_theme() {
        Color32::from_gray(225)
    } else {
        c_text()
    }
}

/// The smaller round buttons at the right end of the call bar.
pub(super) const BAR_TOOL: f32 = 40.0;

/// Which picture a bar tool shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum BarTool {
    /// Switch back to the grid of everyone.
    Grid,
    /// Switch to one large stream with the others below.
    Focus,
    FullScreen,
    ExitFullScreen,
    Settings,
}

/// A quiet round button for the bar's right end: no fill at rest.
pub(super) fn bar_tool(ui: &mut egui::Ui, tool: BarTool, label: &str) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(BAR_TOOL), Sense::click());
    let painter = ui.painter();
    if response.hovered() || response.has_focus() {
        painter.circle_filled(rect.center(), BAR_TOOL * 0.5, tile_fill());
    }
    if response.has_focus() {
        painter.circle_stroke(
            rect.center(),
            BAR_TOOL * 0.5 + 1.0,
            Stroke::new(2.0_f32, c_accent()),
        );
    }
    let glyph = Rect::from_center_size(rect.center(), Vec2::splat(18.0));
    paint_tool(painter, glyph, tool, stage_ink());
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, label));
    response.on_hover_text(label)
}

fn paint_tool(painter: &egui::Painter, r: Rect, tool: BarTool, ink: Color32) {
    let stroke = Stroke::new(1.7_f32, ink);
    match tool {
        BarTool::Grid => {
            let half = (r.width() - 3.0) * 0.5;
            for (dx, dy) in [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0)] {
                let min = r.min + Vec2::new(dx * (half + 3.0), dy * (half + 3.0));
                painter.rect_stroke(Rect::from_min_size(min, Vec2::splat(half)), 1.5, stroke);
            }
        }
        BarTool::Focus => {
            let big = Rect::from_min_max(r.min, Pos2::new(r.max.x, r.min.y + r.height() * 0.62));
            painter.rect_stroke(big, 1.5, stroke);
            let w = (r.width() - 4.0) / 3.0;
            for i in 0..3 {
                let min = Pos2::new(r.min.x + i as f32 * (w + 2.0), big.max.y + 2.5);
                painter.rect_stroke(
                    Rect::from_min_max(min, Pos2::new(min.x + w, r.max.y)),
                    1.0,
                    stroke,
                );
            }
        }
        BarTool::FullScreen | BarTool::ExitFullScreen => {
            let l = r.width() * 0.32;
            let out = tool == BarTool::FullScreen;
            for (corner, dir) in [
                (r.left_top(), Vec2::new(1.0, 1.0)),
                (r.right_top(), Vec2::new(-1.0, 1.0)),
                (r.left_bottom(), Vec2::new(1.0, -1.0)),
                (r.right_bottom(), Vec2::new(-1.0, -1.0)),
            ] {
                // Full screen: corners at the edge, arms inwards. Exit: the
                // corner pulled in, arms pointing back out.
                let (at, sign) = if out { (corner, 1.0) } else { (corner + dir * l, -1.0) };
                painter.line_segment([at, at + Vec2::new(sign * dir.x * l, 0.0)], stroke);
                painter.line_segment([at, at + Vec2::new(0.0, sign * dir.y * l)], stroke);
            }
        }
        BarTool::Settings => draw_icon(painter, r, Icon::Settings, ink),
    }
}

/// How the line above the tiles looks while a call starts or recovers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum StatusTone {
    /// Opening devices or connecting: a spinner, neutral.
    Working,
    /// Reconnecting: a spinner, amber, as Discord's connecting state.
    Recovering,
    /// Paused: red, no spinner.
    Paused,
}

/// The tone for the status line `room_status` gives.
pub(super) fn status_tone(state: RoomConnection) -> StatusTone {
    if state.reconnecting {
        StatusTone::Recovering
    } else if state.encryption_paused {
        StatusTone::Paused
    } else {
        StatusTone::Working
    }
}

/// Discord's amber for a connection that is recovering (8.9:1 on black).
pub(super) const AMBER: Color32 = Color32::from_rgb(240, 178, 50);

/// The status as a centred pill on the stage: a spinner (unless paused) and text.
pub(super) fn status_pill(ui: &mut egui::Ui, text: &str, tone: StatusTone) {
    let ink = match tone {
        StatusTone::Working => stage_ink(),
        StatusTone::Recovering if dark_theme() => AMBER,
        StatusTone::Recovering => Color32::from_rgb(138, 90, 0),
        StatusTone::Paused => c_bad(),
    };
    let max = (ui.available_width() - 48.0).clamp(120.0, 640.0);
    egui::Frame::none()
        .fill(tile_fill())
        .rounding(16.0)
        .inner_margin(Margin::symmetric(14.0, 7.0))
        .show(ui, |ui| {
            ui.set_max_width(max);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                if tone != StatusTone::Paused {
                    ui.add(egui::Spinner::new().size(14.0).color(ink));
                }
                ui.add(
                    egui::Label::new(RichText::new(text).size(font_size(13.0)).color(ink))
                        .wrap(true),
                );
            });
        });
}

/// The bar across the top of the stage while you share your screen, with
/// Stop Sharing one click away. Returns true when Stop Sharing is clicked.
pub(super) fn sharing_bar(ui: &mut egui::Ui, source: Option<&str>) -> bool {
    let mut stop = false;
    egui::Frame::none()
        .fill(tile_fill())
        .rounding(8.0)
        .inner_margin(Margin::symmetric(14.0, 8.0))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let (dot, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
                ui.painter().circle_filled(dot.center(), 5.0, c_bad());
                ui.label(
                    RichText::new("You are sharing your screen")
                        .size(font_size(14.0))
                        .family(egui::FontFamily::Name("Relay Semibold".into()))
                        .color(stage_ink()),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let button = egui::Button::new(
                        RichText::new("Stop Sharing")
                            .size(font_size(14.0))
                            .color(Color32::WHITE),
                    )
                    .fill(c_bad())
                    .rounding(4.0)
                    .min_size(Vec2::new(0.0, 32.0));
                    stop = ui
                        .add(button)
                        .on_hover_text("Stop sharing your screen")
                        .clicked();
                    if let Some(source) = source {
                        ui.add(
                            egui::Label::new(
                                RichText::new(source).size(font_size(13.0)).color(c_muted()),
                            )
                            .truncate(true),
                        );
                    }
                });
            });
        });
    stop
}

/// Discord's connection bars: three bars, the first `level` (0..=3) lit in `color`.
pub(super) fn signal_bars(painter: &egui::Painter, rect: Rect, level: u8, color: Color32) {
    let width = (rect.width() - 4.0) / 3.0;
    for bar in 0..3u8 {
        let height = rect.height() * (0.45 + 0.275 * f32::from(bar));
        let x = rect.left() + f32::from(bar) * (width + 2.0);
        let r = Rect::from_min_max(
            Pos2::new(x, rect.bottom() - height),
            Pos2::new(x + width, rect.bottom()),
        );
        painter.rect_filled(
            r,
            1.0,
            if bar < level {
                color
            } else {
                color.gamma_multiply(0.3)
            },
        );
    }
}

/// The fixed call state a capture fixture shows. Capture only.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct CaptureCall {
    pub connected: bool,
    pub reconnecting: bool,
    pub sharing_screen: bool,
    pub video: bool,
}

/// What the call surfaces show about your own call.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct CallState {
    /// Audio devices are open in the call.
    pub ready: bool,
    pub connected: bool,
    pub reconnecting: bool,
    pub screen_live: bool,
    pub camera_live: bool,
    pub video_connected: bool,
    pub video_connecting: bool,
}

impl CaptureCall {
    pub(super) fn state(self) -> CallState {
        CallState {
            ready: self.connected || self.reconnecting,
            connected: self.connected,
            reconnecting: self.reconnecting,
            screen_live: self.sharing_screen,
            camera_live: false,
            video_connected: self.video,
            video_connecting: false,
        }
    }
}

/// What a waiting stream tile offers, from the own call and video state.
pub(super) fn watch_offer(joined: bool, call: CallState) -> Watch {
    if !joined {
        Watch::JoinFirst
    } else if call.video_connected {
        Watch::Arriving
    } else if call.video_connecting {
        Watch::Connecting
    } else {
        Watch::Offer
    }
}

#[cfg(test)]
mod stage_tests {
    use super::*;

    #[test]
    fn a_waiting_stream_offers_watch_only_when_video_can_be_connected() {
        let call = CallState::default();
        assert_eq!(watch_offer(false, call), Watch::JoinFirst);
        assert_eq!(watch_offer(true, call), Watch::Offer);
        let connecting = CallState {
            video_connecting: true,
            ..call
        };
        assert_eq!(watch_offer(true, connecting), Watch::Connecting);
        let connected = CallState {
            video_connected: true,
            ..call
        };
        assert_eq!(watch_offer(true, connected), Watch::Arriving);
        assert_eq!(Watch::Offer.caption(), "Watch Stream");
    }

    #[test]
    fn reconnecting_is_amber_and_a_pause_is_red() {
        let base = RoomConnection {
            joined: true,
            ..Default::default()
        };
        assert_eq!(status_tone(base), StatusTone::Working);
        assert_eq!(
            status_tone(RoomConnection {
                reconnecting: true,
                ..base
            }),
            StatusTone::Recovering
        );
        assert_eq!(
            status_tone(RoomConnection {
                encryption_paused: true,
                ..base
            }),
            StatusTone::Paused
        );
    }

    #[test]
    fn a_capture_fixture_never_claims_media_it_does_not_show() {
        let state = CaptureCall {
            connected: true,
            ..Default::default()
        }
        .state();
        assert!(state.ready && state.connected && !state.screen_live && !state.camera_live);
        let recovering = CaptureCall {
            reconnecting: true,
            ..Default::default()
        }
        .state();
        assert!(recovering.ready && recovering.reconnecting && !recovering.connected);
    }
}
