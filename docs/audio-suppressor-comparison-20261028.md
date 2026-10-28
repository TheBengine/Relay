# Noise suppressor candidates, 2026-10-28

Worker B of `docs/superpowers/plans/2026-10-28-audio-quality-orchestration.md`
(Task B). Every candidate ran through Worker A's bench
(`docs/audio-bench-baseline-20261028.md`) on the same synthetic corpus, inside
the app's own microphone processing (gain, suppressor, click suppression,
automatic volume, voice gate), release build, one thread.

## Outcome

**The shipped default does not change.** Standard is RNNoise; Enhanced (the
default) is RNNoise plus click suppression at every strength, exactly as
before. Its numbers below match the baseline in every column except CPU time.
No candidate that fits the delay and CPU budgets beats it without losing
somewhere it is good today:

- **WebRTC noise suppression (very high) in front of RNNoise** is the only
  candidate that meets the fan target (-27.1 dB; -27.3 dB with click
  suppression at High). It also fixes the quiet room that automatic volume
  makes louder (`silence_floor` -26.5 dB instead of +17.1 dB). It costs 7 ms
  more (17.0 ms in all) and changes the voice more: spectral distortion 4.6 dB
  instead of 1.8 dB on `keyboard_between_words`, and 4.3 dB instead of 2.2 dB
  counting only the voice band below 8 kHz. It also removes **fewer** clicks.
  The existing test `enhanced_noise_suppression_removes_the_clicks_rnnoise_lets_through`
  runs without the voice gate. With this chain behind High, clicks between
  words came out 7.4 dB below Standard (today's Normal: 9.8 dB; the test
  requires 12 dB for High) and speech 3.8 dB quieter. WebRTC's suppressor
  smears the click onsets before the click remover sees them, the same finding
  as the earlier note at the top of `click_suppression.rs`. So it is not wired
  to any setting. It is one line away (`noise_suppressor::for_setting`) if a
  listening test with a real fan or air conditioner says the trade is worth
  it, and a test pins its fan figure.
- **DeepFilterNet 3** is the only candidate that removes music (-16.3 dB with
  the standard model, -61.4 dB with the low-latency model). On this corpus it
  also damages the synthetic voice when music plays (spectral distortion
  8.9-9.1 dB, speech 3.8-6.8 dB quieter). It lets keyboard and mouse clicks
  through, so automatic volume raises them (+3.9 to +14.6 dB in pauses), and
  it removes less fan than RNNoise. The standard model adds 30 ms (over
  budget). The low-latency model adds 10 ms, the same as RNNoise, at 0.55 ms
  CPU per block. It is not linked into the client (see "DeepFilterNet" below).
- **No candidate reaches the music target (-20 dB) or the keyboard-on-words
  targets.** Music needs a model trained with music as noise. DeepFilterNet is
  the only one measured, and it should be judged again on a recorded voice
  before anything is decided: the synthetic talker is known to confuse neural
  models (see the corpus README).

## Acceptance figures

The best value any candidate reached for each of the plan's figures, with the
shipped chain for comparison.

