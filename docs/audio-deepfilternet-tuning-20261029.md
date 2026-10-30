# DeepFilterNet 3: attenuation limit, post-filter, deep-filter threshold and a speech-aware blend, 2026-10-29

Worker G of `docs/superpowers/plans/2026-10-29-audio-suppressor-round2.md`
(Task G). Follows `docs/audio-deepfilter-decision-20261029.md` (Worker E),
which measured DeepFilterNet 3 low latency (LL) on the user's recorded voice
and found music removed but the voice changed. Nothing is linked into the
client; this document is the whole commit.

## Outcome

**The attenuation limit and the post-filter do not fix the voice. A third
libDF knob, `max_db_df_thresh`, does.** LL with `max_db_df_thresh = 5` and
`post_filter_beta = 0.02` passes every music row and the fan row of the
acceptance table at 10 ms delay, and leaves the voice under music about as
the shipped chain does (LSD 4.60 dB against 4.35). It fails only the click
rows, as every DeepFilterNet setting does.

- **What changes the voice is DeepFilterNet's second stage, the deep filter**
  (complex filter coefficients over the last 5 frames, applied below about
  4.8 kHz). Under 5 dB music, LL's output below 5 kHz has a correlation of
  0.00-0.17 with the clean voice at every lag, while above 5 kHz (gains only)
  it is 0.996. The clean voice alone passes untouched (correlation 1.000).
