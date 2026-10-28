# Encryption in Relay 0.10.0

Relay 0.10.0 is an experimental encrypted Windows beta. New conversation content is encrypted on participating clients, before it reaches the Relay host or video SFU. Relay's integration needs independent security review before a stable public release. Encryption does not make a compromised PC safe or hide network metadata.

## First use

1. Update the client and the self-hosted server together. Older servers cannot accept protected chat, uploads or calls; the new client refuses a plaintext fallback.
2. Just message people. Since 27 October 2026 Relay trusts a person's device the first time it sees it (trust on first use), and the conversation's key manager adds it automatically while online. Nobody has to compare codes first. See [Encryption trust](encryption-trust.md) for the full rules.
3. Optional: to be sure you are talking to the right person, open **Settings → Encryption**, choose **Verify** beside the person and compare the safety number on a call or in person (or paste their code or verification card). A match marks them **Verified**. The full 64-character public code is checked; a card additionally checks the exact community, account and device. Neither contains private keys. After you verify someone, Relay holds any new device or changed code of theirs until you choose **Accept new code** or compare again.
4. A changed code of someone you have not verified is accepted and shown as "X's security code changed". A new device on your own account is trusted and announced. Join a voice channel by clicking it. Relay may wait for key delivery before opening audio. A membership change pauses media while the new keys are established; resume camera or screen sharing explicitly after a protected call is ready.

The page summary says "Messages are end-to-end encrypted" in normal use and stands out only for a change that needs your decision. It does not prove that the directory lists every device or that every conversation has received its keys, and a current encryption problem takes precedence over cached trust. Details, device revocation and reset are under **Advanced**; saved history is under **Backup and recovery**, which also remains available without signing in. A verified invitation (see below) marks the devices Verified.

The first device to create a conversation becomes its key manager. It handles admissions and removals. For an existing call, the first remaining member in the MLS tree can take over after the previous manager leaves. The handover is a signed membership update; a directory entry alone cannot authorize it. If no surviving key manager can update a group, a new channel may be necessary. This beta does not provide password-only group recovery.

## Coverage

| Content | Protection in the new client |
|---|---|
| New channel and direct message bodies | Signed, encrypted MLS application messages |
| Replies and mention controls | Inside the authenticated encrypted message envelope |
| Edits | A new MLS generation, retaining the original client ID and signed reply/file metadata |
| Reactions | Encrypted desired-state actions; only authenticated actions become displayed reaction rows |
| Chat files and GIFs | XChaCha20-Poly1305 before upload; file key, name, MIME and integrity metadata inside MLS |
| Microphone audio | Inner XChaCha20-Poly1305 protection beneath the existing UDP transport protection |
| Camera, screen and selected-window application audio | Native RTC GCM frame encryption using protected call-epoch keys |
| Local private state and saved drafts | Windows current-user DPAPI |
| Explicitly saved downloads | Authenticated first, then saved as plaintext at the chosen destination |

Public profiles, pronouns, profile/banner colors, avatars and server presentation are readable by community members and the host. Existing plaintext history has a **Legacy** label; an upgrade cannot retroactively protect what the host already stored. A message that cannot be authenticated or decrypted gets an unavailable label and no attachment/reaction preview.

The host still sees accounts, public device fingerprints and KeyPackages, channel/DM targets, membership, routing IDs, reply references, encrypted sizes, IP addresses and timing. The host can delay, reorder or suppress delivery. Local observed edit ordering prevents cached reloads from promoting an old mention, but is not a signed edit chronology or proof that the host supplied the newest version. Encryption does not provide anonymity, Tor routing or the operating-system protections associated with Tails. Use HTTPS for public hosting: E2EE content protection does not replace transport protection for login credentials and other public APIs.

## Community conversations in 0.9.0

Voice text is a durable text conversation with its own MLS group, separate from the transient call group. Its access follows the parent voice channel, while opening chat does not join media. Group DMs, their calls and threads also have separate protected groups. Membership removal prevents future delivery; it does not erase content a former member already decrypted. Archived threads reject new sends.