| figure | target | shipped (`rnnoise+enhanced`) | best candidate | pass? |
| --- | --- | ---: | --- | --- |
| `music_bed` residual in pauses | <= -20 dB | +0.5 dB | -61.4 dB `deepfilternet-ll`; best without DeepFilterNet -5.6 dB (`webrtc-ns-veryhigh-then-rnnoise+enhanced-high`) | fail for every linkable candidate; DeepFilterNet-LL passes this row only, with the voice damaged (LSD 9.12 dB) |
| `keyboard_during_speech` LSD | <= 1.0 dB | 2.78 dB | 2.78 dB (shipped); the unprocessed input is 2.94 dB | fail for all |
| `keyboard_during_speech` clicks (noise leak during speech) | <= -25 dB | -1.0 dB | -1.0 dB (shipped); next best -0.6 dB (`webrtc-ns-then-rnnoise+enhanced`) | fail for all |
| `fan_plus_mouse` residual in pauses | <= -25 dB | -17.9 dB | -27.3 dB `webrtc-ns-veryhigh-then-rnnoise+enhanced-high`; -27.1 dB with Normal clicks | pass (only with very high WebRTC NS before RNNoise) |
| added delay | <= 20 ms | 10.0 ms | 17.0 ms for the WebRTC NS two-stage chains, 10.0 ms DeepFilterNet-LL, 30.0 ms DeepFilterNet standard | pass for all except DeepFilterNet standard |
| CPU per 10 ms block | <= 1.5 ms | 0.036 ms | 0.055 ms for the WebRTC NS two-stage chains; DeepFilterNet model alone 0.24 ms (standard) and 0.55 ms (LL), p99 0.77 and 1.30 ms | pass for all (DeepFilterNet-LL's p99 is close) |

## How to reproduce

From the MSVC environment with `LK_CUSTOM_WEBRTC` set (as for any client
build):

```
cargo build -p relay-client --release --features audio-bench --bin audio-bench -j 4
target\release\audio-bench.exe --write-wav
```

The WebRTC NS and two-stage chains are in `noise_suppressor::bench::chains()`
and run by default. The DeepFilterNet rows need an outside tool (below) and
two environment variables. Without them those rows are left out.

## Full table

Run at 2026-10-28T21:10:37Z on this branch. Three other workers were building
on the same machine, so CPU times are higher than the baseline's (0.036
instead of 0.032 ms for the shipped chain). Every metric column of `none`,
`rnnoise` and `rnnoise+enhanced` matches the baseline exactly.

The DeepFilterNet rows are replayed (see below). Their CPU column covers only
the rest of the chain; the model's own time is in the table after this one.

