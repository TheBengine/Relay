# User instructions checked against behavior (5 November 2026)

Open item from the stable-readiness handoff: "Verify the final instructions on encryption,
history transfer, account recovery and updates against actual behavior." Every statement
below was matched to the code that makes it true and to a test that was run, or a test was
written and run. Source comments alone were not accepted as evidence.

## Scope and assumptions

- **Texts checked:** the installed guide `deploy/windows/LAES-MIG.txt`; the public
  `GUIDE.md` and `README.md` from the last published overlay
  (`work\relay-performance-release-20261104\public-docs-overlay`, release
  v0.10.0-preview.20261104.1); `docs/public-release.md`; `docs/stabil-kanal-og-signering.md`;
  the history-transfer section of `docs/encryption.md`; in-app help (`help_ui.rs`,
  `recovery_help.rs`); and settings help text (`update_ui.rs`, `settings_pages.rs`,
  `history_transfer_ui.rs`, `history_backup_ui.rs`, `encryption_ui.rs`).
- **Topics:** encryption, history transfer, account recovery and updates. Statements about
  connections (iroh, Tailscale), Windows startup, audio and video were left out.
- **Which code:** the 4 November preview was built from 70e3c3a plus uncommitted changes
  in the shared checkout (`package.json`: `source_dirty: true`). Those changes were
  committed on 5 November as 70e3c3a..15899d4 (branch `session/friends-call-quality-20261021`),
  and this branch is based on 15899d4. Statements were read against that code and its tests
  were run here. 15899d4 may contain changes made after the 4 November build; claims about
  automatic updates, the encryption default and trust on first use rest on code already
  present in 70e3c3a.
- **Public overlay:** `GUIDE.md` and `README.md` are outside the repository. Their
  corrections are given as replacement text under "Overlay corrections" and in the shared
  session notes; they take effect with the next release's overlay.
- **Servers:** every server test uses its own temporary data directory and an in-process
  router or port 0. Nothing used port 4580, the installed Relay, publisher keys or another
  worktree.
- **Verdicts:** *verified* means code and a passing test agree with the statement. *Wrong*
  means behavior contradicts it; the "Fix" column says what changed. *Unverifiable* means
  no test available in this session can establish it, with the reason.

## Results

