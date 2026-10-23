# Relay TLS dependency patch

Source: https://crates.io/crates/serenity/0.12.5 (ISC; LICENSE.md retained).
Official crate SHA-256: `9bde37f42765dfdc34e2a039e0c84afbf79a3101c1941763b0beb816c2f17541`.

Cargo.toml and Cargo.toml.orig change: tokio-tungstenite 0.21.0 ->
0.24.0, with the explicit `url` feature that preserves the upstream URL API.
This moves the Discord gateway from rustls 0.22/webpki 0.102 to
the shared, patched rustls 0.23/webpki 0.103 stack without changing the
Serenity API or its Rust implementation. Runtime implementation, build script and license retain upstream behavior/terms.
The token-validator documentation example now uses an obvious noncredential
placeholder accepted by the same three-part format check, replacing an upstream
Discord-token-shaped literal rejected by GitHub push protection. No token is
validated against Discord, and no runtime validation logic is changed. Unused packaged Cargo.lock and CI
metadata are omitted. Replace this patch when upstream ships a compatible
release using a supported TLS stack.

The retired stack matches RUSTSEC-2026-0049, 0098, 0099, 0104, 0346,
0349 and 0382 (some IDs alias GHSA records); version matches alone do not
establish exploitability in Relay's fixed Discord endpoint.