Poll questions, options and mention lists are message content inside MLS. Votes are authenticated encrypted reaction controls; clients project the totals after decryption. The mentions inbox scans paginated encrypted history on the client rather than disclosing message text to the server.

Roles, audit records, presentation images/emoji, status, event details and routing are host-visible metadata. Friends and blocks are owner scoped; reports are staff scoped. A report shares decrypted evidence only when the user explicitly opts in and submits it. Blocking another shared-channel author suppresses display and notifications, while processing their MLS generations preserves the shared conversation's ratchet.

## Keys, restart and recovery

Remembered logins are stored in Windows Credential Manager with the normalized address, community ID and account session. A remembered token is sent only after an anonymous health check matches the saved community ID; the authenticated bootstrap must also match that community and account before Relay opens its private actor or replaces an active connection. Unknown or conflicting IDs require signing in again and preserve private history. An older remembered login without an embedded community ID needs a previously saved community pin before it can be validated and upgraded. IDs are continuity checks; use a trusted address and HTTPS for an internet host. This does not approve another person's encryption device or replace fingerprint comparison.

Device identity combines a community ID, account ID and random device ID with an Ed25519 signing key. The fingerprint binds that identity to its public signing key. Public MLS KeyPackages are verified against the listed fingerprint. A device seen for the first time is trusted on first use; after that its key is pinned, and a different key is either announced (person not verified) or held for the user's decision (person verified). See [Encryption trust](encryption-trust.md).

The private actor saves its ratchets and exact outgoing ciphertext before publishing a message or membership change. Retries of uncertain delivery reuse that ciphertext. The most recent 512 completed send receipts retain exact bytes for duplicate retries; older duplicate IDs are refused rather than encrypted again. Incoming state, local decrypted history and group delivery cursors have durable checkpoints. Offline catch-up consumes messages from an epoch before its keys are retired. Welcomes consume one-use KeyPackages with durable, idempotent acknowledgements, so a removed device's rejoin uses a fresh package. The bounded public package stock is replenished when entering an inactive conversation; consumed packages are never reused.

Windows DPAPI protects the saved state for the current Windows user. An account-bound Windows ownership lease and an exclusive file lock prevent concurrent actors from using the same account's ratchets, including a current snapshot copied into another storage folder. Relay coordinates credential operations across processes; each revision must advance and its write is checked before publication. Atomic file replacement and a separate Windows Credential Manager marker bind the revision to the device identity. Existing state with a missing marker, an older revision, or a different device generation fails closed. A preview's counter-only marker also requires explicit device reset; it cannot authorize reopening old keys. A failed save pauses encryption; plaintext is never used as a storage fallback.

**Reset This Encryption Device** requires typed confirmation bound to the displayed community, account and device. It preserves the old protected file as a retired archive and creates a new identity. When the live actor identifies the previous device, reset durably retries revoking its registration before registering the replacement. If damaged or unavailable protected state prevents that identification, use the own-device list to revoke the previous registration explicitly. Other participants must compare the new fingerprint. A retired archive is not an automatic history recovery facility; a new device cannot decrypt previous content without its old keys. Own-device revocation is separate from removing local verification of another person's device.

Server backups contain opaque encrypted content and public delivery state, not clients' private keys. Restoring only a server database, copying a device file between Windows users, or rewinding a private snapshot is not a coordinated cryptographic recovery procedure. Preserve both sides before experimenting with restore. Same-user malware or a stolen unlocked session can read endpoint data despite DPAPI.

The beta bounds a private checkpoint to 64 MiB and its MLS provider snapshot to 32 MiB. Version 0.6.1 keeps at most 1,024 active authenticated envelopes in that checkpoint and archives older envelopes in immutable DPAPI-protected segments of at most 256 records. Each committed checkpoint binds the archive names, hashes and lookup filters; archive contents are bound to the account and device generation. A segment is written, synced and read back before the checkpoint may reference it. An interrupted write cannot authorize an abandoned segment. Missing or changed referenced segments prevent reopening the device; failed writes pause encryption rather than discarding history or reusing ratchets.

