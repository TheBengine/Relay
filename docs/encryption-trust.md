# Encryption trust: trust on first use

Decided on 27 October 2026. This replaces the rule that two people must compare security codes before they can message each other. The user's words: "it's weird that you can't just text someone without doing a bunch of stuff."

Relay now works like Signal and WhatsApp. Messages are end-to-end encrypted from the first message. Comparing security codes is optional and adds a **Verified** mark. Once you have verified someone, Relay never trusts a new device or code of theirs without asking you.

## What happens

| Situation | What Relay does | What you see |
|---|---|---|
| A person's device is seen for the first time (new friend, new member) | Trusted at once. The conversation's key manager adds it to the MLS group. | Nothing. Messages just work. |
| A new device of a person you have **not** verified | Trusted at once, as a first contact. | Nothing. |
| A known device presents a **different key**, person not verified | The new key replaces the old one. The key manager removes the old key from each conversation and adds the new one. | "X's security code changed" in the DM and under Encryption, until you dismiss it. Messaging keeps working. |
| A new device or a changed key of a person you **verified** | Held. Nothing is shared with it, and updates from others that add it are refused. The old, verified devices keep working. | "X's security code changed" with **Accept new code** and **Compare codes**. A conversation that needs the held device to continue waits with the same message. |
| A new device on **your own** account | Trusted at once, never held, so your new PC is never stuck. | "A new device was added to your account", with the device ID and a pointer to Revoke. Not shown on a device's very first start. |
| You compare codes and they match | The device is marked Verified. | Verified badge. |
| You remove the Verified mark | The same key stays trusted (as if trusted on first use). Calls and conversations continue. | The badge goes. |

**Accept new code** trusts the new key without the Verified mark. Comparing the new code (safety number dialog, pasted code or verification card) marks it Verified directly. A one-time invitation proof also counts as verified, and can upgrade a device that was already trusted on first use with exactly the named key.

## Why this is safe enough, and where it is weaker

Trust on first use protects against a passive server and against a server that starts impersonating someone *after* you first talked to them. It does not protect the very first contact: a server that is malicious from the start can list its own device for a new person, and Relay will trust it. This is the same trade-off Signal makes. Comparing codes closes the gap for people where it matters, and after that Relay holds every change for them.

Two consequences to be aware of:

- **Channel membership follows the community.** Before, a member nobody had verified could not read a protected channel. Now every member with access to the channel gets the keys automatically, like a new member in any group chat. A host that creates an account and gives it access to a channel can therefore read new messages there, but that account is visible in the member list like any other member. Old messages stay unreadable to it.
- **Group updates from the key manager.** If the key manager added a device of someone you have not verified, your device follows that update even if the device is no longer listed (it may have been added and revoked while you were offline; refusing would stop the conversation for good). A device of someone you verified is never accepted this way.

The guarantees that stay:

- The server still cannot read messages, files or calls. It never sees private keys or group secrets.
- Every member's key is pinned. A changed key is never used silently in place of a pinned key; either it is announced (unverified person) or held (verified person).
- **A device the server injects for a person you verified is never trusted without your decision.** Your key manager does not add it, and your device refuses a group update from another key manager that adds it (the update stays queued until you accept or it is replaced).
- A device ID with another key is never mixed up with the old key inside a group: the key manager removes the old leaf and adds the new one in a later epoch.
- MLS group mechanics, anti-rollback markers in Windows Credential Manager, revocation, explicit reset confirmation and the open-mode rules are unchanged.

## The pieces

**relay-crypto (`Device`).** Each pin (`ApprovedDevice`) now records `first_use`. Older snapshots have no such field and keep all their pins as verified, which is what they were. New calls:

- `first_use_decision` / `trust_on_first_use`: the policy above, in one place. `Held` for a verified person (any verified pin of that user other than the own account), `KeyReplaced` for a changed key of an unverified pin, `Trusted` for a new device, `Deferred` for a listing without a KeyPackage.
- `accept_new_key(device, compared)`: the user's explicit decision. `None` is the one-click accept; `Some(code)` must equal the new fingerprint and marks it verified.
- `unverify`: removes only the Verified mark.
- `untrusted_members`: group members with no pin or a pin for another key. The key manager removes them.
- `forget_unused_first_use`: keeps the 256-pin store from filling with retired devices. Verified pins and current group members are never forgotten.

A listing without a KeyPackage (an existing member whose spare package was used) cannot be pinned from the listing, because the listing carries only a fingerprint. Such a device becomes an in-memory *candidate*. When it then appears in an authenticated Welcome or commit, its leaf is pinned only if `fingerprint(identity, signing key)` equals the listed fingerprint. Candidates are never saved; the next directory read rebuilds them. A device that is not listed at all is pinned from an authenticated commit of the key manager (or as a non-sender member of a Welcome) only if its person is not verified here; the Welcome's sender, the key manager itself, must always be listed.

**relay-client (`CryptoSession`).** `trust_directory` runs trust on first use over the listed devices after each directory read (every conversation preparation and every full refresh). It writes a checkpoint only when something changed. Notices (`TrustNotice`: `CodeChanged`, `NewOwnDevice`) are kept in the protected state until dismissed, at most 64. `trust_started` records the first complete directory read, so a fresh install does not announce the owner's existing devices.

**Worker (`network_e2ee.rs`, `network_background_crypto.rs`).** The same trust pass also runs when a parked community catches up in the background. The key manager adds every trusted device automatically. Members no longer wait for the key manager to finish adding other people's new devices: such a device reads along from the epoch that adds it, as any later member does. Waits that remain:

- `DeviceApproval`: this device is not in the group yet. The key manager adds it automatically while online.
- `PeerUnavailable`: the other person in a DM has no device ready yet (usually never opened Relay since).
- `KeyChangeReview`: a verified person's new device or code holds this conversation.
- `KeyRotation`, `KeyUpdates`: unchanged.

The former `PeerVerification` and `AddingDevices` waits are gone.

**UI.** The DM banner no longer asks you to verify anyone. It shows only a wait the worker reported, or a key change of the other person. The Encryption page says "Messages are end-to-end encrypted" in normal use. Only a change for a verified person is shown as needing attention; announced key changes and new own devices are listed as dismissible notices.

## Tests

- `crates/relay-crypto/src/trust_tests.rs`: first contact with a real MLS DM, new device of an unverified vs a verified person, changed key for both, removal of a replaced key by the key manager, adoption of a package-less listing only with its exact fingerprint, own devices, snapshot compatibility and pruning.
- `crates/relay-client/src/e2ee_trust_tests.rs`: the same with real encryption sessions, including a key change notice that survives a restart, the one-click accept, and a device injected by the server for a verified person that is refused in a real commit until the user accepts it.
- `actual_server_first_contact_messages_work_both_ways_without_comparing_codes` (opt-in, real server): a first DM in both directions and a protected channel with nobody verified; the host stores ciphertext only.
- Existing opt-in real-server tests that relied on the old rule were adapted: an unverified channel member now reads the channel; tests that need a membership change while someone is offline use a member who joins at that point.

## Not covered yet

- The very first contact cannot be authenticated without a comparison or an invitation link; see above.
- Older Relay versions reading a newer device state treat first-use pins as verified. Downgrading is not supported for this reason alone, but it is not blocked either.
- Group conversations show key changes on the Encryption page and as a short notice; only direct messages also show them above the composer.
