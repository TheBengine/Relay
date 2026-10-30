# DPDFNet on the recorded voice: evaluation, 2026-10-29

Task F of `docs/superpowers/plans/2026-10-29-audio-suppressor-round2.md`.
Follows `docs/audio-deepfilter-decision-20261029.md` (DeepFilterNet 3:
music gone, voice changed) and `docs/audio-suppressor-comparison-20261028.md`.
Nothing is linked into the client; this round delivers numbers, a listening
set and this document.

## Outcome

**DPDFNet is the first candidate that removes the music without changing the
voice.** On the user's recorded voice it meets every music figure in the
acceptance table by a wide margin, keeps the voice closer to the clean
recording than the shipped chain does in every row with speech, and leaves a
clean voice nearly untouched. **It fails the two cost budgets:** it delays the voice by
**50 ms** (budget 20 ms, "orchestrator decides" up to 30 ms), and the larger
model needs **3.5x the CPU budget** per block (the smaller one 1.2x).

| figure (recorded voice) | target | shipped | dpdfnet2 | dpdfnet8 | dpdfnet8, then clicks |
| --- | --- | ---: | ---: | ---: | ---: |
| `music_bed` (5 dB) residual in pauses (dB) | ≤ -20 | -0.8 | **-37.0** | **-49.3** | **-58.2** |
| `music_bed` LSD of the voice (dB) | ≤ 6.0 | 4.35 | **2.53** | **2.37** | **3.04** |
| `music_bed` noise leak during speech (dB) | ≤ +3 | +0.1 | **-6.3** | **-7.1** | **-7.4** |
| `music_bed_12` residual in pauses (dB) | ≤ -25 | +1.7 | **-29.9** | **-43.3** | **-52.5** |
| `music_bed_12` LSD (dB) | ≤ 5.0 | 3.41 | **2.20** | **2.10** | **2.76** |
| `fan_plus_mouse` residual in pauses (dB) | ≤ -25 | -27.0 | **-45.0** | **-41.6** | **-44.5** |
| `keyboard_during_speech` leak (dB) | ≤ -10 (wish -25) | -0.1 | -4.8 | -5.0 | -5.2 |
| `mouse_clicks` residual in pauses (dB) | ≤ -15 | -16.7 | **-33.1** | **-33.9** | **-38.9** |
| speech attenuation, every row (dB) | ±1.5 | -0.5..0.3 | -0.4..-0.1 | -0.4..-0.1 | -0.2..0.1 |
| `clean_voice`: attenuation / LSD, suppressor alone | ≤ 0.5 / ≤ 1.0 | n/a | **0.3 / 0.85** | **0.3 / 0.51** | n/a |
| `clean_voice`: attenuation / LSD, whole send chain | ≤ 0.5 / ≤ 1.0 | -0.2 / 2.12 | -0.3 / 1.78 | -0.3 / 1.52 | -0.1 / 2.30 |
| added delay (ms) | ≤ 20 (30) | 10 | 50 | 50 | 60 |
| CPU p99 per 10 ms block, one thread (ms) | ≤ 1.5 | 0.07 | 1.78 | 5.33 | 5.33 + 0.06 |
| model file (MB) | ≤ 40 | n/a | 10.5 | 14.9 | 14.9 |

Bold: meets the target. Every number above except the "suppressor alone"
row is through the whole send chain (the replayed model, then the app's
automatic volume and voice gate), as in the decision doc.

- **Music.** dpdfnet8 takes the music in the pauses down 49 dB (5 dB SNR)
  and 43 dB (12 dB SNR), and the music that plays *while the user talks*
  comes out 7 dB and 4 dB below what went in (shipped: +0.1 and +2.7 dB). The voice
  under the music is closer to the clean recording than the unprocessed input
  is (LSD 2.37 against 4.36 at 5 dB). DeepFilterNet-LL on the same row and
  voice: -41.6 dB in pauses, but leak +7.8 dB and LSD 9.64.
- **The voice alone.** The model itself changes a clean voice by 0.3 dB and
  LSD 0.51 (dpdfnet8) / 0.85 (dpdfnet2), within the 1.0 target. Through the
  whole send chain the figure is 1.52, but that is the app's own chain:
  replaying the input unchanged through the same automatic volume and gate
  (`replay:identity`, below) already gives LSD 1.41. No suppressor can pass
  "LSD ≤ 1.0 through the chain" on this voice until that changes.
