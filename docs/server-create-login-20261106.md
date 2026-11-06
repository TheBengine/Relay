# Creating a server asked for the login again (6 November 2026)

Report: in the installed preview, creating a new server made the user sign in
to their Relay account again, although the app had a saved login.

Branch `session/server-login-20261106`. Assumptions written down instead of
asked (the user said "do not ask"):

1. "Log in again" means the Relay account page asked for the account password
   (or the user had to press "Continue with my Relay account" and then type it).
   Both are covered.
2. The security properties to keep are the ones in
   `docs/security-review-accounts-20261105.md`: the destination-bound nonce for
   every proof a server receives over the network, no silent trust of another
   account, and device approval unchanged.
3. The in-app account page stays incognito (chosen in `c7be655`); changing that
   is a privacy decision and is listed as a follow-up only.

## How the flow works

1. "Create server" (`server_creation.rs::start_new_server`) sends
   `Command::CreateLocal`. The worker stops the current local server, detaches
   local connections, creates a new data folder and starts the stack at
   `http://127.0.0.1:4580` (`network.rs`, `local_host::create_directory`).
2. `Update::LocalReady` (`app_update.rs`) calls `claim_new_server`, which
   starts `begin_account_login` for the loopback address with
   `registering = true` and the creation choices.
3. The worker asks the server for a challenge (`/api/account-login/challenge`).
   Its nonce is `relay-login-v1:<server_id>:<sha256(destination)>:<random>`.
4. `account_login_challenge` opened the account page
   (`central_account::Controller::start_community`): an OIDC authorization
   request whose ID token carries that nonce. `/api/account-login` accepts only
   such a token (`account_login.rs::finish` → `verify_identity` with the
   challenge nonce).

## Every point where the login could be dropped or a new check forced

| Point | Where | Before | After |
|---|---|---|---|
| **A. New server needs a destination-bound ID token** | `account_start_ui.rs::account_login_challenge`, `account_login.rs::finish` | Always the account page. The saved login is a refresh token (`central_account.rs::resume_flow`); the page is incognito (`account_webview.rs`, `with_incognito(true)`), so after any restart, or once the account service's browser session ended, the page asks for the password. **This is the cause.** | The server this PC hosts accepts the saved login's refreshed identity from the host app (below). |
| B. Saved server logins are keyed by address | `credential_vault.rs` target `Relay/session/<url>` | Every local server is `http://127.0.0.1:4580`, so the new server's session replaces the previous local server's. Reopening the old one ("Other servers on this PC") started a new account sign-in, i.e. point A again. | Same slot, but reopening now goes through the silent local route. |
| C. Claim skipped while another sign-in runs | `claim_new_server`, `Update::LocalReady` | Returned silently when `account_start.pending` was set (an automatic reconnect at start-up, Friends): the new server waited on a "Continue with my Relay account" page, and that button led to point A. | Remembered in `claim_waiting` and started right after the running sign-in ends (`account_login_tick`). |
| D. New server id | `begin_account_login` (`deliberate_new_local`), `local_server_opened` | Expected id is left open for a deliberate new server and pinned again from the challenge; no login is dropped. | Unchanged. |
| E. Signing-key change | `refresh_session` key recovery (`237769b`) | Rechecks the same response with fresh keys; keeps the saved login. | Unchanged. The proof for the local route is only handed out after that check. |
| F. Profile folder | `credential_vault.rs` `Relay/profile/<sha256(path)>/...` | Server creation does not change the profile, so no vault entry moves. | Unchanged. |
| G. New host route | `confirm_login_authority` | A brand-new local server has no pin; nothing is refused. | Unchanged. |
| H. Account service re-auth | `account_login.rs::finish` | `recent_auth = false` for ordinary sign-in (no `max_age`), so only the missing browser session (A) forced a password. | Unchanged. Linking (`max_age=300`) still requires a recent sign-in. |
| I. Session expiry after restart | `network.rs` refresh → `SignInRequired`, `reconnect_saved_server` | A local server whose 7-day session ended became "Sign in again"; signing in was point A, and the automatic background reconnect only marked it. | Both go through the silent local route for the server this PC hosts. |

