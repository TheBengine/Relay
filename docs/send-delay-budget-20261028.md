# Send-side delay budget, 2026-10-28

Worker C of `docs/superpowers/plans/2026-10-28-audio-quality-orchestration.md`.
How long a piece of microphone sound waits inside Relay, from the capture
callback that hands it over until the voice packet carrying it leaves, part by
part. It also covers why a quiet room with a fan used to be sent louder than it
was. Every number below was measured or comes from a test. Where something was
not measured, it says so.

## The budget

`send_delay_budget_ms(&AudioSettings)` in `crates/relay-client/src/media.rs`
returns these parts. It is built on the settings page's `voice_delay_budget()`
(same RNNoise, Opus and speech-start figures), adds the parts that budget left
implicit, and `send_delay_budget_for_capture(settings, rate, period_ms)`
gives the same for a measured device. Tests:
`send_delay_budget_sums_its_parts_for_default_settings` and
`send_delay_budget_counts_a_capture_period_that_does_not_fit_the_block`.

Default settings (10 ms packets, RNNoise + Enhanced, voice activation with the
speech-start history, echo cancellation off), in milliseconds:

| part | 48 kHz, 10 ms period (Windows default) | this PC: Scarlett Solo, 7 ms period | where it comes from |
| --- | ---: | ---: | --- |
| capture period (block waits for the callback that completes it) | 0.0 | 6.0 worst, 3.0 mean | period − gcd(period, 480 samples); 0 when the period divides 10 ms |
| resample | 0.0 | 0.0 | 48 kHz passes straight through (was one sample held until the next callback, see below) |
| echo cancellation | 0.0 | 0.0 | off by default; when on, AEC3 works on the current 10 ms block in place |
| noise suppression (RNNoise) | 10.0 | 10.0 | overlap-add returns each frame one frame late; bench measures 9.96 |
| click suppression | 0.0 | 0.0 | current block only (bench: no delay beyond RNNoise) |
| gate attack | 0.0 | 0.0 | a fade on the current block; test `default_attack_fades_the_current_block_in_without_lookahead` |
| Opus lookahead | 2.5 | 2.5 | LowDelay, 120 samples, from the linked encoder |
| packetization | 10.0 | 10.0 | a packet's oldest sample waits for its newest |
| **steady speech, total** | **22.5** | **28.5 worst / 25.5 mean** | |
| speech-start history (first packet after the gate opens only) | +10.0 | +10.0 | one retained block sent with the first packet |
| **first packet of a talkspurt** | **32.5** | **38.5 worst / 35.5 mean** | |

Before this change (commit 587b8b4), 48 kHz capture held back the last sample
of every callback until the next one, so no 10 ms block was complete until the
following callback arrived. Test
`capture_at_48khz_completes_each_10ms_block_in_the_callback_that_delivers_it`
failed then with 479 of 480 samples in the queue. That cost one extra capture
period:

| | before | after |
| --- | ---: | ---: |
| 48 kHz, 10 ms period: steady / first packet | 32.5 / 42.5 | 22.5 / 32.5 |
| this PC, 7 ms period: worst wait for the callback | 7.0 | 6.0 |
| this PC, 7 ms period: mean wait for the callback | 4.0 | 3.0 |

Not in the budget, because nothing here measures them: the time Windows and the
USB device hold sound before the first callback (the Scarlett driver reports a
stream latency of 0, which is not believable), and the worker thread's wake-up
after a callback. The worker is woken by the callback itself and drains a
backlog without waiting (`media.rs`, the capture-ready check before
`wait.wait`), so the capture queue holds no standing delay.

## The extra RNNoise block

