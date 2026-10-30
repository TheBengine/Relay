# DeepFilterNet as an opt-in suppressor: decision, 2026-10-29

Follow-up to `docs/audio-suppressor-comparison-20261028.md` (Worker B), which
asked for DeepFilterNet 3 to be judged again on a recorded voice before
anything is decided.

## Decision

**No. DeepFilterNet is not linked into the client.** RNNoise with click
suppression stays the only suppressor, and no setting is added. Nothing in the
client changed; this document is the whole commit.

On a recorded voice, DeepFilterNet 3 low latency (LL) does what nothing else
does: it removes background music in the pauses (-41.6 dB, target -20 dB;
the shipped chain -0.8 dB). But while the person talks over the music it
changes the voice much more than the shipped chain does, and the user, after
listening to both, judged neither good enough:

| `music_bed`, recorded voice | shipped (`rnnoise+enhanced`) | DeepFilterNet-LL | DeepFilterNet-LL, then RNNoise + clicks | unprocessed input |
| --- | ---: | ---: | ---: | ---: |
| residual noise in pauses (dB) | -0.8 | **-41.6** | -46.4 | 0.0 |
| noise leak during speech (dB) | 0.1 | 7.8 | 7.6 | 0.0 |
| speech segSNR (dB) | 3.4 | 0.0 | 0.0 | 3.6 |
| LSD (dB) | 4.35 | 9.64 | 10.10 | 4.36 |
| speech attenuation (dB) | -0.2 | 0.4 | 0.7 | -1.2 |
| added delay (ms) | 10 | 10 | 20 | 0 |

The shipped chain leaves the voice under music as it came in (LSD 4.35
against the input's 4.36): it does nothing to the music. DeepFilterNet-LL
more than doubles the spectral distance of the voice from the clean
recording, and its output for the noisy input differs from its output for the
clean input by more than the music itself (+7.8 dB leak). That is the voice
changing whenever music plays, which is what the user heard.

The rule for this task was to link it only if the numbers and the user's ear
agree that it wins on music without ruining the voice. The numbers show the
voice damaged under music, and the ear agreed: none of the versions was good
enough.

Worker B's other findings hold on a real voice:

- **Clicks pass through.** `mouse_clicks` residual in pauses +0.4 dB (shipped
  -16.7 dB); `keyboard_during_speech` leak +11.7 dB (shipped -0.1 dB). Running
  RNNoise with click suppression after it recovers only part of that (-2.5 dB
  on `mouse_clicks`) and doubles the delay to 20 ms, the whole budget.
- **Fan:** -23.7 dB against the shipped -27.0 dB, with the voice changed more
  (LSD 11.33 against 5.33).
- **Where it is better:** when the noise is only in the pauses, the voice
  comes out cleaner than through RNNoise (`keyboard_between_words` LSD 1.47
  against 2.16, segSNR 12.1 against 9.0 dB), and it softens the second talker
  (-7.9 dB against +0.2 dB).
- **Cost** is as estimated: 10 ms, 0.35-0.55 ms CPU per 10 ms block (p99
  1.3 ms), +36 MB model, about 60 more crates with `tract-*` pinned to
  `=0.21.4`.

## The recorded voice

- **What:** 30 s (29.94 s) of the user talking Danish at a normal pace with
  pauses between sentences, recorded by the user to a 44.1 kHz mono 16-bit
  WAV and resampled to 48 kHz with ffmpeg. Loud syllables around -18 dBFS per
  10 ms frame. The recording is not in the repository.
- **Prepared for the bench** (script kept with the hand-over notes, not
  committed): the 30th loudest 10 ms frame is the speech reference; frames
  more than 6 dB above it would be turned down (none were); frames more than
  40 dB below it, with 20 ms hold after speech and 10 ms before it, become
  digital silence, with 5 ms ramps. That made 6.9 s of the 30 s silent. The
  synthetic talker's pauses are digital silence too, and the bench relies on
  it: the keyboard row places its keys where the voice is below 1e-4 RMS, and
  the pause metrics count frames 60 dB below the loudest.
- **In the bench** the recording replaces the synthetic talker in every row
  and sets every row's length (30 s instead of 10 s). It is scaled to the
  synthetic talker's active level, so gain, voice gate and SNRs start where
  they did before. The second talker, keys, mouse, fan and music are the same
  generators and seeds, run for 30 s.
- **DeepFilterNet leaves this voice alone.** On the clean recording alone,
  both models' output on speech frames is within 0.1 dB of the input (the
  synthetic talker loses 1.0-1.2 dB). So the damage above comes from the music
  and the fan, not from the voice.

