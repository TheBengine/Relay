# GTCRN on the recorded voice, against Enhanced, DeepFilterNet 3 LL and Studio, 2026-11-06

Worker X. The question: can GTCRN (0.35 MB) replace DPDFNet (Studio, 25 MB of
models, 60 ms) as the level that removes music? Measured the way the Studio
and DeepFilterNet documents measured (`docs/audio-studio-level-20261030.md`,
`docs/audio-deepfilternet-tuning-20261029.md`): the bench `audio-bench` on the
user's recorded voice, every row, the app's own `MicrophoneProcessor` in
process, all chains in one run.

## Outcome

**No. GTCRN cannot replace DPDFNet as the level that removes music.**

- **Music**: it removes music at 5 dB (−24.6 dB in pauses, target −20) but
  not the realistic 12 dB case (−18.9 dB, target −25). Studio: −39.0 / −38.2 dB.
- **Everything else is worse than Studio, and most of it worse than
  Enhanced**: fan + mouse −7.2 dB in pauses (Enhanced −27.0, Studio −43.4),
  mouse clicks −10.3 (−16.8 / −34.4), keys between words −15.3 (−24.4 / −42.5),
  a second talker −2.7 (+0.2 / −53.5).
- **It cuts the voice off above 8 kHz.** The voice's energy is unchanged up to
  7.5 kHz, 10.6 dB lower at 8–9 kHz and 52 dB lower above 9 kHz. Clean-voice LSD
  2.71 dB (Enhanced 2.12, Studio 1.53).
- **It is cheap and small**: 0.35 MB model, 0.22 ms mean / 0.41 ms p99 per
  10 ms block (Studio 2.72 / 2.96 ms). But it is not short: 44.6 ms measured,
  15 ms less than Studio's 60 ms and 35 ms more than Enhanced's 10 ms.

DeepFilterNet 3 LL, as the settings page wires it (libDF's default runtime
parameters), passes every music row (−53.9 / −46.7 dB) at 20 ms, but fails
the fan (−20.7), mouse (−5.3) and keyboard rows.

## Assumptions (decided, not asked)

- **DeepFilterNet LL runs as wired, not with Worker G's tuned setting.** The
  brief asked for deep-filter threshold 5 and post-filter 0.02 "if that
  setting is reachable". It is not: `relay_audio_models::Stream::new` builds
  every DeepFilterNet stream with `RuntimeParams::default_with_ch(1)`
  (post-filter off, `max_db_df_thresh` 20), and no setting or API changes
  that. Changing the model crate was outside this task. Worker G's tuned
  replay figures are quoted for reference under "DeepFilterNet LL" below.
- **The comparison models run inline on the bench**, as Studio already does.
  In the app, `for_setting(NoiseModel::Gtcrn)` gives
  `SuppressorKind::Comparison`, which hands each block to a worker thread
  and plays that thread's result one block later. On the bench that worker
  would almost never answer in time, because the bench does not wait 10 ms
  between blocks. The new `SuppressorKind::ComparisonInline` (bench and tests
  only) runs the same engine on the calling thread with the same one-block
  hold. Test `inline_matches_the_worker_when_it_keeps_up` shows its output is
  bit-identical to what `Comparison` plays when the worker answers every
  block, for GTCRN and the newer RNNoise. So the bench's CPU column is the
  worker thread's load.
- **The chain around the model is the one the noise setting gives.**
  `Candidate::for_model` uses `AudioSettings { noise_model, ..Default }`:
  input volume 150 %, automatic volume, the voice gate, and no click
  suppression. The app runs click suppression only for Enhanced and Studio,
  and only with a voice probability, which no comparison model gives.
- **The listening set's quieter fan folder is `blaeser-svag-15db`**
  (`fan_plus_mouse_15`), as the brief named it, not Worker G's 10 dB folder.

## Acceptance table (Round 2 plan), recorded voice

