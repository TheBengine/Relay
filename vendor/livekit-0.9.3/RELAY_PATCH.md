Relay local patch to LiveKit Rust SDK 0.9.3

Base: crates.io archive SHA-256 de12597fd22a31e2401309d9461f3129bcbc21f3cfa55d7d5139d6b4dedd5d5a. All extracted base files matched the existing compiled registry source before patching.

Adds opt-in TrackPublishOptions.stereo (default false), forwarded only for audio publications to AddTrackRequest.stereo. Relay enables it only for selected application audio. No mono microphone default, bitrate, E2EE, lifecycle, queue or media topology change.

Upstream: https://github.com/livekit/rust-sdks ; Apache-2.0. Existing notices/license are retained. Local generated-media acceptance is recorded separately in Relay evidence; no WAN/hardware/listening claim.