### Two recordings that were not used

The first two 30 s recordings (ffmpeg from the Scarlett Solo, left channel)
turned out to hold no direct speech: the user had not started talking. They
looked like speech by pitch (100-120 Hz) but peaked around -40 dBFS. On them
DeepFilterNet removed 7-16 dB (LL) and 25-36 dB (standard) of the "clean"
signal at every level tried, and every chain's LSD went to 10-14 dB. That is
DeepFilterNet treating distant background speech as noise, which is its job;
it says nothing about a near voice. If a later run shows DeepFilterNet cutting
a clean voice by more than about 2 dB, check the recording before the model.

## How it was measured

1. Worker A's bench (`audio-bench`, feature `audio-bench`), release build,
   with a small patch that is **not committed** (on a "no" this task commits
   only this document): `corpus()` reads `VOICE_TEST_WAV` (48 kHz mono
   16-bit) as the talker, scaled to the synthetic talker's active level, and
   the rows follow its length; `second_talker` takes its length from the row;
   and the replay route gains a `deepfilternet-ll-then-rnnoise+enhanced
   (replayed)` chain (the replayed DeepFilterNet-LL output through RNNoise
   with click suppression at Normal, declared delay 20 ms). Without
   `VOICE_TEST_WAV` the corpus is bit-identical to before.
2. `audio-bench --write-wav --chain none` writes the corpus WAVs.
3. Worker B's outside tool (`dfprobe`: libDF `DfTract::process` per
   480-sample block, git rev `d375b2d8`, `tract-*` `=0.21.4`; its source was
   still with Worker B's files and was rebuilt unchanged) runs the standard
   model and the LL model (`DeepFilterNet3_ll_onnx.tar.gz` from the same
   checkout) over them.
4. `audio-bench --write-wav` with `RELAY_REPLAY_DFN` / `RELAY_REPLAY_DFN_LL`
   pointing at the tool's two output folders.

The same pipeline on the synthetic talker reproduces Worker B's
DeepFilterNet-LL rows to within 0.2 dB in every column except `silence_floor`
(LL `music_bed`: -61.4 dB and LSD 9.12 dB, identical), so the route itself is
unchanged. The other differences from Worker B's table (`silence_floor` in
every chain, the shipped chain's `music_bed` -1.8 instead of +0.5 dB, the
standard model's `music_bed` speech attenuation 5.2 instead of 3.8 dB) come
from 255dad5, which landed after Worker B measured: automatic volume no
longer lifts a fan the voice detector calls speech, and the replayed rows run
the app's automatic volume too.

The user listened to `music_bed` as clean voice, voice with music, shipped,
DeepFilterNet-LL, and DeepFilterNet-LL then RNNoise + clicks (outside the
repository, in `audio-worker-e-20261029\lyt`).

## Full table, recorded voice

Run at 2026-10-29T15:02:39Z, release build, one thread, on the user's machine
while it was otherwise in use. Columns as in the comparison doc. The
DeepFilterNet rows are replayed; their CPU column covers only the rest of the
chain.