| figure | target | GTCRN | DFN 3 LL (as wired) | Studio (dpdfnet8) | Enhanced |
| --- | --- | ---: | ---: | ---: | ---: |
| `music_bed` (5 dB) residual in pauses | ≤ −20 dB | **−24.6 ✓** | −53.9 ✓ | −39.0 ✓ | −0.8 ✗ |
| `music_bed` LSD of the voice | ≤ 6.0 dB | **4.12 ✓** | 2.88 ✓ | 2.38 ✓ | 4.35 ✓ |
| `music_bed` noise leak during speech | ≤ +3 dB | **−1.9 ✓** | −4.4 ✓ | −7.1 ✓ | +0.1 ✓ |
| `music_bed_12` residual in pauses | ≤ −25 dB | **−18.9 ✗** | −46.7 ✓ | −38.2 ✓ | +1.7 ✗ |
| `music_bed_12` LSD | ≤ 5.0 dB | **3.64 ✓** | 2.24 ✓ | 2.10 ✓ | 3.41 ✓ |
| `fan_plus_mouse` residual in pauses | ≤ −25 dB | **−7.2 ✗** | −20.7 ✗ | −43.4 ✓ | −27.0 ✓ |
| `keyboard_during_speech` leak during speech | ≤ −10 dB | **−0.7 ✗** | −2.2 ✗ | −4.8 ✗ | −0.1 ✗ |
| `mouse_clicks` residual in pauses | ≤ −15 dB | **−10.3 ✗** | −5.3 ✗ | −34.4 ✓ | −16.8 ✓ |
| speech attenuation, every row | within ±1.5 dB | **−0.5…−0.1 ✓** | −0.7…−0.1 ✓ | −0.4…−0.1 ✓ | −0.5…+0.3 ✓ |
| clean voice: level within 0.5 dB | | **−0.4 ✓** | −0.4 ✓ | −0.3 ✓ | −0.2 ✓ |
| clean voice: LSD ≤ 1.0 dB | | **2.71 ✗** | 1.42 ✗ | 1.53 ✗ | 2.12 ✗ |

GTCRN passes 6 of 11 and fails three that Studio passes: 12 dB music, the
fan, and mouse clicks. Under the plan's rule ("passes the music rows and
keeps the other rows within 3 dB of shipped") it is not a contender: it
misses 12 dB music, and it is 19.8 dB behind Enhanced on the fan and 9.1 dB
behind on keys between words.

## Delay, CPU and size

| chain | declared delay (ms) | measured delay (ms) | CPU mean (ms / 10 ms block) | p50 | p99 | max | model size |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Enhanced (`rnnoise+enhanced`) | 10.00 | 10.00 | 0.030 | 0.031 | 0.039 | 0.215 | built in |
| GTCRN | 44.67 | 44.62 | 0.221 | 0.336 | 0.410 | 0.825 | 0.35 MB (352 084 bytes) |
| DFN 3 LL | 20.00 | 20.00 | 0.273 | 0.139 | 0.491 | 0.815 | 36.4 MB (36 359 660 bytes, tar.gz) |
| Studio, dpdfnet8 | 60.00 | 60.00 | 2.719 | 2.713 | 2.960 | 4.518 | 14.9 MB (dpdfnet2 for the step down: +10.5 MB) |

- **Delay.** GTCRN's 44.67 ms: 34.67 ms (1664 samples) for the 16 kHz
  model's 512-point STFT, its 48/16 kHz resampling and the 16 ms hop, plus
  the app's 10 ms worker hand-off. The bench measures 44.62 ms. That is over
  the plan's 20 ms budget and its 30 ms "orchestrator decides" line, like
  Studio's 60 ms.
- **CPU**: the whole `MicrophoneProcessor::process` per 10 ms block, release,
  single thread (the model on the calling thread), the noisy input of all
  11 rows, 2 994 blocks each (32 934 per chain). GTCRN's hop is 16 ms, so 5 of every 8 blocks run the model
  and 3 do not. That is why its p50 (0.336) is above its mean (0.221).
  GTCRN's p99 is 7× under the plan's 1.5 ms budget; Studio's is 2× over it.
  Machine: AMD Family 26 Model 68, 16 logical processors, run
  2026-11-06T10:10:13Z. Other sessions may have been using the machine. Studio's p50 is
  within 1.5 % of the Studio document's (2.71 against 2.75 ms); its p99 is
  lower (2.96 against 3.68 ms).
- **Size**: 0.35 MB against 25.4 MB for Studio's two models. The Studio
  document measured `relay.exe` +48.5 MB for Studio, about half of it tract's
  code. GTCRN also runs in tract (`relay-audio-models`), so dropping DPDFNet
  for GTCRN saves the 25.4 MB of models but not tract.

