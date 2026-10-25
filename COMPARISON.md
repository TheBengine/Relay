# Relay vs Discord

**Relay is the better fit for groups who want their own community backend, end-to-end encrypted text and quality controls without a Relay subscription.** Those are concrete reasons to choose Relay today.

Comparison checked on **21 October 2026**. Relay refers to the published experimental Windows preview, not unreleased account or private-calling work.

## Where Relay gives you more control

| What matters to your group | Relay | Discord |
| --- | --- | --- |
| **Run the actual community backend** | Host on your Windows PC or a separate machine you choose. | The community backend runs on Discord's infrastructure. [Hosting architecture](https://discord.com/blog/behind-the-scenes-of-the-3-25-26-voice-outage). |
| **Choose where community data and backups live** | Your host stores the community database; you choose where to keep backups. | Discord manages the backend storage. A personal data export is available, which is a different level of control. [Message storage](https://discord.com/blog/how-discord-stores-trillions-of-messages), [personal export](https://support.discord.com/hc/en-us/articles/360004957991-Your-Discord-Data-Package). |
| **End-to-end encrypted text message bodies** | Included in the optional protected mode; approved devices hold content keys. | Text messages are not end-to-end encrypted. Discord encrypts them in transit and at rest. [Text E2EE position](https://discord.com/blog/every-voice-and-video-call-on-discord-is-now-end-to-end-encrypted), [privacy policy](https://discord.com/privacy). |
| **4K / 60 FPS screen-sharing settings** | Available without a Relay subscription. Actual output depends on hardware, bandwidth and load. | Discord documents up to 4K/60 for Nitro subscribers; free streaming and server boosts have other limits. [Screen-sharing quality](https://support.discord.com/hc/en-us/articles/360040816151-Go-Live-and-Screen-Share), [boost tiers](https://support.discord.com/hc/en-us/articles/360028038352-Server-Boosting-FAQ). |

For a group that values **hosting ownership, private text or subscription-free quality settings**, these are Relay's advantages over Discord. Running your own host can still cost electricity or hosting fees. Available quality settings do not prove sustained frame rate or better picture quality.

## What the comparison also needs to show

| Area | Relay | Discord |
| --- | --- | --- |
| **Audio/video E2EE** | Available in protected communities. The owner can explicitly choose an open mode, with participant acceptance. | Ordinary calls use DAVE E2EE; Discord documents an exception for Stage channels. This is not a Relay-only feature. [Discord's current coverage](https://discord.com/blog/every-voice-and-video-call-on-discord-is-now-end-to-end-encrypted). |
| **Keeping the community available** | You keep the host online, update it and protect its backups. | Discord operates the hosting infrastructure for you. [Hosting architecture](https://discord.com/blog/behind-the-scenes-of-the-3-25-26-voice-outage). |
| **Available clients** | Experimental Windows preview. | Windows, macOS, Linux, mobile and other supported platforms. [Downloads](https://discord.com/download). |
| **Security review** | Relay's integration has not received an independent security audit. | Discord publishes external DAVE design and implementation reviews. [DAVE review background](https://discord.com/blog/meet-dave-e2ee-for-audio-video). |

Relay's E2EE protects content from the community host; it does not hide membership, sizes or timing, and participants can share what they receive. Open mode lets the server read content. Backing up a community database does not recover lost private device keys. See [Relay's security overview](SECURITY.md).

## Speed and sound quality: still to be measured

No controlled Relay-versus-Discord result establishes lower RAM or CPU use, lower microphone-to-speaker latency, or better audible quality. A nearby self-hosted server can give you a useful routing choice; the actual network path, devices and processing decide the result.

Future comparisons should use the same PCs, devices, network and workload, report versions and routes, include all relevant app/server/helper processes, and report delay percentiles together with interruptions and sound quality. A ping or a 10 ms audio setting is not an end-to-end call measurement.

[Get Relay and set up](GUIDE.md) · [Back to Relay](README.md) · [Privacy and security](SECURITY.md)