Archived messages retain author, file metadata, reaction controls and observed edit revisions. Reading cached ciphertext does not consume an MLS ratchet again. Archived own send IDs remain reserved even outside the 512-receipt exact-retry window. Pending outgoing ciphertext is never moved out of its checkpoint. History iteration reads one segment at a time; mention reconciliation keeps version summaries rather than entire historic message bodies. Long-history scanning and resource measurements still need broader acceptance.

The archive has a 1 GiB encrypted-byte quota and at most 4,096 segments per device/account/community binding. Pending sends remain limited to 128, pending uploads to 1,024 and groups to 128. A conversation supports at most 64 devices. Disk or quota exhaustion produces an explicit error and preserves existing keys/history; there is no automatic history deletion. Files left by interrupted writes are ignored and may consume additional disk space. Never manually delete a referenced archive to free space.

Local checkpoints now use format v2. The new client reads valid v1 checkpoints and migrates them on the next durable change; older clients reject v2 instead of overlooking archived send identities. After using 0.6.1, keep using a compatible client. A complete same-device storage copy must include the `.device` file and its adjacent device-generation `.history` directory, as well as the existing Windows marker requirements. Copying these files alone does not provide portable key recovery, and restoring an older counter remains blocked.

## Protocol and review scope

The client uses pinned [OpenMLS 0.9.0](https://blog.openmls.tech/posts/2026-08-25-0.9.0-release/) with the RustCrypto provider and the standard MLS X25519/ChaCha20-Poly1305/SHA-256/Ed25519 suite. MLS is specified in [RFC 9420](https://www.rfc-editor.org/rfc/rfc9420.html). Relay separately enforces application identity, device verification, conversation binding, bounded input and durable storage.

Call keys come from the MLS exporter with separate purpose, conversation and sender labels. UDP inner envelopes bind the authenticated device, epoch and source timing; the host can route packets but lacks the inner keys. Native RTC supplies per-participant keys and rejects unencrypted or unknown-participant media. [LiveKit's encryption documentation](https://docs.livekit.io/transport/encryption/start/) describes the SDK mechanism; Relay's key distribution and acceptance checks are additional application code.

Authorized recipients possess content/media keys and can record or redistribute what they receive. Per-sender symmetric media keys do not independently prove a sender against a colluding authorized participant. Revocation protects subsequent updated epochs, not content already read or recorded. Relay does not claim an independently audited protocol integration, a complete anonymity system, indefinite history recovery or verified protection from endpoint compromise.

The release record distinguishes ordinary tests, synthetic multi-client server tests and native RTC fixtures from unperformed physical two-PC/WAN tests. See [verification](verification.md), [known issues](known-issues.md), [dependency review](dependency-review-0.6.0.md) and [security policy](SECURITY.md).

## Welcome, notices and search in 0.9.0

Welcome/system channel choices and bounded join/leave notices are community metadata, readable only by authenticated members with current channel access. They are displayed separately from encrypted chat; the server never injects plaintext into an MLS message. Blocking hides those authors. Migration does not fabricate historical join events.

Community search downloads bounded history pages from accessible channels, voice text, groups, threads and existing DMs. It decrypts messages and filenames on the device. Search terms never enter the history URL/body or a host plaintext index. Ten pages are checked per action; Search More History resumes. Results exclude blocked authors and inaccessible targets, and report missing device keys instead of presenting ciphertext as searchable content. Empty-room reads do not create MLS groups. Profile effects are bounded local drawing; saved effect names remain public profile metadata.

## Before the first protected message (from the README)

*Moved from the project README on 16 October 2026 so the README can stay short. The text is unchanged; only link paths are adjusted for this folder.*

*Updated 27 October 2026:* no manual device check is needed before the first message; devices are trusted on first use ([Encryption trust](encryption-trust.md)). Comparing codes is optional: under **Your security code**, copy your code or verification card and exchange it through a contact method you already trust, or compare the safety number on a call. The code is a public fingerprint; no private key is shared. A name, directory entry or Tailscale connection does not approve a Relay encryption device. The conversation's key manager must be online to admit new devices; Relay waits for keys instead of sending plaintext. Keep your existing device state: losing it cannot be repaired with an account password alone. Read the [encryption model](encryption.md) before relying on protected history or calls. Real first-time sharing, calls and reconnect between independent accounts on two PCs remain acceptance work in the current plan.

## Local data on Windows

*Moved from the project README on 16 October 2026 so the README can stay short. The text is unchanged; only link paths are adjusted for this folder.*

Version 0.6.1 migrates local encryption checkpoints to format v2; older clients reject those checkpoints. Preserve the adjacent device-generation `.history` directory together with the `.device` file. History segments are Windows-user-bound, checked against their committed hashes and subject to an explicit 1 GiB archive quota. They are not a portable key backup.

On Windows, persisted encryption state, UI preferences and remembered drafts use user-bound DPAPI protection. Disabling **Remember drafts after restarting** keeps conversation drafts during the running app session but excludes them from disk; **Clear saved drafts** clears both saved and current drafts. Account passwords are never saved. Remembered sessions are opt-in and use the Windows credential vault. Notification message previews are opt-in; disabling them avoids showing decrypted text in desktop notifications. Forgetting a community or signing out clears its remembered login; server logout revokes that session. DPAPI does not isolate data from malicious code running as your Windows account.


## Invitation-based verification

Relay can approve a friend's encryption device for you when they join with a link you created. It marks the device Verified without comparing codes. (Since 27 October 2026 every other device is also trusted automatically on first use, but not marked Verified; see [Encryption trust](encryption-trust.md).) It was approved for implementation by the user on 16 October 2026 with one-time invitations only.

### What it does

1. When you copy an invitation, your Relay creates a one-time secret (32 random bytes) and keeps it in your protected device state. The link carries, in the part after `#`: the server's identifier for the invitation (`iid`), your device ID (`hd`), your device's full security fingerprint (`hf`) and the secret (`vk`). The part after `#` is never sent to a server.
2. The invitation is single use and lasts 24 hours, the same limits the server already enforces. The server's invitation list and your own record both stop accepting it after one use or after expiry.
3. When your friend signs up with the link, their Relay computes `HMAC-SHA-256` with the secret over the community ID, the invitation ID, their account ID, their device ID and their device's fingerprint, and gives the result to the server. The server stores it with the device but cannot create or check it: it never sees the secret.
4. Your Relay reads the community's device directory while an invitation of yours is still open. If a device carries a proof that verifies against a secret you created, for exactly that account, device and fingerprint, it approves that device, marks the invitation as used and tells you ("... joined and was verified through your invitation").
5. Your friend's Relay approves your device only if the directory lists the same device ID and fingerprint that the link named.

After the attestation response, a failed directory read or a missing host entry can finish on the normal refresh cycle for up to 30 seconds. The retry holds only the exact public device ID/fingerprint in memory, never the invitation secret, and does not repeat the attestation request. A mismatch, existing pin, manual approval/removal or app restart ends that retry. This mechanism covers only the host-directory lookup.

Initial attestation delivery gets at most one extra attempt after a transport error, HTTP 408 or a server error. Each request has a two-second timeout. The retry sends the identical device-bound proof; if the first request committed but its response was lost, the server refuses replacement with HTTP 409 and the existing confirmation flow continues. Other client errors, including rate limits, are not replayed. The command retains no secret or request for a later refresh. A longer outage may still require manual device verification; signup and device provisioning are not retried by this change.

### What it protects against, and what it does not

- **The link is the trusted channel.** The approval is as strong as the way you sent the link. It equals sending your security code over the same chat. Anyone who gets an unused link gets both access to the community and an automatic approval of their device. The limits are: one use, 24 hours, the host sees "Verified via invitation" for that device and can remove the verification at any time, and you can compare a safety number or code with the person whenever you want to confirm it yourself.
- **A server operator cannot forge the proof.** Replacing a friend's device with another one, or relabelling a device, makes the check fail, because the proof covers the account, device and fingerprint. The operator can still refuse to deliver the proof (the device then simply stays unapproved) or lock people out, as before.
- **Only devices you have not already decided about.** A device that is already approved, one whose identity changed since you approved it, and your own account's other devices are never approved this way.
- **A proof that does not verify** leaves the device unapproved, shows "The invitation proof for this device could not be confirmed. Verify it manually.", and does not use up the invitation.
- **Nothing else changes.** Conversation access, key-manager rules, calls and history keep their existing requirements. Approval of your device by your friend and of theirs by you are separate; this feature performs both, once.
- **Older Relay versions** cannot read these links (they ask the person to update). Links without verification keep working exactly as before.

### Where the pieces live

- The proof is stored by the server in `device_attestations` (one per device); the invitation a member joined with is in `member_invitations`. Both appear in database schema 18. A server with this schema cannot be opened by an older release, and older backups must be migrated offline first, as for every schema change.
- The directory (`GET /api/e2ee/directory`) gives each device's proof to other members of the community. The proof reveals nothing that could be used without the secret.
- The secret is kept by the host only in the protected device state, for unused invitations, and is removed once the invitation is used or expired.


## Turning end-to-end encryption off for a community

By default every community uses Relay's end-to-end encryption. On 16 October 2026 the user decided that the community owner must be able to turn it off completely. This covers sending and reading ordinary text and files. Voice, video, edits and reactions are unchanged and keep needing working encryption keys.

### How it works

- The mode is stored by the server and announced to every member in the bootstrap: `protected` (the default) or `open`, with a revision number that increases on every change. Only an administrator can change it, with the revision they last saw; two administrators cannot overwrite each other unnoticed. A repeated change to the same mode does nothing. Each change is written to the audit log (mode only, never content).
- **The announcement does not change what anyone sends.** A client sends unprotected text only when its own user has accepted the exact revision that is announced. Until then it keeps sending encrypted messages, so the owner, the server or an invitation cannot lower anybody's protection. Turning encryption back on ends unprotected sending as soon as each client receives the change (a refresh after the server notifies it). Turning it off again is a new revision, so an earlier agreement never carries over.
- The agreement is stored on the person's own PC with the saved community and can be withdrawn at any time in Settings > Encryption > Message protection.
- The mode is always visible. In `open` mode a notice above the message box says "Not end-to-end encrypted" (or asks the person to decide), and unprotected messages are labelled the same way.
- Plain messages already existed as "legacy" messages. Any client can read them, so mixed communities work: someone who has not accepted keeps sending encrypted messages, and everyone who can already read those still can. Someone who has no encryption keys can read plain messages but not the encrypted ones.

### What is and is not protected in this mode

- The server host can read text sent unprotected. The connection to the server (private network, TLS or QUIC) is still encrypted; this changes the message layer only. Earlier encrypted history stays encrypted.
- Files follow the same rule as text. After a person accepts, files they attach are uploaded as they are, with their real name, and anyone in the community can open them without keys. A file that was prepared while encryption was on cannot be sent in a plain message (nobody could open it); the person removes and attaches it again. Reading a file that has no key is allowed only while the community is open; when the owner turns encryption back on, such files can no longer be opened from Relay and the server's copy is the only one.
- Not yet covered (each follows in later steps): voice and calls, video, editing a plain message and reactions.
- Older Relay versions ignore the setting and keep sending encrypted messages, which still work for anyone who can decrypt them.

### What it does not change

The measured cost of the extra layer is small (about 0.055 ms per message), so this setting is not a speed feature and the interface does not promise lower delay. Its original benefit was that nobody needed to approve devices before text worked; since trust on first use, protected mode does not need that either.