## Bandwidth: what GTCRN keeps of the clean voice

Clean voice row, speech frames, output shifted back by its measured delay and
level-matched to the input; band level of the output against the input:

| band | GTCRN | Enhanced | Studio |
| --- | ---: | ---: | ---: |
| 0–4 kHz | −0.0 | −0.0 | −0.0 |
| 4–6 kHz | +2.4 | +0.1 | +2.4 |
| 6–7 kHz | +2.4 | +0.1 | +2.5 |
| 7–7.5 kHz | +2.4 | +0.3 | +3.1 |
| 7.5–8 kHz | −1.6 | +2.4 | +3.4 |
| 8–9 kHz | **−10.6** | +0.9 | +2.7 |
| 9–12 kHz | **−51.7** | −0.1 | +2.4 |
| 12–24 kHz | **−35.2** | −1.0 | +2.2 |

Share of the voice's energy above 8 kHz: **−30.6 dB before, −45.1 dB after**
GTCRN (8–24 kHz band −14.4 dB after level matching; Enhanced +0.3, DFN LL
+3.2, Studio +2.5). This recording has little energy up there to lose, but
what is lost is the "s", "f" and breath detail that makes a 48 kHz call sound
open; the result is wideband-telephone quality. This loss is built in:
GTCRN works at 16 kHz.

## Can GTCRN replace DPDFNet as the music-removing level?

No. GTCRN takes 5 dB music down by 24.6 dB, which meets the plan's figure
for that row, but at 12 dB, the realistic case, it leaves −18.9 dB against
Studio's −38.2 dB. Unlike Studio, it does not also clean up the rest of the
room. Fan, mouse clicks, keys between words and a second voice all come out
louder than with Enhanced, which is the default today. So a user who picks it
for music gets more of everything else. It also cuts the voice above 8 kHz
on every call, clean ones included, and its 45 ms delay is only 15 ms
shorter than Studio's 60 ms. What it does well is cost: 0.35 MB and
0.41 ms p99 per block against 25 MB and 2.96 ms. If the 25 MB and the CPU
are why DPDFNet might go, a better candidate on these figures is DeepFilterNet 3 LL as wired.
It clears every music row by a wide margin at 20 ms and 0.49 ms p99. Its own
size is 36 MB, and it fails the fan and the clicks. The user's listening test (folder below) should
decide between keeping Studio and having no music-removing level. GTCRN is
not a third option on these numbers.

## DeepFilterNet LL: in process disagrees with Worker E/G's replay