| chain | item | SNR in (dB) | residual noise in pauses (dB) | noise leak during speech (dB) | speech segSNR (dB) | LSD (dB) | speech attenuation (dB) | CPU (ms / 10 ms block) |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| none | keyboard_between_words | 10.0 | 0.0 | n/a | 35.0 | 0.00 | 0.0 | 0.000 |
| none | keyboard_during_speech | 10.0 | 0.0 | 0.0 | 28.4 | 2.94 | -0.1 | 0.000 |
| none | mouse_clicks | 15.0 | 0.0 | 0.0 | 32.7 | 0.90 | -0.0 | 0.000 |
| none | fan_plus_mouse | 5.0 | 0.0 | 0.0 | 2.8 | 9.90 | -1.2 | 0.000 |
| none | music_bed | 5.0 | 0.0 | 0.0 | 2.8 | 5.14 | -1.3 | 0.000 |
| none | second_talker | 0.0 | 0.0 | 0.0 | 5.8 | 1.51 | -1.7 | 0.000 |
| none | silence_floor | n/a | 0.0 | n/a | n/a | n/a | n/a | 0.000 |
| rnnoise | keyboard_between_words | 10.0 | -3.5 | n/a | 9.5 | 1.46 | -1.1 | 0.029 |
| rnnoise | keyboard_during_speech | 10.0 | -1.7 | 1.2 | 9.2 | 3.06 | -1.1 | 0.027 |
| rnnoise | mouse_clicks | 15.0 | -0.7 | 3.5 | 9.4 | 2.10 | -1.0 | 0.027 |
| rnnoise | fan_plus_mouse | 5.0 | -17.2 | -1.4 | 4.7 | 5.07 | -1.3 | 0.034 |
| rnnoise | music_bed | 5.0 | 0.7 | 2.0 | 1.8 | 5.23 | -1.6 | 0.029 |
| rnnoise | second_talker | 0.0 | -0.1 | 0.4 | 4.8 | 2.01 | -1.4 | 0.033 |
| rnnoise | silence_floor | n/a | 16.9 | n/a | n/a | n/a | n/a | 0.029 |
| rnnoise+enhanced | keyboard_between_words | 10.0 | -10.7 | n/a | 9.3 | 1.80 | -0.9 | 0.035 |
| rnnoise+enhanced | keyboard_during_speech | 10.0 | -7.0 | -1.0 | 9.2 | 2.78 | -0.9 | 0.034 |
| rnnoise+enhanced | mouse_clicks | 15.0 | -9.5 | 5.1 | 9.1 | 2.36 | -0.8 | 0.033 |
| rnnoise+enhanced | fan_plus_mouse | 5.0 | -17.9 | -0.6 | 4.8 | 4.96 | -1.0 | 0.035 |
| rnnoise+enhanced | music_bed | 5.0 | 0.5 | 2.1 | 1.8 | 5.25 | -1.6 | 0.039 |
| rnnoise+enhanced | second_talker | 0.0 | -0.1 | 0.4 | 4.6 | 2.25 | -1.3 | 0.037 |
| rnnoise+enhanced | silence_floor | n/a | 17.1 | n/a | n/a | n/a | n/a | 0.037 |
| rnnoise+enhanced-high | keyboard_between_words | 10.0 | -14.6 | n/a | 8.2 | 2.22 | -0.6 | 0.033 |
| rnnoise+enhanced-high | keyboard_during_speech | 10.0 | -11.2 | 3.0 | 8.1 | 2.87 | -0.5 | 0.032 |
| rnnoise+enhanced-high | mouse_clicks | 15.0 | -13.4 | 7.6 | 8.2 | 2.67 | -0.5 | 0.036 |
| rnnoise+enhanced-high | fan_plus_mouse | 5.0 | -17.9 | 0.2 | 4.6 | 5.07 | -0.8 | 0.034 |
| rnnoise+enhanced-high | music_bed | 5.0 | -0.4 | 2.4 | 1.8 | 5.30 | -1.6 | 0.035 |
| rnnoise+enhanced-high | second_talker | 0.0 | 0.1 | 0.6 | 4.2 | 2.48 | -1.0 | 0.038 |
| rnnoise+enhanced-high | silence_floor | n/a | 17.3 | n/a | n/a | n/a | n/a | 0.035 |
| webrtc-ns | keyboard_between_words | 10.0 | 3.9 | n/a | -0.2 | 3.54 | -1.1 | 0.020 |
| webrtc-ns | keyboard_during_speech | 10.0 | 2.6 | 3.3 | -0.2 | 4.24 | -1.2 | 0.018 |
| webrtc-ns | mouse_clicks | 15.0 | -1.4 | 10.4 | -0.2 | 3.94 | -0.9 | 0.018 |
| webrtc-ns | fan_plus_mouse | 5.0 | -2.9 | -0.1 | -0.2 | 5.89 | -1.6 | 0.017 |
| webrtc-ns | music_bed | 5.0 | 0.6 | 1.8 | -0.4 | 4.89 | -1.6 | 0.018 |
| webrtc-ns | second_talker | 0.0 | -0.2 | 0.7 | -0.5 | 3.91 | -1.3 | 0.019 |
| webrtc-ns | silence_floor | n/a | 13.6 | n/a | n/a | n/a | n/a | 0.020 |
| webrtc-ns-veryhigh | keyboard_between_words | 10.0 | 3.9 | n/a | -0.2 | 3.81 | -0.9 | 0.021 |
| webrtc-ns-veryhigh | keyboard_during_speech | 10.0 | 2.5 | 4.1 | -0.2 | 4.30 | -1.1 | 0.019 |
| webrtc-ns-veryhigh | mouse_clicks | 15.0 | -2.5 | 13.7 | -0.2 | 4.23 | -0.7 | 0.020 |
| webrtc-ns-veryhigh | fan_plus_mouse | 5.0 | -9.1 | 0.4 | 0.1 | 5.40 | -0.8 | 0.020 |
| webrtc-ns-veryhigh | music_bed | 5.0 | -0.1 | 2.3 | -0.3 | 5.18 | -1.5 | 0.018 |
| webrtc-ns-veryhigh | second_talker | 0.0 | -0.2 | 1.0 | -0.5 | 4.06 | -1.2 | 0.020 |
| webrtc-ns-veryhigh | silence_floor | n/a | 3.7 | n/a | n/a | n/a | n/a | 0.019 |
| rnnoise-then-webrtc-ns+enhanced | keyboard_between_words | 10.0 | -13.1 | n/a | -0.3 | 4.27 | -0.8 | 0.055 |
| rnnoise-then-webrtc-ns+enhanced | keyboard_during_speech | 10.0 | -9.9 | 1.0 | -0.3 | 4.31 | -0.9 | 0.056 |
| rnnoise-then-webrtc-ns+enhanced | mouse_clicks | 15.0 | -15.0 | 10.9 | -0.2 | 4.59 | -0.6 | 0.057 |
| rnnoise-then-webrtc-ns+enhanced | fan_plus_mouse | 5.0 | -17.8 | -1.7 | 0.0 | 5.23 | -0.9 | 0.057 |
| rnnoise-then-webrtc-ns+enhanced | music_bed | 5.0 | -1.3 | 2.5 | -0.3 | 5.55 | -1.3 | 0.060 |
| rnnoise-then-webrtc-ns+enhanced | second_talker | 0.0 | -0.2 | 0.7 | -0.4 | 4.64 | -1.1 | 0.052 |
| rnnoise-then-webrtc-ns+enhanced | silence_floor | n/a | 9.9 | n/a | n/a | n/a | n/a | 0.059 |
| rnnoise-then-webrtc-ns-veryhigh+enhanced | keyboard_between_words | 10.0 | -13.9 | n/a | -0.3 | 4.46 | -0.6 | 0.056 |
| rnnoise-then-webrtc-ns-veryhigh+enhanced | keyboard_during_speech | 10.0 | -10.8 | 2.4 | -0.3 | 4.37 | -0.9 | 0.054 |
| rnnoise-then-webrtc-ns-veryhigh+enhanced | mouse_clicks | 15.0 | -16.6 | 14.0 | -0.2 | 4.83 | -0.4 | 0.053 |
| rnnoise-then-webrtc-ns-veryhigh+enhanced | fan_plus_mouse | 5.0 | -17.9 | -1.9 | 0.0 | 5.31 | -0.8 | 0.055 |
| rnnoise-then-webrtc-ns-veryhigh+enhanced | music_bed | 5.0 | -2.0 | 2.9 | -0.2 | 5.85 | -1.1 | 0.056 |
| rnnoise-then-webrtc-ns-veryhigh+enhanced | second_talker | 0.0 | -0.2 | 0.9 | -0.4 | 4.78 | -1.0 | 0.055 |
| rnnoise-then-webrtc-ns-veryhigh+enhanced | silence_floor | n/a | 8.1 | n/a | n/a | n/a | n/a | 0.054 |
| webrtc-ns-then-rnnoise+enhanced | keyboard_between_words | 10.0 | -10.2 | n/a | -0.3 | 4.40 | -0.8 | 0.056 |
| webrtc-ns-then-rnnoise+enhanced | keyboard_during_speech | 10.0 | -8.4 | -0.6 | -0.2 | 4.37 | -0.9 | 0.054 |
| webrtc-ns-then-rnnoise+enhanced | mouse_clicks | 15.0 | -9.5 | 11.1 | -0.2 | 4.65 | -0.7 | 0.056 |
| webrtc-ns-then-rnnoise+enhanced | fan_plus_mouse | 5.0 | -22.7 | -0.8 | 0.1 | 5.57 | 0.0 | 0.056 |
| webrtc-ns-then-rnnoise+enhanced | music_bed | 5.0 | -3.8 | 1.3 | -0.1 | 5.90 | -0.6 | 0.055 |
| webrtc-ns-then-rnnoise+enhanced | second_talker | 0.0 | -0.2 | 0.7 | -0.4 | 4.63 | -1.2 | 0.056 |
| webrtc-ns-then-rnnoise+enhanced | silence_floor | n/a | -23.0 | n/a | n/a | n/a | n/a | 0.056 |
| webrtc-ns-high-then-rnnoise+enhanced | keyboard_between_words | 10.0 | -13.6 | n/a | -0.3 | 4.50 | -0.7 | 0.052 |
| webrtc-ns-high-then-rnnoise+enhanced | keyboard_during_speech | 10.0 | -9.3 | 0.8 | -0.2 | 4.39 | -0.9 | 0.054 |
| webrtc-ns-high-then-rnnoise+enhanced | mouse_clicks | 15.0 | -9.8 | 13.2 | -0.2 | 4.82 | -0.5 | 0.055 |
| webrtc-ns-high-then-rnnoise+enhanced | fan_plus_mouse | 5.0 | -23.7 | 0.2 | 0.2 | 5.93 | 0.4 | 0.054 |
| webrtc-ns-high-then-rnnoise+enhanced | music_bed | 5.0 | -4.3 | 1.7 | -0.1 | 6.04 | -0.6 | 0.054 |
| webrtc-ns-high-then-rnnoise+enhanced | second_talker | 0.0 | -0.3 | 0.9 | -0.4 | 4.72 | -1.1 | 0.052 |
| webrtc-ns-high-then-rnnoise+enhanced | silence_floor | n/a | -18.7 | n/a | n/a | n/a | n/a | 0.052 |
| webrtc-ns-veryhigh-then-rnnoise+enhanced | keyboard_between_words | 10.0 | -13.0 | n/a | -0.3 | 4.59 | -0.6 | 0.054 |
| webrtc-ns-veryhigh-then-rnnoise+enhanced | keyboard_during_speech | 10.0 | -9.5 | 1.3 | -0.3 | 4.41 | -0.9 | 0.052 |
| webrtc-ns-veryhigh-then-rnnoise+enhanced | mouse_clicks | 15.0 | -10.0 | 14.1 | -0.2 | 4.92 | -0.4 | 0.055 |
| webrtc-ns-veryhigh-then-rnnoise+enhanced | fan_plus_mouse | 5.0 | -27.1 | 1.5 | 0.3 | 6.19 | 0.8 | 0.060 |
| webrtc-ns-veryhigh-then-rnnoise+enhanced | music_bed | 5.0 | -4.2 | 2.1 | -0.1 | 6.15 | -0.5 | 0.055 |
| webrtc-ns-veryhigh-then-rnnoise+enhanced | second_talker | 0.0 | -0.3 | 1.0 | -0.4 | 4.76 | -1.1 | 0.058 |
| webrtc-ns-veryhigh-then-rnnoise+enhanced | silence_floor | n/a | -26.5 | n/a | n/a | n/a | n/a | 0.053 |
| webrtc-ns-veryhigh-then-rnnoise+enhanced-high | keyboard_between_words | 10.0 | -17.6 | n/a | -0.3 | 4.65 | -0.3 | 0.055 |
| webrtc-ns-veryhigh-then-rnnoise+enhanced-high | keyboard_during_speech | 10.0 | -13.7 | 4.0 | -0.3 | 4.45 | -0.5 | 0.056 |
| webrtc-ns-veryhigh-then-rnnoise+enhanced-high | mouse_clicks | 15.0 | -13.3 | 12.8 | -0.2 | 5.03 | -0.1 | 0.052 |
| webrtc-ns-veryhigh-then-rnnoise+enhanced-high | fan_plus_mouse | 5.0 | -27.3 | 1.1 | 0.2 | 6.24 | 0.8 | 0.058 |
| webrtc-ns-veryhigh-then-rnnoise+enhanced-high | music_bed | 5.0 | -5.6 | 1.6 | -0.1 | 6.18 | -0.5 | 0.057 |
| webrtc-ns-veryhigh-then-rnnoise+enhanced-high | second_talker | 0.0 | -0.2 | 1.1 | -0.4 | 4.90 | -0.8 | 0.052 |
| webrtc-ns-veryhigh-then-rnnoise+enhanced-high | silence_floor | n/a | -45.1 | n/a | n/a | n/a | n/a | 0.053 |
| deepfilternet (replayed) | keyboard_between_words | 10.0 | 4.9 | n/a | 11.0 | 2.71 | -0.9 | 0.004 |
| deepfilternet (replayed) | keyboard_during_speech | 10.0 | 5.2 | 16.4 | 7.0 | 3.07 | -0.4 | 0.003 |
| deepfilternet (replayed) | mouse_clicks | 15.0 | 10.8 | 22.4 | 8.6 | 2.57 | -0.9 | 0.004 |
| deepfilternet (replayed) | fan_plus_mouse | 5.0 | -14.2 | 8.7 | -0.0 | 6.64 | -0.7 | 0.003 |
| deepfilternet (replayed) | music_bed | 5.0 | -16.3 | 5.7 | -0.0 | 8.89 | 3.8 | 0.003 |
| deepfilternet (replayed) | second_talker | 0.0 | -6.7 | 4.2 | 0.1 | 8.01 | 3.1 | 0.003 |
| deepfilternet (replayed) | silence_floor | n/a | -35.4 | n/a | n/a | n/a | n/a | 0.003 |
| deepfilternet-ll (replayed) | keyboard_between_words | 10.0 | 3.9 | n/a | 8.5 | 4.73 | -0.2 | 0.004 |
| deepfilternet-ll (replayed) | keyboard_during_speech | 10.0 | 10.3 | 16.5 | 5.4 | 3.57 | -0.6 | 0.004 |
| deepfilternet-ll (replayed) | mouse_clicks | 15.0 | 14.6 | 22.6 | 7.2 | 4.56 | -0.2 | 0.003 |
| deepfilternet-ll (replayed) | fan_plus_mouse | 5.0 | -8.5 | 8.2 | -0.0 | 6.71 | -0.6 | 0.003 |
| deepfilternet-ll (replayed) | music_bed | 5.0 | -61.4 | 5.2 | 0.0 | 9.12 | 6.8 | 0.003 |
| deepfilternet-ll (replayed) | second_talker | 0.0 | -7.5 | 4.3 | 0.1 | 7.99 | 2.8 | 0.003 |
| deepfilternet-ll (replayed) | silence_floor | n/a | -16.0 | n/a | n/a | n/a | n/a | 0.003 |

