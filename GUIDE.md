# Using Relay

[Back to the short overview](README.md) · [Relay vs Discord](COMPARISON.md) · [Privacy](SECURITY.md)

## Download

| Package | Best for |
| --- | --- |
| [**Relay-Setup.exe**](https://github.com/TheBengine/Relay/releases/download/v0.10.0-preview.20261026.2/Relay-Setup.exe) | Recommended: install or update Relay like a normal Windows app. |
| [Relay-Portable.zip](https://github.com/TheBengine/Relay/releases/download/v0.10.0-preview.20261026.2/Relay-Portable.zip) | Extract the whole folder and open `Relay.exe`. Keep the files together. |
| [SHA256SUMS.txt](https://github.com/TheBengine/Relay/releases/download/v0.10.0-preview.20261026.2/SHA256SUMS.txt) | Check that downloaded files match the release. |

This is an **unsigned preview**. Real audio hardware, extended streaming and calls between different homes still need practical acceptance. Local synthetic media tests do not establish that these work on every PC or network.

## Get started

1. **Sign in or create a Relay account.** No invitation is needed. Sign-in opens inside the app, with a browser option available.
2. **Choose Friends, or create/join a server.** Friends lets you message and call without sharing a community. End-to-end encryption off is the recommended default for new servers. You can choose protected mode during creation; existing servers keep their settings.
3. **Invite your friends.** Open **Invite friends**, choose a reachable connection and copy the invitation, then send the link. Your friend signs in with their Relay account and joins your server.

With an HTTPS invitation and Relay installed, your friend chooses **Open Relay** in the browser. Already have a `relay://` invitation? The [invitation page](https://thebengine.github.io/Relay/) can turn it into a clickable link. Pasting the original invitation in **Join friends** remains available.

**Keep the host computer on.** Closing or updating the host interrupts access while its server is stopped. A private LAN address alone will not connect a friend in another home.

<details>
<summary><b>Connection choices</b></summary>

- **Account-free connection (iroh):** experimental source support is included, but this route is disabled in this Windows preview while practical acceptance is completed. Use a reachable HTTPS address, LAN or Tailscale.
- **With Tailscale:** both friends sign in with their own Tailscale accounts and accept sharing. A separate Tailscale app is not required.
- **Direct address:** for the same network or a server that is already reachable. Public hosting can require additional network configuration.

Use **Invite friends → Change connection** to change the route, then copy a new invitation. Relay can choose between routes included in an invitation. If a browser does not open the app, use **Join friends** and paste the invitation.

</details>

## What is included?

| Conversations | Voice & sharing | Your community |
| --- | --- | --- |
| Channels, DMs and groups | Microphone selection and push-to-talk | Local server managed through the app |
| Replies, reactions and threads | Noise reduction and volume controls | Roles and permissions |
| Files and GIF search | Camera and screen sharing | Profile and server settings |
| Search on your own PC | Quality controls | Themes and adjustable text size |

New one-use invitations can confirm the host's and friend's devices automatically. Send them through a trusted contact route. You can compare the security number under **Settings → Encryption**, in person or over a call you already trust.

## Update Relay

1. Right-click Relay in the Windows notification area and choose **Quit Relay**.
2. Run the newer **Relay-Setup.exe** over your installation. The installer offers **Stop my local server and continue** for the installation's local server. Friends will be disconnected during the update.
3. Open Relay from the Start menu or your shortcut. Update both PCs for media fixes.

The installer preserves accounts, settings, device keys and server data. Uninstalling through Windows also leaves your data. Automatic background updates are not enabled.

## Current limits

- Windows only, experimental and unsigned; no independent security review of Relay's integration yet.
- Hardware listening tests, long camera/screen-sharing sessions and reliable WAN reconnects still need practical acceptance.
- Friends, private calls and groups require a configured, compatible Friends service. Updating the Windows app does not update a separately hosted server.
- Losing local device keys cannot be repaired with an account password or server backup alone.
- Public connection helpers and relays can still be involved. Self-hosted does not mean every connection is direct or independent of all infrastructure.

---

This repository contains Relay's application source, presentation and build guidance. Relay-authored code is **source available under PolyForm Perimeter 1.0.0**; read [the licensing scope](LICENSING.md), including third-party terms and previous MIT rights. Relay is independent of Discord. Online GIF search uses GIPHY.