| # | Statement | Where | Evidence | Verdict | Fix |
|---|---|---|---|---|---|
| **Encryption** | | | | | |
| E1 | "Server encryption is off by default (recommended). You can turn it on when creating a server." | LAES-MIG.txt:8 | `server_creation.rs:54-59` returns `ProtectionLevel::Protected`; flipped in e85fbee (26 Oct) without updating the guide. Test `server_creation::tests::every_new_server_starts_from_the_recommended_choices`; new `test-binary-package-guide.py::test_guide_states_the_encryption_default_new_servers_actually_use` | wrong | "on by default (recommended). You can turn it off" |
| E2 | "End-to-end encryption off is the recommended default for new servers. You can choose protected mode during creation" | GUIDE.md:26 | as E1 | wrong | Overlay correction 1 |
| E3 | "New servers select end-to-end encryption by default; existing servers keep their settings." | README.md:20 | as E1; the choice is applied only by the creation flow (`host_setup_ui.rs:358-367`) | verified | |
| E4 | Text privacy: "End-to-end encrypted in protected mode" | README.md:17 | `network_v2.rs:122` sends text unprotected only in open mode; Friends refuses plaintext: `people::tests::people_encrypted_messages_survive_offline_recipient_and_restart_without_plaintext_storage` | verified | |
| E5 | "Private messages and calls are end-to-end encrypted." (signed-out help) | help_ui.rs:103 | True for Friends (E4 test). A server created with encryption off sends DMs and calls without end-to-end protection (`server_creation.rs:76-79`, open mode) | wrong | "Friends messages and calls, and servers with encryption on, are end-to-end encrypted." New assertion in `help_ui::tests::every_topic_is_short_and_names_existing_buttons` |
| E6 | Verify only opens devices; nothing is approved until Approve Device; a replacement device needs a new check | help_ui.rs:107 | `e2ee_trust_tests::a_new_device_of_an_unverified_person_is_trusted_and_one_of_a_verified_person_waits`, `a_device_the_server_injects_for_a_verified_person_never_receives_or_joins_silently` | verified | |
| E7 | "New one-use invitations can confirm the host's and friend's devices automatically." | GUIDE.md:53 | `e2ee_invitation_tests::a_valid_proof_approves_the_friends_device_once_and_survives_a_restart`, `the_friend_pins_only_the_exact_host_device_the_link_named`, `a_wrong_unknown_or_expired_proof_never_approves` | verified | |
| E8 | "compare the security number under Settings → Encryption" | GUIDE.md:53 | The app calls it "security code" (`encryption_ui.rs:450-451,550`); the page exists (`settings_pages.rs:188`) | wrong (label) | Overlay correction 2 |
| E9 | "If a protected conversation requests confirmation, follow Settings > Encryption." | LAES-MIG.txt:23 | Settings has an Encryption page (`settings_pages.rs:188`); a held key change shows Accept new code there (`encryption_ui.rs:867`); `e2ee_trust_tests::a_changed_key_is_announced_for_unverified_people_and_held_for_verified_people` | verified | |
| E10 | "explicit fingerprint trust" | public-release.md:18 | Trust on first use since 27 Oct (`docs/encryption-trust.md`); `relay_crypto::trust_tests::a_first_contact_is_trusted_and_both_sides_exchange_messages_without_a_comparison`, `e2ee_trust_tests::first_contact_works_both_ways_without_comparing_codes_or_any_warning` | wrong | "trust on first use with optional security-code verification" |
| E11 | Backend storage carries public devices and opaque bytes; KeyPackages are not reused when a removed device rejoins | public-release.md:18 | `e2ee::tests::consumed_key_packages_renew_for_removed_member_rejoin_without_reuse_or_stale_ack_loss`, `ciphertext_messages_have_only_tagged_bounded_budget_and_no_server_full_text_index`, `devices_are_owned_bounded_immutable_and_revoked_without_identity_reuse` | verified | |
| E12 | Authenticated inner Opus packets and native RTC GCM frame encryption | public-release.md:18 | Needs the native-RTC media path between two clients; not run in this session | unverifiable | |
| **History transfer** | | | | | |
| T1 | Steps: Encryption > Backup and recovery > This is my new PC; enter the code on the original PC, Approve and send history; Receive history on the new PC | GUIDE.md:21, recovery_help.rs:61, encryption_ui.rs:947, encryption.md:44-48 | Labels drawn at `history_transfer_ui.rs:73,81,88`; section name checked by `recovery_help::tests::screens_and_buttons_named_in_the_steps_exist_in_the_app` | verified | |
| T2 | "Recovered history is read-only." | GUIDE.md:21 | `network_history_transfer_tests::history_transfer_page_is_read_only_and_rejects_delivery_substitution`; `e2ee_transfer_tests::history_transfer_survives_restart_without_importing_live_ratchets_or_counters` | verified | |
| T3 | Both PCs must be signed in to the same account and community | history_transfer_ui.rs:47, encryption.md:45 | Server keys entries by account and checks session + active membership (`history_transfer.rs:98-110`): `history_transfer::tests::owner_only_expiring_transfer_is_atomic_and_consumed` (other account gets 404); client AAD binds community and account: `history_transfer::tests::history_transfer_authenticates_code_owner_community_and_ciphertext` | verified | |
| T4 | A temporary encrypted copy travels through the server; the random 256-bit code stays on the PCs | history_transfer_ui.rs:48, encryption.md:50-52 | XChaCha20-Poly1305 with a 32-byte random key (`history_transfer.rs:20-24,132-148`); server sees only `SHA-256(domain, code)`. New `history_transfer_never_uploads_its_code_and_bounds_records` | verified | |
| T5 | The server stops serving it after 15 minutes, or on restart; not an online backup | history_transfer_ui.rs:48, encryption.md:52-59 | `TTL` 15 min, in-memory `Store` (`relay-server/src/history_transfer.rs:11-16`); expiry: `owner_only_expiring_transfer_is_atomic_and_consumed`; new `history_transfer_is_never_written_to_disk_and_is_gone_after_restart` | verified | |
| T6 | "Update this server before transferring history between PCs." | history_transfer_ui.rs:63, encryption.md:48 | `network_history_transfer.rs:40-42`; new `history_transfer_refuses_a_server_without_the_transfer_capability` | verified | |
| T7 | Each snapshot at most 32 MiB and 100,000 records; 128 MiB server budget including in-flight uploads and retained responses | encryption.md:54-56 | `transfer_bounds_body_and_aggregate_storage_before_accepting`, `history_transfer_slow_response_remains_charged_after_delete`; new `history_transfer_never_uploads_its_code_and_bounds_records` (100,001 rows refused) | verified | |
| T8 | Session and membership rechecked before publishing; slow upload releases the maintenance lock; after restore create a new transfer | encryption.md:60-64 | `history_upload_rechecks_session_and_membership_after_reading_the_body`, `history_transfer_is_discarded_only_after_successful_restore` | verified | |
| T9 | Messages appear only where the server still supplies matching ciphertext, marked "History transferred from your other PC" | encryption.md:66-67, encryption_ui.rs:630-631 | `history_transfer_page_is_read_only_and_rejects_delivery_substitution` (five substitutions give "unavailable", provenance `Recovered`) | verified | |
| T10 | Reactions not reconstructed; unread order, device verification, pending sends and MLS ratchets not imported; each PC keeps its own identity | encryption.md:68-72 | `history_transfer_rejects_unbound_records_and_invalid_actions`, `history_transfer_survives_restart_without_importing_live_ratchets_or_counters`, `history_transfer_prepared_for_an_old_device_cannot_modify_a_reset_device` | verified | |
| T11 | Separate DPAPI segments; failed or cancelled staging removes files; referenced segments preserved | encryption.md:72-75 | `history_transfer_cancelled_or_failed_checkpoint_cleans_staged_files`, `history_transfer_marker_failure_preserves_installed_segments_for_restart` | verified | |
| T12 | Attachment keys transfer, but the original uploaded files must still exist | encryption.md:68 | No test sends an attachment through a transfer | unverifiable | |
| **Account recovery** | | | | | |
| A1 | An account password (or server backup) cannot restore lost device keys or the history only they could read | LAES-MIG.txt:24, GUIDE.md:68, recovery_help.rs:64,79,81 | Keys are created per device and never sent (`relay-crypto/src/lib.rs:227`, server stores public devices only, E11). New `relay_crypto::trust_tests::a_replacement_device_for_the_same_account_cannot_read_what_the_lost_device_could` | verified | |
| A2 | Relay account and community accounts are separate with their own passwords; authenticator 6-digit or recovery codes | recovery_help.rs:49,51 | `account_identity::tests::relay_account_login_binds_signed_proofs_and_never_bypasses_a_community_authenticator`, `enabled_local_authenticator_is_required_and_recovery_is_spent_before_browser_approval` | verified | |
| A3 | Password, authenticator and recovery codes are changed at the Relay account service; Account links to it | recovery_help.rs:52 | `central_account_ui.rs:253`; `recovery_help::tests::screens_and_buttons_named_in_the_steps_exist_in_the_app` | verified | |
| A4 | A new device gets its own identity; old keys do not move over | recovery_help.rs:55 | A1 test; `history_transfer_survives_restart_without_importing_live_ratchets_or_counters` | verified | |
| A5 | "They open your device in Encryption, check the code and choose Approve Device. Until then, protected conversations and calls with them cannot resume on the new device." | recovery_help.rs:57 | Since trust on first use only people who verified you hold a new device; everyone else trusts it at once: `e2ee_trust_tests::a_new_device_of_an_unverified_person_is_trusted_and_one_of_a_verified_person_waits`, `a_new_device_on_the_own_account_is_trusted_and_announced_except_on_first_start` | wrong | Only people who verified you must choose Approve Device or Accept new code; step 2 made optional. Test now also requires "Accept new code" |
| A6 | "Other devices and people you trust may need to approve a new device…" | recovery_help.rs:80 | as A5: own devices never approve, and it is people who verified *you* | wrong | "People who verified you may need to approve…" |
| A7 | Lost device: Encryption > Advanced > Revoke This Device under Your registered devices | recovery_help.rs:58 | `encryption_ui.rs:966,975`; `e2ee::tests::devices_are_owned_bounded_immutable_and_revoked_without_identity_reuse` | verified | |
| A8 | A history backup is a read-only copy of messages this PC opened, opened with its passphrase (also signed out); it does not restore live chat, approve a device or let you send | recovery_help.rs:62-63, history_backup_ui.rs:44-45 | `e2ee_tests::portable_history_from_real_mls_archive_never_restores_send_identity_or_ratchets`, `recovery::tests::portable_history_authenticates_password_header_content_and_account`, `existing_files_invalid_archives_and_short_passwords_are_preserved` | verified | |
| **Updates** | | | | | |
| U1 | "Automatic background updates are not enabled." | GUIDE.md:61 | The 4 Nov build has a signed beta policy (`relay-updater/publisher/policy.json`), the live feed offered `in-place-v2` (`relay-performance-release-20261104/native-feed-verification.json`). An installed copy checks every 6 h and installs after 2 idle minutes (`update_ui.rs:14-16,614-635`): `an_installed_copy_shows_one_quiet_line_and_later_stops_automatic_updates`, new `automatic_checks_setting_decides_whether_an_installed_copy_updates_itself` | wrong | Overlay correction 3; LAES-MIG gained the same line |
| U2 | "Update and restart downloads, verifies and installs only when you choose it. Download only keeps Relay open." | settings_pages.rs:2280 | as U1; after Download only, the automatic step still installs when idle (`update_ui.rs:629-632`) | wrong | Describes the automatic install, the two-minute wait, call/draft blocking and Later/Cancel |
| U3 | Relay updates itself only when not in a call, with no unsent message; Relay also does this by itself | update_ui.rs:519-520,822-829 | `automatic_restart_waits_for_two_idle_minutes_and_never_drops_a_draft`, `update_and_restart_control_allows_local_hosting_but_blocks_calls_and_work`, `automatic_install_consent_is_exact_and_call_or_work_cancellation_is_final` | verified | |
| U4 | "Servers hosted on this PC pause briefly." during an update | update_ui.rs:569,828 | Needs a real installer handoff with a hosted server; the installer fixtures belong to the readiness-install session | unverifiable | |
| U5 | Preview notice "For this preview, updates open on GitHub and you run the installer…" | update_ui.rs:769 | Shown only when the build has no update policy (`update_ui.rs:80-83`): `preview_banner_opens_only_the_release_page_and_later_hides_it`. Not shown by the 4 Nov build | verified (scope: unconfigured builds) | |
| U6 | Quit Relay from the notification area, then run a newer Relay-Setup.exe | GUIDE.md:57-58, LAES-MIG.txt:30-32 | `tray.rs:123`; manual route still works beside U1 | verified | |
| U7 | The installer's "Stop my local server and continue" requests normal shutdown; friends disconnect | GUIDE.md:58, LAES-MIG.txt:33 | `Relay.iss:796-806`, `StopLocalServer.ps1:96-110`; `scripts/test-installer-server-stop.ps1` (12 checks passed) | verified | |
| U8 | The installer preserves accounts, settings, device keys and server data; uninstall leaves data | GUIDE.md:61, LAES-MIG.txt:34,39 | Program folder must be separate from `%LOCALAPPDATA%\Relay` and `%APPDATA%\Relay` (`Relay.iss:650-664`); no data deletion on uninstall. Fixture records (not re-run here): `relay-stable-readiness-20261105/publisher-installed-fixture-followup/verification.json` (`fixture_removed_and_data_preserved: true`), `current-automatic-handoff/execution` | verified (existing fixture record) | |
| U9 | Unsigned preview; signed beta update feed | GUIDE.md:13, README.md:8, public-release.md:3 | `native-feed-verification.json`: `authenticode_verified: false`, `repository_verified: true`; beta `timestamp.json` HTTP 200 on 5 Nov | verified | |
| U10 | Updating the Windows app does not update a separately hosted server | GUIDE.md:67 | The updater installs only this installation (`relay-updater/src/handoff.rs:234-256`) | verified | |
| U11 | "Update both PCs for media fixes." | GUIDE.md:59 | Advice that depends on the content of each release | unverifiable | |
| U12 | Only beta is live; the stable folder is created at the first stable release | stabil-kanal:3,20 | `policy.json` channel beta; `/updates/stable/metadata/timestamp.json` HTTP 404 on 5 Nov | verified | |
| U13 | Stable uses the same reviewed root with its own `/updates/stable/` addresses; `build.ps1 -Channel stable` selects it; default beta; `--build-info` shows the channel | stabil-kanal:9-10 | `publisher_policy::tests::beta_and_stable_policies_share_the_reviewed_root_and_differ_only_in_channel`, `compiled_policy_matches_the_selected_channel_feature` (default and `stable-channel`); `build.ps1:14,85`; `build_metadata.rs:35` | verified | |
| U14 | A stable client refuses prerelease versions | stabil-kanal:11 | `lib.rs:199`; new `stable_publisher_refuses_a_correctly_signed_prerelease` | verified | |
| U15 | The publisher gate checks signer, timestamp and hashes and recomputes the seven tests from both PCs' ZIPs before opening the key store | stabil-kanal:12, public-release.md:5 | `test_publish_updates.py` (7 passed, incl. `test_stable_requires_signed_installer_and_original_two_pc_measurements_for_this_build`), `test_windows_authenticode.py` (4 passed), `build-windows-installer.py --self-test` (13 passed) | verified | |
| U16 | Existing beta clients stay on beta; a bad stable needs a new candidate with a higher sequence | stabil-kanal:18,21 | `signed_platform_channel_app_and_installer_binding_cannot_be_substituted` (channel), `signed_older_release_and_same_identifier_collision_are_refused` | verified | |
| U17 | Both root keys are in the same DPAPI store | stabil-kanal:19 | Checking requires opening the publisher key store, which this session must not touch | unverifiable | |

