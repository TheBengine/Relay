# Relay patch to audiopus_sys 0.2.2

Upstream: https://github.com/lakelezz/audiopus_sys/tree/v0.2.2
Source: the crates.io audiopus_sys 0.2.2 archive, including its ISC license and bundled Opus source/license.

The only behavior change adds the explicit `livekit-opus` Cargo feature. Relay enables it only for Windows builds with `native-rtc`, where the pinned LiveKit `webrtc.lib` already exports the standard Opus C API used by the unchanged `opus` Rust wrapper. In that configuration, this binding crate supplies declarations without compiling or linking a second Opus implementation. Missing SDK symbols remain normal link errors. Other configurations use the original upstream build and bundled Opus.

This avoids duplicate native symbols in the optimized Windows build; it does not suppress linker errors, rename symbols, or modify the codec ABI. Upstream generated bindings and codec source are unchanged. See workspace/client manifests for feature wiring.
