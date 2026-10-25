# Publisher setup for the existing updater

## Current preparation — 25 October 2026

The `.7` bootstrap is being prepared; it is not yet a published release. The
source now bundles the real public TUF root and the beta policy below. Fresh
production keys were created locally, separate from test keys, and are stored
with Windows current-user DPAPI protection and restricted permissions in
`%USERPROFILE%/.relay-publisher`. Private keys are not published.

The root requires two distinct Ed25519 signatures. Targets, snapshot and timestamp
have separate signing keys. Both root keys currently belong to the same Windows
profile: this is not independent custody, and copying the DPAPI files alone is
not a portable recovery backup. Operator backup/recovery must preserve that
distinction.

`scripts/publish-updates.py publish` stages signed metadata and the exact installer
locally. Publish its `repository/` contents under the selected channel, retaining
numbered roots and immutable installer targets. Timestamp and snapshot expire
after 30 days; targets after 90 days. Refresh before expiry by preparing the same
package, installer, notes and handoff again; the tool increments metadata versions
without changing the release identity. No refresh schedule has been installed.

The new `in-place-v2` flow downloads, verifies, normally closes Relay and its own
hosts, installs into the verified existing installation and reopens Relay after
one **Update and restart** choice. Portable copies retain manual installation.
The existing `.6` client needs one manual installation of the forthcoming `.7`
bootstrap before it can use this channel. Publication and the current candidate's
final checks are tracked separately; configuration is not proof of deployment.

TUF signatures authenticate approved update bytes. No Authenticode certificate
exists, so this does not establish a Windows-verified publisher identity or remove
warnings from the initial manual installation. Older fixture evidence below is
historical and does not establish production distribution acceptance.

## Compiled public policy

The updater now reads `crates/relay-updater/publisher/policy.json` and `root.json`
with `include_bytes!`. These are reviewed source inputs, not runtime paths.
No environment variable, community, account response, preference or downloaded
metadata can select the initial root or replace its HTTPS bases.

The prepared beta policy is:

```json
{
  "schema": 1,
  "mode": "signed",
  "metadata": "https://thebengine.github.io/Relay/updates/beta/metadata/",
  "targets": "https://thebengine.github.io/Relay/updates/beta/targets/",
  "channel": "beta",
  "root_sha256": "383afc351bd45cf480be147e2a23f5831be25ec04f179e08acee3bbc188c7770"
}
```

These are the selected publication addresses; this document does not assert that
the candidate or metadata has been published yet.
Only `stable` and `beta` are accepted. Both bases require HTTPS with trailing
slashes and no credentials/query/fragment. The existing transport refuses
redirects; a GitHub release URL that redirects to another host is insufficient.
Unknown fields, schema/mode errors, root hash mismatch, unsigned roots and
insufficient root signatures are rejected. Malformed policy is visibly unavailable
in settings, rather than silently treated as intentionally unconfigured. Neither
state starts an update worker or resolves personal update storage.

`Publisher::from_bundled_root` retains the existing threshold of at least two
root signatures, and rejects root-key aliases with the same decoded public
material even when they have different key IDs. The bundled public policy does
not establish independent custody or the human publisher's identity on its own.

## Historical owner/operations decisions

On 23 October the owner selected **TheBengine** as Relay's publisher brand.
The Windows installer source now sets `AppPublisher=TheBengine` for future
builds. This is display metadata, not an Authenticode identity or a certificate.
Existing packaged installers are unchanged. The owner has now confirmed that
the publisher is an **individual**, not a registered organization. TheBengine
remains the product/maintainer brand; it does not substitute for a certificate
authority's validated subject. No signing certificate or account was created.

The owner confirmed on 21 October that no signing certificate/service exists.
Before opening a paid account, establish the real publisher's legal identity and
eligibility. Microsoft's current [Artifact Signing prerequisites](https://learn.microsoft.com/en-us/azure/artifact-signing/quickstart#prerequisites)
allow Public Trust for EU organizations, but individual developers must be in
the US or Canada. An individual EU publisher must not be assumed eligible for
that route. No account, subscription, certificate or private signing key was
created, and no purchase was made by this preparation.

### Selected direction: public source under PolyForm Perimeter — 23 October 2026