It cannot be removed without changing the suppressor. nnnoiseless analyses a
960-sample window (two 10 ms frames, `features.rs` `WINDOW_SIZE`) and adds
each output frame to the second half of the previous frame's synthesis, which
it keeps in `DenoiseFeatures::synthesis_mem` (`features.rs:28`, used at lines
272–273). The output for a frame is only final once the next frame has been
analysed, so the 10 ms is part of the algorithm, not a buffer Relay adds.
Relay's own block alignment adds nothing more with 10 ms packets: the
processing block is the packet. With 5 ms packets the block is still 10 ms, and
that shows up as packetization 10 instead of 5. Removing the frame needs a
suppressor with a low-delay window, which is Worker B's choice.

## Windows capture period, measured

`windows_audio.rs` already asks `IAudioClient3` for the smallest shared-mode
period. It now also reports the device's minimum, step and driver latency (in
the `WASAPI capture:` and `WASAPI output:` log lines and in `PeriodInfo`). The
ignored test `actual_shared_streams_report_period_and_callback_interval` opens
the default microphone and speakers for 3 s each and records only callback
times and sizes. Two runs on 2026-10-28, same result:

| device | rate | negotiated period | device default / minimum | callbacks: mean, median, p95, max | frames per callback |
| --- | --- | ---: | --- | --- | --- |
| Microphone (Scarlett Solo USB), default input | 48 kHz, 2 ch | 7.000 ms (336 frames) | 10.000 / 7.000 ms | 7.006, 7.0, 7.2–7.3, 8.0 ms | 336–337 |
| Speakers (Scarlett Solo USB), default output | 44.1 kHz, 2 ch | 7.007 ms (309 frames) | 10.000 / 7.007 ms | 7.006, 7.0, 7.04–7.05, 8.0 ms | 309 |

The EMEET C960 named in the plan is connected as a camera, but Windows lists no
active microphone endpoint for it. Its microphone is disabled or hidden in
Sound settings, so it could not be measured. Changing system sound settings was
out of scope.

The period request was not changed: the brief allows that only if the period is
above 5 ms and the device supports less, and this device's minimum is the
7 ms it already gets. One finding for later: 7 ms does not divide the 10 ms
block, which costs 3 ms on average and 6 ms at worst (row "capture period"
above). The device's 10 ms default would line up with the block and cost 0.
Whether 10 ms is faster overall depends on how much Windows buffers per period
before the callback, which needs a physical loopback measurement, not a log
line.

## A quiet room with a fan

On the bench's `silence_floor` row (a laptop fan at -45 dBFS, nobody talking)
the shipped chain sent the fan 17.1 dB louder than the microphone heard it.
Two things combine:

1. RNNoise removes the fan well for about a second, then lets more of it
   through: its output rises from about -75 to -49 dBFS by second four, partly
   in a jump of 14 dB within 0.3 s near 1.1 s.
2. The voice detector calls that residue speech. With automatic sensitivity
   its decision replaced the level comparison completely, so the gate stayed
   open, and automatic volume learned from those blocks and climbed from 1x to
   its 16x (+24 dB) maximum within half a second. Its gain only ever goes down
   when it learns from a louder block, so it then stayed there.

### What this branch changes (voice_gate.rs)

The gate keeps the quietest level of the last 2 s (minimum statistics, no audio
held). With automatic sensitivity, a block opens the gate only if the detector
says speech *and* it stands at least 6 dB above that floor. A steady fan now
shuts the gate once the floor has caught up: test
`a_detector_alone_cannot_hold_the_gate_open_on_a_steady_noise_floor` had the
gate open in 300 of 300 fan blocks before and in at most the first 25 after. On
`silence_floor` the gate is shut from 3.9 s on. Speech is not cut: on every
speech row of the bench 98-100 % of speech blocks still pass, and a speaker
talking from the first block of a call is never measured against themselves
(test `the_floor_never_shuts_out_speech_in_a_quiet_room_or_at_the_start`). The
2 s memory is deliberate: a shorter one closed on the rising fan sooner, but
would also cut a long vowel after half a second. The manual threshold is
unchanged.

