//! R6.1 step 4: a channel row's tooltip appears only after the pointer has
//! stayed on that row for 500 ms, and to the right of the row instead of under
//! the pointer. Leaving or switching rows starts over. egui's global tooltip
//! delay is unchanged for every other widget.
use super::*;

const HOVER_DELAY: f64 = 0.5;
const GAP: f32 = 6.0;

/// The one hover timer, bound to the stable identity of the hovered row (not
/// egui's row slot, which another channel can take over after a reorder,
/// removal or community switch).
#[derive(Clone, Copy)]
struct Hover {
    identity: egui::Id,
    since: f64,
    seen_frame: u64,
}

fn hover_id() -> egui::Id {
    egui::Id::new("relay-row-tooltip-hover")
}

/// egui's tooltip id for a row identity; its painted area is `id.with(0usize)`.
pub(super) fn tooltip_id(identity: egui::Id) -> egui::Id {
    identity.with("delayed-row-tooltip")
}

/// Shows `text` beside `row` after 500 ms of continuous hover on the row with
/// this `identity` (for channels: server and channel id).
pub(super) fn delayed_row_tooltip(
    ui: &egui::Ui,
    row: &egui::Response,
    identity: egui::Id,
    text: &str,
) {
    let ctx = ui.ctx();
    let frame = ctx.frame_nr();
    let menu_open = ctx.memory(|memory| memory.any_popup_open());
    if !row.hovered() || menu_open || row.dragged() {
        ctx.data_mut(|data| {
            if data
                .get_temp::<Hover>(hover_id())
                .is_some_and(|hover| hover.identity == identity)
            {
                data.remove::<Hover>(hover_id());
            }
        });
        return;
    }
    let now = ctx.input(|input| input.time);
    let since = ctx.data_mut(|data| {
        // Continue only the same row seen in this or the previous frame; a
        // different identity, or one that was gone for a frame, starts over.
        let since = data
            .get_temp::<Hover>(hover_id())
            .filter(|hover| hover.identity == identity && hover.seen_frame + 1 >= frame)
            .map_or(now, |hover| hover.since);
        data.insert_temp(
            hover_id(),
            Hover {
                identity,
                since,
                seen_frame: frame,
            },
        );
        since
    });
    let waited = now - since;
    if waited < HOVER_DELAY {
        ctx.request_repaint_after(std::time::Duration::from_secs_f64(HOVER_DELAY - waited));
        return;
    }
    egui::show_tooltip_at(
        ctx,
        tooltip_id(identity),
        Some(Pos2::new(row.rect.right() + GAP, row.rect.top())),
        |ui| {
            ui.label(text);
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Frame {
        rows: Vec<Rect>,
        tooltip: Option<Rect>,
        screen: Rect,
        clicked: Option<usize>,
    }

    /// A channel as the sidebar sees it: server, channel id and display name.
    type Channel = (&'static str, &'static str, &'static str);

    const CHANNELS: [Channel; 3] = [
        ("server-a", "c-general", "general"),
        (
            "server-a",
            "c-long",
            "a-very-long-channel-name-that-is-truncated",
        ),
        ("server-a", "c-lounge", "Lounge"),
    ];

    fn identity(channel: Channel) -> egui::Id {
        channel_tooltip_identity(channel.0, channel.1)
    }

    /// The real `channel_row` in a left side panel, as in the sidebar. The
    /// tooltip is read from egui's painted tooltip layer of the one production
    /// call inside `channel_row`.
    fn frame(
        ctx: &egui::Context,
        size: Vec2,
        time: f64,
        channels: &[Channel],
        events: Vec<egui::Event>,
    ) -> Frame {
        let mut result = Frame {
            rows: Vec::new(),
            tooltip: None,
            screen: Rect::NOTHING,
            clicked: None,
        };
        let _ = ctx.run(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, size)),
                time: Some(time),
                events,
                ..Default::default()
            },
            |ctx| {
                result.screen = ctx.screen_rect();
                egui::SidePanel::left("channels-test")
                    .exact_width(220.0)
                    .show(ctx, |ui| {
                        ui.add_space(size.y - 160.0);
                        for (index, channel) in channels.iter().enumerate() {
                            let response = channel_row(
                                ui,
                                channel.2,
                                identity(*channel),
                                index == 2,
                                false,
                                false,
                                (0, 0),
                            );
                            result.rows.push(response.rect);
                            if response.clicked() {
                                result.clicked = Some(index);
                            }
                        }
                    });
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.label("Owned content");
                });
            },
        );
        result.tooltip = channels.iter().find_map(|channel| {
            let area = tooltip_id(identity(*channel)).with(0usize);
            let layer = egui::LayerId::new(egui::Order::Tooltip, area);
            ctx.memory(|memory| {
                memory
                    .areas()
                    .visible_last_frame(&layer)
                    .then(|| memory.area_rect(area))
                    .flatten()
            })
        });
        result
    }

    #[test]
    fn tooltip_waits_500_ms_then_shows_right_of_the_row_inside_the_window() {
        for size in [Vec2::new(940.0, 650.0), Vec2::new(360.0, 300.0)] {
            let ctx = egui::Context::default();
            configure(&ctx);
            let rows = frame(&ctx, size, 0.0, &CHANNELS, Vec::new()).rows;
            let on = rows[1].center();
            let mut shown = frame(
                &ctx,
                size,
                0.1,
                &CHANNELS,
                vec![egui::Event::PointerMoved(on)],
            )
            .tooltip;
            for time in [0.2, 0.35, 0.55] {
                shown = frame(&ctx, size, time, &CHANNELS, Vec::new()).tooltip;
                assert!(shown.is_none(), "{size:?}: shown after {time} s");
            }
            let mut drawn = None;
            for time in [0.65, 0.7, 0.75] {
                let current = frame(&ctx, size, time, &CHANNELS, Vec::new());
                drawn = current.tooltip.map(|rect| (rect, current.screen));
            }
            let (tooltip, screen) = drawn.expect("shown after 500 ms of hover");
            assert!(shown.is_none());
            // With room, it sits right of the row; a narrow window may push it
            // back inside the screen, but never outside.
            if size.x > 600.0 {
                assert!(
                    tooltip.left() >= rows[1].right(),
                    "{size:?}: tooltip {tooltip:?} is not right of row {:?}",
                    rows[1]
                );
            }
            // egui's own tooltip area is clamped to the screen with sub-pixel
            // rounding (observed: 0.4 px); allow at most one point.
            assert!(
                screen.expand(1.0).contains_rect(tooltip),
                "{size:?}: tooltip {tooltip:?} leaves {screen:?}"
            );
        }
    }

    #[test]
    fn leaving_or_switching_rows_starts_over_and_never_leaves_a_stale_tooltip() {
        let ctx = egui::Context::default();
        configure(&ctx);
        let size = Vec2::new(940.0, 650.0);
        let rows = frame(&ctx, size, 0.0, &CHANNELS, Vec::new()).rows;
        frame(
            &ctx,
            size,
            0.1,
            &CHANNELS,
            vec![egui::Event::PointerMoved(rows[0].center())],
        );
        assert!(frame(&ctx, size, 0.7, &CHANNELS, Vec::new())
            .tooltip
            .is_some());
        // Another row: its own 500 ms, the old tooltip is gone at once.
        let switched = frame(
            &ctx,
            size,
            0.8,
            &CHANNELS,
            vec![egui::Event::PointerMoved(rows[1].center())],
        );
        assert!(switched.tooltip.is_none());
        assert!(frame(&ctx, size, 1.1, &CHANNELS, Vec::new())
            .tooltip
            .is_none());
        assert!(frame(&ctx, size, 1.4, &CHANNELS, Vec::new())
            .tooltip
            .is_some());
        // Leaving the list removes it; coming back waits again.
        let away = Pos2::new(600.0, 300.0);
        assert!(frame(
            &ctx,
            size,
            1.5,
            &CHANNELS,
            vec![egui::Event::PointerMoved(away)]
        )
        .tooltip
        .is_none());
        frame(
            &ctx,
            size,
            1.6,
            &CHANNELS,
            vec![egui::Event::PointerMoved(rows[1].center())],
        );
        assert!(frame(&ctx, size, 1.9, &CHANNELS, Vec::new())
            .tooltip
            .is_none());
        assert!(frame(&ctx, size, 2.2, &CHANNELS, Vec::new())
            .tooltip
            .is_some());
    }

    #[test]
    fn rows_still_open_with_a_click_without_an_early_tooltip() {
        let ctx = egui::Context::default();
        configure(&ctx);
        let size = Vec2::new(940.0, 650.0);
        let rows = frame(&ctx, size, 0.0, &CHANNELS, Vec::new()).rows;
        let at = rows[2].center();
        let press = |pressed| egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        frame(
            &ctx,
            size,
            0.1,
            &CHANNELS,
            vec![egui::Event::PointerMoved(at), press(true)],
        );
        let released = frame(&ctx, size, 0.15, &CHANNELS, vec![press(false)]);
        assert_eq!(released.clicked, Some(2), "the voice row still opens");
        // A click does not leave a tooltip behind it within the delay.
        assert!(frame(&ctx, size, 0.3, &CHANNELS, Vec::new())
            .tooltip
            .is_none());
    }

    #[test]
    fn another_channel_in_the_same_row_slot_waits_its_own_500_ms() {
        let size = Vec2::new(940.0, 650.0);
        let general = ("server-a", "c-general", "general");
        for replacement in [
            ("server-a", "c-random", "random"),
            // Same display name, different channel.
            ("server-a", "c-general-2", "general"),
            // Same channel id in another community.
            ("server-b", "c-general", "general"),
        ] {
            let ctx = egui::Context::default();
            configure(&ctx);
            let first = [general, CHANNELS[1], CHANNELS[2]];
            let rows = frame(&ctx, size, 0.0, &first, Vec::new()).rows;
            frame(
                &ctx,
                size,
                0.1,
                &first,
                vec![egui::Event::PointerMoved(rows[0].center())],
            );
            assert!(frame(&ctx, size, 0.75, &first, Vec::new())
                .tooltip
                .is_some());
            // The list changes under a still pointer: the slot now holds
            // another channel, which must wait for its own 500 ms.
            let second = [replacement, CHANNELS[1], CHANNELS[2]];
            assert!(
                frame(&ctx, size, 0.8, &second, Vec::new())
                    .tooltip
                    .is_none(),
                "{replacement:?} inherited the old timer"
            );
            assert!(frame(&ctx, size, 1.1, &second, Vec::new())
                .tooltip
                .is_none());
            assert!(frame(&ctx, size, 1.35, &second, Vec::new())
                .tooltip
                .is_some());
        }
    }

    #[test]
    fn reordering_or_a_channel_returning_starts_over() {
        let size = Vec2::new(940.0, 650.0);
        let ctx = egui::Context::default();
        configure(&ctx);
        let rows = frame(&ctx, size, 0.0, &CHANNELS, Vec::new()).rows;
        frame(
            &ctx,
            size,
            0.1,
            &CHANNELS,
            vec![egui::Event::PointerMoved(rows[0].center())],
        );
        assert!(frame(&ctx, size, 0.7, &CHANNELS, Vec::new())
            .tooltip
            .is_some());
        // Reordered: another channel is now under the pointer.
        let reordered = [CHANNELS[2], CHANNELS[0], CHANNELS[1]];
        assert!(frame(&ctx, size, 0.8, &reordered, Vec::new())
            .tooltip
            .is_none());
        // The first channel comes back to the slot: it starts over too.
        assert!(frame(&ctx, size, 0.9, &CHANNELS, Vec::new())
            .tooltip
            .is_none());
        assert!(frame(&ctx, size, 1.2, &CHANNELS, Vec::new())
            .tooltip
            .is_none());
        // Positive control: the same channel still shows after its 500 ms.
        assert!(frame(&ctx, size, 1.45, &CHANNELS, Vec::new())
            .tooltip
            .is_some());
    }
}
