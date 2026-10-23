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

/// The line under the room name. A working call needs none: "Voice connected"
/// lives in the sidebar's connection panel, not repeated on the page.
pub(super) fn room_status(state: RoomConnection) -> Option<&'static str> {
    if state.reconnecting {
        Some("Reconnecting voice… Your mute and deafen settings are preserved. You can disconnect below.")
    } else if state.encryption_paused {
        Some("Voice is paused. Check the call message below.")
    } else if !state.joined {
        Some("Join voice to start talking.")
    } else if state.connected {
        None
    } else if state.media_ready {
        Some("Connecting voice… You can disconnect below.")
    } else {
        Some("Opening your audio devices… You can cancel below.")
    }
}

pub(super) fn participant_count(count: usize) -> String {
    match count {
        0 => "No one in voice".into(),
        1 => "1 person in voice".into(),
        n => format!("{n} people in voice"),
    }
}

pub(super) fn room_header(ui: &mut egui::Ui, name: &str, count: usize, status: Option<&str>) {
    ui.vertical_centered(|ui| {
        ui.add(
            egui::Label::new(
                RichText::new(name)
                    .size(font_size(26.0))
                    .family(egui::FontFamily::Name("Relay Semibold".into()))
                    .strong(),
            )
            .wrap(true),
        );
        ui.add_space(4.0);
        ui.label(
            RichText::new(participant_count(count))
                .size(font_size(14.0))
                .color(c_muted()),
        );
        if let Some(status) = status {
            ui.add_space(4.0);
            ui.add(
                egui::Label::new(RichText::new(status).size(font_size(13.0)).color(c_muted()))
                    .wrap(true),
            );
        }
    });
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

const TILE_GAP: f32 = 12.0;
const GRID_MARGIN: f32 = 24.0;
const TILE_MAX_WIDTH: f32 = 360.0;
const TILE_MIN_WIDTH: f32 = 150.0;

/// Columns and tile size; each row is centred in the available width.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct RoomGrid {
    pub columns: usize,
    pub tile: Vec2,
}

/// A balanced grid (1, 2×2, 3×3 …) that never needs more width than `width`.
pub(super) fn room_grid(width: f32, count: usize) -> RoomGrid {
    let width = width.max(1.0);
    let count = count.max(1);
    let balanced = (count as f32).sqrt().ceil() as usize;
    let fits = (((width + TILE_GAP) / (TILE_MIN_WIDTH + TILE_GAP)).floor() as usize).max(1);
    // 1-3 people share a row; from four on the grid is square-ish (2x2, 3x3).
    let preferred = if count <= 3 { count } else { balanced };
    let columns = preferred.min(fits).max(1);
    let gaps = TILE_GAP * (columns - 1) as f32;
    let tile_width = ((width - gaps) / columns as f32).min(TILE_MAX_WIDTH);
    RoomGrid {
        columns,
        tile: Vec2::new(tile_width, tile_height(tile_width)),
    }
}

/// As `room_grid`, with tiles made smaller until every row fits in `height`
/// (each row followed by `step`), so a full room never needs scrolling while
/// the window can hold it. Tiles never go below their minimum width.
pub(super) fn room_grid_in(width: f32, height: f32, count: usize, step: f32) -> RoomGrid {
    let mut grid = room_grid(width, count);
    let rows = count.max(1).div_ceil(grid.columns) as f32;
    while grid.tile.y * rows + step * rows > height && grid.tile.x > TILE_MIN_WIDTH {
        let width = (grid.tile.x - 10.0).max(TILE_MIN_WIDTH);
        grid.tile = Vec2::new(width, tile_height(width));
    }
    grid
}

fn avatar_size(tile_width: f32) -> f32 {
    (tile_width * 0.3).clamp(32.0, 96.0)
}

/// Avatar, name, status and the own microphone line, at the current text size.
fn tile_height(tile_width: f32) -> f32 {
    let lines = font_size(14.0) * 1.3 + font_size(12.0) * 1.3 * 2.0 + 10.0;
    (avatar_size(tile_width) + lines + 36.0).max(150.0)
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
    /// Only for the own tile: the live microphone level, 0 when muted.
    pub own_level: Option<f32>,
}

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

