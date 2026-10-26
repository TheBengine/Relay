# Relay account service

The portable realm template remains **disabled** and contains no signing keys,
accounts or passwords. The selected Debian pilot is now active at
`https://relay-accounts.tail787aee.ts.net/realms/relay-accounts`, after the user's
explicit deployment request and free-address selection. Its separate PostgreSQL
database, HTTPS, actual optional OTP/recovery-code login, signup and real native
Relay callback have been exercised. See
[`account-service-live-20261020.md`](../../docs/account-service-live-20261020.md).
The historical loopback checks below are separate from that live acceptance.

1. Import `relay-realm.draft.json` into a separately operated Keycloak instance.
   Install `assets/security/common-passwords.txt` from the repository as
   `data/password-blacklists/relay-common.txt` first; the new-password policy
   uses Keycloak's `passwordBlacklist` provider and that exact file.
2. Apply `user-profile.draft.json` using Keycloak's user-profile settings/API.
   This is a separate configuration, not a field silently assumed to be imported
   with the realm. Verify signup without email, immutable handles and rejection
   of concurrent duplicate usernames, including case variants.
3. Verify recovery and optional authenticator policy in that actual instance.
   Password reset by email is deliberately disabled while email verification is
   deferred. Do not open public registration without an established recovery path.
4. Serve the realm over HTTPS with discovery, JWKS, authorization and token
   endpoints on the same origin. The native client uses authorization code flow,
   PKCE S256 and the exact loopback redirect above. It has no client secret and
   never uses the password grant. The normal Keycloak `profile` scope must include
   `preferred_username` in signed ID tokens. Keep Keycloak's `basic` default
   client scope too: its `auth_time` ID-token mapper is needed to verify recent
   account-link approval. On an existing client, add `basic` as a default scope
   and verify its `auth_time` mapper is enabled for ID tokens. Do not work around
   a missing claim by removing the server's recent-sign-in check.
   Only a validated `(issuer, subject)`
   identifies an account; `@handle` uniqueness comes from the shared database.
5. Enable only after the provider and client acceptance tests have passed. Set
   `RELAY_ACCOUNT_ISSUER=https://YOUR-SERVICE/realms/relay-accounts` for the Relay
   build/runtime; client ID defaults to `relay-desktop`. A community's messages
   cannot change this provider. An unconfigured app explains that the shared
   service is not connected yet and offers its existing server-account route;
   it makes no identity-service network requests.

The client signs into an app-wide account and renews its session with the
provider's normal refresh grant. Stay signed in on this PC is enabled by default;
Windows protects the refresh credential in its credential vault, separately from
community credentials and preferences. Restart obtains fresh verified identity
tokens before reconnecting servers and Friends. Provider session expiry and
revocation still apply. Relay does not automatically request offline access.
On Windows, the actual HTTPS login and registration page opens inside Relay's
private WebView2 surface. Its theme is therefore identical to the browser page.
No invitation or community membership is needed to create or use a Relay account.
The explicit browser option remains available, including when WebView2 is absent.
Closing Relay keeps remembered logins. Older builds need one interactive sign-in
after upgrading because they did not save the shared refresh credential. Turning
off Stay signed in removes saved credentials; Sign out of Relay also forgets
known server and Friends logins. Shared tokens and global passwords are not sent
to community servers or placed in community vault entries. See
[sign-in persistence](../../docs/sign-in-persistence.md).
The account-first screen now leads to saved servers, creating a server, or
joining with an invitation. Participating servers accept a fresh, signed,
destination-bound proof of that same Relay account: no second server password
is created. Schema 22 adds one-use login challenges. First-owner creation stays
local-only; registration on an existing community requires an invitation.
Removed members need a new invitation; banned members cannot use SSO to return.
E2EE remains the default and the existing creation choice is retained.

Existing community accounts, friends and E2EE keys remain independent. Link an
old account explicitly after its normal password/authenticator sign-in; matching
names never adopt it. A legacy community authenticator cannot be bypassed by
SSO. Account-only members manage password/OTP/recovery at the shared service;
legacy members keep their community credentials. Signing out of Relay clears
its remembered community sessions while keeping the server list and chat keys.
Shared contact discovery is separate and is not implemented by this change.

## Optional community account link

Community operators can opt into linking by setting the same
`RELAY_ACCOUNT_ISSUER` and `RELAY_ACCOUNT_CLIENT_ID` on the server and app, plus
`RELAY_ACCOUNT_COMMUNITY_URL` on the server. The last value is the exact canonical
community base URL that clients use. It accepts HTTPS, loopback HTTP, or a
64-hex-character `.iroh` host; plain LAN HTTP is not accepted for account proofs.
iroh already encrypts its transport independently of chat E2EE. A missing issuer
disables shared-account sign-in/linking; ordinary community login still works.
An omitted canonical community URL defaults to loopback for the app-owned local
host. Set it explicitly for linking through a public or iroh address. New SSO
challenges bind the exact validated address requested by the client.

