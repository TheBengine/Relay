# Audio bench noise corpus

The corpus the send-chain bench (`src/audio_bench.rs`) measures every
noise-suppression chain against. All rows are 10 s of 48 kHz mono (or the length of a
`VOICE_TEST_WAV` recording, below).

**Everything here is synthesized, deterministically, with fixed seeds.** The
repository has no recorded speech or recorded noise (the existing noise test
takes recorded speech only from a `VOICE_TEST_WAV` path outside the
repository), and nothing was recorded from a microphone for this corpus. The
same build always produces the same samples, bit for bit
(`corpus_items_are_deterministic_and_48k`), so numbers from different
branches and machines are comparable. No WAV files are committed: the code
is the corpus. Write them out to listen:

```
cargo run -p relay-client --release --features audio-bench --bin audio-bench -j 8 -- --write-wav
```

That writes `target/audio-bench/wav/<row>.clean.wav`, `<row>.noise.wav`,
`<row>.noisy.wav` and, for every chain, `<row>.<chain>.wav` (16-bit PCM).

## Rows

| row | clean | noise | SNR |
| --- | --- | --- | --- |
| `keyboard_between_words` | synthetic talker (below) | mechanical key presses and releases, placed only where the talker pauses for at least 130 ms | 10 dB |
| `keyboard_during_speech` | same | the same kind of keys, placed only on top of words | 10 dB |
| `mouse_clicks` | same | mouse press and release, 4 clicks a second throughout | 15 dB |
| `fan_plus_mouse` | same | fan: low-passed pink noise with an 87 Hz blade hum that wavers; plus a mouse click about once a second | 5 dB |
| `fan_plus_mouse_15` | same | the same fan and mouse (same generators and seeds) | 15 dB |
| `fan_plus_mouse_10` | same | the same fan and mouse | 10 dB |
| `music_bed` | same | pop-like music at 100 BPM: C-G-Am-F chord pad, plucked arpeggios, bass, kick drum, off-beat hi-hats | 5 dB |
| `music_bed_12` | same | the same music (same generator and seed) | 12 dB |
| `second_talker` | same | a second synthetic voice (different seed and pitch), generated at double speed and stretched to half speed | 0 dB |
| `silence_floor` | 10 s of digital silence | laptop fan only: pink noise without rumble plus a faint 1.87 kHz whine, at -45 dBFS RMS | n/a |
| `clean_voice` | same talker | none (all zeros): the check that a chain leaves a voice alone | n/a |

**Talker.** `voice_test_signals::talker(10 s, f0 105-135 Hz, seed 13, peak 0.3)`:
the same synthetic talker and seed that
`enhanced_noise_suppression_removes_the_clicks_rnnoise_lets_through` uses
without `VOICE_TEST_WAV`. It is a glottal pulse train through moving formants,
with syllables, short gaps and longer pauses. It is not a real voice: RNNoise
recognises it less reliably than recorded speech, so absolute speech numbers
are pessimistic. Compare chains with each other, not with published scores.

**Recorded voice.** `VOICE_TEST_WAV=<48 kHz mono 16-bit WAV>` replaces the
talker in every row with that recording, scaled to the synthetic talker's
active level, and every row takes its length; the noise generators and seeds
stay the same. Its pauses must be digital silence (the keyboard row places
keys where the voice is below 1e-4 RMS, and the pause metrics look for
frames 60 dB below the loudest). How the user's recording was prepared:
`docs/audio-deepfilter-decision-20261029.md`. Without the variable the corpus
is bit-identical to the synthetic one.

**SNR.** Active level against active level: the mean power of the 10 ms
frames within 30 dB of the loudest frame, for the speech and for the noise
alike. For a click train that is the level of the frames with clicks in them.

**Seeds.** Keyboard 21 and 22, mouse 31, fan 41 with mouse 42, music 51,
second talker 77, laptop fan 61. Changing any seed, level or generator changes
every number downstream: do it only in a commit that also updates the
baseline in `docs/`.

## Metrics

All on 10 ms frames, with the chain's output shifted back by its measured
delay. Speech frames are clean frames within 30 dB of the loudest one;
noise-only frames are more than 60 dB below it.

- **residual noise in pauses**: output energy in noise-only frames against
  the noisy input there. 0 dB = nothing removed; above 0 dB the chain makes
  the pauses louder than they came in. Input noise is never taken as quieter
  than -70 dBFS (a real microphone's self-noise), so on a row whose pauses
  are clean this is the output in pauses against -70 dBFS.
- **noise leak during speech**: on speech frames, the chain's output for the
  noisy input minus its output for the clean input, against the noise that
  went in. 0 dB = all the noise comes through while the person talks. This is
  where clicks on words and music under a voice show. The chain is not
  linear: noise that changes its gain or gate decisions also changes how the
  voice comes out, and that counts here too, so values above 0 dB occur.
  n/a when the row has no noise above -70 dBFS during speech
  (`keyboard_between_words`, `silence_floor`).
- **speech segSNR**: mean segmental SNR of the output against the clean speech
  on speech frames, after one least-squares gain for the whole row (clamped
  to -10..35 dB). Higher is better.
- **LSD**: mean log-spectral distance (512-point FFT, Hann window) between
  clean and output speech frames after the same gain; bins more than 50 dB
  below a frame's strongest clean bin count as that floor. 0 = unchanged
  voice; lower is better.
- **speech attenuation**: how much quieter the output speech is than the clean
  speech, before any gain correction. Negative = louder (automatic volume).
- **delay**: declared by the chain, and measured as the lag with the
  strongest match between the clean talker and the chain's output for it:
  within 2 ms of the declared delay, or 0-30 ms for a chain that declares
  none.
- **CPU**: wall time of each 10 ms block on one thread, release build.

The chains run the app's own `MicrophoneProcessor` with `AudioControls` built
from `AudioSettings`, so gain, automatic volume and the voice gate are part of
`rnnoise` and `rnnoise+enhanced`. A block the gate keeps closed comes back as
silence, as in a call. The call's speech-start history (the blocks a call
prepends when the gate opens) is not replayed by the bench.

## Replayed suppressors

A suppressor that cannot be linked into the client is run beforehand by an
outside tool over the corpus WAVs (`--write-wav --chain none` writes them),
and the bench plays its output back in place of the suppressor, then runs the
rest of the app's chain (automatic volume, gate) on it.

`RELAY_REPLAY_DIR=<folder>`: every subfolder `<label>` becomes a chain
`replay:<label>` and holds

- `<row>.noisy.out.wav` and `<row>.clean.out.wav`: the tool's output for
  `<row>.noisy.wav` and `<row>.clean.wav`, 16-bit, the input's length, with
  the tool's delay left in (the clean output is what the leak metric and the
  delay measurement compare against);
- `delay.txt`: that delay in samples, which the chain declares;
- optionally `then.txt` reading `rnnoise+enhanced`: RNNoise with Enhanced
  click suppression at Normal runs after the replayed output (10 ms more).

A chain whose name holds `:` is written to files as `_` (`replay_<label>.json`).
`RELAY_REPLAY_DFN` / `RELAY_REPLAY_DFN_LL` (one such folder each, no
`then.txt`) still give Worker E's DeepFilterNet chains.
