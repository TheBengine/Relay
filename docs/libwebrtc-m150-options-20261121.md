# libwebrtc M150 and the SFU's DTLS: three options (21 November 2026)

Covers findings **W-15** (libwebrtc M150 is behind 17 Chrome WebRTC CVEs) and
**W-16** (the SFU's `livekit/dtls v3.1.5-warp.3` predates the 16 Oct pion/dtls
fixes) from the [5 November dependency review](dependency-advisories-20261105.md)
and the [31 October security review](security-review-20261031.md). The
assumption behind the recommendation is at the end.

## What LiveKit has published since 5 November (checked 21 Nov)

| Item | Finding |
| --- | --- |
| Newest libwebrtc prebuilt | `webrtc-83af826` (10 Nov), built from `webrtc-sdk/webrtc` `m150_release`. Still **M150**. Windows asset 114,361,384 bytes. |
| Newest prebuilt that `webrtc-sys` pins | `webrtc-sys` 0.3.55 (18 Nov, `livekit` 0.12.0) still sets `WEBRTC_TAG = "webrtc-a545de4"` (22 Oct, M150). |
| M151 or later | None. `webrtc-sdk/webrtc` has no branch newer than `m150_release`; there is no `m15x` issue or PR in `livekit/rust-sdks` or `webrtc-sdk/webrtc`. M144 → M150 took LiveKit from 14 to 19 Aug, so the next jump may come without notice, but nothing signals it. |
| Cryptor fixes on M150 | `51c367e` (13 Oct) adds #297 (W-13: verify server-injected frame payloads) and #311 (no key material in logs). `a545de4` adds #302 (`key_size`, 128 default, 256 opt-in) and #317 (drops the distrusted eMudhra roots from the bundled `ssl_roots.h`). `83af826` also has #324 (W-14: drop frames too short to decrypt), #327, #328 and #321. |
| Chrome WebRTC CVEs of W-15 | **Not** in any LiveKit build: the commits on `m150_release` since `89d790b` are the cryptor, Apple, Android and eMudhra items above, not Chromium security merges. |
| SFU | LiveKit 1.13.11 (25 Oct) is still the newest release and still pins `livekit/dtls v3.1.5-warp.3`. LiveKit `master` has moved on: `livekit/dtls v3.1.15` (23 Oct, resumption and cookie fixes) is in its `go.mod` as an indirect module, `pion/dtls/v4 v4.0.0-rc.8` is a direct one, and a 20 Nov commit builds with Go 1.27.4 (CVE-2026-97054). No release carries any of it. |

Relay today: `webrtc-89d790b` (M150, 19 Aug), SHA256 `5c2349c9…cbf58`, pinned in
`scripts/fetch-webrtc.ps1`, `vendor/webrtc-sys-build/src/lib.rs` (`WEBRTC_TAG`) and
`scripts/prepare-webrtc.ps1` (`$baseHash`); the cryptor object is replaced by
`vendor/relay-frame-crypto/frame_crypto_transformer.cc`. The SFU is Relay's own
build of LiveKit 1.13.10 (`deploy/sfu-security/`).

## Option A: wait for a newer LiveKit libwebrtc build

**What it takes here.** Nothing until LiveKit publishes. After that, per prebuilt:

1. `scripts/fetch-webrtc.ps1`: new URL, `$expected` SHA256, cache folder name
   (`work\relay-webrtc-89d790b` is also hard-coded in `scripts/test-native-sdk.py`).
2. `vendor/webrtc-sys-build/src/lib.rs` `WEBRTC_TAG`, and the vendored
   `webrtc-sys-0.3.47` and `libwebrtc-0.3.50` rebased to the matching upstream
   release (`webrtc-sys` 0.3.55 for `a545de4`). Relay's `webrtc-sys` patch
   (`create_apm_with_ns_level`) and `audiopus_sys` `livekit-opus` feature must be
   carried over.
