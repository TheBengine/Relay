# Native media cryptor patch

The pinned upstream `webrtc-89d790b` library repeats a GCM IV when separate video cryptors start with the same key, SSRC and timestamp. [The actual-library reproduction](native-iv-reproduction-20261006.json) uses synthetic VP8 transform payloads. It does not claim those metadata are normally repeated in a Relay call.

Relay's isolated patch uses a fresh 96-bit IV from BoringSSL `RAND_bytes` for every media encryption attempt. A process-wide atomic counter saturates at 2^32 attempts. Quota exhaustion or RNG failure reports encryption failure and returns without a frame. Recreating a cryptor cannot reset that budget. The public headers, IV trailer and native ABI are unchanged; H264/H265 branches are retained. Native diagnostics no longer print input or derived key material.

The random-IV construction and invocation bound follow [NIST SP 800-38D sections 8.2.2 and 8.3](https://nvlpubs.nist.gov/nistpubs/Legacy/SP/nistspecialpublication800-38d.pdf). This is not a certification or independent cryptographic review. Random IV uniqueness is probabilistic. Across processes, Relay separately requires authenticated fresh MLS call keys before media become available; an imported history archive cannot supply live call keys. No backup or restart may restore the cryptor counter with old sending keys.

`scripts/prepare-webrtc.ps1` first verifies the original digest-pinned ZIP and every original SDK file. It creates a new generation, compiles only the reviewed translation unit with the static MSVC runtime and retained codec defines, removes exactly the original cryptor archive member and inserts the new object. Every remaining SDK file is compared against the verified base. Source, compiler, recipe, object and SDK digests are recorded. Original caches are untouched; failed generations are kept for diagnosis. `scripts/build.ps1` uses this preparation automatically. Direct Windows native Cargo builds require its marker and verify source/recipe/library/file integrity. The native helper invalidates cached link metadata when the SDK path/library changes.

`scripts/test-native-cryptor.py --sdk <prepared-sdk> --base-sdk <verified-original-sdk> --work <new-owned-proof-directory>` tests the actual reviewed library. Separate test-only source copies inject RNG failure and a two-attempt limit; production has no fault switches. Existing native RTC tests additionally cover generated protected video, publication replacement, stop, room separation, rejected plaintext/wrong keys and signal recovery.

## Received frames: W-13 and W-14 (21 November 2026)

The [security review](security-review-20261031.md) found two receive-side defects in the same translation unit ([details](dependency-advisories-20261105.md)). Both are fixed by porting LiveKit's upstream changes; the random-IV patch above is unchanged.

- **W-13 (High), port of [webrtc-sdk/webrtc#297](https://github.com/webrtc-sdk/webrtc/pull/297).** A received frame that ended with the SFU's server-injected-frame (SIF) trailer skipped decryption and went to the decoder with no state change, so whoever runs the SFU could inject plaintext audio or video. Now the trailer only marks a frame as the SFU's: after stripping it, the payload must be byte-equal to one of the blank frames the SFU injects on mute and track close (VP8 8x8 key frame; H.264 Annex B frame whose every NALU is the blank SPS, PPS or IDR; Opus silence; the opus/red primary block of that silence; PCMU and PCMA silence). Anything else is dropped and reported as `kDecryptionFailed`. The list is the one in `livekit-server` `pkg/sfu/downtrack.go`, checked byte for byte against v1.13.10, the version of Relay's SFU. The trailer is still installed, because with `failure_tolerance: 0` the SFU's real blank frames would otherwise fail every call. The SFU can still inject those fixed blank frames, which carry no content.
- **W-14 (Medium), port of [webrtc-sdk/webrtc#324](https://github.com/webrtc-sdk/webrtc/pull/324).** `decryptFrame` rejected only empty frames, then read the trailer, the IV and the AES-GCM tag at offsets computed from the frame size. A short frame from any call participant read outside the frame or underflowed a size and crashed the receiver. Now a frame shorter than clear header + 12-byte IV + 2-byte trailer + 16-byte tag is dropped before any offset is computed, and for H.264/H.265 the check repeats after RBSP unescaping, which can shorten the buffer. Both report `kDecryptionFailed` without counting as a key failure, so the next genuine frame still decrypts.

Differences from upstream: `IsKnownSifPayload` stays file-local instead of being exported from the header, so headers and ABI remain unchanged; the length checks use `size_t` arithmetic explicitly. The SDK marker keeps the name `relay-frame-iv-v1`: the patch digest, not the name, identifies the reviewed source.

The native test now covers both. `sif` feeds 9 foreign trailer frames (plaintext video with and without a key, a VP8 blank frame with one bit changed or one byte added, H.264 blank parameter sets followed by a real slice, a bare IDR without start code, Opus speech, opus/red with the F bit set, 159-byte PCMU) and the 7 legitimate blank frames, then checks that an encrypted frame still decrypts after a rejection. `short` runs each frame of 1 byte up to the minimum length (video: 3 + 30 bytes, audio: 1 + 30) in its own process, once starting right after a no-access guard page and once ending right before one; a vectored exception handler reports the offset of any read outside the frame.

| Library | `sif` | `short` (128 frames) |
| --- | --- | --- |
| October generation `build-8ab76c0925024e3aa37ab7fcc5bc4a87` | 9 of 9 foreign frames delivered unauthenticated, no state change; 7 of 7 blank frames delivered | 28 guard-page reads outside the frame (e.g. 1-byte video frame reads offset −1, 2-byte frame offset −12), 35 receiver crashes (`0xC0000409` fail-fast and `0xC0000005`), 65 dropped |
| This generation | 0 of 9 delivered, all 9 reported `kDecryptionFailed`; 7 of 7 blank frames delivered with the trailer stripped; encrypted frame after a rejection decrypts | 128 of 128 dropped with `kDecryptionFailed`, no read outside the frame, no crash |

The source fix is commit `39e6521`. The SDK generation built from it with `prepare-webrtc.ps1` on 21 November 2026, for `LK_CUSTOM_WEBRTC`:

`C:\Important\Projects\2026-10-03\can\work\fix-cryptor-20261121\native\relay-frame-crypto\build-d158872224bd4577b0b0ced24a889a95\sdk`

| Item | SHA-256 |
| --- | --- |
| Patch source `frame_crypto_transformer.cc` | `8a503241ff05068bafcdc496f9e44e56b726714221a510aa16d422de50976ecd` |
| `lib/webrtc.lib` | `165b2306c5d3980eda5a85b7eaaffec642eeb14b15689143991ebcc6b6b554b0` |
| Cryptor object | `83be2fc2f0f2e8a45fa2d6f286cb2970e1b4ccac25c21d09123fe86d3ab484fa` |
| Marker `relay-frame-crypto.json` | `c0e02347f95136a3d257bfb302336d14285ecda87b0e17c759bab31e33fe34e0` |
| Recipe `prepare-webrtc.ps1` | `0f21a22184ea6e21b93dc412bd9d51dff0382e7dd68e8a7fe86d3efd14c54bf3` |
| Base archive (unchanged) | `5c2349c960bae4f06f71c102f58552d0d09c912a142811acfcadfb7c883cbf58` |

Compiler: MSVC `cl.exe` 19.44.35226.0 (SHA-256 `a80f5e7a…`), the same as the October generation. Proof logs are in the work folder beside it: `proof-old-sdk` (the failing reproduction against `build-8ab76c0925024e3aa37ab7fcc5bc4a87`) and `proof-new-sdk` (all checks passing). The client built against this generation (`cargo build -p relay-client`), its `rtc` unit tests passed (46 passed, 0 failed), and `scripts/test-rtc.py` ran three opt-ins with encrypted media between local peers through the pinned LiveKit 1.13.10 SFU on owned unused ports: `actual_sfu_protected_video_rejects_plaintext_wrong_key_and_unknown_sender` (44 frames decoded with the right key; plaintext publication, wrong key and unverified sender rejected), `actual_sfu_synthetic_video_room_scope_stop_rejoin_and_revoke` and `actual_sfu_generated_application_audio_hidden_video_and_cleanup`. All passed. The guard-page check covers reads before and after the frame; the H.264/H.265 re-check after unescaping guards a read inside a heap buffer and is not separately shown by a guard page.

DataPacketCryptor's separate IV code is unchanged. Relay text/files use its own MLS/AEAD protocols; no new use of the SDK data cryptor is introduced. Physical cameras, long calls, WAN behavior and independent review remain separate acceptances under tasks 05/07. The full integration result is recorded separately from the historical reproduction.