| chain | declared delay (ms) | measured delay (ms) | CPU mean (ms / 10 ms block) | CPU p99 | CPU max |
| --- | ---: | ---: | ---: | ---: | ---: |
| none | 0.00 | 0.00 | 0.000 | 0.000 | 0.000 |
| rnnoise | 10.00 | 9.94 | 0.030 | 0.073 | 0.394 |
| rnnoise+enhanced | 10.00 | 9.96 | 0.036 | 0.102 | 0.407 |
| rnnoise+enhanced-high | 10.00 | 9.96 | 0.035 | 0.088 | 0.444 |
| webrtc-ns | 6.98 | 6.38 | 0.019 | 0.045 | 0.364 |
| webrtc-ns-veryhigh | 6.98 | 6.38 | 0.020 | 0.027 | 0.159 |
| rnnoise-then-webrtc-ns+enhanced | 16.98 | 16.33 | 0.057 | 0.076 | 0.440 |
| rnnoise-then-webrtc-ns-veryhigh+enhanced | 16.98 | 16.33 | 0.055 | 0.076 | 0.406 |
| webrtc-ns-then-rnnoise+enhanced | 16.98 | 16.33 | 0.056 | 0.079 | 0.395 |
| webrtc-ns-high-then-rnnoise+enhanced | 16.98 | 16.33 | 0.053 | 0.075 | 0.195 |
| webrtc-ns-veryhigh-then-rnnoise+enhanced | 16.98 | 16.33 | 0.055 | 0.084 | 0.425 |
| webrtc-ns-veryhigh-then-rnnoise+enhanced-high | 16.98 | 16.31 | 0.055 | 0.076 | 0.736 |
| deepfilternet (replayed) | 30.00 | 30.00 | 0.003 | 0.006 | 0.027 |
| deepfilternet-ll (replayed) | 10.00 | 9.98 | 0.003 | 0.006 | 0.023 |

