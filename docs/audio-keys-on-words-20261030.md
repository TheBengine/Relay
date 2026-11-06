# Keys struck while talking, after Studio (DPDFNet), 2026-10-30

Task J of `docs/superpowers/plans/2026-10-30-audio-discord-parity-round3.md`.
Follows `docs/audio-studio-level-20261030.md` (Studio misses one figure: keys
on words leak at -4.8 dB against the -10 dB target).

## Outcome

**Nothing ships. Studio and Enhanced are unchanged** (`noise_suppressor::STUDIO_KEYS = None`,
test `studio_holds_back_no_keys_and_keeps_its_delay`). Six designs were measured on every bench
row with the recorded voice, and none of them lets fewer keys through while you talk than Studio
does on its own. Most of them also change the voice noticeably. Studio keeps its 60 ms. The
voice worker's CPU does not change.

| figure (recorded voice, bench in process) | target | Studio today | best candidate (e, guided hold) | every other candidate |
| --- | --- | ---: | ---: | --- |
| `keyboard_during_speech` leak (dB) | ≤ -10 | -4.8 | -4.3 | -4.0 to +1.0 |
| `mouse_clicks` residual in pauses (dB) | ≤ -15 | -34.0 | -34.1 | -33.3 to -35.4 |
| largest LSD change on any row (dB) | ≤ +0.3 | 0 | +0.04 | +0.85 to +2.05 |
| speech attenuation, every row (dB) | ±0.5 of Studio | -0.4..-0.1 | -0.4..-0.1 | -0.4..+0.3 |
| added delay (ms) | ≤ +10 | 0 | 0 | 0 |
| CPU of the stage per 10 ms block, p99 (ms) | ≤ 0.3 | 0 | 0.006 | 0.005 (b, d); 0.03 (a) |

