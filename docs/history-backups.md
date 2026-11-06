# Portable history backups

In **User Settings → Encryption → History Backup**, enter a separate long passphrase twice and choose **Create History Backup**. Select a new filename. Relay exports already decrypted, delivered history from this account and community; an archive failure cancels the export. Existing files are preserved.

On another PC, choose **Open History Backup** and enter that backup's passphrase. You can view it while signed out. When signed in, its account and community must match. The saved-history viewer is separate from live chat; its contents establish no new device or message trust. It shows saved versions and attachment descriptions. Actual attached file bytes must still be available separately.

A new PC creates a new sending device. Compare and approve its security code normally. A history backup contains no device signing key, MLS checkpoint/ratchet, approvals, pending sends, live media key or session credential. Opening an old backup cannot restore those objects or remove current replay history. Old attachment file keys remain inside the encrypted historical envelopes; they do not grant access to new files or messages.

The file uses versioned `RLYHIST1`, a random 16-byte salt, random 24-byte nonce, fixed Argon2id v19 parameters (64 MiB, three passes, four lanes, 32-byte output), and XChaCha20-Poly1305. The complete header is authenticated. The KDF profile follows [RFC 9106's second recommendation](https://www.rfc-editor.org/rfc/rfc9106.html#section-4). Parameters cannot be increased through an untrusted file. Password, key, plaintext serialization buffers and KDF memory are explicitly wiped. No passphrase is saved in preferences or sent to the server.

Limits: 64 MiB plaintext and 100,000 historical records. Oversized exports fail instead of silently creating a partial backup. Imported files are size-limited before key derivation; changed/truncated files and wrong passphrases fail before a viewer is published. Work runs in one local background worker; its replies are scoped to the requesting account/community and request ID.

The history storage meter reports the current actor's committed archive manifest and active record count. It excludes the active checkpoint and retired-device archives. Relay preserves history and keys at its archive limit. No automatic deletion is introduced.

This describes the implementation under task 28. Native UI acceptance, a second Windows account/PC and independent review remain separate requirements.