/// Draws the centred grid and returns one response per tile, in order, so the
/// caller keeps its profile and volume menus. `space` is the height left on the
/// page for the grid; a grid that fits is centred in it, 0 keeps it at the top.
pub(super) fn participant_grid(
    ui: &mut egui::Ui,
    tiles: &[ParticipantTile],
    space: f32,
) -> Vec<egui::Response> {
    // Each row advances by its height, the item spacing and TILE_GAP.
    let step = ui.spacing().item_spacing.y + TILE_GAP;
    // Keep a margin on both sides, so the tiles never touch the window edge.
    let grid = room_grid_in(
        (ui.available_width() - 2.0 * GRID_MARGIN).max(1.0),
        space,
        tiles.len(),
        step,
    );
    let rows = tiles.len().div_ceil(grid.columns);
    if rows > 0 {
        let height = grid.tile.y * rows as f32 + step * (rows - 1) as f32;
        ui.add_space(vertical_inset(space, height, step));
    }
    let mut responses = Vec::with_capacity(tiles.len());
    for row in tiles.chunks(grid.columns) {
        let row_width = grid.tile.x * row.len() as f32 + TILE_GAP * (row.len() - 1) as f32;
        let inset = ((ui.available_width() - row_width) * 0.5).max(0.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = TILE_GAP;
            ui.add_space(inset);
            for tile in row {
                let (rect, response) = ui.allocate_exact_size(grid.tile, Sense::click());
                paint_tile(ui, rect, tile);
                let name = profile_ui::display_name(tile.user);
                response.widget_info(|| {
                    egui::WidgetInfo::labeled(
                        egui::WidgetType::Button,
                        format!("{name}, {}", tile_status(tile)),
                    )
                });
                responses.push(response);
            }
        });
        ui.add_space(TILE_GAP);
    }
    responses
}