## Counts

| Verdict | Count |
|---|---|
| verified | 35 |
| wrong (corrected) | 9 |
| unverifiable | 5 |

## Overlay corrections for the next release

Replace in `public-docs-overlay/GUIDE.md`:

1. Line 26: "End-to-end encryption on is the recommended default for new servers. You can choose open mode during creation; existing servers keep their settings."
2. Line 53: "You can compare the security code under **Settings → Encryption**, in person or over a call you already trust."
3. Line 61: "The installer preserves accounts, settings, device keys and server data. Uninstalling through Windows also leaves your data. An installed copy checks the signed update feed after startup and every six hours, then updates itself after two minutes without use, never during a call or with an unsent message. Turn this off in **Settings > Advanced > Updates**; the steps above remain the manual route."

`README.md` needs no change for these topics.

## Tests written

- `relay-updater`: `tests::stable_publisher_refuses_a_correctly_signed_prerelease`
- `relay-crypto`: `trust_tests::a_replacement_device_for_the_same_account_cannot_read_what_the_lost_device_could`
- `relay-server`: `history_transfer::tests::history_transfer_is_never_written_to_disk_and_is_gone_after_restart`
- `relay-client`: `update_ui::tests::automatic_checks_setting_decides_whether_an_installed_copy_updates_itself`,
  `history_transfer::tests::history_transfer_never_uploads_its_code_and_bounds_records`,
  `network_history_transfer_tests::history_transfer_refuses_a_server_without_the_transfer_capability`;
  assertions added to `help_ui::tests::every_topic_is_short_and_names_existing_buttons` and
  `recovery_help::tests::screens_and_buttons_named_in_the_steps_exist_in_the_app`