- **Every other row** is better than shipped: keyboard between words -41.9
  against -24.3 dB, second talker -49.0 against +0.2 dB, laptop fan
  (`silence_floor`) -95 against -11.3 dB, and the voice's LSD is lower than
  shipped in all eight rows with speech.
- **Keys on words** are the one quality figure missed: -5.0 dB against the
  -10 dB target (shipped -0.1 dB). Running RNNoise with click suppression
  after dpdfnet8 adds only 0.2 dB there, costs 10 ms more and raises the
  voice's LSD by 0.5-0.8 dB; it is not worth it.
- **The attenuation limit** (the reference's blend with the noisy spectrum)
  works exactly in the model: -18.0 and -24.0 dB in every row, suppressor
  alone. Through the send chain the pauses come back up to -7 and -12 dB on
  music (automatic volume and the gate do this; they are the only things
  after the model in that chain), and the voice gains nothing measurable
  (LSD 2.44 against 2.37 unlimited). The limit is not useful here.

**Reading for the orchestrator.** The quality case is made: dpdfnet8, and
dpdfnet2 one step behind, meet the music targets that nothing else met,
without the voice damage the user heard from DeepFilterNet. The open question
is cost. 50 ms of added delay is fixed by the trained model (below), and only
dpdfnet2 is near the CPU budget. Whether a 50 ms "studio" mode is worth
offering, and whether to look for or train a variant with less look-ahead, is
the orchestrator's and the user's call after listening.

## Delay: 50 ms, fixed by the model

Measured by cross-correlating the probe's streaming output for the clean
recording with its input (FFT, lags 0-100 ms, normalised peak 0.99, next
peak 25 samples away):

| model | atten limit | delay (samples) | delay (ms) |
| --- | --- | ---: | ---: |
| dpdfnet2_48khz_hr | none | 2400 | 50.00 |
| dpdfnet2_48khz_hr | 24 dB | 2400 | 50.00 |
| dpdfnet8_48khz_hr | none | 2400 | 50.00 |
| dpdfnet8_48khz_hr | 24 dB | 2400 | 50.00 |

The bench's replay route declares 2400 samples and measures 50.00 ms for
every DPDFNet chain (60.00 ms with RNNoise after it).

Where it comes from: the network's output frame t is the enhancement of input
frame t-4 (it looks four 10 ms hops ahead). The reference script hides this
offline: it drops the first `2*win_len` = 1920 samples of the output, which
is why its output lines up with its input at lag 0. A live stream cannot drop
samples it has not produced yet, so it keeps those 40 ms and adds 10 ms for
the 20 ms window's overlap-add. The four frames are in the trained weights;
no runtime setting shortens them. (DeepFilterNet 3: 30 ms standard, 10 ms LL.)

## CPU

`dpprobe --stats`, release build, one thread, wall time of each 480-sample
block (STFT, one model step, inverse STFT), 53 892 blocks per variant (the 18
corpus signals of 30 s), no build or test running at the same time
(AMD Family 26 Model 68, 16 logical processors):

| model | mean (ms) | p50 (ms) | p99 (ms) | max (ms) |
| --- | ---: | ---: | ---: | ---: |
| dpdfnet2_48khz_hr | 1.363 | 1.387 | 1.779 | 6.001 |
| dpdfnet8_48khz_hr | 3.925 | 3.824 | 5.329 | 15.563 |
| dpdfnet8_48khz_hr, limit 18 dB | 3.888 | 3.778 | 5.380 | 14.896 |
| dpdfnet8_48khz_hr, limit 24 dB | 3.547 | 3.363 | 5.285 | 11.623 |

For comparison, onnxruntime 1.31.1 from Python (the reference, one thread,
including Python's per-call overhead) averaged 1.17-1.39 ms per frame for
dpdfnet2 and 3.27-3.89 ms for dpdfnet8. Changing the runtime would not fix the
budget. The bench's own CPU column for the replayed chains (0.003-0.008 ms)
covers only the automatic volume and gate after the replayed output.

## The graph, and which runtime loaded it

Both files are self-contained streaming graphs (ONNX opset 17, IR 8, exported
by PyTorch 2.10.0, no subgraphs):

| | dpdfnet2_48khz_hr | dpdfnet8_48khz_hr |
| --- | --- | --- |
| input `spec` / output `spec_e` | (1, 1, 481, 2) float | (1, 1, 481, 2) float |
| input `state_in` / output `state_out` | 56 436 floats | 90 228 floats |
| STFT | 960-point, Vorbis window, hop 480 (`win_len` = 960, F = 481) | same |
| nodes | 493 | 937 |

The initial state comes from the ONNX metadata: `erb_norm_init` (481 values)
then `spec_norm_init` (96 values), zeros after. (The dpdfnet8 file's
`profile` metadata says `dpdfnet2_48khz_hr`, a copy-paste in the export;
everything else in it is dpdfnet8's.)

Op types (count in dpdfnet2 / dpdfnet8): Add 69/153, Concat 16/16, Conv 30/30,
Div 4/4, Flatten 2/2, **GRU 4/16**, Gather 6/6, Gemm 22/58,
**LayerNormalization 8/32**, Log 1/1, MatMul 14/26, Mul 30/54, Pad 1/1,
Pow 2/2, ReduceSum 6/6, Relu 22/22, Reshape 82/166, Sigmoid 19/43,
Slice 37/49, Split 18/42, Sqrt 3/3, Squeeze 11/11, Sub 11/23, Tanh 10/22,
Transpose 47/131, Unsqueeze 18/18.

**`tract-onnx` 0.23.9 (the newest) loads and optimises both graphs**,
GRU and LayerNormalization included, in 0.2 s / 0.6 s. No fallback to `ort`
was needed.

Probe against the Python reference (onnxruntime 1.31.1 through
`infer_dpdfnet_onnx.py`'s own functions), probe output shifted back by 2400
samples:

| input | model | RMS difference (of full scale) | max difference |
| --- | --- | ---: | ---: |
| clean recorded voice | dpdfnet2 | 1.2e-7 | 7.9e-6 |
| clean recorded voice | dpdfnet8 | 4.7e-8 | 3.7e-6 |
| clean recorded voice | dpdfnet8, limit 24 dB | 4.5e-8 | 3.5e-6 |
| `music_bed` corpus WAV | dpdfnet2 | 4.0e-5 | 8.6e-4 |
| `music_bed` corpus WAV | dpdfnet8 | 5.6e-5 | 1.5e-3 |

The plan's criterion (1e-4 RMS on the clean voice) holds by three orders of
magnitude. On the noisy music the two runtimes drift apart more: 1e-6 in the
first 0.1 s, up to 1.6e-4 in the first second, decaying to 1e-5 after 15 s.
That is floating-point rounding (tract and onnxruntime sum in different
orders) carried by the model's recurrent and normalisation state, 57 dB below
the signal.

## Licence, size, integration

- **Licence:** Apache-2.0 for code and weights (`github.com/ceva-ip/DPDFNet`
  LICENSE, `huggingface.co/Ceva-IP/DPDFNet` model card). Attribution goes in
  the installer's notices.
- **Size:** 10.5 MB (dpdfnet2) / 14.9 MB (dpdfnet8), within the 40 MB budget.
  DeepFilterNet was +36 MB.
- **Crates:** `tract-onnx` 0.23.9 plus a 960-point real FFT (`realfft`, already
  in the client's lock). The probe's tree is 124 crates. 32 of them are not
  in the client's lock: the nine `tract-*`, `ndarray`, `matrixmultiply`,
  `num-rational`, `rand_distr`, `minijinja`, `borsh` (and derive), `tar`,
  `filetime`, `xattr`, `inventory`, `erased-serde`, `metrics`, `memo-map`,
  `string-interner`, `rapidhash`, `dyn-hash`, `float-ord`, `maplit`,
  `nom-language`, `derive-new`, `typeid`, `rawpointer`. The rest only take
  newer patch versions of crates the client already has. **tract 0.23.9 does
  not depend on `time` at all**, so DeepFilterNet's blocker (`tract` 0.21.4
  pinning `time` below the client's) does not apply. Not tried: building it
  into the client.
- **Code:** roughly the probe's `Stream`, about 110 lines (window, STFT,
  one `run` per block, overlap-add, state hand-over), behind the existing
  suppressor interface with `delay_samples() = 2400`.
- **Real time:** the model runs on the audio thread once per 10 ms block.
  dpdfnet8's p99 of 5.3 ms (max 15.6 ms) is half the block period and would
  drop blocks on a busy machine. It needs its own thread with a block of
  queue, or dpdfnet2.
- **Delay budget:** 50 ms is added to the send path on top of everything
  else; nothing in this evaluation reduces it.

## How it was measured

1. Models and the reference script downloaded into an empty folder outside
   the repository (`work\audio-worker-f-20261029\models`). Python 3.14 venv
   beside it with onnxruntime 1.31.1, onnx 1.23.2, librosa 1.0.0, numpy
   2.5.4. Run with `python -I`; the reference's functions are loaded from its path, not run from its folder.
2. The bench with the patch committed in this branch: `VOICE_TEST_WAV`
   (Worker E's patch), the new rows `music_bed_12` (same generator and
   seed 51, 12 dB) and `clean_voice` (noise all zeros), and the replay route
   `RELAY_REPLAY_DIR`. `audio-bench --write-wav --chain none` wrote the corpus
   from `voice3-prepared.wav` (the decision doc's recording and preparation;
   not in the repository).
3. `dpprobe` (outside the repository, `work\audio-worker-f-20261029\dpprobe`,
   tract-onnx 0.23.9, realfft 3.5.1) ran each variant over the 18 corpus
   signals one 480-sample block at a time, fresh state per signal, and wrote
   `<row>.<clean|noisy>.out.wav` and `delay.txt` (2400) per label.
   `dpdfnet8-then-clicks` is the dpdfnet8 output with `then.txt`.
4. `RELAY_REPLAY_DIR=<labels> audio-bench --write-wav` with the seven chains
   below. The shipped and unprocessed chains reproduce every metric column
   of the decision doc's table on the seven old rows exactly.
5. `replay:identity` (every input replayed unchanged, delay 0) is the
   control for "what the rest of the send chain does on its own".
6. "Suppressor alone": the probe's output against the clean recording with
   the bench's own frame rules and LSD, in Python (`ref\raw_metrics.py`).

Scripts: `work\audio-worker-f-20261029\ref\` (`run.sh` for steps 2-4,
`make_lyt.py` for the listening set, `inspect_models.py`, `xcorr.py`).

## Listening set

`C:\Important\Projects\2026-10-03\can\work\audio-worker-f-20261029\lyt\`,
48 kHz 16-bit mono, all from the recorded voice, each file's active level
matched to the clean file's (all within 0.01 dB), each chain's output shifted
back by its measured delay (cross-correlation with the clean file: lag 0 in
all 28 files). The outputs are the whole send chain, as a listener would hear
them.

```
musik-12db\  (music_bed_12)          musik-5db\  (music_bed)
tastatur-under-tale\  (keyboard_during_speech)   blaeser-og-mus\  (fan_plus_mouse)
  1-ren-stemme.wav                the clean recording
  2-ind-i-mikrofonen.wav          voice + noise, the input
  3-relay-i-dag.wav               shipped: RNNoise + click suppression
  4-dpdfnet2.wav
  5-dpdfnet8.wav
  6-dpdfnet8-graense-24db.wav     dpdfnet8 with the 24 dB limit
  7-dpdfnet8-plus-klik.wav        dpdfnet8, then RNNoise + click suppression
```

## Full table, recorded voice

Run at 2026-10-29T15:53:25Z, release build, one thread. Columns as in the
comparison doc. The `replay:` rows are replayed; their CPU column covers only
the rest of the chain (CPU of the model itself: above).

| chain | item | SNR in (dB) | residual noise in pauses (dB) | noise leak during speech (dB) | speech segSNR (dB) | LSD (dB) | speech attenuation (dB) | CPU (ms / 10 ms block) |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| none | keyboard_between_words | 10.0 | 0.0 | n/a | 35.0 | 0.00 | 0.0 | 0.000 |
| none | keyboard_during_speech | 10.0 | 0.0 | 0.0 | 25.9 | 3.17 | -0.1 | 0.000 |
| none | mouse_clicks | 15.0 | 0.0 | 0.0 | 32.5 | 0.86 | -0.0 | 0.000 |
| none | fan_plus_mouse | 5.0 | 0.0 | 0.0 | 3.0 | 7.82 | -1.2 | 0.000 |
| none | music_bed | 5.0 | 0.0 | 0.0 | 3.6 | 4.36 | -1.2 | 0.000 |
| none | music_bed_12 | 12.0 | 0.0 | 0.0 | 9.7 | 3.08 | -0.3 | 0.000 |
| none | second_talker | 0.0 | 0.0 | 0.0 | 5.0 | 2.20 | -1.8 | 0.000 |
| none | silence_floor | n/a | 0.0 | n/a | n/a | n/a | n/a | 0.000 |
| none | clean_voice | n/a | -18.4 | n/a | 35.0 | 0.00 | 0.0 | 0.000 |
| rnnoise+enhanced | keyboard_between_words | 10.0 | -24.3 | n/a | 9.0 | 2.16 | -0.3 | 0.032 |
| rnnoise+enhanced | keyboard_during_speech | 10.0 | -16.6 | -0.1 | 8.4 | 3.50 | -0.1 | 0.031 |
| rnnoise+enhanced | mouse_clicks | 15.0 | -16.7 | 7.5 | 8.7 | 2.64 | -0.2 | 0.044 |
| rnnoise+enhanced | fan_plus_mouse | 5.0 | -27.0 | -1.5 | 4.4 | 5.33 | 0.3 | 0.043 |
| rnnoise+enhanced | music_bed | 5.0 | -0.8 | 0.1 | 3.4 | 4.35 | -0.2 | 0.040 |
| rnnoise+enhanced | music_bed_12 | 12.0 | 1.7 | 2.7 | 5.9 | 3.41 | -0.5 | 0.046 |
| rnnoise+enhanced | second_talker | 0.0 | 0.2 | -0.4 | 4.3 | 3.22 | -0.4 | 0.046 |
| rnnoise+enhanced | silence_floor | n/a | -11.3 | n/a | n/a | n/a | n/a | 0.047 |
| rnnoise+enhanced | clean_voice | n/a | -17.5 | n/a | 9.1 | 2.12 | -0.2 | 0.041 |
| replay:dpdfnet2 | keyboard_between_words | 10.0 | -38.8 | n/a | 12.0 | 1.50 | -0.4 | 0.003 |
| replay:dpdfnet2 | keyboard_during_speech | 10.0 | -26.9 | -4.8 | 11.3 | 2.34 | -0.3 | 0.003 |
| replay:dpdfnet2 | mouse_clicks | 15.0 | -33.1 | 2.7 | 11.7 | 1.82 | -0.4 | 0.004 |
| replay:dpdfnet2 | fan_plus_mouse | 5.0 | -45.0 | -6.5 | 7.2 | 4.28 | -0.1 | 0.003 |
| replay:dpdfnet2 | music_bed | 5.0 | -37.0 | -6.3 | 7.5 | 2.53 | -0.2 | 0.004 |
| replay:dpdfnet2 | music_bed_12 | 12.0 | -29.9 | -2.7 | 8.9 | 2.20 | -0.4 | 0.003 |
| replay:dpdfnet2 | second_talker | 0.0 | -48.4 | -9.3 | 9.6 | 2.04 | -0.3 | 0.003 |
| replay:dpdfnet2 | silence_floor | n/a | -95.0 | n/a | n/a | n/a | n/a | 0.002 |
| replay:dpdfnet2 | clean_voice | n/a | -13.3 | n/a | 11.8 | 1.78 | -0.3 | 0.004 |
| replay:dpdfnet8 | keyboard_between_words | 10.0 | -41.9 | n/a | 11.8 | 1.47 | -0.4 | 0.003 |
| replay:dpdfnet8 | keyboard_during_speech | 10.0 | -32.4 | -5.0 | 11.2 | 2.11 | -0.4 | 0.004 |
| replay:dpdfnet8 | mouse_clicks | 15.0 | -33.9 | -0.2 | 11.5 | 1.73 | -0.4 | 0.004 |
| replay:dpdfnet8 | fan_plus_mouse | 5.0 | -41.6 | -6.9 | 7.4 | 4.22 | -0.1 | 0.004 |
| replay:dpdfnet8 | music_bed | 5.0 | -49.3 | -7.1 | 7.8 | 2.37 | -0.3 | 0.004 |
| replay:dpdfnet8 | music_bed_12 | 12.0 | -43.3 | -3.8 | 9.2 | 2.10 | -0.4 | 0.003 |
| replay:dpdfnet8 | second_talker | 0.0 | -49.0 | -10.7 | 10.2 | 1.69 | -0.4 | 0.003 |
| replay:dpdfnet8 | silence_floor | n/a | -95.0 | n/a | n/a | n/a | n/a | 0.004 |
| replay:dpdfnet8 | clean_voice | n/a | -14.2 | n/a | 11.8 | 1.52 | -0.3 | 0.008 |
| replay:dpdfnet8-lim18 | keyboard_between_words | 10.0 | -12.7 | n/a | 11.5 | 1.51 | -0.6 | 0.003 |
| replay:dpdfnet8-lim18 | keyboard_during_speech | 10.0 | -13.3 | -4.9 | 11.3 | 2.34 | -0.4 | 0.003 |
| replay:dpdfnet8-lim18 | mouse_clicks | 15.0 | -11.8 | 3.9 | 11.5 | 1.76 | -0.6 | 0.003 |
| replay:dpdfnet8-lim18 | fan_plus_mouse | 5.0 | -3.9 | -4.9 | 7.5 | 3.98 | 0.3 | 0.004 |
| replay:dpdfnet8-lim18 | music_bed | 5.0 | -7.1 | -5.7 | 7.6 | 2.59 | -0.1 | 0.003 |
| replay:dpdfnet8-lim18 | music_bed_12 | 12.0 | -4.9 | -1.3 | 9.0 | 2.21 | -0.5 | 0.005 |
| replay:dpdfnet8-lim18 | second_talker | 0.0 | -5.7 | -8.5 | 9.4 | 1.74 | -0.5 | 0.004 |
| replay:dpdfnet8-lim18 | silence_floor | n/a | -14.7 | n/a | n/a | n/a | n/a | 0.003 |
| replay:dpdfnet8-lim18 | clean_voice | n/a | -14.6 | n/a | 11.9 | 1.46 | -0.3 | 0.006 |
| replay:dpdfnet8-lim24 | keyboard_between_words | 10.0 | -19.0 | n/a | 11.6 | 1.50 | -0.5 | 0.004 |
| replay:dpdfnet8-lim24 | keyboard_during_speech | 10.0 | -19.0 | -4.9 | 11.3 | 2.16 | -0.4 | 0.004 |
| replay:dpdfnet8-lim24 | mouse_clicks | 15.0 | -18.9 | 0.3 | 11.5 | 1.72 | -0.4 | 0.004 |
| replay:dpdfnet8-lim24 | fan_plus_mouse | 5.0 | -15.6 | -5.8 | 7.5 | 3.89 | -0.0 | 0.003 |
| replay:dpdfnet8-lim24 | music_bed | 5.0 | -11.7 | -5.8 | 7.7 | 2.44 | -0.4 | 0.003 |
| replay:dpdfnet8-lim24 | music_bed_12 | 12.0 | -10.7 | -1.0 | 9.1 | 2.13 | -0.6 | 0.003 |
| replay:dpdfnet8-lim24 | second_talker | 0.0 | -7.4 | -9.1 | 9.9 | 1.74 | -0.5 | 0.004 |
| replay:dpdfnet8-lim24 | silence_floor | n/a | -40.5 | n/a | n/a | n/a | n/a | 0.004 |
| replay:dpdfnet8-lim24 | clean_voice | n/a | -14.4 | n/a | 11.9 | 1.48 | -0.3 | 0.006 |
| replay:dpdfnet8-then-clicks | keyboard_between_words | 10.0 | -46.8 | n/a | 9.1 | 2.18 | -0.1 | 0.039 |
| replay:dpdfnet8-then-clicks | keyboard_during_speech | 10.0 | -37.8 | -5.2 | 8.8 | 2.76 | -0.2 | 0.036 |
| replay:dpdfnet8-then-clicks | mouse_clicks | 15.0 | -38.9 | 0.9 | 8.9 | 2.44 | -0.2 | 0.037 |
| replay:dpdfnet8-then-clicks | fan_plus_mouse | 5.0 | -44.5 | -6.9 | 6.6 | 4.66 | 0.1 | 0.035 |
| replay:dpdfnet8-then-clicks | music_bed | 5.0 | -58.2 | -7.4 | 6.9 | 3.04 | 0.0 | 0.034 |
| replay:dpdfnet8-then-clicks | music_bed_12 | 12.0 | -52.5 | -4.3 | 7.9 | 2.76 | -0.1 | 0.036 |
| replay:dpdfnet8-then-clicks | second_talker | 0.0 | -55.2 | -11.3 | 8.4 | 2.44 | -0.1 | 0.036 |
| replay:dpdfnet8-then-clicks | silence_floor | n/a | -95.0 | n/a | n/a | n/a | n/a | 0.026 |
| replay:dpdfnet8-then-clicks | clean_voice | n/a | -19.3 | n/a | 9.0 | 2.30 | -0.1 | 0.038 |

| chain | declared delay (ms) | measured delay (ms) | CPU mean (ms / 10 ms block) | CPU p99 | CPU max |
| --- | ---: | ---: | ---: | ---: | ---: |
| none | 0.00 | 0.00 | 0.000 | 0.000 | 0.000 |
| rnnoise+enhanced | 10.00 | 10.00 | 0.041 | 0.068 | 0.282 |
| replay:dpdfnet2 | 50.00 | 50.00 | 0.003 | 0.006 | 0.035 |
| replay:dpdfnet8 | 50.00 | 50.00 | 0.004 | 0.011 | 0.068 |
| replay:dpdfnet8-lim18 | 50.00 | 50.00 | 0.004 | 0.010 | 0.034 |
| replay:dpdfnet8-lim24 | 50.00 | 50.00 | 0.004 | 0.007 | 0.175 |
| replay:dpdfnet8-then-clicks | 60.00 | 60.00 | 0.035 | 0.063 | 1.729 |

Control: the input replayed unchanged through the same automatic volume and
gate (`replay:identity`, same run inputs, 2026-10-29). What the send chain
does with no suppressor at all:

| chain | item | SNR in (dB) | residual noise in pauses (dB) | noise leak during speech (dB) | speech segSNR (dB) | LSD (dB) | speech attenuation (dB) | CPU (ms / 10 ms block) |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| replay:identity | keyboard_between_words | 10.0 | 6.0 | n/a | 11.7 | 1.53 | -0.6 | 0.003 |
| replay:identity | keyboard_during_speech | 10.0 | 4.2 | 1.6 | 11.0 | 4.10 | -0.5 | 0.004 |
| replay:identity | mouse_clicks | 15.0 | 6.2 | 4.9 | 11.7 | 2.19 | -0.6 | 0.003 |
| replay:identity | fan_plus_mouse | 5.0 | 0.5 | 0.4 | 2.5 | 7.89 | 0.7 | 0.003 |
| replay:identity | music_bed | 5.0 | 0.3 | 0.2 | 2.9 | 4.52 | 0.4 | 0.004 |
| replay:identity | music_bed_12 | 12.0 | 2.4 | 2.0 | 6.6 | 3.48 | 0.2 | 0.003 |
| replay:identity | second_talker | 0.0 | 0.4 | -0.5 | 5.9 | 2.34 | -0.5 | 0.004 |
| replay:identity | silence_floor | n/a | 2.0 | n/a | n/a | n/a | n/a | 0.003 |
| replay:identity | clean_voice | n/a | -14.5 | n/a | 12.4 | 1.41 | -0.4 | 0.006 |

The suppressor alone (probe output against the clean recording, before
automatic volume and gate; bench frame rules, Python):

| variant | item | residual noise in pauses (dB) | LSD (dB) | speech attenuation (dB) |
| --- | --- | ---: | ---: | ---: |
| dpdfnet2 | music_bed | -40.9 | 2.06 | 0.3 |
| dpdfnet2 | music_bed_12 | -41.0 | 1.60 | 0.2 |
| dpdfnet2 | fan_plus_mouse | -51.9 | 4.24 | 0.3 |
| dpdfnet2 | keyboard_during_speech | -31.7 | 1.73 | 0.2 |
| dpdfnet2 | clean_voice | -19.7 | 0.85 | 0.3 |
| dpdfnet8 | music_bed | -55.1 | 1.84 | 0.3 |
| dpdfnet8 | music_bed_12 | -49.0 | 1.44 | 0.2 |
| dpdfnet8 | fan_plus_mouse | -44.4 | 4.17 | 0.4 |
| dpdfnet8 | keyboard_during_speech | -37.5 | 1.40 | 0.3 |
| dpdfnet8 | clean_voice | -19.6 | 0.51 | 0.3 |
| dpdfnet8-lim18 | music_bed | -18.0 | 2.07 | 0.2 |
| dpdfnet8-lim18 | music_bed_12 | -18.0 | 1.48 | 0.1 |
| dpdfnet8-lim18 | fan_plus_mouse | -18.0 | 3.73 | 0.3 |
| dpdfnet8-lim18 | keyboard_during_speech | -17.8 | 1.53 | 0.3 |
| dpdfnet8-lim18 | clean_voice | -20.0 | 0.42 | 0.3 |
| dpdfnet8-lim24 | music_bed | -24.0 | 1.82 | 0.3 |
| dpdfnet8-lim24 | music_bed_12 | -24.0 | 1.39 | 0.2 |
| dpdfnet8-lim24 | fan_plus_mouse | -23.9 | 3.66 | 0.4 |
| dpdfnet8-lim24 | keyboard_during_speech | -23.6 | 1.39 | 0.3 |
| dpdfnet8-lim24 | clean_voice | -19.8 | 0.46 | 0.3 |

`clean_voice` has no noise, so its "residual in pauses" is the output in the
pauses against -70 dBFS (the bench's floor for a noise-free input), not a
noise figure: the prepared recording's pauses are digital silence apart from
its 5 ms ramps (`none`: -18.4 dB).

## Addendum 2026-10-30: a quieter fan, and LSD under a level-matching gain

After listening, the user found dpdfnet8 clearly best and asked for the fan
scene again with a quieter fan. Two rows were added to the bench:
`fan_plus_mouse_15` and `fan_plus_mouse_10`, the same fan (seed 41) and mouse
(seed 42, x8) as `fan_plus_mouse`, at 15 and 10 dB SNR. Run on the recorded
voice, release build; the old rows reproduce the table above exactly.

| chain | item | SNR in (dB) | residual noise in pauses (dB) | noise leak during speech (dB) | speech segSNR (dB) | LSD (dB) | speech attenuation (dB) |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| none | fan_plus_mouse_15 | 15.0 | 0.0 | 0.0 | 11.9 | 4.24 | -0.1 |
| none | fan_plus_mouse_10 | 10.0 | 0.0 | 0.0 | 7.2 | 6.03 | -0.4 |
| rnnoise+enhanced | fan_plus_mouse_15 | 15.0 | -28.1 | 2.9 | 7.1 | 4.07 | 0.1 |
| rnnoise+enhanced | fan_plus_mouse_10 | 10.0 | -28.9 | 0.3 | 6.0 | 4.67 | 0.2 |
| replay:dpdfnet2 | fan_plus_mouse_15 | 15.0 | -42.0 | -1.0 | 9.8 | 2.93 | -0.3 |
| replay:dpdfnet2 | fan_plus_mouse_10 | 10.0 | -44.1 | -3.9 | 8.7 | 3.57 | -0.2 |
| replay:dpdfnet8 | fan_plus_mouse_15 | 15.0 | -43.8 | -3.5 | 9.8 | 2.87 | -0.3 |
| replay:dpdfnet8 | fan_plus_mouse_10 | 10.0 | -43.3 | -5.2 | 8.7 | 3.50 | -0.2 |

With the quieter fan the shipped chain removes it about as well in the pauses
(-28 dB) but lets more through while the user talks (+2.9 dB at 15 dB).
dpdfnet8 takes it 43-44 dB down in the pauses and 3.5-5.2 dB down under the
voice, and the voice ends up closer to the clean recording than the input
was (LSD 2.87 against 4.24). Listening files:
`work\audio-worker-f-20261029\lyt\blaeser-svag-15db\` and
`blaeser-svag-10db\` (files 1-5 as in the other folders; aligned, level
within 0.01 dB).

**LSD under a level-matching gain.** Worker G found that the bench's
least-squares gain collapses when a model changes the voice's phase, which
inflates LSD (DeepFilterNet-LL: 9.64 with the bench's gain, 5.65 with a gain
that matches levels). For DPDFNet it does not: the least-squares gain is
0.7-0.9 dB below the level-matching gain (the same as for the shipped chain),
and the LSD moves by at most 0.09 dB (dpdfnet8 `music_bed` 2.37 -> 2.39,
`music_bed_12` 2.10 -> 2.13, `fan_plus_mouse` 4.22 -> 4.13, `clean_voice`
1.52 -> 1.57; `ref\lsd_gain.py`, which reproduces the bench's LSD exactly
with its gain). So DPDFNet's lead over DeepFilterNet-LL under music is
smaller than 2.37 against 9.64 suggests, but it is still a lead:
2.39 against 5.65.

## What could change the answer

- **A model with less look-ahead.** The 40 ms is four frames of look-ahead
  in the trained weights. The DPDFNet repository publishes the model
  definition and inference code but no training recipe, so a retrain with one
  frame of look-ahead (10 ms model + 10 ms overlap-add = 20 ms, the budget)
  means rebuilding the training from DeepFilterNet2's recipe and the paper's
  losses. It is the obvious next step if the user likes what they hear; its
  quality at that look-ahead is unknown.
- **dpdfnet2 on its own thread.** It meets every music target at p99 1.8 ms;
  with a block of queueing the CPU budget may be acceptable. The delay is not.
- **The keyboard figure** (-5 dB on keys over words) is the one quality gap,
  and nothing tried here closes it.