## The fix

Server (`crates/relay-server/src/account_login.rs`, `account_identity.rs`, `lib.rs`):

- New route `POST /api/local/account-login`, body `{challenge_id, id_token}`.
  It is reachable only by the host app on this PC: direct loopback connection,
  no `Origin`/`Forwarded`/`X-Forwarded-*` header, never through the network
  listener (all `/api/local/*` routes are already refused there), the local
  control secret (`x-relay-local-control`) and this exact instance and server
  (`x-relay-instance-id`, `x-relay-server-id`). The check is the one
  `register_destination` already used, now shared as `host_app`.
- It completes the same one-use challenge as `/api/account-login` and then runs
  the same code (`complete`): first owner only on a server without users,
  existing linked identities sign in, a new account needs an invitation, a
  community authenticator is never bypassed, the creation's protection choice
  is applied.
- The ID token is checked like every proof (signature, issuer, audience,
  expiry, issue time within 5 minutes, valid subject and handle), except that
  its nonce is the desktop sign-in's own. A token whose nonce is a
  `relay-login-v1:` or `relay-link-v1:` proof is refused: those are valid only
  where they are bound.
- It is in the sign-in rate limit (15 per minute per source).

Client (`crates/relay-client`):

- `central_account::Controller::request_server_proof` asks the running refresh
  loop of the saved login for one extra refresh and returns its ID token after
  the usual checks (same saved subject, nonce, signing keys). The rotated
  refresh credential is saved by that same loop, so there is never a second
  refresher. When the loop ends (revoked, ended, sign-out) waiting requests are
  released, and the caller falls back to the account page.
- `account_login_begin` marks the challenge `local_owner` only when the address
  is this PC's loopback server and its runtime record proves it is the server
  answering now (`network_discord_host.rs::LocalControl`, the same strict check
  the Discord bot setup uses: server id, instance id, process id, address and
  data folder, control secret format).
- `account_login_challenge` then waits for the saved login's identity instead
  of opening the page (`ServerProof`), and sends
  `AccountLoginFinish { local_owner: true }`; the worker proves the runtime
  record again right before sending and posts to the local route. With no saved
  login running, an ended login, or no answer within 20 seconds, the ordinary
  account page opens with the same challenge (background reconnects are marked
  "Sign in again" as before).
- Point C: `claim_new_server` keeps the claim until the running sign-in ends.

What does not change: every server reached over the network (invitations,
Friends, HTTPS, iroh, Tailscale) still requires the destination-bound proof;
the client still refuses challenges that name another server or address; the
account shown in the app is still the one the server signs in (the server
links by issuer and subject, and the client's refresh loop rejects another
subject); E2EE device identities are per community and the first device on a
new server is the first own device, exactly as before, so device approval is
not touched.

Security note: the local control secret is the same capability that already
lets the host app shut the server down and change its Discord credentials. A
process that can read it (same Windows user) is out of scope in
`docs/SECURITY.md`; it gains nothing it could not already do with the saved
refresh token in Credential Manager.

## Related paths

- **Joining a second server by invitation** (remote): unchanged and still
  needs the account page. The remote server can only trust a proof bound to
  its challenge, and the app's page has no account-service session after a
  restart, so the password is asked there once per new server. This is the
  same root (incognito page + refresh-only saved login) and cannot be fixed on
  the client alone without weakening the nonce binding. Follow-up options: keep
  the account page's browser session in a persistent WebView profile (cleared
  on sign-out; limited by the account service's session lifetime), or an
  account-service token exchange that binds the server's nonce. Both are
  product/operator decisions.