As wired, in process: `music_bed` −53.9 dB / LSD 2.88 (level-matched 2.89) /
leak −4.4. Worker E's replay of upstream libDF at default settings gave
LSD 9.64 (level-matched 5.65 per Worker G), and Worker G's best tuned
setting (threshold 5, post-filter 0.02) gave −47.6 / LSD 4.60 / leak +2.2.
So the app's DeepFilterNet keeps the voice under music much better than the
replays did. Feeding it the 16-bit corpus (`--input-16bit`) changes no
figure by more than 0.1 dB, so the difference is either the app's 150 %
input volume before the model (the replays ran at 100 %) or the vendored
libDF (`vendor/deep-filter`, tract 0.23.9, "mutable feature tensors are
owned Tensor values"). **Not investigated.** It matters for any decision
built on Worker G's tuning. Run `q16` at
`D:\Projects\relay-build-targets\audio-worker-x-runs\q16`.

## Full table, recorded voice

Bench default (float input), release build, 2026-11-06T10:10:13Z, `VOICE_TEST_WAV` =
Worker E's `voice3-prepared.wav` (not committed), all chains in one run. LSD
is the bench's (least-squares gain); the level-matched LSD follows the table.

| chain | item | SNR in (dB) | residual noise in pauses (dB) | noise leak during speech (dB) | speech segSNR (dB) | LSD (dB) | speech attenuation (dB) |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| none | keyboard_between_words | 10.0 | 0.0 | n/a | 35.0 | 0.00 | 0.0 |
| none | keyboard_during_speech | 10.0 | 0.0 | 0.0 | 25.9 | 3.17 | -0.1 |
| none | mouse_clicks | 15.0 | 0.0 | 0.0 | 32.5 | 0.86 | -0.0 |
| none | fan_plus_mouse | 5.0 | 0.0 | 0.0 | 3.0 | 7.82 | -1.2 |
| none | fan_plus_mouse_15 | 15.0 | 0.0 | 0.0 | 11.9 | 4.24 | -0.1 |
| none | fan_plus_mouse_10 | 10.0 | 0.0 | 0.0 | 7.2 | 6.03 | -0.4 |
| none | music_bed | 5.0 | 0.0 | 0.0 | 3.6 | 4.36 | -1.2 |
| none | music_bed_12 | 12.0 | 0.0 | 0.0 | 9.7 | 3.08 | -0.3 |
| none | second_talker | 0.0 | 0.0 | 0.0 | 5.0 | 2.20 | -1.8 |
| none | silence_floor | n/a | 0.0 | n/a | n/a | n/a | n/a |
| none | clean_voice | n/a | -18.4 | n/a | 35.0 | 0.00 | 0.0 |
| rnnoise+enhanced | keyboard_between_words | 10.0 | -24.4 | n/a | 9.0 | 2.16 | -0.3 |
| rnnoise+enhanced | keyboard_during_speech | 10.0 | -16.6 | -0.1 | 8.4 | 3.50 | -0.1 |
| rnnoise+enhanced | mouse_clicks | 15.0 | -16.8 | 7.5 | 8.7 | 2.64 | -0.2 |
| rnnoise+enhanced | fan_plus_mouse | 5.0 | -27.0 | -1.5 | 4.4 | 5.37 | 0.3 |
| rnnoise+enhanced | fan_plus_mouse_15 | 15.0 | -28.5 | 2.9 | 7.1 | 4.10 | 0.1 |
| rnnoise+enhanced | fan_plus_mouse_10 | 10.0 | -29.6 | 0.3 | 6.0 | 4.70 | 0.2 |
| rnnoise+enhanced | music_bed | 5.0 | -0.8 | 0.1 | 3.4 | 4.35 | -0.2 |
| rnnoise+enhanced | music_bed_12 | 12.0 | 1.7 | 2.7 | 5.9 | 3.41 | -0.5 |
| rnnoise+enhanced | second_talker | 0.0 | 0.2 | -0.4 | 4.3 | 3.22 | -0.4 |
| rnnoise+enhanced | silence_floor | n/a | -11.3 | n/a | n/a | n/a | n/a |
| rnnoise+enhanced | clean_voice | n/a | -17.5 | n/a | 9.1 | 2.12 | -0.2 |
| gtcrn | keyboard_between_words | 10.0 | -15.3 | n/a | 11.9 | 2.79 | -0.5 |
| gtcrn | keyboard_during_speech | 10.0 | -29.9 | -0.7 | 11.0 | 3.74 | -0.3 |
| gtcrn | mouse_clicks | 15.0 | -10.3 | 0.3 | 11.5 | 3.14 | -0.4 |
| gtcrn | fan_plus_mouse | 5.0 | -7.2 | -3.0 | 5.5 | 5.13 | -0.1 |
| gtcrn | fan_plus_mouse_15 | 15.0 | -3.4 | 1.4 | 8.9 | 4.07 | -0.4 |
| gtcrn | fan_plus_mouse_10 | 10.0 | -5.5 | -0.9 | 7.3 | 4.54 | -0.3 |
| gtcrn | music_bed | 5.0 | -24.6 | -1.9 | 5.2 | 4.12 | -0.1 |
| gtcrn | music_bed_12 | 12.0 | -18.9 | 0.2 | 7.6 | 3.64 | -0.3 |
| gtcrn | second_talker | 0.0 | -2.7 | -1.4 | 6.9 | 3.34 | -0.4 |
| gtcrn | silence_floor | n/a | -28.0 | n/a | n/a | n/a | n/a |
| gtcrn | clean_voice | n/a | -19.4 | n/a | 12.1 | 2.71 | -0.4 |
| deepfilternet3-ll | keyboard_between_words | 10.0 | -7.6 | n/a | 12.2 | 1.46 | -0.5 |
| deepfilternet3-ll | keyboard_during_speech | 10.0 | -6.7 | -2.2 | 11.1 | 2.36 | -0.4 |
| deepfilternet3-ll | mouse_clicks | 15.0 | -5.3 | 6.1 | 11.8 | 1.80 | -0.6 |
| deepfilternet3-ll | fan_plus_mouse | 5.0 | -20.7 | -5.1 | 6.6 | 4.11 | -0.1 |
| deepfilternet3-ll | fan_plus_mouse_15 | 15.0 | -4.9 | 1.7 | 9.6 | 2.83 | -0.7 |
| deepfilternet3-ll | fan_plus_mouse_10 | 10.0 | -15.4 | -2.8 | 8.3 | 3.38 | -0.3 |
| deepfilternet3-ll | music_bed | 5.0 | -53.9 | -4.4 | 6.6 | 2.88 | -0.1 |
| deepfilternet3-ll | music_bed_12 | 12.0 | -46.7 | -1.2 | 8.8 | 2.24 | -0.4 |
| deepfilternet3-ll | second_talker | 0.0 | -9.7 | -6.0 | 9.1 | 1.83 | -0.2 |
| deepfilternet3-ll | silence_floor | n/a | -23.3 | n/a | n/a | n/a | n/a |
| deepfilternet3-ll | clean_voice | n/a | -17.3 | n/a | 12.4 | 1.42 | -0.4 |
| dpdfnet8 | keyboard_between_words | 10.0 | -42.5 | n/a | 11.8 | 1.47 | -0.4 |
| dpdfnet8 | keyboard_during_speech | 10.0 | -38.6 | -4.8 | 11.2 | 2.11 | -0.4 |
| dpdfnet8 | mouse_clicks | 15.0 | -34.4 | 0.0 | 11.5 | 1.73 | -0.4 |
| dpdfnet8 | fan_plus_mouse | 5.0 | -43.4 | -6.9 | 7.4 | 4.25 | -0.1 |
| dpdfnet8 | fan_plus_mouse_15 | 15.0 | -55.3 | -3.5 | 9.8 | 2.86 | -0.3 |
| dpdfnet8 | fan_plus_mouse_10 | 10.0 | -46.0 | -5.2 | 8.7 | 3.50 | -0.2 |
| dpdfnet8 | music_bed | 5.0 | -39.0 | -7.1 | 7.8 | 2.38 | -0.3 |
| dpdfnet8 | music_bed_12 | 12.0 | -38.2 | -3.7 | 9.2 | 2.10 | -0.4 |
| dpdfnet8 | second_talker | 0.0 | -53.5 | -11.0 | 10.3 | 1.69 | -0.3 |
| dpdfnet8 | silence_floor | n/a | -95.0 | n/a | n/a | n/a | n/a |
| dpdfnet8 | clean_voice | n/a | -14.6 | n/a | 11.8 | 1.53 | -0.3 |

Every Enhanced figure is within 0.1 dB of the Studio document's table. Studio
matches it within 0.5 dB except in the pauses of three rows:
`keyboard_during_speech` (−38.6 here, −32.0 there), `fan_plus_mouse` (−43.4,
−41.7) and `second_talker` (−53.5, −50.3). The Studio document already
found DPDFNet's pause residual sensitive to tiny input changes, and later
commits have changed the chain. Not traced.

LSD with a level-matching gain (Worker G's check for models that rebuild
phase; the bench's least-squares gain collapses when they do). Every value is
within 0.1 dB of the bench's, except `second_talker`, which comes out up to
0.7 dB lower for every chain (Enhanced 2.55 against 3.22). So none of the
models here is penalised by the bench's gain:

| row | Enhanced | GTCRN | DFN 3 LL | Studio |
| --- | ---: | ---: | ---: | ---: |
| keyboard_between_words | 2.12 | 2.78 | 1.53 | 1.53 |
| keyboard_during_speech | 3.50 | 3.70 | 2.39 | 2.13 |
| mouse_clicks | 2.59 | 3.11 | 1.85 | 1.76 |
| fan_plus_mouse | 5.39 | 5.05 | 4.09 | 4.16 |
| fan_plus_mouse_15 | 4.09 | 4.04 | 2.85 | 2.82 |
| fan_plus_mouse_10 | 4.71 | 4.49 | 3.37 | 3.43 |
| music_bed | 4.38 | 4.08 | 2.89 | 2.40 |
| music_bed_12 | 3.40 | 3.61 | 2.27 | 2.12 |
| second_talker | 2.55 | 3.05 | 1.90 | 1.76 |
| clean_voice | 2.08 | 2.70 | 1.48 | 1.58 |

## Listening set

`C:\Important\Projects\2026-10-03\can\work\audio-worker-x-20261106\lyt\`,
Round 2's format: 48 kHz 16-bit mono, from the recorded voice, every output
shifted back by its chain's measured delay and level-matched to the clean
file over its speech frames (every file within 0.01 dB; no file clips).

| folder | row |
| --- | --- |
| `musik-12db` | `music_bed_12` |
| `musik-5db` | `music_bed` |
| `tastatur-under-tale` | `keyboard_during_speech` |
| `blaeser-og-mus` | `fan_plus_mouse` |
| `blaeser-svag-15db` | `fan_plus_mouse_15` |

Files in each: `1-ren-stemme`, `2-ind-i-mikrofonen`, `3-relay-i-dag`
(Enhanced), `4-gtcrn`, `5-deepfilternet-ll` (as wired), `6-studio-dpdfnet8`.

## How to rerun

- Chains `gtcrn` and `deepfilternet3-ll` (`noise_suppressor::bench`, via
  `Candidate::for_model`), next to `rnnoise+enhanced` and `dpdfnet8`. Since
  the models left the release (below), build the bench with
  `--features audio-bench,model-lab`, then run:
  `audio-bench --write-wav --chain none --chain rnnoise+enhanced --chain gtcrn --chain deepfilternet3-ll --chain dpdfnet8`
  with `VOICE_TEST_WAV` set.
- Scripts (outside the repository):
  `work\audio-worker-x-20261106\run\run.sh` (the bench run),
  `analyse.py` (listening set, bandwidth, level-matched LSD), `bands.py`
  (the band table). Run folder:
  `D:\Projects\relay-build-targets\audio-worker-x-runs\voice` (`bench.md`,
  `analyse.md`, `audio-bench\*.json`, `audio-bench\wav\`).

## Tests and checks

- New tests: `audio_model_lab::inline_matches_the_worker_when_it_keeps_up`
  (inline output bit-identical to `Comparison`'s for GTCRN and the newer
  RNNoise, same declared delay) and
  `noise_suppressor::comparison_bench_chains_run_the_settings_model_at_the_apps_delay`
  (the `gtcrn` and `deepfilternet3-ll` chains run the model `for_setting`
  picks, declare the delay of the app's `Comparison` at the default packet
  size, and their output lines up there within 0.5 ms).
- `cargo test -p relay-client --features audio-bench -- --test-threads 8
  audio_model_lab noise_suppressor audio_bench`: 24 passed, 0 failed (both
  binaries).
- `cargo test -p relay-client -- --test-threads 8`: 1748 passed, 6 failed,
  129 ignored. The 6 are account and sign-in vault tests that need the
  isolated test profiles (`RELAY_PROFILE_DIR`, `RELAY_TEST_WORK_ROOT`,
  "the sign-in vault test requires its isolated profile"), which this run did
  not set. None of them touch audio.
- `cargo clippy -p relay-client --features audio-bench --all-targets --no-deps`:
  no diagnostic in the changed files. The test targets stop on a pre-existing
  deny-by-default `never_loop` in `local_voice_gateway.rs:945`. With that one
  lint allowed, the rest is the existing warnings, none in this change.
- `cargo check -p relay-client --bin relay` (no bench feature): clean.
  `Cargo.lock` unchanged.

## Removing the comparison-only models from the release

The orchestrator's decision after this evaluation: Studio stays on DPDFNet 8,
with DPDFNet 2 as its overrun fallback. The shipped build stops carrying
DeepFilterNet 3 (standard and low latency) and GTCRN.

**`relay.exe`, release build (`cargo build -p relay-client --release --bin relay`, default features as
`scripts/build.ps1` builds them):**

| | bytes | MB |
| --- | ---: | ---: |
| before (73eb28b) | 203 148 288 | 203.1 |
| after | 154 458 624 | 154.5 |
| **saved** | **48 689 664** | **48.7** |

44 694 880 bytes of that are the three embedded models:
- DeepFilterNet 3 LL: 36 359 660.
- DeepFilterNet 3: 7 983 136.
- GTCRN: 352 084.

The other 4.0 MB is the DeepFilterNet runtime (`relay-deepfilter`) and the 15
crates only it pulls in (rubato, rust-ini, tract-pulse, tract-transformers
and others). tract itself stays, because Studio uses it. A 256-byte sample from the middle of each model file
is found in the old binary and not in the new one, for all three. Both
DPDFNet models are in both binaries.

**How:**

- **`relay-audio-models`**: a non-default feature `model-lab` now holds
  `relay-deepfilter`, tract, realfft and ndarray, the `gtcrn` and `resample`
  modules and every `include_bytes!` of the DeepFilterNet and GTCRN models.
  The crate compiles and tests either way: 3 tests without the feature, 4
  with it.
  - `Model` keeps all four variants.
  - The new `Model::is_built_in()` is false for the three removed models
    without the feature.
  - `Stream::new` refuses them with "This build does not include this
    comparison model".
  - Test `only_the_lab_feature_builds_deepfilternet_and_gtcrn`.
  - The newer native RNNoise stays built in.
- **`relay-client`**: the feature `model-lab = ["relay-audio-models/model-lab"]`,
  off by default and not passed by `scripts/build.ps1`. The model files
  stay in `vendor/models/` for `model-lab` builds and the crate's reference
  tests; nothing outside that feature reads them.
- **"Compare voice models"** lists only `NoiseModel::is_built_in()` models.
- **A saved choice of a removed model** still parses (the enum keeps its
  values). `AudioSettings::normalized()` turns it into Enhanced and keeps
  the old choice in `removed_noise_model` (never saved).
  - The app now normalizes the loaded draft too, so the page does not show
    "unsaved changes".
  - Under the two choices the page shows one line: "GTCRN · 16 kHz is no
    longer included in Relay, so your saved choice now uses Enhanced."
  - The next save writes `"noise_model":"enhanced"`.
  - Test `a_saved_model_this_build_lacks_loads_as_enhanced_with_a_note`.
- **Notices**: `scripts/generate-notices.py` now writes the embedded-model
  blocks itself. Before, they had been pasted into the file by hand.
  - Always: Silero VAD, native RNNoise and DPDFNet.
  - Only with the new `--model-lab` flag: DeepFilterNet (MIT and Apache-2.0)
    and GTCRN. That flag also adds `relay-client/model-lab` to the
    dependency graph.
  - The script also reads vendored path dependencies from their source
    (`relay-deepfilter`). Before, it could not find them.
  - `THIRD_PARTY_NOTICES.txt` is regenerated for the default build: 608
    packages and no unresolved licence. Compared with the committed file,
    it drops the 15 DeepFilterNet-only crates and the DeepFilterNet and
    GTCRN blocks. A `--model-lab` run lists 624 packages, including
    `relay-deepfilter` 0.5.7 and both model blocks.
- `docs/audio-model-comparison-20261102.md` marks the three as
  `model-lab` only.

**Checks:**
- `cargo clippy -p relay-client -p relay-audio-models --all-targets --no-deps`
  with no features, with `audio-bench`, and with `audio-bench,model-lab`: no
  diagnostic in the changed files (with `-A clippy::never_loop`, for the
  existing `local_voice_gateway.rs` error).
- `cargo test -p relay-audio-models`: 3 passed. With `--features model-lab`:
  4 passed.
- `cargo test -p relay-client --features audio-bench,model-lab --
  audio_model_lab noise_suppressor audio_settings audio_bench`: 47 passed.
- `cargo test -p relay-client` (default): 1747 passed and 7 failed in the
  first run.
  - Six are the account and sign-in-vault tests that need the isolated test
    profiles, as above.
  - The seventh, `media::comparison_handoff_tracks_real_capture_callbacks_through_200ms`,
    used DeepFilterNet 3 as its comparison model, which a default build now
    loads as Enhanced. It uses the newer RNNoise now. After that fix, the
    media, settings, app, suppressor and model-lab tests passed: 151 passed,
    0 failed.
- `Cargo.lock` unchanged.

## What is not done

- **No listening test.** The figures above are the bench's; the user decides.
- **DeepFilterNet LL with Worker G's tuned parameters** was not measured in
  process (not reachable; see Assumptions). The gap between in-process and
  replay DeepFilterNet figures is not explained.
- **No real-time run of the GTCRN worker hand-off** (`Comparison` on its
  thread, paced at 10 ms). The inline runner's output equals the worker's
  only when the worker answers every block in time. With a p99 of 0.41 ms it should,
  but that was not measured in a paced run.
