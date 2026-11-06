# Everyone equally loud, and cleaning up what you hear, 2026-10-30

Task I of `docs/superpowers/plans/2026-10-30-audio-discord-parity-round3.md`
(Worker I, receive side). Code: `crates/relay-client/src/receive_level.rs`,
wired into the playback mixer of `voice_worker` in `media.rs`.

## What the user gets

Settings → Voice, a new last section **What You Hear**:

> **Make everyone the same volume** (on). Turns quiet people up and loud
> people down, so everyone sounds about as loud. It only changes their
> voice, never the silence between their words. Each person's own volume
> still applies on top.
>
> **Clean up what I hear** (off). Removes background noise, music and
> keyboard sounds from other people's voices, for people who send without noise suppression. You
> hear everyone about 70 ms later, and it uses more of this PC's processor.

Both apply during a call as soon as they are saved. Old settings files load
with leveling on and clean-up off (test
`audio_settings::tests::hearing_settings_are_at_the_end_and_older_files_get_leveling_on_and_cleanup_off`).
The settings page's delay estimate counts the clean-up (67.5 ms) when it is
on; leveling adds none.

## Leveling

Unit tests in `receive_level.rs` (debug build), the synthetic talker of
`voice_test_signals.rs` scaled to the speech level named (10 ms blocks within
30 dB of the loudest, as the bench measures speech), 8 s per speaker. The
-12 dBFS talker peaks at +0.9 dBFS, which Opus's float decoder passes
unclamped; its output peaks at -3.4 dBFS. The tests print these figures
(`--nocapture`).

| figure | target | result |
| --- | --- | --- |
| speaker at -32 dBFS: speech level from 3 s on | -20 ±2 dB | **-20.99 dBFS** (gain +11.92 dB, the +12 dB limit) |
| speaker at -32 dBFS: gain within 2 dB / 1 dB of the gain that lands it on -20 dBFS, from | 3 s | **1.8 s** / 3.3 s |
| speaker at -12 dBFS: speech level from 3 s on | -20 ±2 dB | **-18.57 dBFS** (gain -8.22 dB at 8 s) |
| speaker at -12 dBFS: gain within 2 dB / 1 dB, from | 3 s | **0.6 s** / 4.7 s |
| pauses between words over a -60 dBFS background, lifted at most (gain +11.7 dB) | not lifted | **+0.00 dB** |
| 3 s of the background alone after the last word, lifted | not lifted | **+0.00 dB** |
| gain before / after those 3 s | unchanged | +11.68 / +11.68 dB |
| 300 ms burst 12 dB louder: the 1.5 s of words after it against the 1.5 s before | no pumping | **-0.55 dB** |
| lowest leveling gain during and after the burst, against before | | -0.56 dB (+5.72 to +5.16 dB) |
| a lifted peak, at most | -1 dBFS | -1.00 dBFS |
| a fan alone (-40 dBFS, no speech), gain | 0 dB | 0 dB, output bit-identical |
| delay added | 0 ms | 0 ms (a gain only) |
| CPU on the voice worker per 10 ms of playback, 1 / 8 / 32 people talking (release, p50 / p99) | ≤ 0.5 ms | 0.0006 / 0.0008, 0.0047 / 0.0080, 0.0212 / 0.0352 ms |

### How it works

Per person you hear (per mixer stream: a whisper from someone is a stream
of its own), on each decoded 2.5 ms quantum, before that person's volume and
priority ducking:

- **Speech or not**: the 10 ms level is speech when it is above -55 dBFS and
  12 dB above the background under it (a minimum tracker that drops at once
  and rises 3 dB/s). A sender who transmits only a fan is never speech
  (test `noise_alone_is_never_taken_for_speech`).
- **Loudness**: the power of the speech quanta over the last 3 s of speech
  (an exponential average; from the first word on, the plain average of all
  speech so far, so a new speaker is measured within 100 ms). Only quanta
  within 20 dB of the loudness count, so the quiet ends of words do not
  pull it down.
- **Gain**: toward -20 dBFS minus the loudness, within -18…+12 dB, up at
  most 6 dB/s and down at most 20 dB/s. It moves only while the person talks
  and for 500 ms after; in a longer silence it stays where it is.
- **Pauses**: the gain is applied in full while the person talks and for
  200 ms after (word endings), then returns to 0 dB at 120 dB/s, so what is
  not speech plays as loud as it arrived, never louder. A cut (loud
  speaker) stays on in pauses.
- **Bursts**: speech more than 10 dB above the loudness (a laugh, a cough,
  a shout) does not count toward it until the level is back within 4 dB, so
  the words after it are not turned down. Bursts that add up to 600 ms of
  speech without 400 ms of ordinary speech between them are the person's new
  level and count.
