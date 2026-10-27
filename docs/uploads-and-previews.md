# Files, previews and upload limits

Selecting a file adds a local review card above the composer. Images are identified
from their contents, so a PNG named `rh` can still show a preview. Review the image,
name and size, then choose **Upload selected files**. Send the message after its
files finish uploading. Text and replies remain in the composer. Send stays
disabled while selected files still need review or upload.

File review runs in the background. Ordinary phone photos get a scaled thumbnail;
GIF files retain their original animation. A file that cannot be previewed shows
an explanation and can still be uploaded if its size is allowed. Preview decoding
has independent byte, pixel and memory bounds. Chat files are never resized or
converted as a side effect of showing a thumbnail.

The upload worker checks the reviewed size and SHA-256 against the exact bytes it
will send. If the file changed, remove it and select it again. Failed uploads keep
their review cards and the message draft. Removing a file or changing accounts
invalidates late review/upload results. Completed files belong to their original
conversation even when another conversation is now open. At most two transfers
run at once.

## Server and role allowances

Server administrators can configure **Server Settings → Uploads**. The page sets
the server ceiling, the default member allowance and an optional allowance for
each role. These settings are separate from the permissions to manage roles.
Only built-in administrator accounts may change the policy.

Role entries may inherit the default, disable uploads, or specify an allowance.
When several assigned roles have explicit values, the largest value applies;
the server ceiling always caps it. An explicit zero disables uploads only when
no other assigned role explicitly grants a positive allowance. Administrators
use the server ceiling. Deleted roles have no effect.

The supported ceiling is 25 MiB per transferred file, including the 24-byte
encryption overhead for protected files. The default remains 10 MiB. There is a
separate 50 MiB limit for files uploaded by one account but not yet attached to
sent messages, as well as the host's existing account and total storage quotas.
Larger historical files remain downloadable when an administrator lowers a limit.

The server authenticates and checks an account's allowance before reading its
body, then checks current roles and policy again in the transaction that stores
the file. Settings saves use revisions to reject conflicting edits. The policy
is included in backups, host transfers and filtered replica snapshots/deltas;
schema 28 updates existing replica metadata triggers.

Older servers retain their 10 MiB behavior and need a server update to expose
the new controls. Updating a client does not upgrade a separately hosted server.

## Icons and message links

Profile and server images are cropped, scaled and normalized to PNG locally
before upload. Icons/avatars use a maximum of 256 pixels per side; server banners
use a bounded 1200 × 480 output. ICO, BMP and WebP input support is enabled alongside
the existing image formats. Profile edits still require their normal Save action.
Chat images keep their original format and bytes.

Messages can show up to three HTTPS link cards, including recognized video, music
and code-hosting links. Cards are derived locally from the URL and open only on an
explicit click. They do not fetch arbitrary website metadata or remote thumbnails.
Links in code and unrevealed spoilers do not generate cards.

Direct Messages and authenticated startup use the regular Friends shell while
connecting or showing Retry. The welcome/server picker no longer replaces that
view. Cancelling navigation preserves remembered sessions; actual sign-in,
account cleanup and explicit server-creation/invitation flows keep their guards.

Presentation-image selection accepts static PNG, JPEG, WebP, BMP and ICO inputs
up to 25 MiB, 16 megapixels and 8192 pixels per side. The crop editor first
reduces its working image to at most 2048 pixels per side, without enlarging
small originals. Avatar, icon and banner uploads are normalized to PNG with a
2 MiB output limit; these conversions do not change chat attachment files.

## Checked in the running app (26 Oct 2026)

Two owned QA profiles (owner and member) against a local community and Keycloak:
an extensionless PNG shows a review card preview before upload and an inline
preview after it; the Uploads page lists the ceiling, the member default and one row
per custom role; a member with a 20 MiB role may upload 12 MiB, the same member
without the role (4 MiB default) gets HTTP 413, and the owner may upload past the
member limits up to the ceiling; a 1600 × 900 JPEG became a 256 × 256 PNG server icon
and an ICO file became a PNG profile avatar; two pasted URLs produced link cards.
Receiving previews on a second device needs that device to be verified first (the
end-to-end encryption approval is unchanged).
