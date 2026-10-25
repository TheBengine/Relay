# Building and verifying Relay

Current build guidance: 21 October 2026. Use the exact locked dependencies and reviewed prepared native inputs.

## Build and verify

Install Rust stable MSVC, Visual Studio C++ Build Tools with a Windows SDK, CMake, and Go compatible with the minimum in `network-helper/go.mod` (minimum 1.27.2, including the October security fixes). Run from an x64 Visual Studio Developer PowerShell so the C++ compiler and Windows SDK are configured. Prepare Rust dependencies, a Go module cache and the reviewed native SDK before an offline build. The build does not install tools or download Go modules/toolchains.

When tools are already configured on a permitted local `PATH`, set `GOMODCACHE` and `GOCACHE` explicitly, then run:

```powershell
.\scripts\build.ps1 -SfuCacheDirectory 'C:\prepared\relay-sfu'
```

For the prepared tools on this Windows workspace, this single command uses explicit paths, an isolated Cargo target, and a separate output folder:

```powershell
.\scripts\build.ps1 -BuildDirectory 'C:\Important\Projects\2026-10-03\can\work\relay-local-build' -OutputDirectory 'C:\Important\Projects\2026-10-03\can\work\relay-local-output' -SfuCacheDirectory 'C:\prepared\relay-sfu' -CMakeDirectory 'C:\Important\Projects\2026-10-03\can\work\tools\cmake-4.4.4-windows-x86_64\bin' -NativeSdkDirectory 'C:\Important\Projects\2026-10-03\can\work\relay-next\native-frame-production-20261006\build-8ab76c0925024e3aa37ab7fcc5bc4a87\sdk' -GoExecutable 'C:\Important\Projects\2026-10-03\can\work\embedded-network-20261013\tools\go\bin\go.exe' -GoModuleCache 'C:\Important\Projects\2026-10-03\can\work\embedded-network-20261013\gomodcache' -GoBuildCache 'C:\Important\Projects\2026-10-03\can\work\embedded-network-20261013\gocache'
```

For the shared Relay account service, explicitly set `RELAY_ACCOUNT_ISSUER` to the reviewed HTTPS realm URL and `RELAY_ACCOUNT_CLIENT_ID` to its public desktop client ID before compiling. These are public configuration values, not credentials. A self-hosted build may choose another issuer or retain independent server accounts.

Run it from this repository's root. The example paths describe prepared local tools; they are not installed or discovered by the script. An explicit `GoExecutable` is invoked by its absolute path. An explicit `CMakeDirectory` binds `cmake.exe` through `CMAKE`, both target-specific variable spellings, `HOST_CMAKE`/`TARGET_CMAKE`, and the build's temporary `PATH`. Module/build caches come only from the explicit parameters or the caller's `GOMODCACHE`/`GOCACHE`; missing or redirected paths fail before Cargo compilation. `GoModuleCache` must already exist; Go may create the chosen build-cache directory. The helper uses `GOTOOLCHAIN=local`, `-mod=readonly`, and offline module resolution, so missing dependencies/checksums fail with a cache-preparation error. It ignores hidden Go configuration, inherited Go flags/workspaces and external cache programs. `GOTMPDIR`/`TEMP`/`TMP` are bound to a guarded temporary directory inside the selected build cache. It restores the caller's environment on success or failure, removes only its empty temporary directory, and preserves nonempty failed work for inspection.

The script builds the native-RTC release with static CRT, normally stores intermediates under `work/relay-build-rtc`, and publishes the Relay client, server, private-network helper and launcher together. Without `NativeSdkDirectory` it prepares the pinned checksum-verified WebRTC SDK; the native build requires the prepared, hash-pinned SFU security build described in [its recipe](../deploy/sfu-security/README.md). Pass `-SfuCacheDirectory <prepared-sfu>` to build.ps1. It verifies and stages that binary and license before expensive compilation, and refuses older or missing SFU inputs. WebRTC downloads are separate from the offline Go helper. Every reused WebRTC file is checked by the build script. The native dependency licenses ship alongside Rust [dependency notices](../THIRD_PARTY_NOTICES.txt); see [native dependencies](native-dependencies.md). Set `CARGO_NET_OFFLINE=true` in the caller when Rust must also use only prepared dependencies.

```powershell
cargo test --workspace
python .\scripts\test-integration.py --server .\dist\relay-server.exe
python .\scripts\test-hosting.py
python .\scripts\test-backup.py
python .\scripts\test-update.py
python .\scripts\test-package.py
python .\scripts\test-shell-runtime.py
python .\scripts\test-data-privacy.py --server .\dist\relay-server.exe
python .\scripts\test-native-sdk.py
```

Opt-in real-server and owned-window/RTC fixtures have explicit environment/setup requirements in verification. Tests use isolated `work` folders and owned processes, not a running community's data. `test-soak.py` exercises 100 authenticated simulated clients on localhost; it does not represent 100 physical voice/video users.

`scripts/package.ps1` produces a source-included ZIP, binary manifest and SHA256 sidecar, excluding runtime credentials, databases, logs and build artifacts. It derives the release version from the workspace unless `-Version` is supplied. Supply the exact tested client/server binaries with `-ClientBinary` and `-ServerBinary`.

Five matched text-idle pairs measured about 93.00 MiB working set and 198.50 MiB private memory for the 0.3.0 client; this historical run showed no RAM reduction over 0.2.1. It is not a measurement of this release. Resource measurements remain scenario-specific. Include the client, Relay server and SFU, and measure CPU, memory, GPU, startup and end-to-end latency before claiming an improvement. The [quality gates](quality.md), [architecture](architecture.md) and [remaining roadmap](roadmap.md) distinguish implemented work from unverified acceptance.

The archive is unsigned and has no automatic update service. Obtain its checksum from a trusted source before running or updating. See the [0.9.1 release notes](release-0.9.1.md), [public beta checklist](public-release.md), [known issues](known-issues.md), preserved [dependency review](dependency-review-0.6.0.md), [security policy](SECURITY.md) and [work status](tasks/STATUS.md). TLS/transport dependencies and the Windows SFU changed in the [21 October scoped dependency review](dependency-security-20261021.md); its residual findings and acceptance limits still apply. Clean Windows installs, real WAN calls and personal audio/video hardware remain acceptance work. No external publication is performed by these build scripts.

Relay-authored code is source available under PolyForm Perimeter 1.0.0; see [licensing scope](../LICENSING.md). Relay is independent of Discord or Mumble. Dependencies retain their own licenses. The encrypted-content integration has not received an independent Relay security review. Server operators still see identities, profiles, targets, membership, message/file sizes and timing, and can deny service. Legacy plaintext is not retroactively encrypted. Relay makes no anonymity, Tor/Tails, endpoint-compromise or one-millisecond latency claim.

## Windows runtime

The shipping MSVC build uses the static C++ runtime. It still depends on supported Windows system APIs and graphics/audio drivers. A clean-machine acceptance run is separate from development-machine testing.
