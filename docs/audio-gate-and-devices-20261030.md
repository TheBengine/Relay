# Voice gate with the suppressor's speech model, push-to-talk release, microphone following Windows, 2026-10-30

Task K of `docs/superpowers/plans/2026-10-30-audio-discord-parity-round3.md`.
Three changes on the send side, each with a test, and the figures behind them.
Every number below comes from a test or the gate bench in this branch; where
something was not measured, it says so.

## What the user gets

- **Voice activation no longer stays open on a fan or keys.** The gate now
  also asks the noise suppressor whether a block is speech. On the bench,
  with the user's recorded voice, the gate was open in 56.9 % of the pauses
  under a fan at 5 dB and 40.5 % of the pauses between words with keys; it
  is now 13.8 % and 16.4 %. With Studio, music at 12 dB no longer holds the
  gate open at all (6.9 % → 0 %). No word start is cut by the gate, before or
  after, on any row.
- **Push-to-talk keeps the microphone open after the key comes up**:
  Settings → Voice → Input Mode → Push to Talk → Shortcut, slider "Release
  delay", 0–2000 ms, default 200 ms. "Keep the microphone open after
  releasing push-to-talk, so the end of your last word is not cut off."
- **The microphone follows Windows.** That part already shipped (commit
  d5f5f26: `device_watch.rs` with `IMMNotificationClient`, `device_follow.rs`,
  `MediaEngine::follow_device_change`). This branch adds the fake-notification
  test the plan asks for and measures the audio gap on this PC: 78 ms median,
  82 ms worst.

Nothing adds delay: the gate still acts on the current block, and the
suppressor's verdict is a ratio of two block energies the chain already has.

## 1. The gate asks the suppressor

### How it works

`crates/relay-client/src/voice_gate.rs`, called from
`MicrophoneProcessor::process` (`media.rs`):

- **`SuppressionRatio`**: per 10 ms block, the energy the noise suppressor
  returned over the energy of the raw block that output belongs to. The
  suppressor returns its output late (RNNoise 1 block, Studio 6 blocks,
  `Suppressor::delay_samples`), so the raw energies of the last 8 blocks are
  remembered as numbers; no audio is held. It is measured right after the
  suppressor, **before** click suppression, because that stage also thins a
  voice (see "Found on the way").
- **`SpeechModel`** = that ratio plus the suppressor's own speech probability
  when it has one (RNNoise's; Studio's is its ratio).
