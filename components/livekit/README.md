# Bundled LiveKit component

Relay's Windows preview uses LiveKit 1.13.10 with a small, reproducible dependency patch.
The patch changes Go to 1.27.3, x/net to 0.60.0 and the displayed component version;
it does not change media code. The Go rebuild includes the certificate-distrust
correction in [GO-2026-6809](https://vuln.go.dev/ID/GO-2026-6809.json).

- [Exact source, compiler, build arguments and hashes](recipe.json)
- [Patch against the pinned upstream source](livekit-1.13.10.patch)
- [Upstream source and Apache 2.0 license](https://github.com/livekit/livekit/tree/e5655ac20267fa5005dccaf3f0e3bfc3ea688c07)

Verify the upstream archive hash, extract ordinary files into an empty folder,
apply the patch preserving LF bytes and verify all three patched file hashes.
Use the official Go 1.27.3 Windows amd64 compiler with CGO disabled, the recorded
build arguments and checksum-verified modules. The pinned Windows executable
hash is in the recipe. Third-party notices accompany the downloadable app.
This is component provenance, not an independent audit or Linux deployment.
