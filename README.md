<picture>
  <source media="(max-width: 640px)" srcset="assets/relay-banner-compact.svg">
  <img src="assets/relay-banner-en.svg" alt="Relay — Your friends. Your server. A self-hosted Discord alternative for chat, voice and screen sharing." width="100%">
</picture>

**[Download for Windows](https://github.com/TheBengine/Relay/releases/download/v0.10.0-preview.20261026.2/Relay-Setup.exe)** · [Portable version](https://github.com/TheBengine/Relay/releases/download/v0.10.0-preview.20261026.2/Relay-Portable.zip)

Experimental, unsigned Windows x64 preview. [Release notes](https://github.com/TheBengine/Relay/releases/tag/v0.10.0-preview.20261026.2) · [Open an invitation](https://thebengine.github.io/Relay/)

## Relay vs Discord

**More control. Private text. No Relay subscription for quality settings.**

| What matters | Relay | Discord |
| --- | --- | --- |
| **Hosting & data** | You control the server and backups | Hosted by Discord |
| **Text privacy** | End-to-end encrypted in protected mode | No text E2EE |
| **4K / 60 FPS settings** | Included without a Relay subscription | Nitro feature |

New servers select end-to-end encryption by default; existing servers keep their settings. The complete Windows package includes iroh for invitation-based connections without an extra network account. Keep your host online. Actual video quality depends on hardware and connection.

Check your microphone with **Listen to Myself** in Voice & Video, choose built-in or custom event sounds in **Sounds**, and inspect measured video upload in **Voice & Video > Debugging**.

[Full comparison & sources](COMPARISON.md) · [Setup & updates](GUIDE.md) · [Privacy & security](SECURITY.md)

## Get started

1. **Create a Relay account.** No invitation needed.
2. **Open Friends** for private messages and calls, or create/join a community.
3. **Share an invitation.** Your friend opens the link, chooses **Open Relay**, and joins. Keep the host online.

If the browser does not open the app, copy the invitation from the page and paste it into **Join a server**.

## Source and contributions

Relay's client, server, connection helpers and reviewed dependency patches are
now available in this repository under **PolyForm Perimeter 1.0.0**.
You can inspect the code and contribute fixes. Using the covered code to offer a
competing product is excluded, including free competing products. This is
**source available**, rather than OSI open source.

[License and earlier MIT rights](LICENSING.md) · [Contribute](CONTRIBUTING.md) ·
[Build from source](docs/building.md)

The latest preview includes the matching application source. Hardware, WAN latency
and resource-usage comparisons still require measurements on real devices.
