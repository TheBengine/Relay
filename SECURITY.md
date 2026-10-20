# Privacy and security in Relay

## Who can read the content?

**Protected communities:** content is encrypted on participant devices. Approved devices hold the keys; the Relay community server forwards encrypted content. Participants can still save or share what they receive.

**Open communities:** the community server can read messages, files and calls that participants agree to send without end-to-end protection. The change is announced, and each participant must explicitly accept it. Switching to open mode again requires fresh acceptance. End-to-end encryption is recommended and selected by default when creating a server.

## What does an invitation prove?

A new one-use invitation can confirm the host's and friend's devices automatically. Send it through a trusted contact route to the intended person. A leaked, unused invitation can give someone else access and automatic device confirmation.

Compare the security number under **Settings → Encryption**, in person or through an existing trusted call. Check again if a device or number changes. Confirming one device does not approve every future device.

## What do iroh and Tailscale protect?

They protect transport between computers, including for an open community. Relay's content end-to-end encryption is a separate layer between approved participant devices, protecting that content from the community server. Selecting a route does not itself disable content encryption.

Direct connections are not always possible; relays, discovery services and network providers can remain involved. The server still sees accounts, profiles, membership, sizes and timing. Relay does not provide anonymity or retroactively encrypt older plaintext history.

Encryption is not unique to Relay: [Discord also supports end-to-end encrypted audio/video calls](https://support.discord.com/hc/en-us/articles/25968222946071-End-to-End-Encryption-for-Audio-and-Video). Relay's self-hosted community backend is a separate choice.

## Can my password recover device keys?

No. Protect your local profile and device keys. A community server backup or account password alone cannot restore lost private device keys.

## Distribution and review

The published Windows preview is unsigned. No new Microsoft Defender scan was performed for the 18 October 2026 preview files; earlier scans applied to different, precisely identified files.

Relay's integration has not had an independent security review. SHA-256 checksums identify the published files; they do not prove the absence of defects or malware. Local synthetic checks do not establish real hardware/WAN acceptance.

[Download and get started](README.md) · [Releases](https://github.com/TheBengine/Relay/releases)
