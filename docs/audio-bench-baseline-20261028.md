# Audio bench baseline, 2026-10-28

The send chain as shipped on `session/friends-call-quality-20261021`, measured
before any work by Workers B, C or D. Every later change to noise suppression,
the gate or automatic volume is compared with this table.

## How to run it

From a shell with the MSVC environment (`vcvars64.bat`) and Relay's patched
WebRTC SDK in `LK_CUSTOM_WEBRTC`, as for every native client build
(`build-client.ps1` sets both):

```
cargo build -p relay-client --release --features audio-bench --bin audio-bench -j 8
target\release\audio-bench.exe
```

or in one step `cargo run -p relay-client --release --features audio-bench --bin audio-bench -j 8`.
Options: `--chain NAME` (repeatable) and `--write-wav` (corpus and every
output under `target/audio-bench/wav/`). One run takes about 2 s. It writes
`target/audio-bench/<chain>.json` with the same numbers plus build, machine
and time.

The corpus, the metrics and their exact definitions are in
`crates/relay-client/bench-corpus/README.md`. Short version: seven 10 s rows
of a synthetic talker plus synthetic noise (no recording exists in the
repository and none was made), active-level SNR, and every chain running the
app's own `MicrophoneProcessor` block by block.

## Chains

- `none`: no processing, the noisy microphone signal itself.
- `rnnoise`: the shipped default settings with noise suppression set to
  Standard (RNNoise only).
- `rnnoise+enhanced`: the shipped default settings (`AudioSettings::default()`):
  RNNoise, Enhanced click suppression and voice isolation at Normal,
  microphone gain 1.5, automatic volume, voice activation with automatic
  sensitivity, 5 ms attack, 200 ms hold, 80 ms release. Echo cancellation is
  off by default and off here.

Both RNNoise chains include gain, automatic volume and the voice gate, because
that is what a call sends. A block the gate keeps closed is silence.

## Baseline

Release build, AMD64 Family 26 Model 68 (16 logical processors), one
thread, run at 2026-10-28T17:12:25Z. All metric columns were bit-identical
over three runs; only the CPU column moves (default chain: 0.031 to 0.032 ms
mean).

| chain | item | SNR in (dB) | residual noise in pauses (dB) | noise leak during speech (dB) | speech segSNR (dB) | LSD (dB) | speech attenuation (dB) | CPU (ms / 10 ms block) |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| none | keyboard_between_words | 10.0 | 0.0 | n/a | 35.0 | 0.00 | 0.0 | 0.000 |
| none | keyboard_during_speech | 10.0 | 0.0 | 0.0 | 28.4 | 2.94 | -0.1 | 0.000 |
| none | mouse_clicks | 15.0 | 0.0 | 0.0 | 32.7 | 0.90 | -0.0 | 0.000 |
| none | fan_plus_mouse | 5.0 | 0.0 | 0.0 | 2.8 | 9.90 | -1.2 | 0.000 |
| none | music_bed | 5.0 | 0.0 | 0.0 | 2.8 | 5.14 | -1.3 | 0.000 |
| none | second_talker | 0.0 | 0.0 | 0.0 | 5.8 | 1.51 | -1.7 | 0.000 |
| none | silence_floor | n/a | 0.0 | n/a | n/a | n/a | n/a | 0.000 |
| rnnoise | keyboard_between_words | 10.0 | -3.5 | n/a | 9.5 | 1.46 | -1.1 | 0.027 |
| rnnoise | keyboard_during_speech | 10.0 | -1.7 | 1.2 | 9.2 | 3.06 | -1.1 | 0.026 |
| rnnoise | mouse_clicks | 15.0 | -0.7 | 3.5 | 9.4 | 2.10 | -1.0 | 0.027 |
| rnnoise | fan_plus_mouse | 5.0 | -17.2 | -1.4 | 4.7 | 5.07 | -1.3 | 0.029 |
| rnnoise | music_bed | 5.0 | 0.7 | 2.0 | 1.8 | 5.23 | -1.6 | 0.028 |
| rnnoise | second_talker | 0.0 | -0.1 | 0.4 | 4.8 | 2.01 | -1.4 | 0.027 |
| rnnoise | silence_floor | n/a | 16.9 | n/a | n/a | n/a | n/a | 0.028 |
| rnnoise+enhanced | keyboard_between_words | 10.0 | -10.7 | n/a | 9.3 | 1.80 | -0.9 | 0.031 |
| rnnoise+enhanced | keyboard_during_speech | 10.0 | -7.0 | -1.0 | 9.2 | 2.78 | -0.9 | 0.031 |
| rnnoise+enhanced | mouse_clicks | 15.0 | -9.5 | 5.1 | 9.1 | 2.36 | -0.8 | 0.031 |
| rnnoise+enhanced | fan_plus_mouse | 5.0 | -17.9 | -0.6 | 4.8 | 4.96 | -1.0 | 0.032 |
| rnnoise+enhanced | music_bed | 5.0 | 0.5 | 2.1 | 1.8 | 5.25 | -1.6 | 0.032 |
| rnnoise+enhanced | second_talker | 0.0 | -0.1 | 0.4 | 4.6 | 2.25 | -1.3 | 0.031 |
| rnnoise+enhanced | silence_floor | n/a | 17.1 | n/a | n/a | n/a | n/a | 0.033 |

