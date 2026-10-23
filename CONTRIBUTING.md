# Contributing to Relay

Bug reports, usability feedback, feature suggestions and reviewed code changes
are welcome at [TheBengine/Relay](https://github.com/TheBengine/Relay).

## Report a problem or suggest a feature

Search existing issues first. Describe what you wanted to do, the steps, expected
and actual behavior, your Relay build/version, and the relevant Windows version.
For a call problem, describe both directions and whether voice activity or push
to talk was used. Report measured results with their scope; do not call server
response time microphone-to-speaker latency.

Use disposable accounts and synthetic media where possible. Remove passwords,
tokens, invitations, private keys, recovery codes, account identifiers and private
messages from reports and screenshots. Do not upload a personal profile or raw
database. Report security vulnerabilities privately using [SECURITY.md](SECURITY.md).

## Submit a change

Discuss substantial features in an issue before implementing them. Keep a pull
request focused, explain the user-visible behavior, and include the relevant
checks and remaining limitations. The maintainer decides what enters official
Relay releases. A public fork is not permission to offer a competing product;
read [LICENSING.md](LICENSING.md) and [LICENSE](LICENSE).

By intentionally submitting original code for inclusion in Relay, you agree that
your contribution may be used, modified and distributed under the project's
PolyForm Perimeter 1.0.0 terms. You retain your copyright. Submit only work you
have authority to license; identify third-party material and its original terms.
This is an inbound licensing statement, not a copyright assignment or permission
to relicense your contribution under arbitrary future terms.

Keep existing attribution. Do not add dependencies with incompatible terms or
change security/privacy behavior without describing the impact. Clearly label
generated or AI-assisted contributions and review their correctness and origin.

Run the checks relevant to your change after implementing it. Do not test against
somebody else's live server or capture a person's microphone, camera or screen
without their consent. Real hardware and different-network results must be
reported separately from local/generated tests.
