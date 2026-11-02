# Optional Sonora microphone volume

In Voice settings, enable **Keep microphone volume even**, then choose
**Sonora · Experimental**. **Relay · Standard** remains the default, including
for existing profiles. The selection is saved as `automatic_volume_model` in
audio preferences. Turning automatic volume off bypasses both controllers.
Use the existing microphone preview to compare the choices before a call.

Sonora 0.2.0 runs only its adaptive digital AGC2 and limiter. Relay still owns
manual gain, echo cancellation, noise suppression, click removal and the voice
gate. Gate detection uses the level before automatic gain. Sonora's internal
RNN VAD guides its gain; it does not replace Relay's transmission decisions.
The operating system's microphone volume is never changed.

The adapter accepts 48 kHz mono in whole 480-sample blocks. Selecting it makes
the capture worker process at least 10 ms at a time even for 2.5/5 ms Opus
packets; packet duration itself is unchanged. The settings/preview delay
budget includes that batching. The gain-only adapter has no added PCM queue
or enabled filter bank; its impulse test checks sample alignment. This is
not a physical microphone-to-speaker latency measurement.

Gain starts at unity, is capped at 24 dB, rises at Sonora's default 6 dB/s,
and uses its default -50 dBFS output noise limit. Relay retains its 0.95 peak
ceiling. When the existing echo-path detector reports far-end speech with
an unknown/present echo path, or an echo-like residue, Sonora is reset and
bypassed at unity to avoid learning/amplifying echo. This conservative policy
can make near-end speech quieter during double talk and needs headset/speaker
listening tests. PTT release and the existing call privacy resets discard its
state. Switching models also clears pending preview/call audio.

## Repeatable comparison

The existing `audio-bench` target includes `rnnoise+enhanced+sonora`, using the
actual `MicrophoneProcessor` and the same defaults as `rnnoise+enhanced`
except for the volume model:

```powershell
audio-bench.exe --chain rnnoise+enhanced --chain rnnoise+enhanced+sonora --write-wav
```

Set `VOICE_TEST_WAV` to a permitted local 48 kHz mono 16-bit speech recording
to compare real speech; without it, the corpus is synthetic. See
[the corpus guide](../crates/relay-client/bench-corpus/README.md). Synthetic correctness tests and
benchmarks do not establish perceptual improvement. No new recording is made
by these tests or by the benchmark.

Sources: [Sonora](https://github.com/dignifiedquire/sonora),
[pinned API](https://docs.rs/sonora/0.2.0/sonora/), and
[adaptive digital parameters](https://docs.rs/sonora/0.2.0/sonora/config/struct.AdaptiveDigital.html).
BSD-3-Clause notices for the new dependencies are in `THIRD_PARTY_NOTICES.txt`.