**Why nothing reaches -10 dB (Step 1):** the keys that are left are not sharp clicks for a
detector to catch. The model already takes them below the voice. What leaks is spread over whole
20 ms model frames, and about half of it is the model changing the voice itself around each key
(below 1 kHz, where keys carry almost nothing). A stage after the model would need to tell that
apart from the voice. Even an ideal mask that knows the clean answer gets to ≈ -7 dB on the
bench. A stage before the model would need to remove the key from inside the voice: removing all
key energy above 1 kHz there (an oracle) gets to -6.1 dB. With this model and one microphone,
-10 dB on this row needs something other than a stage around the model (a model trained on keys
over speech, or a second signal such as the keyboard's own key events).

## Step 1: where the key energy sits after Studio

`keyboard_during_speech`, recorded voice (`voice3-prepared.wav`), Studio = dpdfnet8 in process.
"Leak" is what the bench's leak column counts: Studio(voice + keys) minus Studio(voice), over the
bench's speech frames, against the keys that went in. Scripts: `work\audio-worker-j-20261030\ref\`
(`where_clicks.py`, `where3.py`, `oracle.py`, `pre_oracle.py`, `decompose.py`, `ibm.py`).

| band (Hz) | keys in, share of their energy | leak against the keys in that band (dB) | share of the leak |
| --- | ---: | ---: | ---: |
| 0-300 | 2.5 % | +6.5 | 34.2 % |
| 300-1 000 | 5.8 % | -0.7 | 15.6 % |
| 1 000-2 000 | 10.7 % | -6.1 | 8.4 % |
| 2 000-4 000 | 41.7 % | -7.1 | 25.4 % |
| 4 000-8 000 | 33.1 % | -8.3 | 15.2 % |
| 8 000-16 000 | 3.7 % | -9.6 | 1.3 % |
| 16 000-24 000 | 3.2 % | -29.0 | 0.0 % |

- **Both, but not as a sharp transient.** About half the leak is the 2-8 kHz key (the model
  takes it down only 7-8 dB while the user talks). The other half is below 1 kHz, where the keys
  have 8 % of their energy. There the leak is 2× the keys that went in: per 5 ms, 65 % of it is a
  gain change on the voice's own low band. The model reacts to the key by moving the voice.
- **When.** Relative to each key press (50 presses with no other key 200 ms before), the leak sits
  at the press (0-20 ms) and at the release (30-70 ms after it). It spreads over 10 ms model
  frames rather than the key's 1-2 ms onset. 30 % of the leak lies more than 50 ms from any onset,
  and 37 % lies where the keys carry no energy at all.
- **Under the voice.** In the windows where the keys have energy, the voice's own band above 1 kHz
  is **13×** stronger than the leak there. Cutting that band where keys are makes the leak worse
  (an oracle that knows where every key is: -4.8 → +3.5 dB with the band above 1 kHz set to zero
  where the keys are within 20 dB of their loudest).
- **The rest of the chain.** The gate never treats a speech block differently because of the keys,
  and automatic volume's gain differences add -16 dB. The ~3 dB between the model's own leak
  (-7.9 dB, model alone on the 16-bit WAVs) and the bench's -4.8 is the 150 % input volume and
  automatic volume raising what the model leaves. The leak column counts the output against the
  keys at the input.

Picture: `work\audio-worker-j-20261030\ref\step1-studio-leak.png`, 6.0-7.5 s of the row, 0-16 kHz,
rows: keys in / leak / clean voice. The leak follows the voice's harmonics below 1 kHz and fills
whole model frames around each key.

### Upper bounds (oracles)

| what (all know where the keys are or what the clean answer is) | model alone (dB) | through the chain, bench (dB) |
| --- | ---: | ---: |
| Studio as it is | -7.9 | -5.0 |
| keys 6 dB quieter before the model | -9.8 | -6.5 |
| keys 12 dB quieter before the model | -11.9 | -8.3 |
| keys' band above 1 kHz removed before the model | -11.7 | -6.1 |
| keys' band above 1 kHz 12 dB quieter before the model | -10.4 | -6.5 |
| keys' band above 2 kHz removed before the model | -10.7 | -5.7 |
| keys' band below 1 kHz removed before the model | -8.1 | -4.7 |
| ideal magnitude mask after the model (knows Studio(clean)), 128-1024-point frames | -10.3 to -9.9 | ≈ -7 (+3 dB as above) |

"Through the chain": dpprobe's output for the modified input replayed through the app's automatic
volume and gate (`RELAY_REPLAY_DIR`), as Worker F's replays (input volume applied after the model,
which is why the base row reads -5.0, not -4.8).

## Step 2: candidates, every row

All run inside Studio's model thread (`dpdfnet::Core`), so the voice worker's cost does not
change, and none adds delay: they work on whole 10 ms blocks, and the model's input is lined up
with its output (2 400 samples earlier) without waiting.

- **(a) `dpdfnet8+ratio-clicks`:** Enhanced click suppression (`ClickSuppressor`, Normal) after
  the model, fed the model's output-to-input energy ratio as voice probability (Worker H's method,
  already in `dpdfnet.rs`; this task found nothing to add to it).
- **(b) `dpdfnet8+keys-after`:** the plan's transient detector on the Studio output: band above
  1 kHz, 1 ms energy 12 dB over its 50 ms median and over its level 2 ms before, then held at its
  level from before the jump for at most 8 ms; below 1 kHz untouched (`click_suppression::KeyHold`).
  A time-domain hold of the band's level, not a spectral fill: a spectral fill needs a short STFT,
  4-10 ms more delay, and the ideal-mask bound above says it could not reach the target either.
- **(c) `dpdfnet8+ratio-clicks+keys-after`:** (a) and (b).
- **(d) `dpdfnet8+keys-before`:** the same hold on the model's input (up to 40 ms), so the model
  never sees the key.
- **(e) `dpdfnet8+keys-guided`:** (b), but the jump is looked for in the model's input, and only
  counts where the model removed at least 6 dB of it. This keeps consonants, which the model keeps,
  and catches keys, which it takes down.