In My Account, a signed-in user explicitly chooses to show their Relay name,
confirms the current community password and optional community authenticator,
then approves the same global account in the browser. The new signed ID token
is sent only to that community for verification. It is bound to the actual
community URL, local user, server and a single-use session challenge. The server
checks signature, issuer, audience, nonce, expiry and recent authentication;
no ID token is stored in the database. One global subject/handle can link to
only one local user in that community.

Linking makes the handle visible to that community's active members in profiles
and the existing friends search. “Linked Relay name” means the community checked
account ownership; it is not independent E2EE device verification. Nothing links
automatically because names match. Removing the link requires fresh local
password/authenticator confirmation and works without the global service.
Local IDs, permissions, friends, messages and chat keys remain unchanged.

Schema 21 adds only link/challenge tables through the existing migration path.
Focused signed-provider/API and client-flow checks are recorded in
`docs/evidence/community-account-link-20261020.json`. The native combined linking
journey has now also passed against the owned local provider/server. Public
provider operations remain separate from this local acceptance.

Primary references: [Keycloak administration](https://www.keycloak.org/docs/latest/server_admin/index.html),
[OIDC client library](https://docs.rs/openidconnect/4.0.1/openidconnect/).

## Local runtime verification — 20 October 2026

The realm and separate user-profile configuration were actually imported into
an isolated loopback Keycloak 26.8.1 with its development H2 database. Only the
temporary QA copy enabled the realm and allowed loopback HTTP. This tracked
draft remains disabled and requires HTTPS; the production Relay client still
rejects HTTP providers.

Concurrent admin creates produced one 201 and one 409; case variants also
collided, invalid handles were rejected, and email/phone/mandatory OTP were not
required. Two actual self-service form submissions also created exactly one
account. PKCE code exchange and refresh succeeded, and Relay's normal OIDC
library verifier accepted the actual signed ID tokens and stable subject/name.
The owned provider and native QA client were shut down normally.

One usability finding remains: under a self-service registration race, the
losing H2 request returned a generic 400 page after the unique-index collision.
Do not label that public registration as accepted. Verify the chosen production
database, useful duplicate-name feedback, HTTPS/browser operation, backup and
recovery before enabling the service. No provider/database patch or security
relaxation is included here. See `docs/evidence/keycloak-local-20261020.json`.

## Combined local account-link journey

The real Keycloak service, normal Relay OIDC worker and a real Relay HTTP server
are also exercised together with owned Alice/Bob accounts. This found an actual
configuration omission: assigning only `profile` left `auth_time` out of ID
tokens. Login succeeded but the server correctly rejected linking. Keeping the
default `basic` scope restores that signed claim; the strict verifier is unchanged.
Keycloak documents the claim's move to this scope in its
[upgrade notes](https://github.com/keycloak/keycloak/blob/main/docs/documentation/upgrading/topics/changes/changes-25_0_0.adoc#new-default-client-scope-basic).

The combined check covers fresh app sign-in, second destination-bound approval,
linking Alice without replacing her local user/profile, rejecting proof replay,
rejecting a second mapping to Bob, and unlinking. Logs retain the original
failure and the corrected result, with session tokens and authorization URLs
redacted. See `docs/evidence/community-account-link-live-20261020.json` for exact
inputs, outputs and cleanup. This uses an owned HTTP form harness on loopback,
not native browser UI, public TLS, recovery or production hosting acceptance.

## Native browser and Relay window verification

Codex also completed the real native journey through Cua Driver on the secondary
screen: My Account, browser sign-in, visible global handle, explicit community
link with fresh local password, a second real browser approval, visible linked
name, and unlink with fresh local authentication. An independent server readback
observed the mapping appear and disappear. Alice's local name and signed-in
global account remained intact. The owned browser/profile and client were closed
normally; the test provider and server exited 0.

This uses the normal Relay UI inside an explicitly selected `cfg(test)` wrapper
that supplies the owned HTTP issuer and routes browser requests into an owned
Chrome profile. Production HTTPS policy is unchanged. Relay's Cua bitmap was
black, so its existing actual-renderer probe provided visual evidence; all input
was delivered by Cua. The final selected functional pass also reports 80 passing
tests. See `docs/evidence/focused-native-account-link-and-final-checks-20261020.json`.
Public identity hosting, recovery, production signup feedback and TLS deployment
were not enabled by those historical local checks. The later live pilot and
its separate acceptance are recorded in the deployment document linked above.
