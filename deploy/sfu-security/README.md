# Build the pinned LiveKit component

This Apache-2.0 component retains its upstream license. Relay's source-available
license does not replace it. `recipe.json` binds the upstream commit/archive,
ordered patches, output identities and license. This is the shipping `.5` recipe;
later local candidates are not included in this source snapshot.

1. Fetch the official upstream source archive at the exact URL in `recipe.json`
   and compare its SHA-256 before extraction. Extract only regular files into a
   fresh directory you own. Retain the upstream LICENSE.
2. Apply the patches below in order with Git configured to preserve LF bytes.
3. Compare the final patched-file hashes against the recipe. Prepare its public
   Go modules with normal checksum verification using the required Go toolchain.
4. Build with Go 1.27.3, `CGO_ENABLED=0`, `GOOS=windows`, `GOARCH=amd64`, explicit
   local module/build/temp caches, `GOTOOLCHAIN=local`, `GOENV=off`, `GOWORK=off`,
   and `-mod=readonly`. Preserve the recipe's version/linker flags and output
   identity; inspect the original component recipe and build scripts before use.
5. The prepared directory passed to Relay must contain the exact binary and
   license accepted by `scripts/fetch-sfu.ps1`. A different local build must be
   deliberately reviewed and have its own consistent recipe/pins. Do not bypass
   hash checks or substitute an older binary.

Patch order:

- `livekit-1.13.10.patch`
- `friends-turn-port-20261022.patch`
- `friends-turn-observation-20261022.patch`
- `friends-turn-reply-observation-20261022.patch`
- `friends-local-node-mux-20261022.patch`
- `friends-refresh-issuer-20261022.patch`

Exact build arguments from the recipe:

```text
go build -p=2 -mod=readonly -trimpath -buildvcs=false -ldflags -s -w -buildid= -o livekit-server.exe ./cmd/server
```