Published and read back: [public commit d813bdf](https://github.com/TheBengine/Relay/commit/d813bdfd67fbdb258405d157c48026c695a38e00).
All 1,488 remote blob IDs match the reviewed snapshot; LICENSE, LICENSING.md,
CONTRIBUTING.md, README.md and Cargo.toml were also downloaded and compared.
Private vulnerability reporting is enabled and read back. GitHub's first push
was safely rejected for a token-shaped upstream documentation example. It was
replaced with an obvious noncredential placeholder, its upstream license and
patch notice were preserved, and the rejected unpublished commits were omitted
from the successful normal push. No secret-scanning bypass was used.
See `evidence/source-publication-20261023.json`. Source publication does not enable
the updater or finish real signing/hardware acceptance. PolyForm Perimeter is
not OSI open source and does not qualify by itself for a free OSS signer.

The owner selected the proposed license and requested public source. Implement
PolyForm Perimeter 1.0.0 for Relay-authored code, with third-party terms and
existing MIT rights retained. Describe this as source available, not OSI open
source. This new authorization supersedes the earlier private-source hold and
permits a reviewed source snapshot in TheBengine/Relay; it does not select a paid
signer, purchase a service, install an app or publish a new binary release.

The root license and Cargo metadata are updated. Preserve the previous MIT text
in LICENSES/MIT.txt for earlier grants and third-party notice generation. The
notice generator must no longer derive upstream MIT terms from Relay's new root
license. Vendor code and documented upstream patches retain upstream terms.
Current metadata review covers the locked Windows-native workspace and the
separate iroh graph, with no GPL-only dependency found in those scopes; MPL,
fonts/data/assets and Go/native obligations remain distinct. This is not an
independent legal certification or a guarantee against competing reuse of prior
MIT material. Contributor terms preserve individual copyrights and establish no
blanket commercial relicensing permission.

Public source is a clean snapshot on top of the existing presentation repository.
Do not push private development history, historical operational/QA records,
named maintenance/deployment scripts, databases, profiles, credentials or keys.
The root guide, contributor instructions, bug/feature forms, private vulnerability
reporting and reproducible source/patch inputs belong in that publication.
Source publication is not hardware/WAN acceptance or a signed-binary release.

### Owner clarification: no corporate code reuse — 23 October 2026

The owner clarified that companies must not copy Relay's code. The earlier
conditional willingness to use open source is not consent to corporate copying
under a reciprocal license. Future commercial income and community bug reports
and suggestions remain desired possibilities. Do not infer a prohibition on
companies merely installing/using the compiled app from a restriction on code
reuse; those end-user terms have not been selected.

Ordinary OSI open source licenses, including AGPL-3.0, permit corporate copying,
commercial use and derived works subject to their conditions. AGPL can impose
source-sharing obligations, including modified network services, but it cannot
satisfy an absolute prohibition on corporate code reuse. The earlier AGPL
candidate therefore does not meet the owner's clarified condition. See the
[Open Source Definition, sections 1, 3, 5 and 6](https://opensource.org/osd).

Keep application source private pending a deliberate rights/license decision.
No application source publication, permissive or AGPL license change, or free
OSS-signing application is authorized by these replies. A public bug tracker and
feature suggestions do not require public application source. Selected outside
code contributions would need explicit access and inbound contribution terms;
no contributor agreement or outside access has been established.

The current root `LICENSE` remains MIT, which allows commercial and proprietary
reuse subject to its conditions. Its presence and prior binary/source releases
need a focused ownership and distribution review before any new restrictive
license is drafted. An edit cannot be assumed to withdraw permissions already
granted to recipients. No license text, package metadata, existing attribution,
source visibility or external repository was changed by this clarification.

A source-visible license restricting corporate reuse would not be OSI open
source and would need separate review, as would a proprietary distribution
license. Protecting the actual code does not create exclusive rights to the
underlying ideas; see [WIPO](https://www.wipo.int/en/web/copyright/protection).
Code-signing eligibility and the existing disabled updater remain separate:
SignPath's free OSS route is not a fit for a corporate-copying prohibition, and
no paid route, account, purchase or certificate has been chosen.

### Individual-publisher route review before source publication — 23 October 2026

Current primary sources were checked after the individual-publisher reply:

- [Microsoft's prerequisites](https://learn.microsoft.com/en-us/azure/artifact-signing/quickstart#prerequisites)
  still restrict Public Trust individual enrollment to the US/Canada. A Danish/EU
  individual must not be enrolled as an organization to bypass that restriction.
  This is conditional on residence; the owner's country has not been verified.
- [SignPath Foundation's terms](https://signpath.org/terms) require a qualifying
  open source project and verifiable builds. Foundation approval, MFA and human
  signing approval are necessary; the trusted certificate publisher is the
  Foundation. TheBengine can remain Relay's maintainer brand. Modified upstream
  binaries need separate compliance review. Eligibility is not established.
- At this earlier review, the [public Relay repository](https://github.com/TheBengine/Relay)
  contained presentation/assets/downloads and said application source was
  unpublished. The verified source publication above supersedes that state. The local MIT license does not make that source public. There
  is no tracked GitHub workflow in this isolated source checkout. A source and
  CI release must be separately prepared/reviewed before the Foundation route.
- [Certum Standard](https://www.certum.eu/en/code-signing-certificates/) supports
  individuals. Its certificate identifies a verified person; it is a paid option,
  with identity/address checks handled directly by the provider. Do not collect
  identity documents in chat. EV is an organization product. Exact cloud/card,
  country eligibility and checkout cost must be confirmed before purchase.

The owner was asked which route to **prepare**, with no purchase or publication
implied. No source archive or application was submitted, no third-party account
or certificate was created, and the real updater policy/root remain disabled.
Approval for a free OSS program must not be claimed before the provider grants
it. Do not add a sponsorship badge or invent signing-policy team members.

Both routes must return to the existing release sequence below: sign actual
application/installer bytes as appropriate, verify Windows trust/timestamp,
then hash the final installer for TUF. Neither a provider choice nor a certificate
replaces the independent updater root-key/repository/native-handoff requirements.

1. Establish the real publisher identity, public root fingerprint and custody of
   the existing policy's separate root signing keys. Use public root metadata
   only in source; private keys, recovery material and code-signing credentials
   never belong in the repository, a release, logs or chat. The
   [TUF root/key-management guidance](https://theupdateframework.github.io/specification/latest/#root-role)
   calls for keeping root private keys offline. Online repository keys and their
   rotation/recovery responsibilities must be deliberate.
2. Obtain/configure the Windows signing identity and timestamping service. Signing
   a target in TUF does not provide an Authenticode publisher identity to Windows.
   No certificate/account or private root material has been created by this step.
3. Choose a directly reachable HTTPS repository that can serve the complete
   installer without redirects. Preserve root rotation files and immutable target
   names; publish targets/metadata before publishing the new timestamp. Keep
   sequence/version monotonic and metadata unexpired. The repository layout and
   signature format remain those of the existing updater.
4. After all feature implementation, run the prepared targeted policy/updater/UI
   checks. Replace the placeholder with the approved signed public root, record
   its exact hash, compile the matching policy into the client, then run the
   offline preflight below on that same compiled source. Bind its receipt to the
   actual client build identity and installer hash; a helper from another build
   cannot prove what a client contains.
5. Build the installer, sign it, and verify its actual Windows signature and
   timestamp. Only then hash the final bytes and sign/publish the existing TUF
   release/target metadata. Authenticode changes installer bytes: never publish
   a pre-signing hash. Keep explicit download/install consent and the existing
   normal shutdown/wait-parent-v1 handoff.
6. Finish the configured native download/handoff/restart check in an owned
   profile, interruption/recovery and the public repository readback. Treat
   actual identity, configured app behavior and physical/WAN acceptance as
   separate receipts. No publication is performed by this source preparation.

## Offline preflight (executed with the disabled policy)

The final matching production build compiled and ran
`relay-publisher-check`. `--require-configured` fails for a disabled or invalid
bundle, allowing the release process to reject a manual-update-only build when
it intends to ship automatic updates. Without the flag, a valid disabled bundle
reports `configured:false` with success. It never downloads, writes profile data,
generates keys or signs an installer.

```text
cargo run --locked -p relay-updater --bin relay-publisher-check -- --require-configured
```

The JSON receipt identifies the compiled policy and explicitly says that it did
not check Authenticode or exercise the repository/installer. This is an offline
policy preflight, **not** final distribution acceptance. The full existing
[update engine](update-engine.md) and [native repository evidence](native-update-repository.md)
remain in place; this work adds the missing configuration path, not another
update engine.
