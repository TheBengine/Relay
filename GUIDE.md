# Using Relay

[Back to the short overview](README.md) · [Relay vs Discord](COMPARISON.md) · [Privacy](SECURITY.md)

## Download

| Package | Best for |
| --- | --- |
| [**Relay-Setup.exe**](https://github.com/TheBengine/Relay/releases/download/v0.10.0-preview.20261018.1/Relay-Setup.exe) | Recommended: install or update Relay like a normal Windows app. |
| [Relay-Portable.zip](https://github.com/TheBengine/Relay/releases/download/v0.10.0-preview.20261018.1/Relay-Portable.zip) | Extract the whole folder and open `Relay.exe`. Keep the files together. |
| [SHA256SUMS.txt](https://github.com/TheBengine/Relay/releases/download/v0.10.0-preview.20261018.1/SHA256SUMS.txt) | Check that downloaded files match the release. |

This is an **unsigned preview**. Real audio hardware, extended streaming and calls between different homes still need practical acceptance. Local synthetic media tests do not establish that these work on every PC or network.

## Get started

1. **Choose your name.** Relay asks for your preferred name on first launch.
2. **Create or join.** Choose **Create a server**, or **Join friends** if you have an invitation. Server creation offers an encryption choice; end-to-end encryption is recommended and selected by default.
3. **Invite your friends.** The host opens **Invite friends → Share and copy invitation**, then sends the invitation. Friends create their own account on that community.

With an HTTPS invitation and Relay installed, your friend chooses **Open Relay** in the browser. Already have a `relay://` invitation? The [invitation page](https://thebengine.github.io/Relay/) can turn it into a clickable link. Pasting the original invitation in **Join friends** remains available.

**Keep the host computer on.** Closing or updating the host interrupts access while its server is stopped. A private LAN address alone will not connect a friend in another home.

<details>
<summary><b>Connection choices</b></summary>

- **Without accounts (iroh):** chat, voice, camera and screen sharing without a separate networking account. An external relay may be used if direct connectivity is unavailable. Both PCs need the updated media-capable preview.
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

1. Close the Relay window.
2. Run the newer **Relay-Setup.exe** over your installation. The installer offers **Stop my local server and continue** for the installation's local server. Friends will be disconnected during the update.
3. Open Relay from the Start menu or your shortcut. Update both PCs for media fixes.

The installer preserves accounts, settings, device keys and server data. Uninstalling through Windows also leaves your data. Automatic background updates are not enabled.

## Current limits

- Windows only, experimental and unsigned; no independent security review of Relay's integration yet.
- Hardware listening tests, long camera/screen-sharing sessions and reliable WAN reconnects still need practical acceptance.
- Newer development work is not automatically included in the download. The published preview currently uses private rooms that both participants join; incoming-call ringing and later account work should not be assumed to be released.
- Losing local device keys cannot be repaired with an account password or server backup alone.
- Public connection helpers and relays can still be involved. Self-hosted does not mean every connection is direct or independent of all infrastructure.

---

This repository contains the project's presentation, artwork and downloadable programs. **The application source is not published here yet.** Relay is independent of Discord. Online GIF search uses GIPHY.