- `scripts/test-binary-package-guide.py`: `test_guide_states_the_encryption_default_new_servers_actually_use`

## Test runs

All on this branch (base 15899d4), 5 November, with `CARGO_TARGET_DIR=D:\Projects\relay-build-targets\readiness-docs-20261105\target`:

| Command | Result |
|---|---|
| `cargo test -p relay-server -- history_transfer people_encrypted_messages consumed_key_packages_renew ciphertext_messages_have_only devices_are_owned_bounded relay_account_login_binds enabled_local_authenticator_is_required` | 12 passed |
| `cargo test -p relay-crypto` | 30 passed, 2 ignored (opt-in measurements) |
| `cargo test -p relay-updater` | 62 passed, 4 ignored (need an installed fixture or are subprocess entry points) |
| `cargo test -p relay-updater --features stable-channel publisher_policy` | 4 passed |
| `cargo test -p relay-client --bin relay -- history_transfer e2ee_invitation_tests e2ee_trust_tests recovery_help:: help_ui:: update_ui:: server_creation:: portable_history recovery::tests` | 74 passed, 1 ignored (native screen fixture) |
| `cargo test -p relay-client --bin relay -- e2ee::invitation_tests e2ee::trust_tests e2ee_transfer` | 18 passed |
| `python scripts/test-binary-package-guide.py` | 6 passed |
| `python scripts/test_publish_updates.py` / `test_windows_authenticode.py` / `build-windows-installer.py --self-test` | 7 / 4 / 13 passed |
| `scripts/test-installer-server-stop.ps1` | 12 checks passed |

## Not settled here

- E12, T12, U4, U11 and U17 above.
- Out of scope but contradictory: README.md:20 and LAES-MIG.txt:12 describe iroh invitations, while GUIDE.md:36 says the iroh route is disabled in this preview.
- 70e3c3a still had older lines in `docs/stabil-kanal-og-signering.md` and `docs/public-release.md` (test counts 59/59 and 6/6, "automatic update delivery remain future work"). They were replaced in 2b247b4; this table checks the current text.
