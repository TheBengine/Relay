# Silero comparison and adaptive receive timing

## Try it

In Voice settings, under **Advanced**, enable **Compare speech detection**.
Start the existing microphone test, or compare during an unmuted call. The
Silero reading shows estimated speech probability (50% display threshold).
The current voice-activity controls and indicator still determine transmission.
The switch is previewed live in a call; Save Changes keeps it, while Reset or
closing the settings restores the applied choice. It is off by default,
including for existing profiles (`compare_silero_vad = false`).

This is passive analysis, not a new gate or a noise filter. It never changes
PCM, transmission decisions, Opus duration, capture batching or the send-delay
budget. The existing microphone path supplies audio after echo/noise/click
processing and before automatic volume adjustment. A separate worker converts
an analysis copy from 48 kHz to 16 kHz with Relay's band-limited resampler.
Silero uses 512 new samples (32 ms), 64 context samples and recurrent state.
Its reading is consequently later than the audio it describes. It is not
evidence of a measured microphone-to-speaker delay or of better speech gating.

Analysis uses four queued 10 ms input blocks and one output reading, all with
nonblocking handoff. Dropped/expired input and capture gaps over 250 ms discard
the partial window, resampler history and recurrent state. PTT release,
disabling analysis, and the existing call privacy resets discard the detector.
Stale readings expire after 250 ms. Model loading and inference run off the
audio worker. Failure only disables the reading; it cannot stop the call.
No recordings or audio files are created by this setting.

## Pinned model

The MIT-licensed model is from [Silero v6.2.2](https://github.com/snakers4/silero-vad/tree/60b7ffa243625ebdc1070275a29f18c87843786a).
The original ONNX, full license, hashes and original-runtime reference outputs
are in `vendor/models/silero`. Relay embeds `silero_vad_16k_fixed.onnx`, a
fixed-shape specialization for one 576-sample input and state `[2,1,128]`.
This eliminates shape-dependent ONNX branches that tract 0.23.9 cannot type.
It does not retrain the model or replace its weights.

`vendor/models/silero/prepare.py` reproduces the artifact with pinned development
tools and compares probability **and recurrent state** against the original
using ONNX Runtime over 500 continuous generated windows. The Rust tests then
compare tract's streaming probabilities with independent original-model values,
including fresh state and silence. Python and ONNX Runtime are not client
dependencies. See also [Silero's streaming wrapper](https://github.com/snakers4/silero-vad/blob/60b7ffa243625ebdc1070275a29f18c87843786a/src/silero_vad/utils_vad.py).

## Receive timing

Automatic receive latency now uses relative transit delay and the recent 95th
percentile instead of an arrival-order variation average. The policy subtracts
the minimum transit time in a two-second window, so reordered packets do not
replace the newest source-clock baseline. History is capped at 800 samples
(two seconds of 2.5 ms packets). Late packets contribute measured delays;
duplicates and obsolete timestamps do not alter the distribution. Clock gaps
and explicit talkspurt restarts reset the estimator. Previously measured delays
are not recalculated against a newly faster path.

The existing 10–80 ms target, maximum 10 ms rise per second, gradual recovery
after five seconds of better arrivals, quiet-only queue draining, Opus PLC/FEC
and manual latency choices remain in effect. This is Relay's own Rust policy,
inspired by WebRTC's [PacketArrivalHistory](https://github.com/webrtc-mirror/webrtc/blob/main/modules/audio_coding/neteq/packet_arrival_history.cc)
and [UnderrunOptimizer](https://github.com/webrtc-mirror/webrtc/blob/main/modules/audio_coding/neteq/underrun_optimizer.cc).
It is not the complete NetEq engine and does not introduce audio stretching.

## Verification

Affected native test filters: `silero_vad::tests`, `silero_tests`,
`arrival_delay::tests`, `adaptive_audio::tests`, `voice_jitter::tests`, plus the
existing Sonora/capture/preview regressions. They cover unchanged send PCM and
gate decisions, opt-in serialization, resets, reordered actual Opus packets,
packet loss, faster routes, short packets, bounded storage and stalled playback.

Two explicitly invoked report tests provide reproducible local evidence:
`report_silero_analysis_cost` and
`report_real_opus_playout_under_uneven_arrival_and_loss` (run with
`--ignored --nocapture`). The former reports native **test-profile** processing
cost on generated audio. The latter simulates fixed and automatic buffers with
actual Opus. Neither establishes real-microphone detection accuracy, listening
quality, physical latency or WAN acceptance; see `hardware-acceptance.md` and
`friends-two-device-acceptance.md`.