DeepFilterNet's own processing time, measured by the outside tool block by
block on the same corpus (14 signals, 14 000 blocks), release build, one
thread:

| model | delay (ms) | CPU mean (ms / 10 ms block) | CPU p99 | CPU max |
| --- | ---: | ---: | ---: | ---: |
| DeepFilterNet 3 (standard, 2 blocks of lookahead) | 30.0 | 0.237 | 0.768 | 4.281 |
| DeepFilterNet 3 low latency (no lookahead) | 10.0 | 0.549 | 1.298 | 2.615 |

## The candidates

Chains are named `<suppressor>[+enhanced[-high]]`. `+enhanced` adds Relay's
click suppression and voice isolation at Normal strength, `-high` at High.
`a-then-b` runs `a` first.

1. **WebRTC noise suppression** (`webrtc-ns`, `webrtc-ns-veryhigh`), from the
   WebRTC audio processing module Relay already links for echo cancellation,
   with its high-pass filter and nothing else. No new crates. Its strength
   (Low 6 dB, Moderate 12 dB, High 18 dB, Very high 21 dB) was not reachable
   through the binding, so `vendor/webrtc-sys-0.3.47` gains
   `create_apm_with_ns_level` (`apm.h`, `apm.cpp`, `apm.rs`). The existing
   `create_apm` is unchanged. Delay: 335 samples (7.0 ms), the 48 kHz
   three-band split plus the suppressor's overlap. Alone it removes little:
   it is built for steady noise, and the voice gate then opens on what is
   left. Turning its high-pass filter off changed no number.
