# Friends asks for the password again: cause and fix (21 Nov 2026)

Branch `session/friends-relogin-20261121`. The symptom: after starting Relay,
Friends says *"Friends needs you to sign in again. Select Retry Friends to
continue. Your Relay account is still signed in."*. Retry Friends opens the
account page, which asks for the password.

## How start-up reaches that message

1. `app.rs` starts the central account from its refresh token
   (`central_account.rs::resume`). That gives the app an account, not a
   session on any server.
2. `account_start_ui.rs::account_front` then starts `auto_entry_tick`. It
   resumes the saved servers first, then calls `open_people_with_interaction(false)`.
3. `open_people_with_interaction` sends `Command::Resume` for the people URL
   only when **a saved Friends session exists in Credential Manager**
   (`credential_vault::load`) and the URL is not already marked
   `SignInRequired`. Otherwise it starts a background account sign-in.
4. That account sign-in needs an ID token bound to the server's challenge
   nonce. Only the account page can make one for a remote server (the
   loopback route from 78b83ae is for this PC's own server). A background
   attempt never opens the page. It calls `defer_background_signin`, which
   shows the message above.
5. Retry Friends runs the same path interactively. The account page runs
   incognito (`account_webview.rs`, `with_incognito(true)`), so Keycloak has
   no SSO cookie and asks for the password.

So the message itself shows that **no usable saved Friends session existed
when the app started**. The client deletes the saved session in only three
places:

- the server answers 401 `Sign in again`: during the resume
  (`network_host_resume.rs`), on switching to a parked connection
  (`network_v2.rs` `reject_saved_session`) or on refresh of the open
  connection (`network.rs` refresh error branch);
- explicit Logout or turning off "Stay signed in";
- an identity mismatch (`reject_active_identity`).

The saved session that exists now is the one written by the last Retry. It
will be refused the same way when it reaches its fixed expiry.

## Why the people service refuses a saved session

| Reason | Applies? | How to tell it apart |
|---|---|---|
| **Fixed session lifetime.** `TOKEN_LIFETIME = 7 days` (`relay-server/src/lib.rs`). The expiry is set once at login and never extended, however often the session is used (security review R11). | **Yes. This is the cause.** Each Friends login lasts exactly 7 days, then the next start (or the next refresh while running) gets 401 and the saved login is deleted. | The prompt comes back about 7 days after the last password entry, not on every start. On the server, `SELECT created_at, expires FROM sessions` shows `expires - created_at = 604800` for every row. Reproduced in `sessions_slide_while_used_under_an_absolute_cap` (an expired row gets 401). |
| Server restart or upgrade | No. Sessions are rows in SQLite and survive a restart. | Test `a_restart_on_the_same_data_folder_keeps_sessions`. A restart that does drop sessions means the data folder changed. |
| Lost host seal key (`<data>/credential-seal/seal-key.v1`) | Only if the data folder was rebuilt without that file. Session verifiers are HMACs under that key. | Every user and every device is signed out at the same moment. The key file's modification time matches that moment. |
| Host key or authority mismatch | No 401. `resume_remembered` refuses before sending the token, with `IDENTITY_CHANGED` or "not the same server". It shows a different error and does not delete the saved login. | The Friends error text is the identity text, not the sign-in-again text. |
| Account token refresh racing the Friends resume | No. Friends is started only after `central_account.account` is set, and a server resume does not use the account token. de625a2 already fixed the one real start-up race: a 2-second health probe that timed out on a cold Tailscale/TLS connection. That race gave "could not connect", not the sign-in-again message. | A timeout shows "Friends could not connect…" after 20 s (`RESUME_WAIT`). It never asks for the password. |
| Clock | No. Expiry is compared only against the server's own clock. The client clock is not used for sessions. | — |
| Session slot overwritten by another profile | Not across profiles: the vault target includes `RELAY_PROFILE_DIR`. Within one profile, a Logout in one Relay process (for example a dev build run without its own profile) deletes the shared slot. | It happens right after another Relay build ran without its own profile. |
| Session revoked server-side | Possible but rare: a password change, "sign out other sessions", or an authenticator change all delete sessions. | It matches a user action on that account. |

## Fixes

### a. Sessions renew while in use (server; needs the Debian people service to be upgraded)

`relay-server/src/lib.rs`:

- A session now lasts `SESSION_IDLE` = 30 days past its last authenticated
  use, and never more than `SESSION_ABSOLUTE` = 180 days after
  `created_at`.
- `authenticate_token` calls `renew_session`, which moves `expires` forward
  to `min(now + 30 d, created_at + 180 d)`. It writes at most about once a
  day per session, and only when the server accepts writes (not during a
  read-only handover). It never brings back an expired session.
- A row from before `created_at` existed (`created_at = 0`) keeps its fixed
  expiry.
- A new login gets 30 days.
- The token does not change on renewal, so this needs no protocol change:
  an old client works with a new server and a new client works with an old
  server. The connected Friends client refreshes all the time, so a running
  Relay keeps its session alive with no client change.

Tests (written first, failing before):

- `sessions_slide_while_used_under_an_absolute_cap`: renewal, the cap, the
  legacy row, and that an expired session stays expired;
- `a_fresh_session_is_not_rewritten_on_every_request`;
- `a_restart_on_the_same_data_folder_keeps_sessions` (cause check).

Decision on rotation: the token is **extended, not rotated**. Rotation would
need the client to store each new token. Concurrent requests, the event
socket and parked connections can all present the old token during the
switch, and an old client would lose its session at the first rotation.
Renewal does not add replay surface: the token was only ever sent to this
server and is still bound to it.

Security change, written down: a stolen remembered session can now stay
usable while it is being used, up to 180 days instead of 7. R2 and R11 in
`security-review-accounts-20261105.md` get worse by that much. Device
approval is unchanged. A password change and "sign out other sessions"
still end every session at once.

### b. Silent re-authentication: evaluated, not implemented

The server accepts a Relay account only through an ID token whose `nonce`
is the server's challenge. Here is how Keycloak could provide one without
the password:

- **Refresh token:** Keycloak's refresh grant returns an ID token, but with
  the nonce of the original login. The client cannot set a new nonce. Token
  exchange (RFC 8693) has no nonce either. Accepting such a token would
  make it replayable to any other server, which the security review forbids.
  **Ruled out.**
- **`prompt=none` in a persistent, app-private WebView profile:** this works
  only while Keycloak's SSO session is alive. The realm
  (`deploy/identity/relay-realm.draft.json`) sets no `ssoSessionIdleTimeout`
  or `rememberMe`, so Keycloak uses its defaults: 30 minutes idle and
  10 hours maximum. After a normal restart the SSO session is gone,
  `prompt=none` answers `login_required`, and the page would still ask for
  the password. It would also store an SSO cookie on disk. **Not
  implemented.** It becomes worth doing only together with a realm change
  (Remember Me on, with SSO idle and max at least the app's restart
  interval), and the profile would have to be cleared on sign-out.

With (a), the page is needed only after 30 days without using Friends, or
after 180 days.

### c. Start-up race

None found that leads to the password prompt (see the table). No change.

## What the user will see

- **Now, with only the new client:** nothing changes. The people service
  still ends each session 7 days after the password was typed. Then Friends
  shows the same message once, and Retry Friends asks for the password.
- **After the Debian people service is upgraded to this server build:** the
  saved Friends login made by the next password entry stays valid as long as
  Relay is used at least once every 30 days, up to 180 days. Sessions that
  exist at upgrade time renew too: their `created_at` is set, so the user may
  not need the password even once more, as long as the current session has
  not expired yet. Rows older than the `created_at` column keep their fixed
  expiry.
- The message and Retry Friends stay as the fallback after 30 idle days,
  after 180 days, or after a password change.

## Verification

- `cargo test -p relay-server --lib`: 465 passed, 0 failed, 2 ignored. The
  new renewal test fails with the renewal call removed. One existing test,
  `live_voice_renewal_is_capped_by_login_expiry_and_requires_valid_login`,
  failed because bootstrap now renews the login it caps voice by. It now
  pins a non-renewing (`created_at = 0`) login, which keeps its intent:
  voice never outlives its login.
- `cargo clippy --workspace --all-targets --features relay-client/native-rtc -- -D warnings`: zero.
- Run after the user's game ended, with `RELAY_PROFILE_DIR`,
  `RELAY_TEST_WORK_ROOT`, `RELAY_CONFIG_DIR` and `RELAY_CRYPTO_DIR` on C::
  `cargo test -p relay-server` (all targets) gave 465 + 27 passed, 0 failed.
  `cargo test -p relay-client` gave 1777 passed, 0 failed, 130 ignored.