| chain | declared delay (ms) | measured delay (ms) | CPU mean (ms / 10 ms block) | CPU p99 | CPU max |
| --- | ---: | ---: | ---: | ---: | ---: |
| none | 0.00 | 0.00 | 0.000 | 0.000 | 0.000 |
| rnnoise | 10.00 | 9.94 | 0.027 | 0.044 | 0.099 |
| rnnoise+enhanced | 10.00 | 9.96 | 0.032 | 0.039 | 0.087 |

## What the numbers say

- **Delay and CPU are not the problem.** The shipped chain adds 9.96 ms
  measured (RNNoise's one held frame; click suppression and the gate add
  none) and costs 0.032 ms per 10 ms block on one thread, against budgets of
  20 ms and 1.5 ms. This covers the processing only: the capture block, the
  Opus lookahead, packetization and the call's speech-start history are
  outside it (Worker C's budget).
- **Music is not removed at all.** `music_bed` keeps all of its level in
  pauses (+0.5 dB with Enhanced, +0.7 dB with RNNoise alone: slightly louder
  than it came in) and leaks fully under speech (+2.1 dB). Worker B's target
  is -20 dB.
- **Keyboard and mouse clicks are only partly removed.** Enhanced takes
  clicks between words down 10.7 dB (RNNoise alone 3.5 dB) and mouse clicks
  9.5 dB (RNNoise 0.7 dB); clicks on top of words are not reduced
  (leak -1.0 dB). Worker B's target for `keyboard_during_speech` is -25 dB.
- **A steady fan is handled:** -17.9 dB in pauses with Enhanced.
- **A second talker passes untouched** (-0.1 dB), as expected from RNNoise.
- **The default chain makes a quiet room louder.** On `silence_floor` (a
  laptop fan at -45 dBFS, nobody talking) the default chain sends audio
  17.1 dB louder than the input. Per second of the 10 s row, the gate was
  open in 92, 100, 100, 90, 70, 57, 100, 3, 71 and 0 of the 100 blocks, at
  about -25 dBFS. Automatic sensitivity treats the fan left after RNNoise
  as speech, and automatic volume then raises it. This is the plan's review
  focus 5 (automatic volume pumping the noise floor) and belongs to Worker C.
- **The voice is changed by every RNNoise chain:** segSNR falls from 35 dB to
  about 9 dB and LSD rises to 1.5-1.8 dB even where the noise sits only in
  pauses. Part of this is RNNoise's reaction to a synthetic talker (it
  recognises recorded speech more reliably, see the README), part is the
  gain changes of automatic volume, which one least-squares gain per row does
  not remove. Compare chains with each other on this corpus, not with
  published scores.

## Limits

- Synthetic speech and noise only. A recorded corpus (for example from the
  EMEET C960, if the orchestrator asks for one) would make the speech columns
  more realistic; the bench code takes any 48 kHz mono signal.
- The bench does not replay the call's speech-start history (blocks a call
  prepends when the gate opens), so onsets after a closed gate come out a
  little shorter than in a call.
- CPU is wall time on the machine above with other sessions possibly
  running; treat differences under about 0.01 ms as noise.