3. `vendor/relay-frame-crypto`: take the new upstream
   `frame_crypto_transformer.cc`, re-apply only the random-IV change (the #297,
   #324 and #311 ports then come free), update `$patchHash` and `provenance.json`,
   and confirm the archive member name used by `prepare-webrtc.ps1`
   (`obj/api/crypto/frame_crypto_transformer/frame_crypto_transformer.obj`) still exists.
4. [native-frame-crypto.md](native-frame-crypto.md) and
   [native-dependencies.md](native-dependencies.md): new tag, digest, file count
   (38,755 today), regenerated `docs/LICENSE-libwebrtc.md` and integrity JSON.
5. Rebuild, then `scripts/test-native-cryptor.py` and the native RTC fixtures.

**What breaks.** For a same-milestone build (`a545de4` or `83af826`): the
hashes and pins above, and `KeyProviderOptions` gains `key_size` (#302), so the
wrapper must come from the matching `webrtc-sys`, not from the vendored 0.3.47.
Not verified here: whether 0.3.47 compiles against the new header. For a
future M15x build: also any libwebrtc API removed since M150, which shows up as
C++ compile errors in the `webrtc-sys` wrapper.

**Closes.** A same-milestone build closes W-13, W-14 and the eMudhra roots, not
W-15. Only an M15x build closes W-15. W-16: wait for a LiveKit server release
that drops `v3.1.5-warp.3`, then rebuild per `deploy/sfu-security/` (Go 1.27.4).

**Cost / risk.** Cheap per step, but the date is not ours; W-15 stays open
while we wait.

## Option B: build a newer milestone ourselves from the LiveKit fork

**What it takes here.**

1. A libwebrtc checkout and Windows x64 release build (depot_tools, a recent
   Chromium WebRTC branch-head, MSVC), then LiveKit's patch series from
   `m150_release` ported onto it. That series is not small: the M150 upgrade
   itself was a squashed "patch from m144_release", and the fork has
   Windows-relevant changes (ADM, `external_audio_source`, frame cryptor,
   simulcast factory, field trials). Expect days of conflict resolution on the
   first port and hours per build; disk need is tens of GB (estimate, not
   measured). Needs a free workstation: other build jobs on this PC would make it slower.
2. Package the result like LiveKit's `webrtc-win-x64-release.zip` (headers,
   `lib/webrtc.lib`, license aggregate) so that steps 1 to 4 of Option A apply
   unchanged, with Relay's own release in place of `livekit/rust-sdks`:
   `fetch-webrtc.ps1` then downloads from a Relay-controlled location.
3. For W-16, the SFU equivalent is to build LiveKit `master` (dtls v3.1.15 or
   v4) with Go 1.27.4, extending the `deploy/sfu-security/` patch and recipe,
   then re-run its Windows/Linux tests.

**What breaks.** Everything in Option A, plus: the `webrtc-sys` wrapper against
M15x headers (nobody upstream has done this yet, so there is no reference
patch), LiveKit's Windows-specific patches that may not apply, binary size and
codec set (H.264/H.265 defines in `prepare-webrtc.ps1` must match the new
build), and the aggregate third-party license text. The result is a Relay-only
libwebrtc that nobody else exercises, so media-quality regressions (jitter
buffer, AEC/APM, VP8 simulcast) would be ours to find; physical-camera and WAN
acceptance would have to be redone.

**Closes.** W-15 for the CVEs fixed in the chosen branch-head, W-13 and W-14 by
construction. W-16 only with the SFU part.

**Cost / risk.** Highest. Reasonable only with a person who can own a libwebrtc
build for months, because every Chrome release after the one we pick
re-opens the same finding.

## Option C: stay on M150 and document the residual risk

**What it takes here.** Documentation and a decision, plus the one change that
is already owed: W-13 and W-14 still need a native rebuild (fix in the vendored
cryptor, `prepare-webrtc.ps1`, rebuild), independent of W-15. Add W-15 and W-16
to the release notes and to `docs/native-dependencies.md` as accepted residual
risk with the exposure and the re-check date.

**Exposure to state.** The vulnerable code is reachable by whoever can send
media to a client: call participants and the SFU host (W-13 shows the host can
already inject frames). Chromium bug details are restricted, so it is not known
which of the 17 CVEs sit in libwebrtc rather than Chrome's Blink glue; treat
them as applicable. For W-16, a participant with a valid media ticket can reach
the SFU's DTLS handshake, and Relay's SFU is run by the community host, not
exposed anywhere else by default.

**What breaks.** Nothing technically. The security statement in the docs
changes: "the native client contains known unfixed Chromium WebRTC issues" must
be visible before a stable release goes to people outside the household.

**Closes.** Nothing. Cost is zero now, and the risk grows each month.

## Recommendation

**C now, with A taken at the next native rebuild, and B only on a trigger.**

1. Do the W-13/W-14 native rebuild on the newest M150 build that
   `webrtc-sys` pins (`webrtc-a545de4` with `webrtc-sys` 0.3.55; use
   `83af826` only if #324 is wanted from upstream rather than ported). It
   brings the cryptor fixes, #311 and the eMudhra roots, and keeps the milestone
   unchanged, so the media quality risk is small. It does not close W-15.
2. Record W-15 and W-16 as accepted residual risk in the release notes, with
   the exposure above, until the trigger fires.
3. Re-check weekly: `webrtc-sdk/webrtc` for a branch newer than `m150_release`,
   `livekit/rust-sdks` for a `webrtc-*` tag that is not M150, and
   `livekit/livekit` for a release after 1.13.11. When LiveKit publishes M15x,
   take Option A for it. Take Option B only if there is still no M15x
   build **and** a release is planned for people outside the household; a first
   port of the fork's patches is a project of its own, not a patch.
4. For W-16, building the SFU from `master` is a smaller job than B (one Go
   module set, existing recipe) but means running an unreleased LiveKit with
   `pion/dtls/v4` release candidates. Prefer to wait for 1.13.12 or the next
   minor release; revisit if that has not shipped by the next release freeze.

### Assumptions

- The user said not to ask, so this is decided, not agreed: the recommendation
  assumes the next stable release is for the household and invited friends, not
  a public launch, and that one developer cannot own a libwebrtc fork.
- Counts and dates come from GitHub on 21 Nov 2026 and from the 5 Nov review;
  the Chrome CVE list was not re-read. Nothing in this note was built or run.
- Whether 83af826 or a545de4 compiles against the vendored `webrtc-sys-0.3.47`
  was not tested.