| chain | item | SNR in (dB) | residual noise in pauses (dB) | noise leak during speech (dB) | speech segSNR (dB) | LSD (dB) | speech attenuation (dB) | CPU (ms / 10 ms block) |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| none | keyboard_between_words | 10.0 | 0.0 | n/a | 35.0 | 0.00 | 0.0 | 0.000 |
| none | keyboard_during_speech | 10.0 | 0.0 | 0.0 | 25.9 | 3.17 | -0.1 | 0.000 |
| none | mouse_clicks | 15.0 | 0.0 | 0.0 | 32.5 | 0.86 | -0.0 | 0.000 |
| none | fan_plus_mouse | 5.0 | 0.0 | 0.0 | 3.0 | 7.82 | -1.2 | 0.000 |
| none | music_bed | 5.0 | 0.0 | 0.0 | 3.6 | 4.36 | -1.2 | 0.000 |
| none | second_talker | 0.0 | 0.0 | 0.0 | 5.0 | 2.20 | -1.8 | 0.000 |
| none | silence_floor | n/a | 0.0 | n/a | n/a | n/a | n/a | 0.000 |
| rnnoise | keyboard_between_words | 10.0 | -0.1 | n/a | 9.1 | 1.67 | -0.6 | 0.028 |
| rnnoise | keyboard_during_speech | 10.0 | -2.3 | -0.6 | 8.5 | 3.25 | -0.4 | 0.030 |
| rnnoise | mouse_clicks | 15.0 | -0.5 | 8.0 | 8.9 | 2.12 | -0.6 | 0.031 |
| rnnoise | fan_plus_mouse | 5.0 | -21.9 | -1.9 | 4.7 | 4.69 | 0.0 | 0.030 |
| rnnoise | music_bed | 5.0 | -0.8 | 0.1 | 3.5 | 4.15 | -0.2 | 0.031 |
| rnnoise | second_talker | 0.0 | 0.3 | -0.5 | 4.7 | 2.61 | -0.5 | 0.031 |
| rnnoise | silence_floor | n/a | 7.0 | n/a | n/a | n/a | n/a | 0.031 |
| rnnoise+enhanced | keyboard_between_words | 10.0 | -24.3 | n/a | 9.0 | 2.16 | -0.3 | 0.033 |
| rnnoise+enhanced | keyboard_during_speech | 10.0 | -16.6 | -0.1 | 8.4 | 3.50 | -0.1 | 0.033 |
| rnnoise+enhanced | mouse_clicks | 15.0 | -16.7 | 7.5 | 8.7 | 2.64 | -0.2 | 0.033 |
| rnnoise+enhanced | fan_plus_mouse | 5.0 | -27.0 | -1.5 | 4.4 | 5.33 | 0.3 | 0.037 |
| rnnoise+enhanced | music_bed | 5.0 | -0.8 | 0.1 | 3.4 | 4.35 | -0.2 | 0.034 |
| rnnoise+enhanced | second_talker | 0.0 | 0.2 | -0.4 | 4.3 | 3.22 | -0.4 | 0.036 |
| rnnoise+enhanced | silence_floor | n/a | -11.3 | n/a | n/a | n/a | n/a | 0.035 |
| rnnoise+enhanced-high | keyboard_between_words | 10.0 | -29.6 | n/a | 8.5 | 2.70 | 0.2 | 0.033 |
| rnnoise+enhanced-high | keyboard_during_speech | 10.0 | -20.9 | 2.6 | 8.0 | 3.82 | 0.3 | 0.032 |
| rnnoise+enhanced-high | mouse_clicks | 15.0 | -20.4 | 10.2 | 8.1 | 3.23 | 0.3 | 0.033 |
| rnnoise+enhanced-high | fan_plus_mouse | 5.0 | -28.6 | -2.0 | 4.2 | 5.67 | 0.7 | 0.035 |
| rnnoise+enhanced-high | music_bed | 5.0 | -0.8 | 0.0 | 3.2 | 4.59 | 0.0 | 0.037 |
| rnnoise+enhanced-high | second_talker | 0.0 | 0.3 | -0.7 | 4.0 | 3.57 | -0.2 | 0.033 |
| rnnoise+enhanced-high | silence_floor | n/a | -11.1 | n/a | n/a | n/a | n/a | 0.035 |
| webrtc-ns | keyboard_between_words | 10.0 | 5.0 | n/a | 0.4 | 3.71 | -0.6 | 0.017 |
| webrtc-ns | keyboard_during_speech | 10.0 | 3.4 | 1.1 | 0.3 | 4.46 | -0.5 | 0.018 |
| webrtc-ns | mouse_clicks | 15.0 | 6.2 | 6.9 | 0.4 | 3.89 | -0.6 | 0.016 |
| webrtc-ns | fan_plus_mouse | 5.0 | -6.6 | -0.9 | 0.5 | 5.18 | 1.8 | 0.019 |
| webrtc-ns | music_bed | 5.0 | -2.9 | 0.1 | -0.1 | 4.78 | 0.8 | 0.016 |
| webrtc-ns | second_talker | 0.0 | 0.1 | -0.5 | -0.2 | 4.78 | -0.5 | 0.020 |
| webrtc-ns | silence_floor | n/a | 0.1 | n/a | n/a | n/a | n/a | 0.019 |
| webrtc-ns-veryhigh | keyboard_between_words | 10.0 | 4.8 | n/a | 0.4 | 3.80 | -0.6 | 0.017 |
| webrtc-ns-veryhigh | keyboard_during_speech | 10.0 | 3.3 | 1.1 | 0.3 | 4.48 | -0.4 | 0.018 |
| webrtc-ns-veryhigh | mouse_clicks | 15.0 | 5.5 | 7.2 | 0.4 | 3.96 | -0.6 | 0.018 |
| webrtc-ns-veryhigh | fan_plus_mouse | 5.0 | -11.4 | 0.1 | 0.5 | 5.84 | 1.7 | 0.016 |
| webrtc-ns-veryhigh | music_bed | 5.0 | -3.0 | 0.8 | -0.1 | 5.05 | 0.8 | 0.016 |
| webrtc-ns-veryhigh | second_talker | 0.0 | -0.0 | -0.5 | -0.2 | 4.80 | -0.4 | 0.018 |
| webrtc-ns-veryhigh | silence_floor | n/a | -8.4 | n/a | n/a | n/a | n/a | 0.017 |
| rnnoise-then-webrtc-ns+enhanced | keyboard_between_words | 10.0 | -28.2 | n/a | 0.2 | 4.77 | -0.2 | 0.055 |
| rnnoise-then-webrtc-ns+enhanced | keyboard_during_speech | 10.0 | -20.2 | -0.6 | 0.2 | 5.23 | -0.1 | 0.049 |
| rnnoise-then-webrtc-ns+enhanced | mouse_clicks | 15.0 | -17.1 | 6.9 | 0.2 | 4.85 | -0.2 | 0.048 |
| rnnoise-then-webrtc-ns+enhanced | fan_plus_mouse | 5.0 | -29.8 | -2.2 | 0.2 | 6.23 | 0.5 | 0.050 |
| rnnoise-then-webrtc-ns+enhanced | music_bed | 5.0 | -0.4 | 0.1 | -0.1 | 5.44 | -0.2 | 0.050 |
| rnnoise-then-webrtc-ns+enhanced | second_talker | 0.0 | -0.3 | -0.4 | -0.2 | 5.68 | -0.4 | 0.050 |
| rnnoise-then-webrtc-ns+enhanced | silence_floor | n/a | -15.0 | n/a | n/a | n/a | n/a | 0.047 |
| rnnoise-then-webrtc-ns-veryhigh+enhanced | keyboard_between_words | 10.0 | -28.7 | n/a | 0.2 | 4.81 | -0.2 | 0.052 |
| rnnoise-then-webrtc-ns-veryhigh+enhanced | keyboard_during_speech | 10.0 | -17.4 | -0.7 | 0.2 | 5.24 | -0.1 | 0.048 |
| rnnoise-then-webrtc-ns-veryhigh+enhanced | mouse_clicks | 15.0 | -18.8 | 6.9 | 0.2 | 4.89 | -0.2 | 0.047 |
| rnnoise-then-webrtc-ns-veryhigh+enhanced | fan_plus_mouse | 5.0 | -30.1 | -2.2 | 0.2 | 6.25 | 0.5 | 0.050 |
| rnnoise-then-webrtc-ns-veryhigh+enhanced | music_bed | 5.0 | -0.8 | 0.3 | -0.1 | 5.56 | -0.2 | 0.051 |
| rnnoise-then-webrtc-ns-veryhigh+enhanced | second_talker | 0.0 | -0.3 | -0.4 | -0.2 | 5.65 | -0.4 | 0.048 |
| rnnoise-then-webrtc-ns-veryhigh+enhanced | silence_floor | n/a | -17.2 | n/a | n/a | n/a | n/a | 0.055 |
| webrtc-ns-then-rnnoise+enhanced | keyboard_between_words | 10.0 | -6.9 | n/a | 0.2 | 4.80 | -0.2 | 0.048 |
| webrtc-ns-then-rnnoise+enhanced | keyboard_during_speech | 10.0 | -15.2 | -1.3 | 0.2 | 5.15 | -0.2 | 0.047 |
| webrtc-ns-then-rnnoise+enhanced | mouse_clicks | 15.0 | -17.7 | 4.6 | 0.2 | 4.94 | -0.2 | 0.048 |
| webrtc-ns-then-rnnoise+enhanced | fan_plus_mouse | 5.0 | -23.7 | -0.9 | 0.4 | 6.65 | 1.0 | 0.050 |
| webrtc-ns-then-rnnoise+enhanced | music_bed | 5.0 | -1.8 | 0.5 | -0.1 | 5.83 | 0.0 | 0.051 |
| webrtc-ns-then-rnnoise+enhanced | second_talker | 0.0 | -0.3 | -0.4 | -0.3 | 5.62 | -0.3 | 0.054 |
| webrtc-ns-then-rnnoise+enhanced | silence_floor | n/a | -27.9 | n/a | n/a | n/a | n/a | 0.055 |
| webrtc-ns-high-then-rnnoise+enhanced | keyboard_between_words | 10.0 | -7.0 | n/a | 0.2 | 4.81 | -0.2 | 0.047 |
| webrtc-ns-high-then-rnnoise+enhanced | keyboard_during_speech | 10.0 | -15.2 | -0.8 | 0.2 | 5.17 | -0.2 | 0.046 |
| webrtc-ns-high-then-rnnoise+enhanced | mouse_clicks | 15.0 | -18.2 | 4.5 | 0.2 | 4.96 | -0.2 | 0.048 |
| webrtc-ns-high-then-rnnoise+enhanced | fan_plus_mouse | 5.0 | -22.4 | -0.2 | 0.4 | 6.97 | 1.2 | 0.049 |
| webrtc-ns-high-then-rnnoise+enhanced | music_bed | 5.0 | -1.5 | 0.9 | -0.1 | 6.06 | 0.1 | 0.049 |
| webrtc-ns-high-then-rnnoise+enhanced | second_talker | 0.0 | -0.4 | -0.4 | -0.2 | 5.63 | -0.3 | 0.049 |
| webrtc-ns-high-then-rnnoise+enhanced | silence_floor | n/a | -23.5 | n/a | n/a | n/a | n/a | 0.048 |
| webrtc-ns-veryhigh-then-rnnoise+enhanced | keyboard_between_words | 10.0 | -3.1 | n/a | 0.2 | 4.87 | -0.2 | 0.047 |
| webrtc-ns-veryhigh-then-rnnoise+enhanced | keyboard_during_speech | 10.0 | -15.2 | -0.8 | 0.2 | 5.19 | -0.2 | 0.046 |
| webrtc-ns-veryhigh-then-rnnoise+enhanced | mouse_clicks | 15.0 | -18.4 | 4.4 | 0.2 | 4.97 | -0.2 | 0.049 |
| webrtc-ns-veryhigh-then-rnnoise+enhanced | fan_plus_mouse | 5.0 | -29.3 | 0.3 | 0.4 | 7.35 | 1.6 | 0.051 |
| webrtc-ns-veryhigh-then-rnnoise+enhanced | music_bed | 5.0 | -1.5 | 1.1 | -0.1 | 6.22 | 0.3 | 0.053 |
| webrtc-ns-veryhigh-then-rnnoise+enhanced | second_talker | 0.0 | -0.4 | -0.4 | -0.2 | 5.65 | -0.3 | 0.052 |
| webrtc-ns-veryhigh-then-rnnoise+enhanced | silence_floor | n/a | -31.2 | n/a | n/a | n/a | n/a | 0.052 |
| webrtc-ns-veryhigh-then-rnnoise+enhanced-high | keyboard_between_words | 10.0 | -19.4 | n/a | 0.2 | 5.04 | 0.2 | 0.048 |
| webrtc-ns-veryhigh-then-rnnoise+enhanced-high | keyboard_during_speech | 10.0 | -20.9 | 3.3 | 0.2 | 5.44 | 0.2 | 0.046 |
| webrtc-ns-veryhigh-then-rnnoise+enhanced-high | mouse_clicks | 15.0 | -22.2 | 9.5 | 0.2 | 5.24 | 0.2 | 0.048 |
| webrtc-ns-veryhigh-then-rnnoise+enhanced-high | fan_plus_mouse | 5.0 | -29.7 | -0.2 | 0.4 | 7.45 | 1.7 | 0.050 |
| webrtc-ns-veryhigh-then-rnnoise+enhanced-high | music_bed | 5.0 | -1.5 | 0.7 | -0.1 | 6.38 | 0.5 | 0.056 |
| webrtc-ns-veryhigh-then-rnnoise+enhanced-high | second_talker | 0.0 | -0.3 | -0.7 | -0.2 | 5.88 | -0.1 | 0.049 |
| webrtc-ns-veryhigh-then-rnnoise+enhanced-high | silence_floor | n/a | -49.9 | n/a | n/a | n/a | n/a | 0.047 |
| deepfilternet (replayed) | keyboard_between_words | 10.0 | -13.9 | n/a | 10.9 | 1.89 | -0.4 | 0.003 |
| deepfilternet (replayed) | keyboard_during_speech | 10.0 | -4.5 | 15.7 | 1.9 | 5.10 | 0.3 | 0.003 |
| deepfilternet (replayed) | mouse_clicks | 15.0 | -3.4 | 18.8 | 6.4 | 3.09 | 0.1 | 0.003 |
| deepfilternet (replayed) | fan_plus_mouse | 5.0 | -20.3 | 8.0 | 0.0 | 10.89 | 0.5 | 0.003 |
| deepfilternet (replayed) | music_bed | 5.0 | -27.2 | 7.8 | 0.0 | 10.23 | 0.6 | 0.003 |
| deepfilternet (replayed) | second_talker | 0.0 | -22.4 | 2.8 | 1.5 | 5.03 | 0.5 | 0.003 |
| deepfilternet (replayed) | silence_floor | n/a | -27.0 | n/a | n/a | n/a | n/a | 0.003 |
| deepfilternet-ll (replayed) | keyboard_between_words | 10.0 | -6.9 | n/a | 12.1 | 1.47 | -0.5 | 0.003 |
| deepfilternet-ll (replayed) | keyboard_during_speech | 10.0 | -3.1 | 11.7 | 8.2 | 2.94 | -0.2 | 0.003 |
| deepfilternet-ll (replayed) | mouse_clicks | 15.0 | 0.4 | 15.4 | 10.9 | 1.92 | -0.4 | 0.003 |
| deepfilternet-ll (replayed) | fan_plus_mouse | 5.0 | -23.7 | 8.1 | 0.0 | 11.33 | 0.4 | 0.003 |
| deepfilternet-ll (replayed) | music_bed | 5.0 | -41.6 | 7.8 | 0.0 | 9.64 | 0.4 | 0.003 |
| deepfilternet-ll (replayed) | second_talker | 0.0 | -7.9 | 2.2 | 3.5 | 4.32 | 0.1 | 0.003 |
| deepfilternet-ll (replayed) | silence_floor | n/a | -24.3 | n/a | n/a | n/a | n/a | 0.002 |
| deepfilternet-ll-then-rnnoise+enhanced (replayed) | keyboard_between_words | 10.0 | -14.6 | n/a | 9.1 | 2.11 | -0.2 | 0.033 |
| deepfilternet-ll-then-rnnoise+enhanced (replayed) | keyboard_during_speech | 10.0 | -5.6 | 11.7 | 5.8 | 3.67 | 0.0 | 0.035 |
| deepfilternet-ll-then-rnnoise+enhanced (replayed) | mouse_clicks | 15.0 | -2.5 | 15.4 | 7.8 | 2.62 | -0.2 | 0.040 |
| deepfilternet-ll-then-rnnoise+enhanced (replayed) | fan_plus_mouse | 5.0 | -31.9 | 7.8 | -0.0 | 11.43 | 0.8 | 0.035 |
| deepfilternet-ll-then-rnnoise+enhanced (replayed) | music_bed | 5.0 | -46.4 | 7.6 | 0.0 | 10.10 | 0.7 | 0.032 |
| deepfilternet-ll-then-rnnoise+enhanced (replayed) | second_talker | 0.0 | -7.5 | 2.0 | 2.5 | 5.07 | 0.3 | 0.033 |
| deepfilternet-ll-then-rnnoise+enhanced (replayed) | silence_floor | n/a | -95.0 | n/a | n/a | n/a | n/a | 0.029 |