- **Restarting after creating a server**: the new server's session is saved in
  `Relay/session/http://127.0.0.1:4580` (`RememberNextLogins` follows
  "Stay signed in"); automatic entry resumes it with the new server id. After
  its 7 days, or after switching to another local server, the sign-in goes
  through the local route without a page.

## Verified

Build environment as in the Global Constraints of
`docs/superpowers/plans/2026-10-29-audio-suppressor-round2.md`
(`CARGO_TARGET_DIR=D:\Projects\relay-build-targets\server-login-20261106`,
`-j 8`, 8 test threads). The `RELAY_*` folders (`RELAY_PROFILE_DIR`,
`RELAY_CONFIG_DIR`, `RELAY_CRYPTO_DIR`, `RELAY_SERVER_DATA_DIR`,
`RELAY_REPLAY_DIR`, `RELAY_TEST_WORK_ROOT`) were siblings in one scratch folder.
No server was started on port 4580; every test server is in-process with its
own temporary data folder.

Reproduction, before the fix (`cargo test -p relay-server --lib local_login_tests`):
3 failed. `a_server_created_on_this_pc_accepts_the_saved_relay_account_from_its_host_app`
passed its first assertion (the existing `/api/account-login` refuses the
refreshed identity with 400) and then failed: there was no other way in (401).

| Command | Result |
|---|---|
| `cargo test -p relay-server --lib account_identity` | 16 passed, 2 ignored (owned live journeys) |
| `cargo test -p relay-server` | 462 + 27 passed, 0 failed |
| `cargo test -p relay-client` | 1750 passed, 5 failed, 129 ignored; the 5 needed `RELAY_TEST_WORK_ROOT` next to the profile folder |
| the same 5 plus `windows_vault_owned_refresh_survives_two_process_restarts`, with `RELAY_TEST_WORK_ROOT` | 6 passed |
| `cargo clippy -p relay-client -p relay-server` | clean |
| `cargo clippy -p relay-server --all-targets` | clean |
| `cargo clippy -p relay-client --all-targets` | one error, `never_loop` in `local_voice_gateway.rs:945` (test code, not touched here, from `1b4c4f8`); no warning in changed code |

New tests:

- `relay-server` `account_identity::tests::local_login_tests`:
  - `a_server_created_on_this_pc_accepts_the_saved_relay_account_from_its_host_app`
    (the reproduction; first owner with the creation's protection choice, the
    same account again after a restart, one-use challenge);
  - `the_local_account_route_refuses_other_computers_proxies_wrong_secrets_and_bound_proofs`
    (LAN peer, wrong or missing secret, forwarded header, replaced instance,
    network listener, destination-bound and linking nonces, stale and forged
    tokens; nobody is created);
  - `the_local_account_route_keeps_owner_and_membership_rules_for_other_accounts`.
- `relay-client`:
  - `central_account::tests::a_restored_login_gives_this_pcs_server_a_fresh_identity_without_opening_a_page`
    (no page, no authorization code, one refresh per request, rotated
    credential saved, the desktop nonce and the saved subject);
  - `central_account::tests::an_ended_saved_login_releases_waiting_server_identity_requests`;
  - `account_start_ui::tests::this_pcs_server_uses_the_saved_login_and_falls_back_to_the_account_page`;
  - the existing `account_login_begin` network test now also checks that a
    server at another address is never marked as this PC's own.

Not reproduced or not run:

- The installed preview itself (real account service, WebView2 page, the
  stack on port 4580) was not driven: port 4580 and the user's installation
  are off limits. The cause is shown with the in-process server and the
  synthetic account service; the client glue between them
  (`account_login_challenge` → `ServerProof` → `AccountLoginFinish
  { local_owner }` → `LocalControl`) is covered by unit tests of each piece,
  not by one end-to-end run.
- Point C (claim during another sign-in) has no automated test: it lives in
  `RelayApp`, which needs a window to construct.
- Joining another person's server by invitation still asks for the password
  after a restart (see Related paths); that is unchanged by design.
