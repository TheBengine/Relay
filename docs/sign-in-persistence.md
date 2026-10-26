# Staying signed in

Relay's shared-account sign-in is remembered by default. The welcome screen,
account settings and Privacy & Safety expose **Stay signed in on this PC**.
It keeps the shared account and subsequent server/Friends logins. Independent
password-only community accounts retain their separate remembered-login choice;
existing explicit choices are preserved.

On Windows the refresh credential is stored in Credential Manager, separately
from community session credentials and preferences. It is bound to the identity
service, desktop client and Relay profile. Passwords and shared-account access tokens are not
saved. Restart requests fresh, signed identity tokens before opening saved
servers. Refresh-token rotation is saved; temporary connection failures retry
without deleting the saved login or opening another sign-in page.

Closing Relay keeps remembered logins. **Sign out of Relay** and the main
**Log Out** action remove both shared-account and known server/Friends logins.
Turning off the saved-login choice removes stored credentials while allowing
the current session to finish. Changing shared accounts discards the previous
account's remembered connections before opening the new account's servers.

Older builds kept shared-account refresh credentials only in memory. They
cannot be recovered after closing the app. An upgrade therefore needs one
interactive sign-in; previously unsaved server/Friends sessions also need one
fresh approval. Later launches resume the protected saved sessions. A revoked
or expired refresh grant still requires sign-in again.
