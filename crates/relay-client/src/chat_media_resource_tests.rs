use super::*;

fn media(delays: &[Duration]) -> DecodedMedia {
    DecodedMedia {
        source_size: [1, 1],
        frames: delays
            .iter()
            .map(|delay| DecodedFrame {
                image: Arc::new(ColorImage::new([1, 1], egui::Color32::WHITE)),
                delay: *delay,
            })
            .collect(),
        decoded_bytes: delays.len() * 4,
    }
}
fn reference(
    position: &mut Duration,
    delays: &[Duration],
    elapsed: Duration,
    visible: bool,
    paused: bool,
    reduced: bool,
) -> (usize, Duration) {
    if delays.len() < 2 || reduced {
        return (0, Duration::ZERO);
    }
    let total: Duration = delays.iter().copied().sum();
    if visible && !paused {
        *position = position.saturating_add(elapsed);
    }
    let nanos = position.as_nanos() % total.as_nanos().max(1);
    let mut cursor = 0u128;
    for (i, d) in delays.iter().enumerate() {
        cursor += d.as_nanos();
        if nanos < cursor {
            return (
                i,
                Duration::from_nanos((cursor - nanos).min(u128::from(u64::MAX)) as u64),
            );
        }
    }
    (0, delays[0])
}

#[test]
fn all_frame_boundaries_zero_delays_and_many_loops_match_previous_algorithm() {
    for delays in [
        vec![Duration::ZERO; 4],
        vec![
            Duration::from_millis(50),
            Duration::from_millis(120),
            Duration::from_millis(70),
            Duration::from_millis(90),
        ],
        (0..120)
            .map(|i| Duration::from_nanos((i % 17) * 1_000_003))
            .collect(),
    ] {
        let mut image = AnimatedImage::new(media(&delays));
        let mut position = Duration::ZERO;
        for i in 0..10000 {
            let elapsed = Duration::from_nanos((i * 137_017) % 100_000_000);
            let actual = image.advance(elapsed, true, false);
            assert_eq!(
                actual,
                reference(&mut position, &delays, elapsed, true, false, false)
            );
            assert_eq!(image.position, position);
        }
        let mut end = Duration::ZERO;
        for d in &delays {
            end += *d;
            image.position = end;
            let mut position = end;
            assert_eq!(
                image.advance(Duration::ZERO, true, false),
                reference(&mut position, &delays, Duration::ZERO, true, false, false)
            );
        }
    }
}

#[test]
fn hiding_pausing_reduced_motion_and_replacing_media_preserve_clock_semantics() {
    let delays = vec![
        Duration::from_millis(10),
        Duration::from_millis(20),
        Duration::ZERO,
        Duration::from_millis(30),
    ];
    let mut image = AnimatedImage::new(media(&delays));
    let mut position = Duration::ZERO;
    for i in 0..1000 {
        let paused = i % 7 == 0;
        let visible = i % 5 != 0;
        let reduced = i % 13 == 0;
        image.set_paused(paused);
        let elapsed = Duration::from_millis(i * 3);
        assert_eq!(
            image.advance(elapsed, visible, reduced),
            reference(&mut position, &delays, elapsed, visible, paused, reduced)
        );
    }
    let replacement = vec![Duration::from_millis(7); 120];
    image.replace_media(media(&replacement));
    image.set_paused(false);
    assert_eq!(
        image.advance(Duration::from_millis(19), true, false),
        reference(
            &mut position,
            &replacement,
            Duration::from_millis(19),
            true,
            false,
            false
        )
    );
    image.replace_media(media(&[Duration::ZERO]));
    assert_eq!(
        image.advance(Duration::from_secs(100), true, false),
        (0, Duration::ZERO)
    );
}

#[test]
fn actual_decoded_gif_frame_and_repaint_deadline_match_reference_over_texture_updates() {
    let bytes = super::tests::disposal_gif();
    let decoded = decode(&bytes).unwrap();
    let delays: Vec<_> = decoded.frames.iter().map(|f| f.delay).collect();
    let mut image = AnimatedImage::new(decoded);
    let ctx = egui::Context::default();
    for _ in 0..3 {
        let _ = ctx.run(egui::RawInput::default(), |_| {});
    }
    let start = Instant::now();
    for millis in [0, 1, 49, 50, 51, 169, 170, 239, 240, 329, 330, 5000] {
        let t = start + Duration::from_millis(millis);
        let output = ctx.run(egui::RawInput::default(), |ctx| {
            image.texture_internal(ctx, true, false, true, t);
        });
        let mut reference_position = Duration::from_millis(millis);
        let expected = reference(
            &mut reference_position,
            &delays,
            Duration::ZERO,
            true,
            false,
            false,
        );
        assert_eq!(image.texture_frame, expected.0);
        assert_eq!(image.advance(Duration::ZERO, true, false), expected);
        assert_eq!(
            output.viewport_output[&egui::ViewportId::ROOT].repaint_delay,
            expected.1
        );
    }
}

#[test]
#[ignore = "Controlled release component benchmark; no GUI, media device or network"]
fn animation_timeline_measurement() {
    use std::hint::black_box;
    let rounds = 9;
    let calls = 200_000;
    for (frames, images) in [(1usize, 16usize), (4, 1), (120, 1), (120, 16), (120, 64)] {
        let delays: Vec<_> = (0..frames)
            .map(|i| Duration::from_millis((i % 17 + 1) as u64))
            .collect();
        let mut players: Vec<_> = (0..images)
            .map(|_| AnimatedImage::new(media(&delays)))
            .collect();
        let mut samples = Vec::new();
        for round in 0..rounds + 1 {
            let start = Instant::now();
            for i in 0..calls {
                let player = &mut players[i % images];
                black_box(player.advance(
                    black_box(Duration::from_micros(((i * 7919) % 33000) as u64)),
                    true,
                    false,
                ));
            }
            let nanos = start.elapsed().as_nanos();
            if round > 0 {
                samples.push(nanos);
            }
        }
        println!(
            "TIMELINE_MEASURE {}",
            serde_json::json!({"frames":frames,"images":images,"calls_per_round":calls,"rounds":rounds,"elapsed_ns":samples,"player_struct_bytes":std::mem::size_of::<AnimatedImage>(),"scope":"release animation lookup component; not UI/native-process/GPU/network/power"})
        );
    }
}
