# Studio noise suppression (DPDFNet), 2026-10-30

Task H of `docs/superpowers/plans/2026-10-29-audio-suppressor-round2.md`.
Follows Worker F's evaluation (`docs/audio-dpdfnet-eval-20261029.md`) and the
user's listening test, after which the user chose to keep RNNoise as it is
and offer DPDFNet as a choice in the app.

## What the user gets

Settings → Voice → Noise suppression has a third choice next to Enhanced
(still the default) and Standard:

> **Studio.** Also removes music, other people talking and fans, and leaves
> your voice as it is. Others hear you about 50 ms later than with Enhanced,
> and it uses more of this PC's processor.

Behind it is dpdfnet8 (Ceva's DPDFNet, 3.6 M parameters) running inside
`relay.exe`, the model the user found clearly best in Worker F's listening
set. Nothing changes for anyone who does not pick it: Enhanced and Standard
run the same code as before, and old settings files load unchanged (test
`studio_is_a_third_noise_model_and_older_files_still_load`).

| | Enhanced (default) | Studio |
| --- | --- | --- |
| music in pauses, 5 dB / 12 dB (dB) | -0.8 / +1.7 | **-38.5 / -38.0** |
| music while talking (leak, dB) | +0.1 / +2.7 | **-7.0 / -3.7** |
| second talker in pauses (dB) | +0.2 | **-50.3** |
| fan + mouse in pauses (dB) | -27.0 | **-41.7** |
| keys between words (dB) | -24.3 | **-42.2** |
| keys on words (leak, dB) | -0.1 | -4.8 |
| voice LSD, music 5 dB (dB, lower is closer to clean) | 4.35 | **2.37** |
| clean voice, LSD through the send chain (dB) | 2.12 | **1.52** |
| added delay of the suppressor (ms) | 10 | 60 (model 50, hand-off to its thread 10) |
| CPU per 10 ms block on the voice worker, p50 / p99 (ms) | 0.03 / 0.04 | 0.005 / 0.014 |
| CPU per 10 ms block on Studio's own thread, p50 / p99 (ms) | none | 2.75 / 3.68 |

Recorded voice (Worker E's `voice3-prepared.wav`), whole send chain, bench
default (float input), in process: the app's own `MicrophoneProcessor`, not a
replay. Full table below.

## How it works

`crates/relay-client/src/dpdfnet.rs`, behind the existing
`noise_suppressor::Suppressor` interface (`SuppressorKind::DpdfNet`):

- **Per 10 ms block** (480 samples at 48 kHz): a 960-point Vorbis-window STFT
  frame over the last two blocks, one tract-onnx run of the model with its
  state carried from the previous block, an inverse STFT and overlap-add.
  This is Worker F's `dpprobe` port, line for line.
- **Delay**: declared 2880 samples = 60 ms: the model's 50 ms (four 10 ms
  frames of look-ahead in the trained weights, plus 10 ms of overlap-add) and
  one 10 ms block of hand-off to the model's thread (below). The bench
  measures 60.00 ms for both models. `send_delay_budget_ms` and the settings
  page's delay details count it: steady speech 22.5 → 72.5 ms, first packet
  of a talkspurt 32.5 → 82.5 ms (test `send_delay_budget_counts_studio_at_60_ms`).
- **Chain**: Studio replaces RNNoise; the voice gate (with WebRTC's voice
  detector) and automatic volume stay after it, as for Enhanced. Click
  suppression does not run (see "Click suppression" below).
- **Models** are embedded with `include_bytes!` and built by tract (on the
  loader thread, never on the voice worker) the first time they are needed (0.36 s for dpdfnet8 in a release build; the bench
  logs it). Choosing Studio in settings starts that build on a thread of its
  own (`dpdfnet::prepare`), so a call normally starts on the model. If a call
  starts before the model is ready, the model thread uses RNNoise's output
  delayed to the model's 50 ms, and fades over one block to the model once it is ready and
  has seen 60 ms of input. RNNoise runs alongside the model the whole time
  (0.03 ms per block) so that any switch starts from a warm state.
- **Push-to-talk and privacy resets** rebuild the suppressor as before; the
  built model is shared, so a rebuild costs only a fresh state.

### Which thread runs it

The microphone chain does not run in the audio device callback: the WASAPI
callback only queues samples, and `voice_worker` (media.rs) pops one 10 ms
block per loop iteration and calls `MicrophoneProcessor::process`. That same
thread also mixes playback in 2.5 ms quanta, so a model step of 3-6 ms there
would hold back what the user hears. The orchestrator decided (option B)
that playback must never wait on the model, at the price of 10 ms more send
delay.

So the model runs on a thread of its own (`relay-studio`, one per Studio
suppressor). `dpdfnet::Studio`, which the voice worker holds, gives block t to
that thread (a queue of at most 4 blocks, `try_send`) and sends the model's
output for block t-1, which the thread has had a whole block to produce
(`try_recv`). It never waits. When that output is not there, because the
thread is slow, stuck or gone, the block goes out **unprocessed**: the raw
microphone, delayed to the same 60 ms, so the timing never changes. A switch
between model output and raw microphone starts where the last block ended and
eases the difference out over the block, so it is not a click. Results that
arrive too late are dropped. The first such block is logged once
(`Studio noise suppression: the model thread fell behind; sending the
microphone unprocessed until it catches up`), and at the end of a call the
count (`… N blocks, M sent unprocessed because the model was late, K not
queued`).

Measured in real time (release build, a block every 10 ms for 20 s,
dpdfnet8, opt-in test `paced_hand_off_in_real_time`): the voice worker spends
**p50 0.005 ms, p99 0.014 ms, max 0.027 ms** per block on Studio, and **0 of
2000 blocks** went out unprocessed. Test
`a_stuck_model_thread_never_stalls_the_caller` holds the model thread stuck:
every call returns in under 5 ms (debug build), the output is the
microphone at exactly 60 ms, and dropping the suppressor does not wait for
the thread. Test `the_model_thread_gives_the_inline_output_at_the_same_delay`:
when the thread keeps up, its output is bit-identical to the bench's.

The bench and tests run the same model code on the calling thread with the
same one-block hold (`Runner::Inline`), so their output is deterministic and
has the same 60 ms; their CPU column is the model thread's work.

### Overrun guard

This runs on the model thread. If the model takes longer than its block's
10 ms for 3 blocks in a row, the suppressor fades (over one block) to the RNNoise stand-in, logs once
(`Studio noise suppression: dpdfnet8 took longer than 10 ms for 3 blocks in a
row; switching to dpdfnet2`) and starts dpdfnet2, which takes over with a
fade after 60 ms of warm-up. If dpdfnet2 also falls behind, Studio stays on
RNNoise (at the model's 50 ms). A model that fails to load or to run takes
the same path.
The decision is kept for the rest of the app session, so the next call starts
on the model this PC keeps up with. While the thread is behind, the blocks
it has not finished go out unprocessed (above); the voice worker never waits
and the delay stays 60 ms throughout. Test:
`the_overrun_guard_steps_down_without_dropping_a_block` (every block out,
no sample step above 0.01 on a 220 Hz tone across all three switches; a hard
switch would step by up to 0.1).

When a call ends, the app logs the model's own time per block for that call
(`Studio noise suppression: N blocks, model time per 10 ms block p50 …, p99
…, max …; ended on dpdfnet8`).

## CPU, this machine

AMD Family 26 Model 68, 16 logical processors, release build, nothing else
running. The bench runs the model on the calling thread (inline), so this
is the model thread's load plus the rest of the chain: the whole
`MicrophoneProcessor::process` per 10 ms block (model, RNNoise stand-in,
automatic volume, gate), all 18 signals of the recorded-voice bench, 53 892
blocks per chain (run `q16`, 2026-10-30, before the hand-off; the rerun with
it, run `thread`, gave p50 2.77 / p99 3.47 ms for dpdfnet8 and 1.03 / 1.42 ms
for dpdfnet2). What the voice worker itself spends is in "Which thread runs
it": 0.005 ms p50.

| chain | mean (ms) | p50 (ms) | p99 (ms) | max (ms) |
| --- | ---: | ---: | ---: | ---: |
| Enhanced (`rnnoise+enhanced`) | 0.032 | 0.032 | 0.039 | 0.104 |
| Studio, dpdfnet8 | 2.797 | 2.752 | 3.684 | 6.069 |
| Studio after a step down, dpdfnet2 | 1.069 | 1.039 | 1.492 | 27.202 |

dpdfnet8 uses about 28 % of one core, on its own thread, while you talk or
not (the model runs on every block). Against the plan's budget (1.5 ms p99) dpdfnet8 is 2.5x
over; dpdfnet2 meets it.

Worker F's probe measured the model alone at p50 3.82 / p99 5.33 ms
(dpdfnet8) and 1.39 / 1.78 ms (dpdfnet2), more than the whole chain here; the
code is the same, the build settings differ (the client's thin LTO), and
why it is faster was not investigated.

## Does the in-app code match Worker F's replay?

(Measured before the hand-off was added, at 50 ms. With it, every Studio
figure on the bench moved by at most 0.1 dB, run `thread`; the model output
is the same, one block later.)

**Yes, once both are fed the same thing.** Three checks:

1. **The model output, same input.** The app's code on the exact 16-bit WAV
   Worker F's `dpprobe` was given, against his output (opt-in test
   `compare_with_another_implementation`):

   | input | model | RMS difference to `dpprobe` (full scale) |
   | --- | --- | ---: |
   | `music_bed` row, 30 s (bench corpus WAV) | dpdfnet8 | 8.0e-6 (his 16-bit WAV's rounding) |
   | `music_bed_12` row, 30 s | dpdfnet2 | 8.1e-6 |
   | 0.5 s synthetic talker + noise, float | dpdfnet8 | 5.3e-9 |
   | 0.5 s synthetic talker + noise, float | dpdfnet2 | 5.7e-9 |

   Two runs are bit-identical. tract against onnxruntime (the upstream
   Python script), same 0.5 s signal: 1.3e-4 (dpdfnet8) and 9.0e-5
   (dpdfnet2) RMS, 56-59 dB below the signal; Worker F: 4.7e-8 on the clean
   recorded voice. The committed test
   `dpdfnet8_matches_the_replayed_probe_within_1e_4_rms` holds the app's code
   to `dpprobe`'s output on that signal (48 KB of 16-bit samples, sha256 in
   the test): 8.3e-6.

2. **The bench, as the brief asked (in process against `replay:`, every
   row).** Not within 0.5 dB as the bench stands, for two reasons that are
   not the port:
   - The bench feeds in-process chains the float corpus; the replay tool was
     fed the 16-bit WAVs. DPDFNet's pause residual reacts to that rounding
     (below -90 dBFS): dpdfnet8 `music_bed` -38.5 dB with float input,
     -49.3 dB with 16-bit input, same app code.
   - The app applies the input volume (150 % by default) **before** the
     suppressor, so in process the model hears the louder signal. The replay
     route plays the model's output of the unscaled WAV and applies 150 %
     after it. DPDFNet is not level-independent, and the gate after it
     decides on absolute levels.

   The residual in pauses is also sensitive on its own: Worker F's
   `dpprobe`, fed the same WAVs with one LSB added to every seventh sample,
   moves `keyboard_between_words` from -41.9 to -27.8 dB through the same
   replay. The voice columns (segSNR, LSD, attenuation) agree within 0.1 dB
   on every row between the bench-default run and the replays.

   Residual in pauses (dB), in process and replayed:

   | row | dpdfnet8 float in | dpdfnet8 16-bit in | replay:dpdfnet8 | dpdfnet2 float in | dpdfnet2 16-bit in | replay:dpdfnet2 |
   | --- | ---: | ---: | ---: | ---: | ---: | ---: |
   | keyboard_between_words | -42.2 | -42.2 | -41.9 | -42.4 | -42.4 | -38.8 |
   | keyboard_during_speech | -32.0 | -32.1 | -32.4 | -27.5 | -27.5 | -26.9 |
   | mouse_clicks | -34.0 | -34.0 | -33.9 | -34.1 | -34.1 | -33.1 |
   | fan_plus_mouse | -41.7 | -41.7 | -41.6 | -44.9 | -44.9 | -45.0 |
   | music_bed | -38.5 | -49.3 | -49.3 | -36.2 | -37.1 | -37.0 |
   | music_bed_12 | -38.0 | -43.3 | -43.3 | -35.9 | -37.0 | -29.9 |
   | second_talker | -50.3 | -50.2 | -49.0 | -47.8 | -47.5 | -48.4 |
   | silence_floor | -95.0 | -95.0 | -95.0 | -95.0 | -95.0 | -95.0 |
   | clean_voice | -14.6 | -14.7 | -14.2 | -15.1 | -15.2 | -13.3 |

3. **The bench like for like.** With the new switch `--input-16bit` (every
   chain fed exactly the samples the corpus WAVs hold) and, in a temporary
   uncommitted build, the input volume at 100 % on both routes, the largest
   difference over every metric column of every row is **0.3 dB for
   dpdfnet2** and **0.4 dB for dpdfnet8**, except dpdfnet8 `clean_voice` at
   0.6 dB. That row has no noise; its "residual" is what is left in the
   recording's digital-silence pauses against the -70 dBFS floor (Worker F's
   note under his table).

The Studio figures in this document are the in-process ones (float input,
150 % input volume): what the app does. Worker F's replayed figures applied
the input volume after the model, which the app never does. For a later
DPDFNet comparison, compare in process, or replay with the gain set to 100 %.

## Click suppression

Click suppression needs a voice probability; DPDFNet does not give one. The
cheap stand-in is what the model keeps of each input block: the energy of
the output block over the energy of the input block it answers (5 blocks
earlier), clamped to 0..1. Measured on the bench (in process, recorded
voice):

| row | dpdfnet8 residual / leak / LSD | dpdfnet8 + ratio clicks residual / leak / LSD |
| --- | --- | --- |
| keyboard_between_words | -42.2 / n/a / 1.47 | -43.5 / n/a / 1.75 |
| keyboard_during_speech | -32.0 / -4.8 / 2.11 | -36.2 / -4.0 / 2.41 |
| mouse_clicks | -34.0 / 0.1 / 1.73 | -35.4 / 1.3 / 2.02 |
| fan_plus_mouse | -41.7 / -6.9 / 4.22 | -45.7 / -5.8 / 4.79 |
| music_bed | -38.5 / -7.0 / 2.37 | -57.3 / -5.8 / 3.13 |
| music_bed_12 | -38.0 / -3.7 / 2.10 | -53.0 / -3.2 / 2.57 |
| second_talker | -50.3 / -11.0 / 1.69 | -56.1 / -9.5 / 2.53 |
| clean_voice | -14.6 / n/a / 1.52 | -15.5 / n/a / 1.89 |

The ratio makes pauses deeper, but changes the voice more on every row
(LSD +0.3 to +0.8 dB) and lets *more* of the keys on words through (leak
-4.0 against -4.8 dB), the one figure it was meant to help. Worker F found a
similar trade with RNNoise and click suppression after the model. **Studio runs without click
suppression** (`noise_suppressor::STUDIO_CLICKS = false`); the ratio stays in
the code and on the bench (`dpdfnet8+ratio-clicks`, `dpdfnet2+ratio-clicks`)
for a later listening test.

## Model shipping

- `vendor/models/dpdfnet/`: `dpdfnet8_48khz_hr.onnx` (14 857 107 bytes) and
  `dpdfnet2_48khz_hr.onnx` (10 493 337 bytes), unmodified, Hugging Face
  `Ceva-IP/DPDFNet` revision `dd6818d00f50c836fed43a6243ebe49116de5964`;
  the upstream `LICENSE` (Apache-2.0, GitHub `ceva-ip/DPDFNet` commit
  `9bd9844a227bb6aa57e55588d8d0e961fcff1c46`); `README.md` with sources and
  sha256. Test `embedded_models_are_the_vendored_files` checks the embedded
  bytes against those hashes.
- Both are embedded in `relay.exe` with `include_bytes!`, so the installer and
  release pipeline need no change. **`relay.exe` grows by 48.5 MB, not the
  ~25 MB expected**: release build 78 772 736 bytes at the base commit
  `0e629b7`, 127 318 016 bytes with Studio. 25.4 MB of that is the two
  models; the rest is tract's code (its operator set and kernels).
  Leaving out tract-onnx's default `transformers` feature saved 0.9 MB.
  Shipping only dpdfnet8 (dropping the overrun guard's step to dpdfnet2)
  would save 10.5 MB.
- `scripts/generate-notices.py` adds a DPDFNet block with the Apache-2.0 text
  to `THIRD_PARTY_NOTICES.txt`, and the regenerated file carries the new
  crates' licences too: the 33 new packages that are in the Windows
  dependency graph (`borsh`, `borsh-derive` and `xattr` are not built on
  Windows). Only those blocks were added. The committed file was already
  behind the lock before this change: regenerating it in full also adds 5
  packages other work brought in (chacha20 0.10.2, memmap2 0.9.11,
  num-bigint 0.4.8, pastey 0.2.3, rustversion 1.0.23). That refresh is left
  for a separate commit.

## Crates

`tract-onnx = { version = "=0.23.9", default-features = false }` and
`realfft = "3.5.0"` (already in the lock) in `crates/relay-client/Cargo.toml`.
`Cargo.lock` gains 36 packages: **30 new crates** (Worker F counted 32 with
tract-onnx's default features, which add `tract-transformers` and
`float-ord`) and 6 second versions of crates the client already had.

New: borsh 1.8.1, borsh-derive 1.8.1, derive-new 0.7.0, dyn-hash 1.0.0,
erased-serde 0.4.11, filetime 0.2.29, inventory 0.3.25, maplit 1.0.2,
matrixmultiply 0.3.11, memo-map 0.3.4, metrics 0.24.6, minijinja 2.24.0,
ndarray 0.17.2, nom-language 0.1.0, num-rational 0.4.2, rand_distr 0.6.0,
rapidhash 4.5.1, rawpointer 0.2.1, string-interner 0.20.0, tar 0.4.46,
tract-core 0.23.9, tract-data 0.23.9, tract-extra 0.23.9, tract-hir 0.23.9,
tract-linalg 0.23.9, tract-nnef 0.23.9, tract-onnx 0.23.9,
tract-onnx-opl 0.23.9, typeid 1.0.3, xattr 1.6.1.

Second versions: bit-set 0.11.1, bit-vec 0.10.1, downcast-rs 2.0.2,
hashbrown 0.16.1, itertools 0.15.0, nom 8.0.0.

## Full table, recorded voice

Bench default (float input), release build, 2026-10-30T13:35:56Z (before
the hand-off; with it every figure is within 0.1 dB, run `thread`), the same
corpus as Worker F's (the corpus WAVs are byte-identical). Shipped chain and
Studio in process. CPU: see "CPU" above (this run shared the machine with a
test build).

| chain | item | SNR in (dB) | residual noise in pauses (dB) | noise leak during speech (dB) | speech segSNR (dB) | LSD (dB) | speech attenuation (dB) |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| rnnoise+enhanced | keyboard_between_words | 10.0 | -24.3 | n/a | 9.0 | 2.16 | -0.3 |
| rnnoise+enhanced | keyboard_during_speech | 10.0 | -16.6 | -0.1 | 8.4 | 3.50 | -0.1 |
| rnnoise+enhanced | mouse_clicks | 15.0 | -16.7 | 7.5 | 8.7 | 2.64 | -0.2 |
| rnnoise+enhanced | fan_plus_mouse | 5.0 | -27.0 | -1.5 | 4.4 | 5.33 | 0.3 |
| rnnoise+enhanced | music_bed | 5.0 | -0.8 | 0.1 | 3.4 | 4.35 | -0.2 |
| rnnoise+enhanced | music_bed_12 | 12.0 | 1.7 | 2.7 | 5.9 | 3.41 | -0.5 |
| rnnoise+enhanced | second_talker | 0.0 | 0.2 | -0.4 | 4.3 | 3.22 | -0.4 |
| rnnoise+enhanced | silence_floor | n/a | -11.3 | n/a | n/a | n/a | n/a |
| rnnoise+enhanced | clean_voice | n/a | -17.5 | n/a | 9.1 | 2.12 | -0.2 |
| dpdfnet8 | keyboard_between_words | 10.0 | -42.2 | n/a | 11.8 | 1.47 | -0.4 |
| dpdfnet8 | keyboard_during_speech | 10.0 | -32.0 | -4.8 | 11.2 | 2.11 | -0.4 |
| dpdfnet8 | mouse_clicks | 15.0 | -34.0 | 0.1 | 11.5 | 1.73 | -0.4 |
| dpdfnet8 | fan_plus_mouse | 5.0 | -41.7 | -6.9 | 7.4 | 4.22 | -0.1 |
| dpdfnet8 | music_bed | 5.0 | -38.5 | -7.0 | 7.8 | 2.37 | -0.3 |
| dpdfnet8 | music_bed_12 | 12.0 | -38.0 | -3.7 | 9.2 | 2.10 | -0.4 |
| dpdfnet8 | second_talker | 0.0 | -50.3 | -11.0 | 10.3 | 1.69 | -0.3 |
| dpdfnet8 | silence_floor | n/a | -95.0 | n/a | n/a | n/a | n/a |
| dpdfnet8 | clean_voice | n/a | -14.6 | n/a | 11.8 | 1.52 | -0.3 |
| dpdfnet2 | keyboard_between_words | 10.0 | -42.4 | n/a | 12.0 | 1.49 | -0.3 |
| dpdfnet2 | keyboard_during_speech | 10.0 | -27.5 | -5.1 | 11.3 | 2.32 | -0.3 |
| dpdfnet2 | mouse_clicks | 15.0 | -34.1 | 2.4 | 11.7 | 1.80 | -0.3 |
| dpdfnet2 | fan_plus_mouse | 5.0 | -44.9 | -6.8 | 7.2 | 4.29 | -0.1 |
| dpdfnet2 | music_bed | 5.0 | -36.2 | -6.5 | 7.5 | 2.56 | -0.2 |
| dpdfnet2 | music_bed_12 | 12.0 | -35.9 | -3.0 | 9.0 | 2.23 | -0.3 |
| dpdfnet2 | second_talker | 0.0 | -47.8 | -9.3 | 9.6 | 2.03 | -0.3 |
| dpdfnet2 | silence_floor | n/a | -95.0 | n/a | n/a | n/a | n/a |
| dpdfnet2 | clean_voice | n/a | -15.1 | n/a | 11.8 | 1.74 | -0.3 |

## Tests and checks

- `cargo test -p relay-client -- --test-threads 8`: **1623 passed, 0 failed,
  118 ignored** on the feature commit (15:57). From about 16:20 on, tests
  that write to Windows Credential Manager fail on this machine: 13 in the
  next run, 111 in the last (1514 passed, 119 ignored), every one with
  "Windows could not save the device checkpoint marker" or "... the
  remembered login" from `CredWriteW`. The test binary that had passed the
  whole suite fails the same tests now (49 of 63 e2ee tests at 8 threads, 1 at
  1 thread; each passes on its own), so this is the machine's credential
  store, not this change. The store holds 570 `Relay/crypto-revision/*`
  entries, 10 more than an hour earlier: the e2ee tests leave one per test
  and never delete it. They were left alone because a real profile's
  marker cannot be told apart from a test's. Every audio, settings and
  dpdfnet test passed in every run.
  New tests: `audio_settings::studio_is_a_third_noise_model_and_older_files_still_load`,
  `noise_suppressor::enhanced_and_standard_keep_rnnoise_and_studio_runs_guarded_dpdfnet8`,
  `media::send_delay_budget_counts_studio_at_60_ms`, and in `dpdfnet`:
  `embedded_models_are_the_vendored_files`,
  `dpdfnet8_matches_the_replayed_probe_within_1e_4_rms`,
  `declared_delay_matches_the_output`,
  `the_ratio_probability_follows_speech_and_is_off_unless_asked`,
  `the_overrun_guard_steps_down_without_dropping_a_block`,
  `the_stand_in_is_rnnoise_at_the_model_delay`,
  `the_model_thread_gives_the_inline_output_at_the_same_delay`,
  `a_stuck_model_thread_never_stalls_the_caller`; opt-in (ignored)
  `write_reference_input`, `compare_with_another_implementation` and
  `paced_hand_off_in_real_time` (release).
- `cargo clippy -p relay-client`: 26 diagnostics, the same 26 as at the base
  commit (compared by file and message); none in the new code.
- `cargo build -p relay-client --release --bin relay`: builds, 127 318 016
  bytes.
- Bench runs (`audio-bench`, release, recorded voice): `real` (bench
  default, all Studio chains next to Worker F's replays) and `q16`
  (`--input-16bit`, nothing else running: the CPU figures). Run folders under
  `D:\Projects\relay-build-targets\audio-worker-h-runs\`, scripts in
  `work\audio-worker-h-20261030\ref\`.

## What is not done

- **No listening test of the in-app output.** The model code is the one
  behind Worker F's listening set, but the app runs it on the microphone
  after the input volume (150 % by default), which his files did not; the
  pause residual differs on some rows (above), the voice figures do not.
  Nobody has listened to a call with Studio on.
- **No call on two PCs** with Studio on. The hand-off was measured in a
  paced test, not in a call with playback running.
- **Unprocessed blocks when the model is late** are what the orchestrator
  chose over stalling; on a PC where even dpdfnet2 falls behind, the guard
  ends on RNNoise, but until it decides, late blocks carry the raw
  microphone (music and all) for those 10 ms each.
- **60 ms** stays. A model with less look-ahead needs retraining (Worker F,
  "What could change the answer").
- **Keys on words** stay the one figure Studio misses (-4.8 dB against the
  -10 dB target).
- The bench's pause residual is not reproducible to 0.5 dB on noisy rows
  (above); later comparisons of DPDFNet variants should be in process, or
  replays of what the app feeds the model, run with `--input-16bit`; or
  compare the voice columns only.
