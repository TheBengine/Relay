# Build Relay from source

Experimental Windows x64 source snapshot. Published downloads can be older than
this source. A source publication is not a new binary release or a quality claim.

## Prerequisites

- Rust stable for `x86_64-pc-windows-msvc`.
- Visual Studio C++ Build Tools and Windows SDK; use its x64 Developer PowerShell.
- Python 3.11+ for tooling, CMake, and Go 1.27.3 or a compatible newer toolchain.
- Git and prepared dependency caches for offline builds.

Start in the repository root. Cargo dependencies are pinned by `Cargo.lock`;
the iroh helper has its own lockfile. Native dependency patches are in `vendor/`.
Do not remove the patches or silently replace a failed download with another SDK.

## Minimal development build

With the C++ tools available, build the community server and protocol without
opening a microphone or contacting the public account service:

```powershell
cargo build --locked -p relay-server -p relay-protocol
cargo test --locked -p relay-protocol
```

The client includes a native-media feature. The minimal client development
configuration can be built with `cargo build --locked -p relay-client` but does
not represent the complete shipping native-media package.

## Complete Windows package inputs

`scripts/build.ps1` assembles the client, server, launcher, Go network helper and
optional iroh helper. It expects prepared tools/caches and the pinned SFU binary:

```powershell
.\scripts\build.ps1 -BuildDirectory 'C:\Relay-build\target' -OutputDirectory 'C:\Relay-build\output' -SfuCacheDirectory 'C:\Relay-build\sfu' -GoExecutable 'C:\tools\go\bin\go.exe' -GoModuleCache 'C:\Relay-build\gomodcache' -GoBuildCache 'C:\Relay-build\gocache'
```

These are example paths that you choose and prepare, not tools supplied by the
repository. Set `-CMakeDirectory` if CMake is not on your PATH. The Windows build
uses static CRT. WebRTC acquisition and preparation are checksum-bound by
`scripts/fetch-webrtc.ps1` and `scripts/prepare-webrtc.ps1`; its exact native
licenses are retained in `docs/LICENSE-libwebrtc.md`. Read
`deploy/sfu-security/README.md` and its pinned recipe to prepare the required
SFU artifact; the build does not substitute an older unreviewed server.

The Go helper build intentionally uses prepared caches, `GOTOOLCHAIN=local`,
`-mod=readonly` and offline module resolution. Prepare its locked dependencies
with your Go toolchain before invoking the complete build. The iroh helper is
built from its own `iroh-helper/Cargo.lock`. No toolchain is installed implicitly.

`RELAY_ACCOUNT_ISSUER` and `RELAY_ACCOUNT_CLIENT_ID` select a public OIDC realm
and public desktop client ID at compile time. They are public configuration,
never a place for client secrets. A private build can use its own service. Do not
run named-service opt-in tests against a maintainer's live system; they require
explicit access and fresh owned QA accounts.

## Verify a change

Use the relevant crate tests and scripts after implementing the change. Native
and live-service tests are opt-in. Use disposable profiles and generated media;
physical devices and other people's servers require their permission. Broad
synthetic test success is not real two-PC/WAN or headset acceptance.

Generate current Windows third-party notices with
`python scripts/generate-notices.py --offline` after preparing the dependency
and upstream-notice caches. Dependencies keep their own terms; see
[licensing](../LICENSING.md). The updater starts disabled and the public root is
unconfigured. Do not ship test keys, personal data or a falsely signed installer.