- **`NoiseGate::detect(level, threshold, detector, model)`**:
  - open only when the existing decision (level against the threshold, or
    WebRTC's detector above the 2 s noise floor) **and** the model agree:
    ratio ≥ −20 dB (`KEPT_TO_OPEN` 0.01) and speech probability ≥ 0.05
    (`VOICE_TO_OPEN`);
  - once open, stay open while the ratio is ≥ −25 dB (`KEPT_TO_STAY` 0.003),
    whatever the probability, so a soft syllable is not cut;
  - **close on the ratio alone near the floor**: a block the suppressor
    removed (ratio < −25 dB) whose level is within 6 dB of the 2 s floor ends
    the hold at once; the release fade still runs.
  - The hold stays the user's own setting (`activation_hold_ms`, default
    200 ms, inside the plan's 150–250 ms).
- Without noise suppression there is no model and the gate is exactly the
  old one (test `the_input_sensitivity_threshold_keeps_its_meaning_with_the_speech_model`).

**The Input Sensitivity slider keeps its meaning**: the manual threshold is
still the level a block must reach, compared with the same RMS; the model can
only veto a block, never open one the slider would not. In the bench's manual
rows (threshold −40 dBFS) the word starts and speech figures are identical
before and after; only music time-open falls (72.4 % → 56.0 %).

Why these thresholds (from the traces, recorded voice): a voice under the fan
at 5 dB keeps about −6 dB through RNNoise, a quiet word start under it −15 to
−20 dB, the fan alone −40 dB or less. RNNoise's speech probability reaches
0.05 within 30 ms at 10 of 12 clean word starts and 0.02 at all of them; in
key pauses it reaches 0.02 in 12 % of blocks and 0.05 in 8 %. In earlier runs 0.1 closed the
key pauses completely (0 %) but opened two word starts under the fan late; 0.02
left 36 % of key pauses open. 0.05 is the compromise.

### Bench

`voice_gate::bench::gate_bench` (ignored test; command at the end). It runs
the app's own `MicrophoneProcessor` with default settings on the bench rows
with `VOICE_TEST_WAV` (Worker E's `voice3-prepared.wav`, 29.9 s), once with
the model switched off (= the shipped gate) and once with it. Studio runs
in process, model built before the first block (deterministic).

Definitions:

- **Openings on noise**: the gate goes from closed to open with no speech in
  the clean voice from 30 ms before to 10 ms after.
- **Open in pauses**: share of blocks at least 400 ms from any speech in which
  the gate is open. This is what a listener notices as "the gate is open on
  my fan"; a gate stuck open has few openings and a high share.
- **Word start**: three speech blocks in a row (within 45 dB of the voice's
  loudest block) after 300 ms without such a run, reaching within 30 dB of
  the loudest block within 100 ms: 11 in the recording.
- **Chopped by the gate**: in the first 50 ms of a word start, the gate sent
  less than −6 dB of what it received (the speech-start history the call
  sends before an opening is counted).
- **Lost before the gate**: in those 50 ms, the suppressor and click stage
  had already left the voice more than 20 dB below itself, so there was
  nothing for the gate to keep. Counted separately so the gate is not blamed
  for it, and so it stays visible.

Enhanced (the default: RNNoise, click suppression, automatic sensitivity):

| row | openings on noise per 30 s, before → after | open in pauses | chopped by the gate | lost before the gate | speech energy kept by the gate |
| --- | ---: | ---: | ---: | ---: | ---: |
| `keyboard_between_words` | 1 → 1 | 40.5 % → **16.4 %** | 0 → 0 | 0 → 0 | −0.00 → −0.00 dB |
| `fan_plus_mouse` (5 dB) | 1 → 2 | 56.9 % → **13.8 %** | 0 → 0 | 7 → 7 | −0.01 → −0.03 dB |
| `music_bed_12` | 2 → 7 | 100.0 % → 74.1 % | 0 → 0 | 0 → 0 | 0.00 → −0.00 dB |
| `clean_voice` | 0 → 0 | 0 % → 0 % | 0 → 0 | 1 → 1 | −0.00 → −0.00 dB |

Studio (DPDFNet, automatic sensitivity):

| row | openings per 30 s | open in pauses | chopped by the gate | lost before the gate | speech kept |
| --- | ---: | ---: | ---: | ---: | ---: |
| `keyboard_between_words` | 1 → **0** | 0 % → 0 % | 0 → 0 | 0 → 0 | −0.00 → −0.00 dB |
| `fan_plus_mouse` | 1 → **0** | 0 % → 0 % | 0 → 0 | 0 → 0 | −0.00 → −0.07 dB |
| `music_bed_12` | 1 → 1 | 6.9 % → **0 %** | 0 → 0 | 0 → 0 | −0.00 → −0.00 dB |
| `clean_voice` | 0 → 0 | 0 % → 0 % | 0 → 0 | 1 → 1 | −0.00 → −0.00 dB |

Against the plan's targets:

- **Chopped word starts: 0 on every row, met** (it was also 0 before under
  this definition).
- **Openings on the fan, 0 per 30 s**: met with Studio (1 → 0). Not met with
  Enhanced (1 → 2), but the time the gate is open on the fan fell from 57 %
  to 14 %. The extra opening is a short re-opening: the hold now ends early
  on the fan, so a later fan burst that the detector and RNNoise both pass
  reopens it briefly.
- **Openings on music at 12 dB, 0 per 30 s**: not met with Enhanced
  (2 → 7; open time 100 % → 74 %). RNNoise leaves music in (its ratio is
  near 1) and its speech probability calls music speech in 51 % of the music
  pauses, so no RNNoise signal can tell music from a voice. Music is what
  Studio is for. With Studio the one opening left is in the first 0.4 s of
  the call, before the 2 s noise floor exists, and time-open is 0 %.

### Found on the way (for Worker J / the orchestrator)

The energy trace (`GATE_TRACE_DIR`) shows where word starts go before the gate:

- **Enhanced's click suppression thins word starts.** On the clean voice it
  takes about −1.2 dB of the first 50 ms of word starts on average, but one
  quiet start (clean block 1081) from −45 dBFS to about −80 dBFS: 37 dB
  below the voice. The shipped gate opened 90 ms into that word anyway, on
  silence.
- **Under the fan at 5 dB, 7 of 11 word starts are lost before the gate**:
  RNNoise keeps them at −40 to −45 dB (raw fan −22 dB), and click suppression
  then takes 25–40 dB more (−5.2 dB on average over the first 50 ms of all
  starts). In the middle of words at that SNR the click stage also removes
  30–40 dB of voice in some blocks.
- **Studio** removes the same quiet start (block 1081) for its first 30–40 ms
  (−32 dB).

None of this is in my files (`click_suppression.rs`, `dpdfnet.rs`), so it is
reported, not changed.

### CPU and delay

- **CPU**: the gate bench times the whole `MicrophoneProcessor::process`
  per 10 ms block (release, one thread): Enhanced 0.031–0.035 ms before and
  after on every row. The difference is below the run-to-run spread
  (±0.003 ms), against the plan's 0.5 ms budget. The addition is two
  480-sample energy sums and a few comparisons per block.
- **Delay**: none added. The ratio uses energies of blocks already
  processed; the gate decides and fades on the current block, as before
  (`default_attack_fades_the_current_block_in_without_lookahead` still passes).

## 2. Push-to-talk release delay

- Setting `AudioSettings::push_to_talk_release_ms` (last field of the struct),
  0–2000 ms (`voice_gate::MAX_PTT_RELEASE_MS`), default 200 ms, bounded by
  `normalized()`. Older settings files load with 200 ms (test
  `push_to_talk_release_delay_defaults_to_200_ms_for_older_files_and_is_bounded`).
- Slider "Release delay" at the end of the push-to-talk Shortcut section in
  `settings_pages.rs`.
- Applied where the key is read: `poll_push_to_talk` (`media.rs`) stores
  `ptt_pressed` = key down or released less than the delay ago
  (`voice_gate::PushToTalkRelease`). Every check of `ptt_pressed` (the
  send chain, `capture_send_allowed`, already-encoded packets, the gateway
  path and the preview) therefore keeps the tail without its own change.
  Pressing again inside the delay continues the same talkspurt; the privacy
  reset (suppressor history, encoder) happens when the delay ends.
- Tests: `push_to_talk_release_holds_for_the_delay_and_never_beyond_two_seconds`
  (`voice_gate.rs`) and `push_to_talk_release_delay_keeps_sending_the_last_word_then_closes`
  (`media.rs`: blocks are sent for 200 ms after release, then closed; 0 ms
  closes with the key; a new value applies to a running call).
- `poll_push_to_talk` lies outside the send-chain region the plan gave me
  (it is the free function after the worker loop, owned by no worker); the
  change there is three lines plus `push_to_talk_open`. Settings and
  `AudioControls` gained the `ptt_release_ms` / `ptt_release` fields next to
  `ptt_key`.

## 3. The microphone follows Windows

### What is there

Commit d5f5f26 already implements the plan's step:

- `device_watch.rs`: `IMMNotificationClient` on its own MTA thread; callbacks
  only raise flags (default input/output changed, device added/removed/state
  changed) and ask the UI for a repaint.
- `app.rs` `device_follow_tick`: on a flag, lists the devices on a thread of
  its own, ignores "default changed" notifications whose name did not
  change, and calls `device_follow::decide`.
- `device_follow.rs`: "Default" follows the Windows default; a named device
  that is unplugged falls back to the default and returns when it is back;
  the saved choice is never changed; a notice tells the user.
- `MediaEngine::follow_device_change` → the audio owner reopens only the
  device streams (`change_voice_devices`): the call, socket, keys, mute,
  deafen and per-person volumes stay.

The plan put the notification client in `windows_audio.rs`. It lives in
`device_watch.rs`, and I left it there.

### Fake-notification test

`media::tests::fake_windows_notifications_reopen_the_microphone_within_500_ms_and_keep_the_call`
feeds the same `Change` values `DeviceWatch` produces into the app's decision
and the engine, with the fake audio backend:

- "Default" selected, Windows' default microphone moves → the devices reopen
  on the default; the test requires under 500 ms (the audio owner
  polls every 20 ms); a repeated notification with the same default does
  nothing;
- a named microphone unplugged → reopens on Default with the "was unplugged"
  notice; plugged back → returns to it with "is back"; an unrelated device
  coming or going reopens nothing;
- throughout: one call start, the same stop flag and transport, the call
  stays active.

### Gap on this PC

`windows_audio::tests::actual_device_follow_gap_on_the_default_microphone`
(ignored, real devices): opens the microphone and speakers, then ten times
lists the devices as the app does, closes both streams and opens them again
as `change_voice_devices` does, and times the last old and first new
microphone samples. Release build, 2026-11-06:

| microphone / speakers | list devices, median / worst | close and reopen to first new samples | **audio gap**, median / worst |
| --- | ---: | ---: | ---: |
| Windows default: Microphone (Virtual Desktop Audio) / Speakers (Scarlett Solo USB) | 91 / 93 ms | 68 / 68 ms | **78 / 82 ms** |
| Microphone (Scarlett Solo USB) / Speakers (Scarlett Solo USB) | 93 / 97 ms | 72 / 73 ms | **78 / 79 ms** |

An earlier run on the Virtual Desktop microphone had one round of 531 ms
(median 40 ms); 20 later rounds stayed under 82 ms.

From a notification to the new microphone, the app adds to the reopen: the
device listing (about 90 ms; the old microphone keeps running meanwhile when
the default only moved), at most one 20 ms poll of the audio owner and two UI
frames. That is about 200 ms when the old device was unplugged (nothing to
hear meanwhile), within the plan's 500 ms. The voice worker, socket and
packet sequence stay, so the receiver hears a short concealed gap, not a
reconnect.

**Not measured: the physical unplug.** The EMEET C960's microphone is not an
audio endpoint on this PC. Windows lists only the Scarlett Solo and virtual
microphones as active, as Worker C also found on 2026-10-28, so unplugging
the camera changes no microphone. Enabling it is a system sound setting I may
not change, and plugging a USB device needs the user. To measure it: enable
the EMEET microphone in Sound settings, make it the default, start a call,
unplug and replug it, and read the device-change notice and the call's
"concealed" counter. The fake-notification test covers the logic.

## Files

- `voice_gate.rs`: `SuppressionRatio`, `SpeechModel`, the model input to
  `detect`, early end of the hold, `PushToTalkRelease`, `gain_power` for the
  bench, the gate bench (`mod bench`), tests.
- `media.rs` send chain: ratio and probability per block passed to the gate;
  test-only trace fields and switch (`gate_without_ratio`, `trace_energy`);
  `ptt_release_ms` / `ptt_release` in `AudioControls`; `push_to_talk_open`;
  tests (release delay, fake notifications).
- `audio_settings.rs`: `push_to_talk_release_ms` at the end, with its test.
- `settings_pages.rs`: the slider at the end of the push-to-talk section.
- `windows_audio.rs`: the gap measurement (tests only).
- Not touched: `dpdfnet.rs`, `click_suppression.rs`, `noise_suppressor.rs`,
  the encoder region, the receive mix, `Cargo.lock`.

## Rerun

With the build environment of the Round 2 plan:

```
set VOICE_TEST_WAV=C:\Important\Projects\2026-10-03\can\work\audio-worker-e-20261029\voice\voice3-prepared.wav
rem optional: per-block CSV traces (gain, ratio, energies, speech probability)
set GATE_TRACE_DIR=<an empty folder>
cargo test -p relay-client --release -j 8 --bin relay voice_gate::bench::gate_bench -- --ignored --nocapture --test-threads 1

cargo test -p relay-client --release -j 8 --bin relay actual_device_follow_gap -- --ignored --nocapture --test-threads 1
rem a named device instead of the default:
set RELAY_MEASURE_INPUT=Microphone (Scarlett Solo USB)
set RELAY_MEASURE_OUTPUT=Speakers (Scarlett Solo USB)
```

## Not done

- Enhanced cannot keep the gate shut on music; that needs Studio (or a music
  detector, not attempted).
- Physical unplug/replug measurement (above).
- The word starts lost in click suppression and under a loud fan (above) are
  for Worker J's files.