- **Ceiling**: a lift never takes a peak above -1 dBFS: the gain is limited
  by the quantum's own peak (applied over the whole quantum, so no look-ahead)
  and a peak envelope that releases in 50 ms. Audio that arrives above the
  ceiling is never limited by the leveler; the existing output limiter after
  the mix still protects the device.
- The gain moves along a ramp across each quantum, so there are no steps.
- A person's state is dropped when they leave the call (with their mixer
  stream, 3 s after their last packet). Turning the setting off forgets all
  of them; turning it on starts everyone at 0 dB.

### Why 3 s and not the plan's 400 ms

The plan asked for "RMS over about 400 ms". With a 400 ms window the gain
followed the loudness of each phrase instead of the person: on the test
talker, whose speech level over 250 ms swings between -26 and -46 dBFS, the
gain of the -32 dBFS speaker went from +12 down to +7.8 dB on a loud phrase
and back, and the speaker was within 2 dB of the target only after 7.1 s.
With the 3 s speech average the loudness stays within ±1 dB of the speaker's
level, and a new speaker still converges in under 2 s because the average
starts as the plain mean of what has been heard. The plan's 6 and 20 dB/s
limits and +12 dB stay.

## Clean up what I hear

The mixed incoming audio (after leveling, volume and priority) goes through
Studio's small model, dpdfnet2, with Studio's code unchanged
(`dpdfnet::Studio` with the overrun guard): the model runs on a thread of its
own (`relay-studio`), the voice worker hands it each 10 ms block and plays
the result for the block before, never waiting; a block the model has not
finished plays unprocessed at the same delay. Until the model is built
(started in the background when the setting is saved), RNNoise stands in, as
for Studio.

- **Delay: 67.5 ms**, not the plan's 60: Studio's 60 ms (model 50, hand-off
  10) plus 7.5 ms to collect a 10 ms block from three further 2.5 ms mixer
  quanta. The first quantum of each processed block plays in the same mixer
  step that completes the block. Test
  `the_cleanup_plays_67_5_ms_late_and_plays_out_its_tail`: with Studio's
  timing as a plain delay line the output is the input exactly 3240 samples
  later; with the model the best alignment is at 3240 samples. Reaching 60 ms
  would need a hand-off that fetches the model's result within one 2.5 ms
  quantum, which `dpdfnet::Studio` does not offer (it reads the result when
  the next block is handed over).
- **Tail**: after the last incoming quantum the mixer keeps playing for 27
  quanta, until everything that was heard is out.
