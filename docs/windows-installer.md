# Install and update Relay on Windows

Download an installer from [published releases](https://github.com/TheBengine/Relay/releases).
It installs for your Windows user, adds a Start menu shortcut and offers an optional desktop shortcut.
The portable ZIP is an alternative; extract the complete folder before opening `Relay.exe`.
Local candidates prepared by Codex are not published releases until explicitly uploaded.
The installer displays **TheBengine** as publisher. This label does not establish
a verified Windows signing identity.

## Update

The `.7` bootstrap is being prepared and is not yet a published release. Once
published, users of `.6` and earlier need to install it manually once, using the
steps below. That installation adds the trusted publisher configuration for
subsequent in-app updates.

The new flow checks for updates after startup and every six hours. Choose
**Update and restart** in the top notice to download and verify the update,
close Relay and its own hosted servers normally, update the existing installation,
and reopen Relay. No installation guide is needed for this verified in-place
flow. Leave calls and finish pending work first. Hosting is briefly interrupted.
**Later** postpones the choice; **Download only** in **Advanced → Updates** saves
the verified download without installation consent. Automatic checks can be
turned off there. No update is installed just because Relay started.

Portable copies require a manual update. The app will not silently convert a
portable copy into an installed copy or choose a different installation folder.

### Manual installation or bootstrap

Right-click Relay in the notification area and choose **Quit Relay**. The window
disappears while its hosted servers finish closing. The window's cross normally
hides the app; change this under **Settings → Advanced → Window behavior**.

Run the newer installer. Leave **Stop my local server and continue** enabled if
you want setup to request normal shutdown of the server from that installation.
The helper verifies its executable, process and server records; it does not kill
an unrelated server. If the app remains open or identity cannot be established,
setup keeps your data and asks you to close it and retry.

Setup reuses the existing installation and rejects older build sequences over
newer ones. Your profiles, encryption keys, messages and server data stay outside
the installed program files. You do not need to uninstall first.

## Remove

Use **Windows Settings → Apps → Installed apps → Relay → Uninstall**.
The program and its registered shortcuts are removed; profile and server data are preserved.

## Preview limits

The installer has no Authenticode signature, so Windows may warn during the
initial manual installation. The prepared TUF update channel separately verifies
publisher-approved update bytes; it does not remove Windows reputation warnings
or provide a Windows-verified publisher identity. A checksum alone is not a
publisher signature.

Candidate checks and publication are recorded separately. Development fixtures
do not prove every clean Windows installation. Exact build identity, hashes and
checks belong in the release receipt. The publisher must refresh its 30-day update
metadata before expiry; no automatic publisher refresh is scheduled yet.
