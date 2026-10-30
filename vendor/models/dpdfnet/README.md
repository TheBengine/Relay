# DPDFNet 48 kHz streaming models

Relay's **Studio** noise suppression (`crates/relay-client/src/dpdfnet.rs`)
runs these two ONNX graphs with `tract-onnx`. They are embedded in
`relay.exe` with `include_bytes!`, so the installer and release pipeline ship
them without any change of their own.

DPDFNet is by Ceva (arXiv 2512.16420, December 2025): DeepFilterNet2 with
dual-path RNN blocks, trained with an over-attenuation loss and music
(MUSAN) among its noises. Code and weights are licensed Apache-2.0
(`LICENSE` in this folder, copied unchanged from the GitHub repository).
Relay does not modify the files.

| file | bytes | sha256 |
| --- | ---: | --- |
| `dpdfnet8_48khz_hr.onnx` | 14 857 107 | `7b3afbb260a08fe9af3d16e3bda992971be1e7e951d1dee7c2d235f5c43f5631` |
| `dpdfnet2_48khz_hr.onnx` | 10 493 337 | `7f0575a5cec0ba4ffd8f8bd657e06d007e4ccdd955d76faab922b9d3291dc14b` |
| `LICENSE` | 11 357 | `c71d239df91726fc519c6eb72d318ec65820627232b2f796219e87dcf35d0ab4` |

Sources:

- Models: `https://huggingface.co/Ceva-IP/DPDFNet`, revision
  `dd6818d00f50c836fed43a6243ebe49116de5964` (2026-07-05), files
  `onnx/dpdfnet8_48khz_hr.onnx` and `onnx/dpdfnet2_48khz_hr.onnx`. The
  sha256 above equals the Hugging Face LFS object id of each file at that
  revision. Model card licence: `apache-2.0`.
- Licence: `https://github.com/ceva-ip/DPDFNet/blob/main/LICENSE`, commit
  `9bd9844a227bb6aa57e55588d8d0e961fcff1c46` (2026-09-09). The repository has
  no NOTICE file.

Graph facts (from Worker F's evaluation, `docs/audio-dpdfnet-eval-20261029.md`):
input one STFT frame `(1, 1, 481, 2)` (960-point, Vorbis window, hop 480)
and a flat state of 90 228 (dpdfnet8) / 56 436 (dpdfnet2) floats whose
initial value comes from the ONNX metadata (`erb_norm_init`,
`spec_norm_init`, zeros after). Output: the enhanced frame and the new state.
The output lags the input by 50 ms (40 ms of look-ahead in the weights plus
10 ms of overlap-add).

To check the files: `sha256sum vendor/models/dpdfnet/*` and compare with the
table; the test `dpdfnet::tests::embedded_models_are_the_vendored_files`
checks the embedded bytes against the same figures.