That alone is not enough. While RNNoise lets the fan rise (1-4 s), the 2 s
floor lags behind it, and automatic volume has already learned +24 dB, so the
row as a whole still comes out at +13.3 dB.

### What automatic volume needs (media.rs 3250-3600, Worker B's lines, not changed here)

Automatic volume and the automatic-sensitivity code sit entirely inside the
lines Worker B owns. This branch leaves them untouched. The change is ready as
`worker-c-agc-media-3250-3600.patch` in the shared notes folder (it applies to
587b8b4 with `git apply`) and has three parts:

1. Learn only from blocks that are speech-like:
   `self.gate.speech_like_level(rms)`. That means clear of the 2 s floor and
   8 dB above the quietest level of the last 0.3 s. Speech rises and falls
   with every syllable, a fan does not. This is stricter than the gate on
   purpose: not learning from a long vowel costs nothing.
2. Rise at most 15 dB per second (`AGC_RISE_DB_PER_SECOND`). This is what
   stops the jump at 1.1 s from being lifted at once. Without it the fan is at
   +12.4 dB, at 6 dB/s it is at -9.4 dB, but 6 dB/s breaks the existing test
   `automatic_volume_does_not_pump_when_detection_drops_out_mid_word` (it
   expects +12 dB within a second). 15 dB/s passes it.
3. Never lift the 2 s floor above -50 dBFS (`AGC_NOISE_CEILING`).

`gate.speech_like_level` and `gate.noise_floor` are already in `voice_gate.rs`
on this branch, so the media.rs change is only those lines. The test
`agc_after_suppression_does_not_raise_the_noise_floor_between_words` is
committed but ignored until that change lands. It failed on 587b8b4 (fan
+17.1 dB against speech +0.9 dB), fails with this branch alone (+13.3 dB) and
passes with the patch (-7.3 dB against speech +0.7 dB).

One limit: in Continuous and Push-to-Talk modes the gate is rebuilt every block
(`media.rs` around line 3554), so its floor never learns there, and only the
rise limit protects automatic volume in those modes.

## Bench: `silence_floor` and the default chain

Release build, same machine and corpus as `docs/audio-bench-baseline-20261028.md`.
Residual noise in pauses, in dB against the microphone signal.

| default chain (`rnnoise+enhanced`) | before (587b8b4) | this branch | this branch + the media.rs patch |
| --- | ---: | ---: | ---: |
| `silence_floor` | 17.1 | 13.3 | -7.3 |
| `keyboard_between_words` | -10.7 | -10.7 | -10.7 |
| `keyboard_during_speech` | -7.0 | -7.0 | -8.4 |
| `mouse_clicks` | -9.5 | -9.5 | -9.5 |
| `fan_plus_mouse` | -17.9 | -17.9 | -18.5 |
| `music_bed` | 0.5 | 0.5 | -1.8 |
| `second_talker` | -0.1 | -0.1 | -0.3 |
| speech attenuation, `keyboard_between_words` | -0.9 | -0.9 | -0.7 |
| measured chain delay (ms) | 9.96 | 9.96 | 9.96 |

The full tables are in the shared notes folder: `worker-c-bench-before.md`,
`worker-c-bench-after.md` and `worker-c-bench-with-agc-patch.md`.

## Acceptance

- Default send budget ≤ 35 ms, capture callback to packet: **pass at 22.5 ms**
  for steady speech on a 48 kHz, 10 ms device, and **32.5 ms** for the first
  packet of a talkspurt. On this PC's 7 ms Scarlett Solo: steady speech
  **passes at 28.5 ms worst**, the first packet of a talkspurt **fails at
  38.5 ms worst** (35.5 ms mean) because of the period misalignment above.
- Automatic volume, noise-only gain within 3 dB of speech gain: **fail on this
  branch alone** (fan +13.3 dB against speech +0.9 dB), **pass with the
  media.rs patch** (-7.3 dB against +0.7 dB). Before: fan +17.1 dB.
