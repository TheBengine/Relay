# Emoji artwork and data

Relay bundles all 3,953 fully qualified emoji sequences and components in Unicode 17.0.

Artwork: **Twemoji**, copyright Twitter, Inc. and other contributors, maintained by jdecked and contributors. Licensed under Creative Commons Attribution 4.0 International. Original artwork: https://github.com/jdecked/twemoji . Relay downsamples artwork to 48 pixels and repacks it into a transparent sprite atlas. Full license: `LICENSE-TWEMOJI`.

Shortcodes, categories and sprite positions: **iamcal/emoji-data**, copyright Cal Henderson and contributors, MIT. Original data: https://github.com/iamcal/emoji-data . Full license: `LICENSE-DATA`.

Unicode emoji names and sequences: **Unicode, Inc.**, Unicode License V3. Original data: https://www.unicode.org/Public/17.0.0/emoji/emoji-test.txt . Full license: `LICENSE-UNICODE`.

Pinned input revisions, SHA-256 hashes and modifications are recorded in `sources.json`. Rebuild with `scripts/build-emoji-assets.py`; the build uses only Twemoji graphics, including the four hair components. No Apple artwork is included.