- **CPU** (release, this machine, AMD, 16 logical processors, while other
  workers' builds ran: 4 to 5 compilers at the time):

  | | p50 (ms) | p99 (ms) | max (ms) |
  | --- | ---: | ---: | ---: |
  | model per 10 ms block, bench rows, model inline | 1.06 | 1.55 | 14.7 |
  | model thread, real time (Studio's own log), runs 1 / 2 / 3 | 1.10 / 1.60 / 1.70 | 1.35 / 1.95 / 2.65 | 1.93 / 1634 / 8.67 |
  | voice worker per 10 ms block, real time, runs 1 / 2 / 3 | 0.004 / 0.006 / 0.007 | 0.007 / 0.023 / 0.125 | 67.7 / 32.9 / 2.7 |

  Real time (opt-in test `paced_cleanup_in_real_time`): one mixer quantum
  every 2.5 ms for 20 s through the app's clean-up, 2000 blocks. Blocks that
  played unprocessed because the model was late: **7, 263 and 2**. In run 2
  the model thread itself was not scheduled for 1.6 s (its own longest
  step: 1634 ms); the voice worker's maxima are its own thread being held up
  the same way (the clean-up never waits; the 1-person leveling measurement
  shows a 292 ms outlier of the same kind for work that takes 0.6 µs).
  These maxima and late counts need a rerun on an idle machine before they
  are quoted as this PC's figures; the p50 and p99 are steady. The model
  costs about 11-17 % of one core while the setting is on, whether anyone
  talks or not.

### The bench rows as an incoming speaker

The bench corpus (recorded voice, `VOICE_TEST_WAV`) as if the person you
hear sent it without noise suppression, through the clean-up, model inline
(as the bench runs Studio), metrics against the clean voice at 67.5 ms
(opt-in test `cleanup_on_the_bench_rows`). "None" is the incoming audio as it
is today (0 dB residual by definition).

Recorded voice (Worker E's `voice3-prepared.wav`), release build, the
bench corpus as generated (float). The incoming audio goes into the clean-up
exactly as the bench row holds it: no input volume, gate or automatic
volume, which the send-side Studio figures include, so the two tables are
not row-for-row comparable.

| row | SNR in (dB) | residual noise in pauses (dB) | noise leak during speech (dB) | speech segSNR (dB) | LSD (dB) | speech attenuation (dB) |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| keyboard_between_words | 10.0 | -46.5 | n/a | 29.7 | 0.4 | 0.2 |
| keyboard_during_speech | 10.0 | -31.7 | -8.1 | 25.2 | 1.7 | 0.2 |
| mouse_clicks | 15.0 | -38.6 | -2.4 | 27.7 | 1.0 | 0.2 |
| **fan_plus_mouse** | 5.0 | **-50.9** | -9.2 | 10.8 | 4.2 | 0.3 |
| fan_plus_mouse_15 | 15.0 | -47.4 | -5.3 | 17.4 | 2.7 | 0.2 |
| fan_plus_mouse_10 | 10.0 | -49.8 | -7.4 | 14.1 | 3.4 | 0.2 |
| **music_bed** | 5.0 | **-40.1** | -9.0 | 12.1 | 2.1 | 0.3 |
| **music_bed_12** | 12.0 | **-39.8** | -5.8 | 15.6 | 1.7 | 0.2 |
| second_talker | 0.0 | -56.2 | -11.2 | 18.5 | 1.3 | 0.5 |
| silence_floor | n/a | -72.1 | n/a | n/a | n/a | n/a |
| clean_voice | n/a | -19.5 | n/a | 28.4 | 0.8 | 0.3 |

Without the clean-up every residual is 0 dB (the incoming audio plays as it
arrives). The voice stays within 0.5 dB of its level on every row.
`clean_voice`'s residual is what is left in the recording's digital-silence
pauses against the -70 dBFS floor, as on the send-side bench.

## Tests and checks

- `cargo test -p relay-client --bin relay -- --test-threads 8 receive_level audio_settings`
  (debug): **29 passed, 0 failed, 3 ignored** (the three opt-in
  measurements). New tests in `receive_level`:
  `a_quiet_and_a_loud_speaker_land_within_2_db_of_the_target_within_3_s`,
  `the_gain_moves_no_faster_than_6_db_up_and_20_db_down_per_second`,
  `silence_between_words_is_not_lifted_and_a_silent_speaker_keeps_the_gain`,
  `a_loud_burst_does_not_pump_the_following_words`,
  `a_lift_never_takes_a_peak_over_minus_1_dbfs_and_never_limits_what_arrived`,
  `noise_alone_is_never_taken_for_speech`,
  `each_speaker_has_their_own_gain_and_leaving_forgets_it`,
  `turning_leveling_off_leaves_audio_untouched_and_forgets_the_gains`,
  `the_cleanup_plays_67_5_ms_late_and_plays_out_its_tail`; opt-in
  `leveling_cpu_per_block`, `cleanup_on_the_bench_rows`,
  `paced_cleanup_in_real_time` (release, `--ignored --nocapture
  --test-threads 1`; `VOICE_TEST_WAV` for the recorded voice). In
  `audio_settings`:
  `hearing_settings_are_at_the_end_and_older_files_get_leveling_on_and_cleanup_off`.
- `cargo clippy -p relay-client --bin relay --tests`: no diagnostic in
  `receive_level.rs` or on any changed line elsewhere (two in the new file
  were fixed). The `relay` binary has 26, the count Worker H recorded at the
  base; its test target 33, none of them in this change.
- The full client suite was not run (one client test build takes 40-60
  minutes on this machine while the other workers build).
- `Cargo.lock` unchanged; no new crates.
- The "pauses between words" figure comes from one pause long enough to
  measure past the 300 ms the lift needs to return to 0 dB (the test
  talker's other gaps, 40-140 ms, fall inside the lift's 200 ms hold); the
  3 s of background after the words cover the same case at length.

## What is not done

- **No listening test and no call on two PCs** with either setting.
- **Shared audio (`application_audio.rs`)**, the people-call path that mixes
  RTC tracks with per-user volume, is not leveled: it is outside this task's
  files. Voice in rooms and private calls through the voice worker is.
- **The clean-up's log lines** say "Studio noise suppression" (they come
  from `dpdfnet.rs`, which this task does not change). Its overrun guard
  shares Studio's process-wide decision: if this PC cannot keep up with
  dpdfnet2 on what you hear, a Studio microphone also falls back to RNNoise
  for the rest of the session, and the other way round.
- **67.5 ms instead of 60 ms** (above).
- Leveling uses a 3 s loudness instead of the plan's 400 ms (above).