- Sweeps (`RELAY_KEYS_CHAINS`): (d) with the split at 2 kHz (`b2k`), held 6 dB lower (`bm6`), 9 dB
  onset (`b9`); (e) with 10 dB removed and 20 ms (`g10`), split 2 kHz (`g6w`), held 6 dB lower
  (`g6m`), 9 dB onset (`g9`).

Recorded voice, bench default (float input), release build. Rows: residual in pauses / leak
during speech / LSD (dB). Speech attenuation stays within -0.4..+0.3 dB on every row for every
chain (full table below).

| row | Studio | (a) | (b) | (c) | (d) | (e) |
| --- | --- | --- | --- | --- | --- | --- |
| keyboard_between_words | -42.2 / n/a / 1.47 | -43.5 / n/a / 1.75 | -41.8 / n/a / 2.45 | -43.1 / n/a / 2.73 | -43.1 / n/a / 3.44 | -42.4 / n/a / 1.48 |
| **keyboard_during_speech** | -32.0 / **-4.8** / 2.11 | -36.3 / -4.0 / 2.41 | -32.6 / -2.8 / 2.95 | -36.5 / -2.1 / 3.22 | -37.3 / +1.0 / 3.79 | -33.2 / -4.3 / 2.15 |
| mouse_clicks | -34.0 / 0.0 / 1.73 | -35.4 / 1.3 / 2.02 | -33.3 / 3.2 / 2.67 | -35.0 / 3.8 / 2.94 | -34.1 / 4.4 / 3.60 | -34.1 / 1.1 / 1.76 |
| fan_plus_mouse | -41.7 / -6.9 / 4.22 | -45.9 / -5.7 / 4.78 | -41.8 / -5.4 / 4.61 | -45.7 / -4.8 / 5.18 | -42.1 / -2.6 / 4.53 | -41.7 / -6.9 / 4.23 |
| fan_plus_mouse_15 | -43.7 / -3.5 / 2.86 | -52.6 / -2.9 / 3.19 | -43.1 / -1.1 / 3.54 | -52.2 / -0.9 / 3.90 | -43.5 / 2.5 / 3.91 | -43.7 / -3.4 / 2.86 |
| fan_plus_mouse_10 | -43.6 / -5.2 / 3.50 | -47.8 / -4.2 / 3.99 | -43.6 / -3.0 / 4.02 | -47.0 / -2.6 / 4.45 | -44.5 / -0.2 / 4.22 | -43.6 / -5.1 / 3.50 |
| music_bed | -38.5 / -7.1 / 2.37 | -57.3 / -5.8 / 3.13 | -41.0 / -5.6 / 3.04 | -59.0 / -4.7 / 3.59 | -38.5 / -2.4 / 2.61 | -38.5 / -7.0 / 2.37 |
| music_bed_12 | -38.0 / -3.7 / 2.10 | -53.0 / -3.3 / 2.57 | -39.4 / -1.8 / 2.91 | -53.4 / -1.7 / 3.30 | -38.0 / 1.8 / 2.77 | -38.0 / -3.7 / 2.10 |
| second_talker | -50.3 / -11.0 / 1.69 | -56.2 / -9.5 / 2.54 | -50.2 / -10.2 / 2.64 | -56.0 / -8.1 / 3.34 | -50.5 / -7.6 / 2.95 | -50.3 / -11.0 / 1.70 |
| silence_floor | -95.0 | -95.0 | -95.0 | -95.0 | -95.0 | -95.0 |
| clean_voice | -14.6 / n/a / 1.52 | -15.5 / n/a / 1.90 | -13.9 / n/a / 2.51 | -15.0 / n/a / 2.84 | -14.7 / n/a / 3.57 | -14.6 / n/a / 1.56 |

Sweeps, `keyboard_during_speech` leak / largest LSD change on any row:

| chain | leak (dB) | worst LSD change (dB) |
| --- | ---: | ---: |
| (d) `b2k`, split 2 kHz | -2.8 | +1.16 |
| (d) `bm6`, held 6 dB lower | +1.0 | +2.28 |
| (d) `b9`, 9 dB onset | -0.1 | +2.96 |
| (e) `g6`, the guided hold as (e) | -4.3 | +0.04 |
| (e) `g10`, 10 dB removed, 20 ms | -3.7 | +0.06 |
| (e) `g6w`, split 2 kHz, 20 ms | -2.9 | +0.08 |
| (e) `g6m`, held 6 dB lower, 20 ms | -2.8 | +0.13 |
| (e) `g9`, 9 dB onset, 20 ms | -2.4 | +0.11 |

What the table says:

- **A transient detector cannot tell a key from a consonant on a recorded voice.** (b) and (d)
  open on t, k and s as often as on keys: the clean voice alone moves by LSD +1.0 (b) and +2.05
  (d), and every row with speech follows. Holding a consonant to the vowel's level before it is
  exactly the damage the LSD column counts.
- **Even where it does catch a key, holding the band makes the leak worse.** The voice is 13×
  stronger than what is left of the key in that band. Holding the band at its level from before
  the jump swaps the voice's fricatives for key noise at the same level. That is more difference
  to Studio(voice), not less. (e) spares the voice (LSD +0.04 at most), and its leak still rises
  from -4.8 to -4.3 dB.
- **(a)** reproduces Worker H's finding: pauses deeper on every row (music -57 dB), leak -4.0, voice
  changed by LSD +0.3 to +0.85 on every row with speech.
- **Mouse pauses** stay at -33 to -35 dB with every candidate (target ≤ -15).
- **Enhanced:** the stage needs Studio's model (its input and output lined up). Run on RNNoise it
  would be (b), which the table rules out, and Enhanced already has click suppression for clicks
  on words at High. Not changed.

### Delay and CPU

Every chain measures 60.00-60.21 ms on the bench (declared 60.00). The stages add none: they
change samples inside the block they are given.

CPU, `audio-bench --key-hold-cpu` (the stage alone, release, keyboard_during_speech, 5 passes),
on its model thread:

| stage | p50 (ms) | p99 (ms) | max (ms) |
| --- | ---: | ---: | ---: |
| (b), (d): plain hold | 0.0039 | 0.0052 | 0.070 |
| (e): guided hold (also filters the model's input) | 0.0048 | 0.0062 | 0.066 |

Both are about 2 % of the 0.3 ms budget. (a) is `ClickSuppressor`: 0.03 ms, as for Enhanced.

The bench's own CPU column for these runs (eight processes at once, so not a clean figure)
moves within run-to-run noise: Studio p50 4.97 ms, (b) 4.95, (d) 4.91, (e) 5.06.

## Listening files

`C:\Important\Projects\2026-10-03\can\work\audio-worker-j-20261030\lyt\tastatur-under-tale\`,
Round 2's format: 48 kHz 16-bit mono, recorded voice, each file's active level matched to the
clean file's, each chain's output shifted back by its delay. Whole send chain, as a listener
would hear it.

```
tastatur-under-tale\  (keyboard_during_speech)
  1-ren-stemme.wav                    the clean recording
  2-ind-i-mikrofonen.wav              voice + keys, the input
  3-relay-i-dag-studio.wav            Studio as shipped (dpdfnet8)
  4-a-klikfjerner-efter-studio.wav    (a) click suppression fed the model's ratio
  5-b-hold-efter-studio.wav           (b) the plan's hold after the model
  6-c-a-og-b.wav                      (c) (a) and (b)
  7-d-hold-foer-studio.wav            (d) the hold before the model
  8-e-styret-hold-efter-studio.wav    (e) the guided hold after the model
```

Levels within 0.01 dB of the clean file, lag 0-1 samples against it (cross-correlation), peaks
-6.9 to -8.9 dBFS. Bench run `w3` (`--write-wav`), which reproduces every figure above.

## What changed in the code

- `click_suppression.rs`: `KeyHold` (`KeyHoldParams`: split, onset, hold level, longest window,
  and for the guided form how much of the jump the model must have removed). Four tests:
  `a_plain_key_hold_mistakes_a_voice_for_keys` (on the synthetic voice alone: 48 windows,
  the voice changed by -8.8 dB, the reason it is not shipped), `key_hold_adds_no_delay_below_the_split`,
  `key_hold_takes_keys_on_a_voice_down_to_the_voice`,
  `guided_key_hold_acts_only_where_the_model_removed_the_jump`.
- `dpdfnet.rs`: `StudioKind` gains `keys_before` / `keys_after`. `Core` runs the stage on the
  model's input before the model, and on the model's output with the input lined up. Both are
  `None` in the app.
- `noise_suppressor.rs`: `STUDIO_KEYS = None` with the reason. Bench chains (b), (c), (d), (e)
  next to Worker H's (a). `RELAY_KEYS_CHAINS` for sweeps. `key_hold_cpu`. Test
  `studio_holds_back_no_keys_and_keeps_its_delay`.
- `audio_bench.rs`: `--key-hold-cpu`, and the usage text for `RELAY_KEYS_CHAINS`.

## Tests and checks

- `cargo test -p relay-client -- --test-threads 8` (with the four `RELAY_*` folders): **1632
  passed, 0 failed, 119 ignored**. Two tests failed once each in earlier full runs and passed
  alone and in the final full run: `dpdfnet::tests::a_stuck_model_thread_never_stalls_the_caller`
  (one call took 57 ms against its 5 ms limit while eight debug test threads loaded models; the
  model thread is held stuck in that test, so the key stage never runs) and
  `invitation_probe::tests::checked_dns_addresses_are_pinned_for_both_health_requests_without_authentication`
  (network).
- `cargo clippy -p relay-client`: 26 warnings, the count Worker H reported at the base; none in
  the four files this task touched. (`--all-targets` stops on a `never_loop` error in
  `local_voice_gateway.rs` test code, which this task does not own.)
- Building the `relay` test binary once crashed rustc (`STATUS_STACK_BUFFER_OVERRUN`) while other
  workers were building on the same machine; `RUST_MIN_STACK=67108864` and a rebuild went through.
- `Cargo.lock` unchanged.

## How it was measured

1. `audio-bench` with `VOICE_TEST_WAV=…\audio-worker-e-20261029\voice\voice3-prepared.wav`,
   release, one process per chain (`work\audio-worker-j-20261030\ref\run.sh`), runs `w1` (Studio,
   (a)-(d), sweeps of (d)) and `w2` (sweeps of (e)), and `w3` (the six chains again with `--write-wav`, same figures), under
   `D:\Projects\relay-build-targets\audio-worker-j-runs\`. Studio's row reproduces Worker H's table
   to 0.1 dB (mouse_clicks leak 0.0 against 0.1).
2. Step 1 on Worker H's run `real` (the same in-process Studio at 50 ms, before the hand-off; the
   output is the same, one block earlier), using `clean_voice.dpdfnet8.wav` as Studio(voice): every
   row has the same voice and the chain is deterministic.
3. Oracles: modified inputs through Worker F's `dpprobe` (dpdfnet8, 16-bit WAVs), then the bench's
   replay route for the rest of the chain (run `oracle-replay`).

## What is not done

- **The target is not met.** Keys on words stay at -4.8 dB. Closing the gap needs a different kind
  of fix (above), not a stage around this model.
- **No listening test.** The files are there. Of the candidates only (e) leaves the voice as it
  is, and its figures say it lets slightly more of the keys through, not less.
- **The keys are synthetic** (a resonance at 2.5-6 kHz plus a broadband part). Real keyboards
  thump lower, which would put more of them below 1 kHz, where nothing here acts.