2. **RNNoise with another model**: not measured. `nnnoiseless` 0.5.2 loads
   only models with the original RNNoise layout (one dense input layer, three
   GRUs for voice activity, noise and denoising, two dense outputs; see
   `RnnModel` in its `rnn.rs`). The 2024 RNNoise 0.2 models use a different,
   larger network, so they do not load there. That is from the RNNoise 0.2
   release as I know it, not checked against its source here. Older community
   models in the original layout exist, but only as files outside crates.io,
   which this task was not cleared to download. RNNoise's delay stays 10 ms
   whatever the model.
3. **DeepFilterNet 3** (`deepfilternet`, `deepfilternet-ll`): standard and
   low-latency models, 48 kHz native.
4. **Two-stage chains**: WebRTC NS before or after RNNoise, at Moderate, High
   and Very high, with Relay's click suppression after both.

Also measured, since it needs no new code: today's RNNoise with click
suppression at High (`rnnoise+enhanced-high`).

### What each does to `music_bed`

- RNNoise, with or without click suppression: nothing (+0.5 dB, slightly
  louder than it came in). The music is harmonic, so RNNoise calls it voice,
  the voice gate opens, and automatic volume raises it.
- Click suppression at High: -0.4 dB. Its voice isolation follows RNNoise's
  voice probability, which stays high on music.
