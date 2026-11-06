# Public release checklist

Relay 0.10.0 is an experimental Windows x64 beta. The 4 November preview has a verified download and signed beta update feed. Its installer lacks Authenticode signing. Earlier release evidence keeps its original scope.

Stage a signed [stable candidate](stabil-kanal-og-signering.md) locally before acceptance. Staging verifies the timestamped Authenticode signatures and hashes of the installer, staged executables and uninstaller template against the build receipt. The publisher recalculates the seven media and connectivity results from the original measurement ZIPs for PC A and PC B. The collector records the measured client and installer hashes. Use the [two-PC acceptance kit](accepttest-to-pc.md) and record the manually observed steps alongside the release receipt. Separately record clean installation, installed signatures, interrupted update and data restoration on a disposable Windows system. Complete the security and dependency review below before approving stable.

## Concrete audit fixes

| Finding | Remedy and reproducible check |
|---|---|
| Existing updater destination junction redirected writes outside the installation | Preflight application/data/staging paths and guarded rollback; `test-update.py` rejects actual root/nested/data junction fixtures while retaining outside bytes. |
| Direct Windows server inherited permissive data ACLs | Current-user/SYSTEM protected inheritable DACL before opening data; preflight roots, reparse and hard links. `test-data-privacy.py` checks a deliberately permissive existing folder and new SQLite/lock files, plus outside byte/ACL preservation. |
| Reopening a migrated community failed when a generated backup exceeded the ordinary Windows path limit | Lexical extended drive/UNC file-handle names, preserved UTF16 and reparse checks; three regressions and [same-fixture before/after evidence](long-path-0.5.0.json). |
| Authenticated upgraded sockets could consume unbounded connections or prevent graceful stop | Event/RTC per-account and global budgets, event-send deadlines and durable shutdown state. Periodic RTC database validation observes shutdown, fails closed after three seconds and ignores late blocked results. Server regressions hold real sockets, revoke sessions and stop the server. |
| Native SDK builder trusted an existing cache and unsigned downloaded bytes | Pinned official archive SHA256, safe bounded extraction, exact per-file comparison and restored build environment. `test-native-sdk.py` rejects tampering and reparse fixtures; the previous shipping cache also matched. |
| Generic send throttling did not explain recoverable action | Distinct cooldown reasons and countdowns; preserved draft/attachments and explicit retry. Account quota, staff behavior, idempotence, restart and slowmode regressions cover server/client rules. |

New content encryption adds OpenMLS conversation state, trust on first use with optional security-code verification, protected file manifests, authenticated inner Opus packets and native RTC GCM. Local state/outgoing ciphertext are protected and persisted before publication; backend storage carries public devices and opaque control/application bytes. Reservation consumption/renewal prevents reusing a KeyPackage when a removed device rejoins. These controls require their own [encryption review](encryption.md); the earlier hosting audit does not establish their security.

The [dependency review](dependency-review-0.6.0.md) documents remaining advisory/maintenance scope. [Verification](verification.md) owns final source, exact-binary, screenshot and extracted-package results; the external release verification JSON beside the ZIP records archive integrity without a circular self-hash.

## Before wider public publication

1. Verify the private security reporting route and its monitoring. Keep the beta download and signed update feed verifiable. Obtain a trusted Windows signing identity and sign the release binaries and installer with a timestamp before recording final hashes.
2. Reproduce exact-binary installation and startup on a clean Windows PC with ordinary user permissions; test graphics/audio dependencies, launch/stop and a copied-community upgrade/restore. A static DLL-import check alone does not satisfy this gate.
3. Complete a real two-PC LAN call, then public TLS/WAN/TURN call, with scoped private SFU signaling and trusted-proxy rate limiting. Exercise reconnect, session revocation and private-channel permission loss.
4. Complete physical headset, speaker, webcam and selected-window sound tests, sustained calls/capture, source changes and device loss. Record recipient FPS, delay and CPU/RAM/GPU across the client, Relay and SFU. Do not infer results from synthetic fixtures.
5. Review current upstream advisories for the locked Rust graph, iroh, bundled Go helpers/SFU and the pinned native WebRTC and codec build. Triage applicability and record fixed versions and residual risk against the actual package. The earlier review is an input to this check.
6. Rehearse backup retention, restore and interrupted update with a disposable copy of a realistic community. Verify that Relay-managed restore preserves device revocations from the current database when using an older backup of the same community. Protect backup storage. Verify long histories, unread state and day-long mixed media/chat behavior before capacity or stability claims.
7. Commission independent review of account/session handling, MLS integration, device trust and revocation, history transfer, key persistence, attachments, Opus and RTC keys. Exercise a malicious device directory, unauthorized recipients, key-manager departure, offline epoch catch-up, revoked-device rejoin, lost acknowledgements, rollback and key loss.
8. Document visible metadata/public profiles, legacy plaintext, endpoint compromise and saved-download boundaries. Establish a usable device-key recovery plan before promising recoverable encrypted history. Rehearse server restore with separately copied device state; never solve ratchet mismatch through silent plaintext downgrade.

Record evidence separately for each gate. Local fixtures, live device tests and independent review answer different questions. The [voice-model comparison note](audio-model-comparison-20261102.md#note-til-næste-lydtest-før-stabil-udgivelse) records the listening and package-size work before choosing which experimental models to ship.

## Reproduce script checks

From the extracted source package after building, use isolated fixtures:

```powershell
python .\scripts\test-integration.py --server .\dist\relay-server.exe
python .\scripts\test-data-privacy.py --server .\dist\relay-server.exe
python .\scripts\test-hosting.py
python .\scripts\test-backup.py
python .\scripts\test-update.py
python .\scripts\test-package.py
python .\scripts\test-shell-runtime.py
python .\scripts\test-native-sdk.py
python .\scripts\test-soak.py --server .\dist\relay-server.exe --sessions 100 --seconds 120 --workers 16
```

Fixtures create their own `work` directories and owned processes. They do not use a live community. Soak sends are deliberately paced to respect the five-in-five account rule; no generic 429 retry hides quota failures. Successful-attempt latency excludes fixture pacing and transient retries. Live load, long-duration media and adversarial slow-reader acceptance remain separate work.