fn paint_tile(ui: &egui::Ui, rect: Rect, tile: &ParticipantTile) {
    let painter = ui.painter().with_clip_rect(rect.intersect(ui.clip_rect()));
    painter.rect_filled(rect, 8.0, c_card());
    painter.rect_stroke(
        rect,
        8.0,
        Stroke::new(
            if tile.speaking { 3.0_f32 } else { 1.0_f32 },
            if tile.speaking { c_good() } else { c_line() },
        ),
    );
    let pad = 12.0;
    let size = avatar_size(rect.width());
    let avatar = Rect::from_center_size(
        egui::pos2(rect.center().x, rect.top() + pad + 8.0 + size * 0.5),
        Vec2::splat(size),
    );
    profile_ui::paint_user_avatar(ui, avatar, tile.user, tile.avatar.as_ref(), tile.speaking);
    let text_width = rect.width() - pad * 2.0;
    let name = crate::settings_ui::single_line(
        ui,
        profile_ui::display_name(tile.user),
        14.0,
        c_text(),
        text_width,
    );
    let mut y = avatar.bottom() + 10.0;
    painter.galley(
        egui::pos2(rect.center().x - name.size().x * 0.5, y),
        name.clone(),
        c_text(),
    );
    y += name.size().y + 4.0;
    let status = crate::settings_ui::single_line(
        ui,
        tile_status(tile),
        12.0,
        if tile.speaking { c_good() } else { c_muted() },
        text_width,
    );
    painter.galley(
        egui::pos2(rect.center().x - status.size().x * 0.5, y),
        status.clone(),
        c_muted(),
    );
    y += status.size().y + 6.0;
    if let Some(level) = tile.own_level {
        let caption = crate::settings_ui::single_line(
            ui,
            "Your microphone level",
            12.0,
            c_muted(),
            text_width,
        );
        painter.galley(
            egui::pos2(rect.center().x - caption.size().x * 0.5, y),
            caption.clone(),
            c_muted(),
        );
        y += caption.size().y + 4.0;
        let bar = Rect::from_min_size(egui::pos2(rect.left() + pad, y), Vec2::new(text_width, 5.0));
        painter.rect_filled(bar, 3.0, c_line());
        painter.rect_filled(
            Rect::from_min_size(
                bar.min,
                Vec2::new(bar.width() * level.clamp(0.0, 1.0), bar.height()),
            ),
            3.0,
            c_accent(),
        );
    }
    // Indicators in the top corners: what the person shares, and what is off.
    let mut corner = rect.right_top() + Vec2::new(-pad - 16.0, pad);
    for (on, shape, color) in [
        (tile.deafened, Icon::Headphones, c_bad()),
        (tile.muted && !tile.deafened, Icon::Mic, c_bad()),
        (tile.camera, Icon::Camera, c_muted()),
        (tile.sharing, Icon::Screen, c_muted()),
    ] {
        if on {
            draw_icon(
                &painter,
                Rect::from_min_size(corner, Vec2::splat(16.0)),
                shape,
                color,
            );
            corner.x -= 22.0;
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
    fn grid_is_balanced_centred_and_never_wider_than_the_space() {
        for (count, columns) in [(1, 1), (2, 2), (3, 3), (4, 2), (5, 3), (9, 3), (12, 4)] {
            let grid = room_grid(1200.0, count);
            assert_eq!(grid.columns, columns, "{count} participants");
            assert!(grid.tile.x <= TILE_MAX_WIDTH);
        }
        for width in [120.0, 200.0, 219.0, 360.0, 652.0, 940.0] {
            for count in [0, 1, 4, 9] {
                let grid = room_grid(width, count);
                let used = grid.tile.x * grid.columns as f32 + TILE_GAP * (grid.columns - 1) as f32;
                assert!(used <= width + 0.01, "{count} in {width}: {grid:?}");
                assert!(grid.tile.x > 0.0 && grid.tile.y >= 150.0);
            }
        }
        // Below 220 points a single shrinking column remains.
        assert_eq!(room_grid(200.0, 9).columns, 1);
        assert_eq!(room_grid(200.0, 9).tile.x, 200.0);
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
        assert_eq!(
            room_status(RoomConnection::default()),
            Some("Join voice to start talking.")
        );
        assert_eq!(participant_count(0), "No one in voice");
        assert_eq!(participant_count(1), "1 person in voice");
        assert_eq!(participant_count(9), "9 people in voice");
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
            own_level: None,
        };
        assert_eq!(tile_status(&tile(false, false, false)), "Connected");
        assert_eq!(tile_status(&tile(false, false, true)), "Speaking");
        assert_eq!(tile_status(&tile(true, false, false)), "Mic off");
        assert_eq!(tile_status(&tile(true, true, false)), "Deafened");
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
                            room_header(ui, "Lounge", count, Some("Connecting voice…"));
                            let data: Vec<_> = users
                                .iter()
                                .enumerate()
                                .map(|(n, user)| ParticipantTile {
                                    user,
                                    avatar: None,
                                    speaking: n == 1,
                                    muted: n == 2,
                                    deafened: n == 3,
                                    sharing: n == 0,
                                    camera: n == 0,
                                    own_level: (n == 0).then_some(0.5),
                                })
                                .collect();
                            ui.add_space(28.0);
                            top = ui.cursor().top();
                            let space = ui.available_height() - 28.0;
                            tiles = participant_grid(ui, &data, space)
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
        ] {
            let case = format!("{size:?} text {text} with {count}");
            let (_, tiles, _, _, top, bottom) = render(size, text, count, 0.0);
            let above = tiles[0].top() - top;
            let below = (bottom - 28.0) - tiles[count - 1].bottom();
            // Top-aligned (the old placement) would be 0.
            assert!(above > 0.5, "{case}: the grid is not moved down ({above})");
            // Centred, except that a nearly full page keeps the gap below the
            // last row (`step`) so that no scrollbar appears.
            let ctx = egui::Context::default();
            configure(&ctx);
            let step = ctx.style().spacing.item_spacing.y + TILE_GAP;
            let grid = tiles[count - 1].bottom() - tiles[0].top();
            let expected = vertical_inset(above + grid + below, grid, step);
            assert!(
                (above - expected).abs() < 1.0 && below >= step - 0.5,
                "{case}: {above} above and {below} below the grid, expected {expected}"
            );
            if below > step + 1.0 {
                assert!((above - below).abs() < 1.0, "{case}: centred");
            }
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