- WebRTC NS alone: +0.6 dB (Moderate), -0.1 dB (Very high).
- Two-stage: -1.3 to -5.6 dB. Best: very high WebRTC NS first, then RNNoise,
  click suppression at High.
- DeepFilterNet: -16.3 dB (standard), -61.4 dB (low latency; the gate stays
  closed in pauses). Both damage the synthetic voice under music: LSD 8.9 and
  9.1 dB against 5.3 dB, and speech 3.8 and 6.8 dB quieter.

### What each does to `keyboard_during_speech`

No candidate softens clicks on top of words. The shipped chain leaks them at
-1.0 dB (almost all of them), and every other chain falls between -0.6 and
+16.5 dB. WebRTC NS reacts to each click by changing its gains, so the voice
around the click changes too, and the leak column counts that. DeepFilterNet
passes the clicks (+16.4 and +16.5 dB). In the pauses of that row the best was
`webrtc-ns-veryhigh-then-rnnoise+enhanced-high` (-13.7 dB against -7.0 dB),
but the Outcome explains why that chain removes fewer clicks once the voice
gate is not hiding them. LSD stays between 2.8 and 4.5 dB for every chain. The
unprocessed noisy input is already at 2.94 dB, so the 1.0 dB target needs a
suppressor that removes clicks under speech, and none of these does.

## DeepFilterNet: why it is measured outside the client

- The crates.io `deep_filter` (0.2.5) is only DeepFilterNet's dataset
  library. The real-time library with the models lives on GitHub (`libDF`,
  0.5.7-pre). It was fetched once as a git dependency (rev `d375b2d8`) into
  the cargo cache. Nothing was vendored.
- Unpinned, it cannot join the client's dependency tree. `tract-linalg`
  0.21.19 requires `time` below 0.3.42, while `openidconnect` (through
  `serde_with`) requires 0.3.47 or later. `tract` 0.21.19 also moved to
  `ndarray` 0.16, which libDF does not build against.