| chain | declared delay (ms) | measured delay (ms) | CPU mean (ms / 10 ms block) | CPU p99 | CPU max |
| --- | ---: | ---: | ---: | ---: | ---: |
| none | 0.00 | 0.00 | 0.000 | 0.000 | 0.000 |
| rnnoise | 10.00 | 10.00 | 0.030 | 0.056 | 0.821 |
| rnnoise+enhanced | 10.00 | 10.00 | 0.034 | 0.062 | 0.805 |
| rnnoise+enhanced-high | 10.00 | 10.00 | 0.034 | 0.052 | 0.752 |
| webrtc-ns | 6.98 | 6.60 | 0.018 | 0.036 | 0.848 |
| webrtc-ns-veryhigh | 6.98 | 6.60 | 0.017 | 0.030 | 0.900 |
| rnnoise-then-webrtc-ns+enhanced | 16.98 | 16.60 | 0.050 | 0.097 | 0.862 |
| rnnoise-then-webrtc-ns-veryhigh+enhanced | 16.98 | 16.60 | 0.050 | 0.115 | 0.824 |
| webrtc-ns-then-rnnoise+enhanced | 16.98 | 16.60 | 0.050 | 0.132 | 0.858 |
| webrtc-ns-high-then-rnnoise+enhanced | 16.98 | 16.60 | 0.048 | 0.079 | 0.760 |
| webrtc-ns-veryhigh-then-rnnoise+enhanced | 16.98 | 16.60 | 0.050 | 0.121 | 0.864 |
| webrtc-ns-veryhigh-then-rnnoise+enhanced-high | 16.98 | 16.60 | 0.049 | 0.109 | 0.848 |
| deepfilternet (replayed) | 30.00 | 30.00 | 0.003 | 0.005 | 0.589 |
| deepfilternet-ll (replayed) | 10.00 | 10.00 | 0.003 | 0.004 | 0.043 |
| deepfilternet-ll-then-rnnoise+enhanced (replayed) | 20.00 | 20.00 | 0.034 | 0.073 | 3.470 |

DeepFilterNet's own processing time on this corpus (14 signals of 30 s,
about 42 000 blocks), measured by the outside tool, release build, one
thread:

| model | delay (ms) | CPU mean (ms / 10 ms block) | CPU p99 | CPU max |
| --- | ---: | ---: | ---: | ---: |
| DeepFilterNet 3 (standard) | 30.0 | 0.214 | 0.638 | 2.288 |
| DeepFilterNet 3 low latency | 10.0 | 0.351 | 1.314 | 19.507 |

The 19.5 ms maximum is a single block on a busy machine; a real-time audio
thread would have dropped that block.

## What could change the answer

- **DeepFilterNet's attenuation limit** (`RuntimeParams::atten_lim_db`, off
  in these runs) caps how much it removes. Capped around 20-25 dB it might
  still reach the music target (-20 dB) while changing the voice under music
  less. Not measured; it is the first thing to try before dropping the idea.
- A model trained with music as noise, if one appears.
- A different trade: someone who mostly listens while music plays, and rarely
  talks over it, gains the -41.6 dB and loses little. Ask that user rather
  than assume.