- **`max_db_df_thresh`** decides when the deep filter runs: only in frames
  whose local SNR (the model's own estimate, computed every frame) is at or
  below it. The default is 20 dB, and under 5 dB music every frame
  (100 %) is at or below it. At 5 dB the deep filter still runs in 74 % of
  the frames under 5 dB music and 55 % under 12 dB music: in the pauses and
  in the weaker, music-heavy parts of speech. The frames where the voice is
  strongest get only the band gains. Below 5 kHz the output then correlates
  0.66-0.86 with the clean voice (0.00-0.17 before). This is itself a causal,
  speech-aware switch, driven by the model's own detector.
- **`atten_lim_db`** works exactly as documented. It caps what is removed:
  the raw residual in pauses stops at -12.1 / -18.1 / -24.0 dB for limits of
  12 / 18 / 24 dB. But it mixes the noisy input back into a voice that the
  deep filter has already changed, so the voice gets no better (LSD under
  5 dB music 9.92 / 9.71 / 9.66 dB against 9.64 dB without a limit). The
  app's automatic volume then lifts the remaining music: -24.0 dB raw becomes
  -11.5 dB at the output. No limit reaches the -20 dB pause target.
- **`post_filter_beta` 0.02** takes up to 3 dB more out of the pauses
  (`music_bed` -44.4 → -47.6 dB at the best threshold) and changes no LSD by
  more than 0.05 dB.
- **The speech-aware blend** (Step 4: limit 12 dB while speech is detected,
  no limit in pauses) fails with either detector. The energy detector (10 ms
  RMS 6 dB over a 2 s minimum, 50 ms hold, 50 ms release) takes music peaks
  for speech: -3.4 dB in pauses. The music swings 11 dB between its 5th and
  95th percentile 10 ms level, and at 5 dB SNR the speech sits only 6 dB over
  the music's median. Driven by the model's local SNR of the previous block
  (≥ 5 dB = speech) it reaches -27.0 dB, but the voice is that of the 12 dB
  limit (LSD 9.87). The limit does not undo the deep filter, so no limit
  schedule can help.
- **The standard model** is worse at every setting tried: 30 ms, and the
  music and fan pauses, the LSD and the keyboard leak are all worse than LL
  at the same setting. It does remove more mouse clicks (-6.5 dB at
  threshold 5), still short of the -15 dB target.

### Earlier "voice damage" numbers were partly the bench's gain

The bench computes LSD after one least-squares gain (`metrics()` in
`audio_bench.rs`). When the output's waveform does not follow the clean
voice, that gain collapses: 0.036 for LL under 5 dB music, 0.005 under the
fan. The LSD then compares the clean spectrum with one 29-46 dB too quiet.
Recomputed with a plain level-matching gain (phase-blind), LL without a limit
scores 5.65 dB under 5 dB music, not 9.64, and 5.16 dB under the fan, not
11.33. Both tables are below. The "noise leak during speech" has the same
cause. If the voice comes out uncorrelated with the clean voice, output(noisy)
minus output(clean) is about twice the voice energy: 3 dB plus the 5 dB SNR
gives about +8 dB, matching the +7.8 dB measured, even if no music leaks. So
the decision doc's +7.8 dB leak and 9.64 dB LSD measure the deep filter's
phase change more than a change in the voice's spectrum. Whether that phase
change is what the user heard is for the ear to say. The threshold setting
removes most of it, and with it both inflated figures (LSD 4.60 bench / 4.71
level-matched, leak +2.2 dB). This matters for Worker F too: an output that
rebuilds phase will score badly on the bench's LSD and leak however it
sounds. A level-matched LSD column would be a bench change, and Worker F owns
the bench this round.

## Acceptance figures, best setting

LL, `max_db_df_thresh 5`, `post_filter_beta 0.02`, other `RuntimeParams` at
libDF defaults (`min_db_thresh -10`, `max_db_erb_thresh 30`, no attenuation
limit). Bench figures after the app's chain (automatic volume, gate), as for
every replayed row. Shipped `rnnoise+enhanced` in brackets.

| figure | target | LL threshold 5 + post-filter | pass? |
| --- | --- | ---: | --- |
| `music_bed` (5 dB) residual noise in pauses | ≤ -20 dB (-0.8) | **-47.6** | pass |
| `music_bed` (5 dB) LSD of the voice | ≤ 6.0 dB (4.35) | **4.60** (level-matched 4.71) | pass |
| `music_bed` (5 dB) noise leak during speech | ≤ +3 dB (+0.1) | **+2.2** | pass |
| `music_bed_12` residual in pauses | ≤ -25 dB (+1.7) | **-33.5** | pass |
| `music_bed_12` LSD | ≤ 5.0 dB (3.41) | **3.65** | pass |
| `fan_plus_mouse` residual in pauses | ≤ -25 dB (-27.0) | **-27.0** | pass |
| `keyboard_during_speech` noise leak during speech | ≤ -10 dB (-0.1) | +3.9 | **fail** (clicks pass through) |
| `mouse_clicks` residual in pauses | ≤ -15 dB (-16.7) | +1.4 | **fail** (clicks pass through) |
| speech attenuation, every row | within ±1.5 dB | -0.6 to +0.3 | pass |
| clean voice alone: output level | within 0.5 dB | -0.4 dB after the app chain; 0.00 dB from the suppressor alone | pass |
| clean voice alone: LSD | ≤ 1.0 dB (2.12) | 1.42 after the app chain; **0.05** from the suppressor alone | pass for the suppressor; the app chain alone adds about 1.4 dB (the shipped chain scores 2.12) |
| added delay | ≤ 20 ms (10) | 10 ms (declared and measured) | pass |
| CPU per 10 ms block, p99 | ≤ 1.5 ms (0.054) | 0.90-1.05 ms (mean 0.34-0.37) | pass |
| model file | ≤ 40 MB | 36.4 MB (`DeepFilterNet3_ll_onnx.tar.gz`) | pass |

The click rows are the gap. Running RNNoise with click suppression at Normal
after it (Worker E's replay chain) brings `mouse_clicks` to -4.1 dB and
`keyboard_during_speech` leak to +4.1 dB, still failing, at 20 ms (table
below). Click suppression in the client needs a voice probability, which
only RNNoise supplies today. Feeding it DeepFilterNet's local SNR instead
would keep 10 ms. That is for Task H, and it is not measured here.

Neighbouring thresholds trade the same way. 3 dB keeps the voice slightly
closer (LSD 4.48, leak +1.2) and still passes `music_bed_12` (-27.1), but
misses the fan target (-20.2). 7 dB removes a little more music at 12 dB
(-34.6), but the leak under 5 dB music goes over target (+3.6).

## Delay and CPU

- **Delay**: 10 ms for LL (480 samples: 960-sample window minus 480 hop,
  lookahead 0), 30 ms for the standard model, declared by libDF and measured
  by the bench's cross-correlation on the clean voice. None of the knobs
  changes it. The blend is one model pass: the limit is set with
  `DfTract::set_atten_lim` before each block, the decision is made on the
  block's own input (energy) or on the model's previous block (local SNR), so
  it adds no delay.
- **CPU** of the probe (`DfTract::process` per 480-sample block, release,
  one thread, over all 18 corpus signals of 30 s, 53 892 blocks), measured
  twice back to back on the user's machine while Worker F was also running:

| setting | mean (ms / 10 ms block) | p50 | p99 | max |
| --- | ---: | ---: | ---: | ---: |
| LL, threshold 5 + post-filter (best) | 0.336-0.369 | 0.245-0.253 | 0.901-1.052 | 2.0-4.0 |
| LL, defaults (decision doc's setting) | 0.351-0.399 | 0.214-0.250 | 0.890-0.964 | 2.3-3.9 |
| LL, speech-aware blend (local SNR) | 0.391 | 0.230 | 1.195 | 10.5 |
| standard, threshold 5 | 0.221 | 0.159 | 0.506 | 5.0 |

  The single-block maxima (up to 10.5 ms) are one-off stalls on a shared
  machine, as in the decision doc. A real-time thread would drop that block.

## Listening set

`C:\Important\Projects\2026-10-03\can\work\audio-worker-g-20261029\lyt\`, in
the plan's format: folders `musik-12db`, `musik-5db`, `tastatur-under-tale`,
`blaeser-og-mus`; each with `1-ren-stemme`, `2-ind-i-mikrofonen`,
`3-relay-i-dag` and four candidates. All are the bench's outputs after the
app's chain, cut by the chain's delay so they line up with the clean file,
and scaled to the clean file's level over its speech frames (every file
0.00 dB, peaks ≤ 0.69).

| file | setting |
| --- | --- |
| `4-dfn-ll-taerskel-5db-postfilter.wav` | the best setting above |
| `5-dfn-ll-uden-dybfilter.wav` | LL with the deep filter never run (`max_db_df_thresh -100`): voice as the input, music -17 dB in pauses |
| `6-dfn-ll-graense-24db-postfilter.wav` | the plan's limit candidate: `atten_lim_db 24`, post-filter 0.02 |
| `7-dfn-ll-tale-12db-pause-fri.wav` | the speech-aware blend: 12 dB limit while the model's local SNR says speech, none in pauses |

Unprocessed DeepFilterNet-LL (the decision doc's setting) is in Worker E's
set (`audio-worker-e-20261029\lyt\4-deepfilternet-ll.wav`, `music_bed`
only).

## How it was measured

1. Worker A's bench, release build, with Worker E's `VOICE_TEST_WAV` patch
   applied locally and two local rows: `music_bed_12` (generator
   `music_bed`, seed 51, SNR 12 dB) and `clean_voice` (the voice with zero
   noise). Nothing under `crates/` is committed: Worker F owns the bench this
   round. Without these rows the baseline reproduces the decision doc's table
   exactly (shipped `music_bed` -0.8 dB / LSD 4.35).
2. `audio-bench --write-wav --chain none --chain rnnoise+enhanced` with
   `VOICE_TEST_WAV` set to `voice3-prepared.wav` writes the corpus and the
   baseline.
3. Worker E's probe, extended (`audio-worker-g-20261029\dfprobe`, outside
   the repository; libDF git rev `d375b2d8`, `tract-*` `=0.21.4`, unchanged
   `Cargo.lock`). New flags: `--model ll|standard|<tar.gz>`,
   `--atten-lim-db`, `--post-filter-beta`, `--min-db-thresh`,
   `--max-db-erb-thresh`, `--max-db-df-thresh`, `--only`, and the blend
   (`--blend-speech-db`, `--blend-pause-db`, `--blend-detector energy|lsnr`,
   `--blend-floor-db`, `--blend-lsnr-db`, `--blend-hold-ms`,
   `--blend-release-ms`). The energy detector is the voice gate's minimum
   statistics (`voice_gate.rs` `LevelFloor`: quietest 10 ms RMS of 8 windows
   of 25 blocks, never below -80 dBFS). The blend writes one line per block
   (level, local SNR, decision, mixed-back factor). Its output is
   deterministic across runs.
4. Each setting replays through the bench's `deepfilternet-ll (replayed)`
   chain, which takes its delay from the probe's `delay.txt` (so the standard
   model replays through it at 30 ms). The best setting also ran through
   `deepfilternet-ll-then-rnnoise+enhanced (replayed)`.
5. The level-matched LSD, the band correlations and the raw (pre-app-chain)
   pause residuals come from short Python scripts on the same WAVs. They use
   the bench's own frame rules (speech: within 30 dB of the loudest clean
   10 ms frame; pause: more than 60 dB below; LSD: 512-point FFT, Hann, 50 dB
   floor) and reproduce the bench's LSD to the second decimal.

Run at 2026-10-29T15:33-15:55Z, release build, on the user's machine (AMD,
16 logical processors) while it was otherwise in use. Scripts, probe source
and the full run folder are in the shared hand-over notes
(`worker-g-deepfilternet-tuning-handover.md`).

## Sweep, key figures per setting

Bench figures on the recorded voice. "LSD level" is the same LSD with a
level-matching gain instead of the bench's least-squares gain. `ll-limN-pfB`
is `atten_lim_db N` with `post_filter_beta B`. `ll-dfT` is
`max_db_df_thresh T` (default 20, i.e. `ll-limnone-pf0`). `ll-nodf` never
runs the deep filter. "Worst speech attenuation" is the row furthest from
0 dB.

| setting | music 5 dB: pauses | leak | LSD bench | LSD level | music 12 dB: pauses | LSD bench | LSD level | fan: pauses | keyboard: leak | mouse: pauses | clean voice: LSD | worst speech attenuation |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| none | 0.0 | 0.0 | 4.36 | 4.50 | 0.0 | 3.08 | 3.11 | 0.0 | 0.0 | 0.0 | 0.00 | -1.8 |
| rnnoise+enhanced | -0.8 | 0.1 | 4.35 | 4.38 | 1.7 | 3.41 | 3.40 | -27.0 | -0.1 | -16.7 | 2.12 | -0.5 |
| ll-lim12-pf0 | -5.6 | 6.2 | 9.92 | 5.96 | -0.9 | 6.78 | 5.61 | -3.4 | 11.1 | 4.2 | 2.73 | 1.5 |
| ll-lim12-pf0.02 | -5.6 | 6.2 | 9.92 | 5.96 | -0.9 | 6.77 | 5.61 | -3.4 | 11.1 | 4.1 | 2.73 | 1.6 |
| ll-lim18-pf0 | -7.1 | 7.1 | 9.71 | 5.66 | -5.0 | 6.60 | 5.47 | -5.0 | 11.5 | 2.4 | 2.04 | 1.7 |
| ll-lim18-pf0.02 | -7.3 | 7.1 | 9.71 | 5.66 | -5.0 | 6.60 | 5.47 | -4.9 | 11.5 | 2.3 | 2.04 | 1.7 |
| ll-lim24-pf0 | -11.5 | 7.8 | 9.66 | 5.63 | -10.6 | 6.55 | 5.40 | -12.1 | 11.7 | 1.2 | 1.68 | -0.5 |
| ll-lim24-pf0.02 | -11.5 | 7.8 | 9.66 | 5.62 | -10.7 | 6.55 | 5.39 | -12.2 | 11.7 | 1.1 | 1.68 | -0.5 |
| ll-limnone-pf0 | -41.6 | 7.8 | 9.64 | 5.65 | -30.0 | 6.45 | 5.36 | -23.7 | 11.7 | 0.4 | 1.38 | -0.5 |
| ll-limnone-pf0.02 | -44.7 | 7.8 | 9.64 | 5.66 | -30.3 | 6.48 | 5.36 | -24.4 | 11.7 | 0.4 | 1.40 | -0.5 |
| std-limnone-pf0 | -27.2 | 7.8 | 10.23 | 5.69 | -24.9 | 7.36 | 5.44 | -20.3 | 15.7 | -3.4 | 1.47 | 0.6 |
| std-limnone-pf0.02 | -29.2 | 7.8 | 10.24 | 5.74 | -25.9 | 7.47 | 5.47 | -22.5 | 15.8 | -3.5 | 1.48 | 0.7 |
| std-lim24-pf0.02 | -11.6 | 7.7 | 10.26 | 5.61 | -9.8 | 7.49 | 5.48 | -12.4 | 15.7 | -2.5 | 1.75 | 0.5 |
| ll-blend-energy12 | -3.4 | 7.0 | 9.86 | 6.14 | -0.5 | 6.84 | 5.75 | -4.1 | 11.1 | 4.2 | 2.73 | 0.7 |
| ll-blend-lsnr12 | -27.0 | 6.7 | 9.87 | 5.85 | -17.4 | 6.86 | 5.61 | -23.3 | 11.1 | 3.8 | 2.73 | -0.5 |
| ll-nodf | -17.1 | 0.4 | 4.40 | 4.58 | -12.9 | 3.43 | 3.56 | -13.3 | 0.2 | 2.9 | 1.42 | -0.6 |
| ll-nodf-pf0.02 | -17.0 | 0.3 | 4.42 | 4.58 | -12.9 | 3.44 | 3.57 | -13.3 | 0.2 | 2.8 | 1.43 | -0.6 |
| ll-df-5 | -20.8 | 0.4 | 4.39 | 4.55 | -16.7 | 3.45 | 3.58 | -16.3 | 0.4 | 2.1 | 1.42 | -0.6 |
| ll-df0 | -44.1 | 0.5 | 4.41 | 4.55 | -19.8 | 3.51 | 3.64 | -20.3 | 2.0 | 1.7 | 1.41 | -0.6 |
| ll-df3 | -44.5 | 1.2 | 4.48 | 4.61 | -27.1 | 3.57 | 3.70 | -20.2 | 2.7 | 1.1 | 1.41 | -0.6 |
| ll-df5 | -44.4 | 2.2 | 4.55 | 4.68 | -33.1 | 3.63 | 3.75 | -26.4 | 3.9 | 1.5 | 1.41 | -0.6 |
| ll-df5-pf0.02 | -47.6 | 2.2 | 4.60 | 4.71 | -33.5 | 3.65 | 3.77 | -27.0 | 3.9 | 1.4 | 1.42 | -0.6 |
| ll-df7 | -43.5 | 3.6 | 4.70 | 4.81 | -34.6 | 3.72 | 3.85 | -25.0 | 5.1 | 1.8 | 1.40 | -0.6 |
| ll-df10 | -41.8 | 5.8 | 5.27 | 5.18 | -33.0 | 3.95 | 4.08 | -24.3 | 6.8 | 1.5 | 1.40 | -0.6 |
| std-nodf | -12.8 | 0.9 | 5.05 | 5.57 | -12.2 | 4.58 | 4.94 | -8.3 | 11.4 | -15.7 | 1.44 | 1.3 |
| std-df0 | -24.6 | 1.9 | 5.12 | 5.69 | -17.0 | 4.70 | 5.09 | -14.1 | 11.9 | -7.0 | 1.45 | 1.3 |
| std-df5 | -24.2 | 4.7 | 4.93 | 5.50 | -20.0 | 4.72 | 5.15 | -18.0 | 13.1 | -6.5 | 1.47 | 1.2 |

## Full rows: shipped, DeepFilterNet-LL as before, and the candidates

Same columns as the bench, plus the level-matched LSD. Settings named as above.

| setting | item | residual in pauses (dB) | leak during speech (dB) | segSNR (dB) | LSD, bench (dB) | LSD, level-matched (dB) | speech attenuation (dB) |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| none | keyboard_between_words | 0.0 | n/a | 35.0 | 0.00 | 0.00 | 0.0 |
| none | keyboard_during_speech | 0.0 | 0.0 | 25.9 | 3.17 | 3.15 | -0.1 |
| none | mouse_clicks | 0.0 | 0.0 | 32.5 | 0.86 | 0.85 | -0.0 |
| none | fan_plus_mouse | 0.0 | 0.0 | 3.0 | 7.82 | 8.47 | -1.2 |
| none | music_bed | 0.0 | 0.0 | 3.6 | 4.36 | 4.50 | -1.2 |
| none | music_bed_12 | 0.0 | 0.0 | 9.7 | 3.08 | 3.11 | -0.3 |
| none | second_talker | 0.0 | 0.0 | 5.0 | 2.20 | 1.50 | -1.8 |
| none | clean_voice | -18.4 | n/a | 35.0 | 0.00 | 0.00 | 0.0 |
| none | silence_floor | 0.0 | n/a | n/a | n/a | n/a | n/a |
| rnnoise+enhanced | keyboard_between_words | -24.3 | n/a | 9.0 | 2.16 | 2.12 | -0.3 |
| rnnoise+enhanced | keyboard_during_speech | -16.6 | -0.1 | 8.4 | 3.50 | 3.50 | -0.1 |
| rnnoise+enhanced | mouse_clicks | -16.7 | 7.5 | 8.7 | 2.64 | 2.59 | -0.2 |
| rnnoise+enhanced | fan_plus_mouse | -27.0 | -1.5 | 4.4 | 5.33 | 5.34 | 0.3 |
| rnnoise+enhanced | music_bed | -0.8 | 0.1 | 3.4 | 4.35 | 4.38 | -0.2 |
| rnnoise+enhanced | music_bed_12 | 1.7 | 2.7 | 5.9 | 3.41 | 3.40 | -0.5 |
| rnnoise+enhanced | second_talker | 0.2 | -0.4 | 4.3 | 3.22 | 2.55 | -0.4 |
| rnnoise+enhanced | clean_voice | -17.5 | n/a | 9.1 | 2.12 | 2.08 | -0.2 |
| rnnoise+enhanced | silence_floor | -11.3 | n/a | n/a | n/a | n/a | n/a |
| ll-lim24-pf0.02 | keyboard_between_words | -6.4 | n/a | 10.9 | 1.78 | 1.85 | -0.5 |
| ll-lim24-pf0.02 | keyboard_during_speech | -3.0 | 11.7 | 7.1 | 3.31 | 3.43 | -0.2 |
| ll-lim24-pf0.02 | mouse_clicks | 1.1 | 15.4 | 9.6 | 2.25 | 2.34 | -0.5 |
| ll-lim24-pf0.02 | fan_plus_mouse | -12.2 | 7.8 | 0.0 | 11.42 | 5.17 | 0.4 |
| ll-lim24-pf0.02 | music_bed | -11.5 | 7.8 | 0.0 | 9.66 | 5.62 | 0.0 |
| ll-lim24-pf0.02 | music_bed_12 | -10.7 | 14.1 | -0.2 | 6.55 | 5.39 | -0.2 |
| ll-lim24-pf0.02 | second_talker | -6.4 | 2.3 | 2.7 | 4.51 | 4.17 | -0.3 |
| ll-lim24-pf0.02 | clean_voice | 14.3 | n/a | 11.5 | 1.68 | 1.74 | -0.3 |
| ll-lim24-pf0.02 | silence_floor | -20.8 | n/a | n/a | n/a | n/a | n/a |
| ll-limnone-pf0 | keyboard_between_words | -6.9 | n/a | 12.1 | 1.47 | 1.54 | -0.5 |
| ll-limnone-pf0 | keyboard_during_speech | -3.1 | 11.7 | 8.2 | 2.94 | 3.00 | -0.2 |
| ll-limnone-pf0 | mouse_clicks | 0.4 | 15.4 | 10.9 | 1.92 | 2.01 | -0.4 |
| ll-limnone-pf0 | fan_plus_mouse | -23.7 | 8.1 | 0.0 | 11.33 | 5.16 | 0.4 |
| ll-limnone-pf0 | music_bed | -41.6 | 7.8 | 0.0 | 9.64 | 5.65 | 0.4 |
| ll-limnone-pf0 | music_bed_12 | -30.0 | 14.2 | -0.1 | 6.45 | 5.36 | 0.2 |
| ll-limnone-pf0 | second_talker | -7.9 | 2.2 | 3.5 | 4.32 | 4.05 | 0.1 |
| ll-limnone-pf0 | clean_voice | 13.3 | n/a | 12.6 | 1.38 | 1.44 | -0.4 |
| ll-limnone-pf0 | silence_floor | -24.3 | n/a | n/a | n/a | n/a | n/a |
| ll-blend-energy12 | keyboard_between_words | -1.0 | n/a | 7.1 | 2.82 | 2.96 | -0.5 |
| ll-blend-energy12 | keyboard_during_speech | -2.3 | 11.1 | 4.2 | 4.65 | 5.01 | -0.1 |
| ll-blend-energy12 | mouse_clicks | 4.2 | 15.0 | 6.1 | 3.35 | 3.54 | -0.5 |
| ll-blend-energy12 | fan_plus_mouse | -4.1 | 6.8 | -0.0 | 11.52 | 6.79 | 0.7 |
| ll-blend-energy12 | music_bed | -3.4 | 7.0 | 0.0 | 9.86 | 6.14 | -0.3 |
| ll-blend-energy12 | music_bed_12 | -0.5 | 13.2 | -0.2 | 6.84 | 5.75 | -0.4 |
| ll-blend-energy12 | second_talker | -2.7 | 1.9 | 1.0 | 5.18 | 4.60 | -0.3 |
| ll-blend-energy12 | clean_voice | 17.8 | n/a | 7.4 | 2.73 | 2.85 | -0.2 |
| ll-blend-energy12 | silence_floor | -2.2 | n/a | n/a | n/a | n/a | n/a |
| ll-blend-lsnr12 | keyboard_between_words | -2.8 | n/a | 7.1 | 2.80 | 2.94 | -0.5 |
| ll-blend-lsnr12 | keyboard_during_speech | -2.3 | 11.1 | 4.2 | 4.63 | 4.98 | -0.1 |
| ll-blend-lsnr12 | mouse_clicks | 3.8 | 15.0 | 6.2 | 3.34 | 3.53 | -0.5 |
| ll-blend-lsnr12 | fan_plus_mouse | -23.3 | 6.9 | 0.0 | 11.43 | 6.24 | 0.2 |
| ll-blend-lsnr12 | music_bed | -27.0 | 6.7 | 0.0 | 9.87 | 5.85 | 0.2 |
| ll-blend-lsnr12 | music_bed_12 | -17.4 | 12.9 | -0.2 | 6.86 | 5.61 | 0.1 |
| ll-blend-lsnr12 | second_talker | -6.5 | 1.7 | 1.2 | 5.10 | 4.58 | -0.1 |
| ll-blend-lsnr12 | clean_voice | 17.8 | n/a | 7.4 | 2.73 | 2.85 | -0.2 |
| ll-blend-lsnr12 | silence_floor | -24.3 | n/a | n/a | n/a | n/a | n/a |
| ll-nodf | keyboard_between_words | 0.3 | n/a | 11.9 | 1.50 | 1.57 | -0.6 |
| ll-nodf | keyboard_during_speech | -0.3 | 0.2 | 10.7 | 2.82 | 2.92 | -0.4 |
| ll-nodf | mouse_clicks | 2.9 | 5.5 | 11.4 | 1.91 | 1.98 | -0.6 |
| ll-nodf | fan_plus_mouse | -13.3 | 0.7 | 2.4 | 5.95 | 6.32 | -0.6 |
| ll-nodf | music_bed | -17.1 | 0.4 | 2.9 | 4.40 | 4.58 | -0.4 |
| ll-nodf | music_bed_12 | -12.9 | 2.0 | 6.1 | 3.43 | 3.56 | -0.4 |
| ll-nodf | second_talker | -8.0 | -0.9 | 6.4 | 2.35 | 2.02 | -0.5 |
| ll-nodf | clean_voice | -16.4 | n/a | 12.3 | 1.42 | 1.48 | -0.4 |
| ll-nodf | silence_floor | 11.6 | n/a | n/a | n/a | n/a | n/a |
| ll-df5-pf0.02 | keyboard_between_words | -2.8 | n/a | 12.1 | 1.49 | 1.56 | -0.6 |
| ll-df5-pf0.02 | keyboard_during_speech | -3.3 | 3.9 | 9.9 | 2.84 | 2.93 | -0.3 |
| ll-df5-pf0.02 | mouse_clicks | 1.4 | 9.0 | 11.4 | 1.92 | 2.00 | -0.6 |
| ll-df5-pf0.02 | fan_plus_mouse | -27.0 | 2.1 | 2.1 | 5.15 | 5.28 | -0.0 |
| ll-df5-pf0.02 | music_bed | -47.6 | 2.2 | 2.2 | 4.60 | 4.71 | 0.3 |
| ll-df5-pf0.02 | music_bed_12 | -33.5 | 3.8 | 5.6 | 3.65 | 3.77 | -0.3 |
| ll-df5-pf0.02 | second_talker | -8.1 | -1.4 | 6.8 | 2.76 | 2.58 | -0.2 |
| ll-df5-pf0.02 | clean_voice | 7.1 | n/a | 12.4 | 1.42 | 1.49 | -0.4 |
| ll-df5-pf0.02 | silence_floor | -28.4 | n/a | n/a | n/a | n/a | n/a |

Best setting followed by RNNoise with click suppression at Normal (Worker E's replay chain, 20 ms), bench columns only:

| chain | item | SNR in (dB) | residual noise in pauses (dB) | noise leak during speech (dB) | speech segSNR (dB) | LSD (dB) | speech attenuation (dB) | CPU (ms / 10 ms block) |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| ll-df5-pf0.02, then rnnoise+enhanced | keyboard_between_words | 10.0 | -18.4 | n/a | 9.0 | 2.16 | -0.2 | 0.034 |
| ll-df5-pf0.02, then rnnoise+enhanced | keyboard_during_speech | 10.0 | -7.1 | 4.1 | 7.9 | 3.32 | -0.0 | 0.033 |
| ll-df5-pf0.02, then rnnoise+enhanced | mouse_clicks | 15.0 | -4.1 | 9.1 | 8.7 | 2.56 | -0.3 | 0.035 |
| ll-df5-pf0.02, then rnnoise+enhanced | fan_plus_mouse | 5.0 | -34.1 | 1.4 | 2.3 | 5.60 | 0.4 | 0.031 |
| ll-df5-pf0.02, then rnnoise+enhanced | music_bed | 5.0 | -54.6 | 1.9 | 2.1 | 5.24 | 0.6 | 0.033 |
| ll-df5-pf0.02, then rnnoise+enhanced | music_bed_12 | 12.0 | -35.5 | 3.5 | 5.1 | 4.07 | -0.1 | 0.034 |
| ll-df5-pf0.02, then rnnoise+enhanced | second_talker | 0.0 | -7.7 | -1.4 | 4.9 | 3.61 | -0.0 | 0.034 |
| ll-df5-pf0.02, then rnnoise+enhanced | clean_voice | n/a | 5.9 | n/a | 9.1 | 2.16 | -0.2 | 0.041 |
| ll-df5-pf0.02, then rnnoise+enhanced | silence_floor | n/a | -95.0 | n/a | n/a | n/a | n/a | 0.026 |

## Appendix: every setting, every row

| setting | item | residual in pauses (dB) | leak during speech (dB) | segSNR (dB) | LSD, bench (dB) | LSD, level-matched (dB) | speech attenuation (dB) |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| none | keyboard_between_words | 0.0 | n/a | 35.0 | 0.00 | 0.00 | 0.0 |
| none | keyboard_during_speech | 0.0 | 0.0 | 25.9 | 3.17 | 3.15 | -0.1 |
| none | mouse_clicks | 0.0 | 0.0 | 32.5 | 0.86 | 0.85 | -0.0 |
| none | fan_plus_mouse | 0.0 | 0.0 | 3.0 | 7.82 | 8.47 | -1.2 |
| none | music_bed | 0.0 | 0.0 | 3.6 | 4.36 | 4.50 | -1.2 |
| none | music_bed_12 | 0.0 | 0.0 | 9.7 | 3.08 | 3.11 | -0.3 |
| none | second_talker | 0.0 | 0.0 | 5.0 | 2.20 | 1.50 | -1.8 |
| none | clean_voice | -18.4 | n/a | 35.0 | 0.00 | 0.00 | 0.0 |
| none | silence_floor | 0.0 | n/a | n/a | n/a | n/a | n/a |
| rnnoise+enhanced | keyboard_between_words | -24.3 | n/a | 9.0 | 2.16 | 2.12 | -0.3 |
| rnnoise+enhanced | keyboard_during_speech | -16.6 | -0.1 | 8.4 | 3.50 | 3.50 | -0.1 |
| rnnoise+enhanced | mouse_clicks | -16.7 | 7.5 | 8.7 | 2.64 | 2.59 | -0.2 |
| rnnoise+enhanced | fan_plus_mouse | -27.0 | -1.5 | 4.4 | 5.33 | 5.34 | 0.3 |
| rnnoise+enhanced | music_bed | -0.8 | 0.1 | 3.4 | 4.35 | 4.38 | -0.2 |
| rnnoise+enhanced | music_bed_12 | 1.7 | 2.7 | 5.9 | 3.41 | 3.40 | -0.5 |
| rnnoise+enhanced | second_talker | 0.2 | -0.4 | 4.3 | 3.22 | 2.55 | -0.4 |
| rnnoise+enhanced | clean_voice | -17.5 | n/a | 9.1 | 2.12 | 2.08 | -0.2 |
| rnnoise+enhanced | silence_floor | -11.3 | n/a | n/a | n/a | n/a | n/a |
| ll-lim12-pf0 | keyboard_between_words | -1.0 | n/a | 7.1 | 2.82 | 2.96 | -0.5 |
| ll-lim12-pf0 | keyboard_during_speech | -2.3 | 11.1 | 4.2 | 4.65 | 5.01 | -0.1 |
| ll-lim12-pf0 | mouse_clicks | 4.2 | 15.0 | 6.1 | 3.35 | 3.54 | -0.5 |
| ll-lim12-pf0 | fan_plus_mouse | -3.4 | 6.4 | 0.0 | 11.49 | 6.79 | 1.5 |
| ll-lim12-pf0 | music_bed | -5.6 | 6.2 | 0.0 | 9.92 | 5.96 | 1.4 |
| ll-lim12-pf0 | music_bed_12 | -0.9 | 12.8 | -0.2 | 6.78 | 5.61 | 0.4 |
| ll-lim12-pf0 | second_talker | -2.7 | 1.9 | 1.0 | 5.18 | 4.60 | -0.3 |
| ll-lim12-pf0 | clean_voice | 17.8 | n/a | 7.4 | 2.73 | 2.85 | -0.2 |
| ll-lim12-pf0 | silence_floor | 4.1 | n/a | n/a | n/a | n/a | n/a |
| ll-lim12-pf0.02 | keyboard_between_words | -1.2 | n/a | 7.1 | 2.82 | 2.96 | -0.5 |
| ll-lim12-pf0.02 | keyboard_during_speech | -2.4 | 11.1 | 4.2 | 4.65 | 5.00 | -0.1 |
| ll-lim12-pf0.02 | mouse_clicks | 4.1 | 15.0 | 6.1 | 3.35 | 3.54 | -0.5 |
| ll-lim12-pf0.02 | fan_plus_mouse | -3.4 | 6.4 | 0.0 | 11.49 | 6.72 | 1.6 |
| ll-lim12-pf0.02 | music_bed | -5.6 | 6.2 | 0.0 | 9.92 | 5.96 | 1.5 |
| ll-lim12-pf0.02 | music_bed_12 | -0.9 | 12.8 | -0.2 | 6.77 | 5.61 | 0.4 |
| ll-lim12-pf0.02 | second_talker | -2.7 | 1.9 | 1.0 | 5.18 | 4.60 | -0.4 |
| ll-lim12-pf0.02 | clean_voice | 17.7 | n/a | 7.4 | 2.73 | 2.85 | -0.2 |
| ll-lim12-pf0.02 | silence_floor | 4.1 | n/a | n/a | n/a | n/a | n/a |
| ll-lim18-pf0 | keyboard_between_words | -4.7 | n/a | 9.5 | 2.15 | 2.23 | -0.5 |
| ll-lim18-pf0 | keyboard_during_speech | -2.8 | 11.5 | 6.1 | 3.77 | 3.98 | -0.2 |
| ll-lim18-pf0 | mouse_clicks | 2.4 | 15.4 | 8.5 | 2.62 | 2.75 | -0.5 |
| ll-lim18-pf0 | fan_plus_mouse | -5.0 | 7.0 | 0.0 | 11.29 | 5.69 | 1.7 |
| ll-lim18-pf0 | music_bed | -7.1 | 7.1 | 0.0 | 9.71 | 5.66 | 1.0 |
| ll-lim18-pf0 | music_bed_12 | -5.0 | 13.8 | -0.2 | 6.60 | 5.47 | -0.1 |
| ll-lim18-pf0 | second_talker | -5.1 | 2.2 | 2.1 | 4.70 | 4.29 | -0.3 |
| ll-lim18-pf0 | clean_voice | 15.5 | n/a | 10.0 | 2.04 | 2.12 | -0.3 |
| ll-lim18-pf0 | silence_floor | -1.2 | n/a | n/a | n/a | n/a | n/a |
| ll-lim18-pf0.02 | keyboard_between_words | -4.9 | n/a | 9.5 | 2.15 | 2.23 | -0.5 |
| ll-lim18-pf0.02 | keyboard_during_speech | -2.8 | 11.5 | 6.1 | 3.77 | 3.98 | -0.2 |
| ll-lim18-pf0.02 | mouse_clicks | 2.3 | 15.4 | 8.5 | 2.62 | 2.74 | -0.5 |
| ll-lim18-pf0.02 | fan_plus_mouse | -4.9 | 7.0 | 0.0 | 11.28 | 5.56 | 1.7 |
| ll-lim18-pf0.02 | music_bed | -7.3 | 7.1 | 0.0 | 9.71 | 5.66 | 1.0 |
| ll-lim18-pf0.02 | music_bed_12 | -5.0 | 13.8 | -0.2 | 6.60 | 5.47 | -0.1 |
| ll-lim18-pf0.02 | second_talker | -5.1 | 2.2 | 2.1 | 4.70 | 4.30 | -0.3 |
| ll-lim18-pf0.02 | clean_voice | 15.5 | n/a | 10.0 | 2.04 | 2.12 | -0.3 |
| ll-lim18-pf0.02 | silence_floor | -0.9 | n/a | n/a | n/a | n/a | n/a |
| ll-lim24-pf0 | keyboard_between_words | -6.2 | n/a | 10.9 | 1.78 | 1.84 | -0.5 |
| ll-lim24-pf0 | keyboard_during_speech | -2.9 | 11.7 | 7.1 | 3.31 | 3.44 | -0.2 |
| ll-lim24-pf0 | mouse_clicks | 1.2 | 15.3 | 9.6 | 2.25 | 2.34 | -0.5 |
| ll-lim24-pf0 | fan_plus_mouse | -12.1 | 7.8 | 0.0 | 11.43 | 5.34 | 0.4 |
| ll-lim24-pf0 | music_bed | -11.5 | 7.8 | 0.0 | 9.66 | 5.63 | -0.0 |
| ll-lim24-pf0 | music_bed_12 | -10.6 | 14.1 | -0.2 | 6.55 | 5.40 | -0.2 |
| ll-lim24-pf0 | second_talker | -6.4 | 2.3 | 2.7 | 4.50 | 4.17 | -0.3 |
| ll-lim24-pf0 | clean_voice | 14.3 | n/a | 11.5 | 1.68 | 1.74 | -0.3 |
| ll-lim24-pf0 | silence_floor | -17.2 | n/a | n/a | n/a | n/a | n/a |
| ll-lim24-pf0.02 | keyboard_between_words | -6.4 | n/a | 10.9 | 1.78 | 1.85 | -0.5 |
| ll-lim24-pf0.02 | keyboard_during_speech | -3.0 | 11.7 | 7.1 | 3.31 | 3.43 | -0.2 |
| ll-lim24-pf0.02 | mouse_clicks | 1.1 | 15.4 | 9.6 | 2.25 | 2.34 | -0.5 |
| ll-lim24-pf0.02 | fan_plus_mouse | -12.2 | 7.8 | 0.0 | 11.42 | 5.17 | 0.4 |
| ll-lim24-pf0.02 | music_bed | -11.5 | 7.8 | 0.0 | 9.66 | 5.62 | 0.0 |
| ll-lim24-pf0.02 | music_bed_12 | -10.7 | 14.1 | -0.2 | 6.55 | 5.39 | -0.2 |
| ll-lim24-pf0.02 | second_talker | -6.4 | 2.3 | 2.7 | 4.51 | 4.17 | -0.3 |
| ll-lim24-pf0.02 | clean_voice | 14.3 | n/a | 11.5 | 1.68 | 1.74 | -0.3 |
| ll-lim24-pf0.02 | silence_floor | -20.8 | n/a | n/a | n/a | n/a | n/a |
| ll-limnone-pf0 | keyboard_between_words | -6.9 | n/a | 12.1 | 1.47 | 1.54 | -0.5 |
| ll-limnone-pf0 | keyboard_during_speech | -3.1 | 11.7 | 8.2 | 2.94 | 3.00 | -0.2 |
| ll-limnone-pf0 | mouse_clicks | 0.4 | 15.4 | 10.9 | 1.92 | 2.01 | -0.4 |
| ll-limnone-pf0 | fan_plus_mouse | -23.7 | 8.1 | 0.0 | 11.33 | 5.16 | 0.4 |
| ll-limnone-pf0 | music_bed | -41.6 | 7.8 | 0.0 | 9.64 | 5.65 | 0.4 |
| ll-limnone-pf0 | music_bed_12 | -30.0 | 14.2 | -0.1 | 6.45 | 5.36 | 0.2 |
| ll-limnone-pf0 | second_talker | -7.9 | 2.2 | 3.5 | 4.32 | 4.05 | 0.1 |
| ll-limnone-pf0 | clean_voice | 13.3 | n/a | 12.6 | 1.38 | 1.44 | -0.4 |
| ll-limnone-pf0 | silence_floor | -24.3 | n/a | n/a | n/a | n/a | n/a |
| ll-limnone-pf0.02 | keyboard_between_words | -7.1 | n/a | 12.0 | 1.48 | 1.54 | -0.5 |
| ll-limnone-pf0.02 | keyboard_during_speech | -3.2 | 11.7 | 8.0 | 2.98 | 3.01 | -0.2 |
| ll-limnone-pf0.02 | mouse_clicks | 0.4 | 15.4 | 10.8 | 1.94 | 2.02 | -0.4 |
| ll-limnone-pf0.02 | fan_plus_mouse | -24.4 | 8.0 | 0.0 | 11.30 | 5.10 | 0.4 |
| ll-limnone-pf0.02 | music_bed | -44.7 | 7.8 | 0.0 | 9.64 | 5.66 | 0.4 |
| ll-limnone-pf0.02 | music_bed_12 | -30.3 | 14.2 | -0.1 | 6.48 | 5.36 | 0.2 |
| ll-limnone-pf0.02 | second_talker | -7.9 | 2.2 | 3.5 | 4.34 | 4.07 | 0.1 |
| ll-limnone-pf0.02 | clean_voice | 13.2 | n/a | 12.6 | 1.40 | 1.45 | -0.4 |
| ll-limnone-pf0.02 | silence_floor | -28.4 | n/a | n/a | n/a | n/a | n/a |
| std-limnone-pf0 | keyboard_between_words | -13.9 | n/a | 10.9 | 1.89 | 1.90 | -0.4 |
| std-limnone-pf0 | keyboard_during_speech | -4.5 | 15.7 | 1.9 | 5.10 | 4.57 | 0.3 |
| std-limnone-pf0 | mouse_clicks | -3.4 | 18.8 | 6.4 | 3.09 | 3.06 | 0.1 |
| std-limnone-pf0 | fan_plus_mouse | -20.3 | 8.0 | 0.0 | 10.89 | 5.00 | 0.5 |
| std-limnone-pf0 | music_bed | -27.2 | 7.8 | 0.0 | 10.23 | 5.69 | 0.6 |
| std-limnone-pf0 | music_bed_12 | -24.9 | 14.4 | -0.0 | 7.36 | 5.44 | 0.5 |
| std-limnone-pf0 | second_talker | -22.4 | 2.8 | 1.5 | 5.03 | 4.66 | 0.5 |
| std-limnone-pf0 | clean_voice | 16.7 | n/a | 12.2 | 1.47 | 1.52 | -0.5 |
| std-limnone-pf0 | silence_floor | -27.0 | n/a | n/a | n/a | n/a | n/a |
| std-limnone-pf0.02 | keyboard_between_words | -14.0 | n/a | 10.8 | 1.98 | 1.99 | -0.4 |
| std-limnone-pf0.02 | keyboard_during_speech | -4.5 | 15.8 | 1.8 | 5.25 | 4.70 | 0.3 |
| std-limnone-pf0.02 | mouse_clicks | -3.5 | 18.9 | 6.2 | 3.22 | 3.18 | 0.1 |
| std-limnone-pf0.02 | fan_plus_mouse | -22.5 | 8.0 | 0.0 | 10.93 | 5.21 | 0.5 |
| std-limnone-pf0.02 | music_bed | -29.2 | 7.8 | 0.0 | 10.24 | 5.74 | 0.7 |
| std-limnone-pf0.02 | music_bed_12 | -25.9 | 14.5 | -0.0 | 7.47 | 5.47 | 0.5 |
| std-limnone-pf0.02 | second_talker | -23.1 | 2.8 | 1.5 | 5.12 | 4.76 | 0.6 |
| std-limnone-pf0.02 | clean_voice | 16.6 | n/a | 12.1 | 1.48 | 1.54 | -0.5 |
| std-limnone-pf0.02 | silence_floor | -34.2 | n/a | n/a | n/a | n/a | n/a |
| std-lim24-pf0.02 | keyboard_between_words | -11.2 | n/a | 9.7 | 2.20 | 2.22 | -0.5 |
| std-lim24-pf0.02 | keyboard_during_speech | -4.1 | 15.7 | 1.5 | 5.33 | 4.93 | 0.3 |
| std-lim24-pf0.02 | mouse_clicks | -2.5 | 18.9 | 5.8 | 3.33 | 3.32 | 0.1 |
| std-lim24-pf0.02 | fan_plus_mouse | -12.4 | 7.8 | 0.0 | 11.02 | 5.06 | 0.5 |
| std-lim24-pf0.02 | music_bed | -11.6 | 7.7 | 0.0 | 10.26 | 5.61 | 0.4 |
| std-lim24-pf0.02 | music_bed_12 | -9.8 | 14.3 | -0.0 | 7.49 | 5.48 | 0.2 |
| std-lim24-pf0.02 | second_talker | -7.9 | 3.0 | 1.1 | 5.27 | 4.78 | 0.1 |
| std-lim24-pf0.02 | clean_voice | 17.4 | n/a | 11.1 | 1.75 | 1.81 | -0.4 |
| std-lim24-pf0.02 | silence_floor | -32.9 | n/a | n/a | n/a | n/a | n/a |
| ll-blend-energy12 | keyboard_between_words | -1.0 | n/a | 7.1 | 2.82 | 2.96 | -0.5 |
| ll-blend-energy12 | keyboard_during_speech | -2.3 | 11.1 | 4.2 | 4.65 | 5.01 | -0.1 |
| ll-blend-energy12 | mouse_clicks | 4.2 | 15.0 | 6.1 | 3.35 | 3.54 | -0.5 |
| ll-blend-energy12 | fan_plus_mouse | -4.1 | 6.8 | -0.0 | 11.52 | 6.79 | 0.7 |
| ll-blend-energy12 | music_bed | -3.4 | 7.0 | 0.0 | 9.86 | 6.14 | -0.3 |
| ll-blend-energy12 | music_bed_12 | -0.5 | 13.2 | -0.2 | 6.84 | 5.75 | -0.4 |
| ll-blend-energy12 | second_talker | -2.7 | 1.9 | 1.0 | 5.18 | 4.60 | -0.3 |
| ll-blend-energy12 | clean_voice | 17.8 | n/a | 7.4 | 2.73 | 2.85 | -0.2 |
| ll-blend-energy12 | silence_floor | -2.2 | n/a | n/a | n/a | n/a | n/a |
| ll-blend-lsnr12 | keyboard_between_words | -2.8 | n/a | 7.1 | 2.80 | 2.94 | -0.5 |
| ll-blend-lsnr12 | keyboard_during_speech | -2.3 | 11.1 | 4.2 | 4.63 | 4.98 | -0.1 |
| ll-blend-lsnr12 | mouse_clicks | 3.8 | 15.0 | 6.2 | 3.34 | 3.53 | -0.5 |
| ll-blend-lsnr12 | fan_plus_mouse | -23.3 | 6.9 | 0.0 | 11.43 | 6.24 | 0.2 |
| ll-blend-lsnr12 | music_bed | -27.0 | 6.7 | 0.0 | 9.87 | 5.85 | 0.2 |
| ll-blend-lsnr12 | music_bed_12 | -17.4 | 12.9 | -0.2 | 6.86 | 5.61 | 0.1 |
| ll-blend-lsnr12 | second_talker | -6.5 | 1.7 | 1.2 | 5.10 | 4.58 | -0.1 |
| ll-blend-lsnr12 | clean_voice | 17.8 | n/a | 7.4 | 2.73 | 2.85 | -0.2 |
| ll-blend-lsnr12 | silence_floor | -24.3 | n/a | n/a | n/a | n/a | n/a |
| ll-nodf | keyboard_between_words | 0.3 | n/a | 11.9 | 1.50 | 1.57 | -0.6 |
| ll-nodf | keyboard_during_speech | -0.3 | 0.2 | 10.7 | 2.82 | 2.92 | -0.4 |
| ll-nodf | mouse_clicks | 2.9 | 5.5 | 11.4 | 1.91 | 1.98 | -0.6 |
| ll-nodf | fan_plus_mouse | -13.3 | 0.7 | 2.4 | 5.95 | 6.32 | -0.6 |
| ll-nodf | music_bed | -17.1 | 0.4 | 2.9 | 4.40 | 4.58 | -0.4 |
| ll-nodf | music_bed_12 | -12.9 | 2.0 | 6.1 | 3.43 | 3.56 | -0.4 |
| ll-nodf | second_talker | -8.0 | -0.9 | 6.4 | 2.35 | 2.02 | -0.5 |
| ll-nodf | clean_voice | -16.4 | n/a | 12.3 | 1.42 | 1.48 | -0.4 |
| ll-nodf | silence_floor | 11.6 | n/a | n/a | n/a | n/a | n/a |
| ll-nodf-pf0.02 | keyboard_between_words | 0.1 | n/a | 11.9 | 1.49 | 1.56 | -0.6 |
| ll-nodf-pf0.02 | keyboard_during_speech | -0.3 | 0.2 | 10.6 | 2.85 | 2.94 | -0.4 |
| ll-nodf-pf0.02 | mouse_clicks | 2.8 | 5.5 | 11.4 | 1.93 | 1.99 | -0.6 |
| ll-nodf-pf0.02 | fan_plus_mouse | -13.3 | 0.7 | 2.4 | 5.95 | 6.31 | -0.6 |
| ll-nodf-pf0.02 | music_bed | -17.0 | 0.3 | 2.9 | 4.42 | 4.58 | -0.4 |
| ll-nodf-pf0.02 | music_bed_12 | -12.9 | 2.0 | 6.1 | 3.44 | 3.57 | -0.4 |
| ll-nodf-pf0.02 | second_talker | -8.0 | -0.9 | 6.3 | 2.43 | 2.10 | -0.5 |
| ll-nodf-pf0.02 | clean_voice | -18.9 | n/a | 12.3 | 1.43 | 1.49 | -0.4 |
| ll-nodf-pf0.02 | silence_floor | 11.6 | n/a | n/a | n/a | n/a | n/a |
| ll-df-5 | keyboard_between_words | -2.2 | n/a | 12.0 | 1.49 | 1.56 | -0.5 |
| ll-df-5 | keyboard_during_speech | -1.8 | 0.4 | 10.8 | 2.81 | 2.90 | -0.3 |
| ll-df-5 | mouse_clicks | 2.1 | 5.6 | 11.4 | 1.91 | 1.98 | -0.6 |
| ll-df-5 | fan_plus_mouse | -16.3 | 0.5 | 2.7 | 5.84 | 6.16 | -0.6 |
| ll-df-5 | music_bed | -20.8 | 0.4 | 3.2 | 4.39 | 4.55 | -0.4 |
| ll-df-5 | music_bed_12 | -16.7 | 2.1 | 6.1 | 3.45 | 3.58 | -0.4 |
| ll-df-5 | second_talker | -8.1 | -1.0 | 6.6 | 2.39 | 2.09 | -0.5 |
| ll-df-5 | clean_voice | -6.9 | n/a | 12.4 | 1.42 | 1.48 | -0.4 |
| ll-df-5 | silence_floor | -19.4 | n/a | n/a | n/a | n/a | n/a |
| ll-df0 | keyboard_between_words | -2.6 | n/a | 12.0 | 1.49 | 1.56 | -0.5 |
| ll-df0 | keyboard_during_speech | -2.4 | 2.0 | 10.4 | 2.83 | 2.92 | -0.4 |
| ll-df0 | mouse_clicks | 1.7 | 5.7 | 11.4 | 1.91 | 1.98 | -0.6 |
| ll-df0 | fan_plus_mouse | -20.3 | 0.4 | 2.9 | 5.52 | 5.77 | -0.4 |
| ll-df0 | music_bed | -44.1 | 0.5 | 3.1 | 4.41 | 4.55 | -0.2 |
| ll-df0 | music_bed_12 | -19.8 | 2.7 | 6.1 | 3.51 | 3.64 | -0.4 |
| ll-df0 | second_talker | -8.0 | -1.2 | 6.8 | 2.44 | 2.20 | -0.4 |
| ll-df0 | clean_voice | 1.9 | n/a | 12.4 | 1.41 | 1.48 | -0.4 |
| ll-df0 | silence_floor | -24.3 | n/a | n/a | n/a | n/a | n/a |
| ll-df3 | keyboard_between_words | -2.9 | n/a | 12.1 | 1.49 | 1.56 | -0.6 |
| ll-df3 | keyboard_during_speech | -3.8 | 2.7 | 10.2 | 2.83 | 2.92 | -0.3 |
| ll-df3 | mouse_clicks | 1.1 | 8.8 | 11.4 | 1.91 | 2.00 | -0.6 |
| ll-df3 | fan_plus_mouse | -20.2 | 1.2 | 2.6 | 5.23 | 5.46 | -0.3 |
| ll-df3 | music_bed | -44.5 | 1.2 | 2.7 | 4.48 | 4.61 | 0.1 |
| ll-df3 | music_bed_12 | -27.1 | 3.2 | 5.9 | 3.57 | 3.70 | -0.4 |
| ll-df3 | second_talker | -8.1 | -1.3 | 6.9 | 2.57 | 2.36 | -0.3 |
| ll-df3 | clean_voice | 5.9 | n/a | 12.5 | 1.41 | 1.47 | -0.4 |
| ll-df3 | silence_floor | -24.3 | n/a | n/a | n/a | n/a | n/a |
| ll-df5 | keyboard_between_words | -2.7 | n/a | 12.1 | 1.49 | 1.56 | -0.6 |
| ll-df5 | keyboard_during_speech | -3.3 | 3.9 | 10.0 | 2.80 | 2.89 | -0.3 |
| ll-df5 | mouse_clicks | 1.5 | 8.9 | 11.4 | 1.90 | 1.99 | -0.6 |
| ll-df5 | fan_plus_mouse | -26.4 | 2.1 | 2.1 | 5.03 | 5.21 | -0.0 |
| ll-df5 | music_bed | -44.4 | 2.2 | 2.1 | 4.55 | 4.68 | 0.3 |
| ll-df5 | music_bed_12 | -33.1 | 3.8 | 5.6 | 3.63 | 3.75 | -0.3 |
| ll-df5 | second_talker | -8.1 | -1.4 | 6.9 | 2.69 | 2.52 | -0.2 |
| ll-df5 | clean_voice | 7.3 | n/a | 12.4 | 1.41 | 1.47 | -0.4 |
| ll-df5 | silence_floor | -24.3 | n/a | n/a | n/a | n/a | n/a |
| ll-df5-pf0.02 | keyboard_between_words | -2.8 | n/a | 12.1 | 1.49 | 1.56 | -0.6 |
| ll-df5-pf0.02 | keyboard_during_speech | -3.3 | 3.9 | 9.9 | 2.84 | 2.93 | -0.3 |
| ll-df5-pf0.02 | mouse_clicks | 1.4 | 9.0 | 11.4 | 1.92 | 2.00 | -0.6 |
| ll-df5-pf0.02 | fan_plus_mouse | -27.0 | 2.1 | 2.1 | 5.15 | 5.28 | -0.0 |
| ll-df5-pf0.02 | music_bed | -47.6 | 2.2 | 2.2 | 4.60 | 4.71 | 0.3 |
| ll-df5-pf0.02 | music_bed_12 | -33.5 | 3.8 | 5.6 | 3.65 | 3.77 | -0.3 |
| ll-df5-pf0.02 | second_talker | -8.1 | -1.4 | 6.8 | 2.76 | 2.58 | -0.2 |
| ll-df5-pf0.02 | clean_voice | 7.1 | n/a | 12.4 | 1.42 | 1.49 | -0.4 |
| ll-df5-pf0.02 | silence_floor | -28.4 | n/a | n/a | n/a | n/a | n/a |
| ll-df7 | keyboard_between_words | -4.1 | n/a | 12.1 | 1.49 | 1.56 | -0.6 |
| ll-df7 | keyboard_during_speech | -2.9 | 5.1 | 9.8 | 2.77 | 2.87 | -0.3 |
| ll-df7 | mouse_clicks | 1.8 | 10.1 | 11.3 | 1.91 | 1.99 | -0.6 |
| ll-df7 | fan_plus_mouse | -25.0 | 3.7 | 1.1 | 4.90 | 5.02 | 0.2 |
| ll-df7 | music_bed | -43.5 | 3.6 | 1.3 | 4.70 | 4.81 | 0.4 |
| ll-df7 | music_bed_12 | -34.6 | 4.8 | 5.1 | 3.72 | 3.85 | -0.2 |
| ll-df7 | second_talker | -8.1 | -1.2 | 6.8 | 2.81 | 2.71 | -0.1 |
| ll-df7 | clean_voice | 8.3 | n/a | 12.5 | 1.40 | 1.47 | -0.4 |
| ll-df7 | silence_floor | -24.3 | n/a | n/a | n/a | n/a | n/a |
| ll-df10 | keyboard_between_words | -4.2 | n/a | 12.1 | 1.49 | 1.56 | -0.6 |
| ll-df10 | keyboard_during_speech | -4.3 | 6.8 | 9.4 | 2.77 | 2.86 | -0.2 |
| ll-df10 | mouse_clicks | 1.5 | 11.1 | 11.2 | 1.91 | 1.99 | -0.6 |
| ll-df10 | fan_plus_mouse | -24.3 | 5.7 | 0.0 | 5.18 | 5.01 | 0.5 |
| ll-df10 | music_bed | -41.8 | 5.8 | 0.1 | 5.27 | 5.18 | 0.5 |
| ll-df10 | music_bed_12 | -33.0 | 7.8 | 3.6 | 3.95 | 4.08 | -0.2 |
| ll-df10 | second_talker | -8.1 | -0.5 | 6.2 | 3.13 | 3.06 | -0.1 |
| ll-df10 | clean_voice | 8.6 | n/a | 12.5 | 1.40 | 1.47 | -0.4 |
| ll-df10 | silence_floor | -24.3 | n/a | n/a | n/a | n/a | n/a |
| std-nodf | keyboard_between_words | -17.1 | n/a | 11.0 | 1.80 | 1.81 | -0.4 |
| std-nodf | keyboard_during_speech | -10.6 | 11.4 | 5.0 | 3.87 | 3.96 | 1.3 |
| std-nodf | mouse_clicks | -15.7 | 16.2 | 7.5 | 2.61 | 2.58 | 0.4 |
| std-nodf | fan_plus_mouse | -8.3 | 1.7 | 1.9 | 6.04 | 6.67 | -0.5 |
| std-nodf | music_bed | -12.8 | 0.9 | 2.6 | 5.05 | 5.57 | -0.1 |
| std-nodf | music_bed_12 | -12.2 | 4.8 | 5.4 | 4.58 | 4.94 | -0.1 |
| std-nodf | second_talker | -16.0 | -0.8 | 4.9 | 3.23 | 3.39 | 0.3 |
| std-nodf | clean_voice | -17.2 | n/a | 12.3 | 1.44 | 1.49 | -0.4 |
| std-nodf | silence_floor | 10.1 | n/a | n/a | n/a | n/a | n/a |
| std-df0 | keyboard_between_words | -14.4 | n/a | 11.0 | 1.80 | 1.82 | -0.5 |
| std-df0 | keyboard_during_speech | -3.7 | 11.9 | 4.6 | 3.94 | 4.00 | 1.3 |
| std-df0 | mouse_clicks | -7.0 | 16.4 | 7.5 | 2.63 | 2.60 | 0.4 |
| std-df0 | fan_plus_mouse | -14.1 | 2.9 | 1.7 | 5.30 | 5.91 | -0.4 |
| std-df0 | music_bed | -24.6 | 1.9 | 2.2 | 5.12 | 5.69 | -0.0 |
| std-df0 | music_bed_12 | -17.0 | 5.4 | 5.0 | 4.70 | 5.09 | -0.1 |
| std-df0 | second_talker | -18.3 | -0.6 | 4.9 | 3.46 | 3.68 | 0.4 |
| std-df0 | clean_voice | 7.1 | n/a | 12.2 | 1.45 | 1.51 | -0.5 |
| std-df0 | silence_floor | -26.9 | n/a | n/a | n/a | n/a | n/a |
| std-df5 | keyboard_between_words | -14.5 | n/a | 11.1 | 1.80 | 1.82 | -0.5 |
| std-df5 | keyboard_during_speech | -3.4 | 13.1 | 3.8 | 4.04 | 3.94 | 1.2 |
| std-df5 | mouse_clicks | -6.5 | 16.7 | 7.2 | 2.67 | 2.64 | 0.3 |
| std-df5 | fan_plus_mouse | -18.0 | 5.5 | 0.3 | 4.90 | 5.14 | -0.1 |
| std-df5 | music_bed | -24.2 | 4.7 | 0.6 | 4.93 | 5.50 | -0.1 |
| std-df5 | music_bed_12 | -20.0 | 7.0 | 4.3 | 4.72 | 5.15 | -0.1 |
| std-df5 | second_talker | -19.3 | 0.1 | 4.4 | 3.67 | 3.86 | 0.5 |
| std-df5 | clean_voice | 11.7 | n/a | 12.2 | 1.47 | 1.52 | -0.5 |
| std-df5 | silence_floor | -27.0 | n/a | n/a | n/a | n/a | n/a |