- Pinning `tract-core`, `tract-onnx`, `tract-pulse` and `tract-hir` to
  exactly 0.21.4 (the versions libDF's own lock file uses) resolves both:
  `cargo tree` with those pins and libDF as an optional dependency resolved
  without conflict. That adds about 60 crates to `Cargo.lock`, plus the model
  in the binary (8 MB standard, 36 MB low latency, compressed). This was
  checked and reverted. None of it is committed.
- To measure it anyway, an outside tool (not committed) processed the corpus
  WAVs that `--write-wav` writes. It is a 100-line Rust program that calls
  libDF's `DfTract::process` on each 480-sample block, as libDF's own
  real-time plugin does. The bench's replay chain
  (`noise_suppressor::bench::Replay`, only in the bench build) plays the
  tool's output for whichever signal the bench feeds it and runs the rest of
  the app's chain on it. Set `RELAY_REPLAY_DFN` and `RELAY_REPLAY_DFN_LL` to
  the tool's output folders to include those rows. The WAVs are 16-bit, so
  DeepFilterNet saw the corpus rounded to 16 bits; nothing else differs.

## Measurement notes

- **The delay search now stays near the declared delay.** The bench used to
  search 0-30 ms for the strongest match between the clean talker and the
  chain's output. The talker repeats every pitch period (7-10 ms), so for
  WebRTC NS (7.0 ms) it found 2.8 ms, and for the two-stage chains (17.0 ms)
  20.5 ms, and compared misaligned speech frames. It now searches within 2 ms
  of each chain's declared delay (`audio_bench::DELAY_SEARCH_SAMPLES`). The
  declared delays are checked on clicks, which do not repeat
  (`declared_delays_match_what_clicks_show`). Chains whose delay is a whole
  number of blocks (`none`, RNNoise) give the same results as before.
- **segSNR of the WebRTC NS chains is not comparable.** Its band split delays
  low frequencies by 306 samples and clicks by 335, so no single shift lines
  the output up with the clean talker sample by sample, and segSNR falls to
  about 0 dB. LSD compares spectra and is the fairer voice measure for these
  chains.
- **The voice gate hides click leaks.** The bench runs the call's voice gate,
  which keeps pauses silent whenever the voice detector stays quiet. The
  existing `enhanced_noise_suppression_removes_the_clicks_rnnoise_lets_through`
  runs without it, and found WebRTC NS in front of RNNoise worse at clicks.
  Both views matter; the second decided the outcome.
- Synthetic voice and noise only. The neural candidate (DeepFilterNet) is the
  one most likely to be judged unfairly by that.

## What changed in the code

- `crates/relay-client/src/noise_suppressor.rs` (new): the `Suppressor`
  trait (`process`, `delay_samples`, `name`, `voice_probability`),
  `SuppressorKind`, `make`, adapters for RNNoise, WebRTC NS (with level) and
  two-stage chains, `for_setting` (the one place the app chooses), the bench
  candidates, the replay chain, and tests.
- `media.rs` `MicrophoneProcessor`: the suppressor goes through the trait,
  and click suppression runs when the suppressor reports a voice probability.
  The output is unchanged (same samples, see the table).
- `audio_bench.rs`: candidate chains appended to `chains()`, the delay
  search near the declared delay, and `output_directory` made visible to the
  replay chain.
- `vendor/webrtc-sys-0.3.47`: `create_apm_with_ns_level`. The client now
  depends on `webrtc-sys` and `cxx` directly (both were already in the tree),
  under the `native-rtc` feature. `Cargo.lock` gains only those two edges.

## Next steps (for the orchestrator)

1. Record a voice with the EMEET C960 (or use `VOICE_TEST_WAV`) and rerun the
   bench before deciding on DeepFilterNet. It is the only candidate that
   removes music, and the synthetic talker is its worst case.
2. If DeepFilterNet-LL still wins with a real voice, link it behind an opt-in
   setting with the `=0.21.4` tract pins above (10 ms, about 0.6 ms CPU per
   block, +36 MB), and pair it with RNNoise's click handling, since it does
   not remove clicks.
3. If a fan or air conditioner is the user's main complaint, listen to
   `fan_plus_mouse.webrtc-ns-veryhigh-then-rnnoise+enhanced.wav` against
   `fan_plus_mouse.rnnoise+enhanced.wav` before putting that chain behind a
   setting, and tell Worker C's delay budget that it adds 7 ms.
