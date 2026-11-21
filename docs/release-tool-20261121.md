# Release tool (21 November 2026)

`scripts/release/` replaces the 17 scripts that were copied into a new folder and
string-replaced for every preview release. This document is the design (written
before the code), how to cut the next release, what still needs a person, and what
could not be checked without publishing.

## 1. What changed between releases, and what did not

Compared: `relay-release-20261107-1` (+ `-verify`) against `relay-release-20261106-1`
and `relay-performance-release-20261104` in `D:\Projects\relay-archive\work`.

Constant in every release (now code in the package):

- The verification harness (`verify.ps1` was byte-identical 1106 → 1107), the test
  summary, the lint audit against the previous release's `lint-audit.json`, the
  locationless `duplicate_mod` evidence, and the finalize checks.
- `verify-package.py`, `authorized-preflight.py`, `check-pages.py`,
  `verify-retained-downloads.py`, `verify-live-site.py`, `stage-signed-feed.py`
  (apart from the pinned publisher hash), `prepare-pages-plan.py`, `bind.py`,
  `finalize-publication.py`: identical apart from identifiers.
- Safety rules: direct local C:/D: paths on fixed drives, no reparse points or hard
  links, the source guard around every build phase, the overlay path rule, the
  pinned TUF root `383afc35…` and updater policy identity `c0b6644b…`, the native
  updater size bound (512 MiB) and the 100 MiB Git blob bound, the monotonic build
  sequence and metadata version, the public checkout must be clean and on the
  expected base, `--atomic` push of main and tag, digest readback of every upload.

Changed per release (now either the config or derived):

| Value | 1104 → 1106 → 1107 | New source |
| --- | --- | --- |
| tag, title, notes, update notes, commit messages | typed in 6 scripts | config |
| previous tag | typed in 4 scripts | derived: newest published receipt, checked against `gh api releases` and `git ls-remote` |
| previous public commit | typed | derived: previous `publication-receipt.json` `public_commit`, checked against the previous tag on the remote |
| public base | typed in 3 scripts | derived: previous `public_docs_commit` (else `public_commit`), checked against remote `main` |
| pages base | typed in 2 scripts | derived: previous `pages_commit`, checked against remote `gh-pages` |
| previous sequence / payload / inputs | typed paths in 5 scripts | derived from the previous release folder |
| `publish-updates.py` hash | typed | computed: must equal its entry in the frozen `inputs-Build.json` |
| checker hash, policy identity | typed | policy identity computed from `policy.json` and compared with the pin; checker hash stays a reviewed-tool pin in `defaults.json` |
| server reused (schema 30) / fresh (schema 31) | edited asserts in 4 scripts | config `server.mode`, schema read from `community.rs` and compared |
| iroh helper reused | edited asserts | config `iroh.mode`; reuse requires unchanged `iroh-helper/` inputs |
| test minimums (106 selected → ≥1600 full) | edited asserts | config `verification.min_tests` |
| reviewed vendor paths (`VENDOR_REVIEWED`) | duplicated in 2 scripts, grew per release | one list in `allowlist.py` plus config `source.allowlist_additions` |
| overlay allowlist (56 paths for 1107) | hand-listed | derived from the semantic diff against the public base; the human review binds its digest |
| docs to publish | hand-listed | config `source.publish_paths` |
| review / scope / limitations text | edited in `finalize-verification.py` | config `verification` |
| large-target rotation (`targets[1:]`, tag asserted) | typed tag | derived by the retirement rule |
| retired page targets | hard-coded `Relay-0.10.0-2026102` prefix | derived by the retirement rule |

## 2. Config

One file per release, `release.json`, in the release folder:

```json
{
  "schema": 1,
  "tag": "v0.10.0-preview.20261122.1",
  "title": "Relay 0.10.0 - …",
  "notes": "release-notes.md",
  "update_notes": "update-notes.txt",
  "review": "release-review.json",
  "source_commit_message": "…",
  "docs_commit_message": "Point downloads to the … preview",
  "server": {"mode": "fresh", "schema": 32},
  "iroh": {"mode": "reused"},
  "source": {"publish_paths": ["docs/…md"], "allowlist_additions": []},
  "verification": {"review": "…", "scope": "…", "limitations": ["…"],
                   "min_tests": {"client": 1600, "server": 440, "protocol": 70}},
  "pages": {"keep_latest_installers": 3}
}
```

Optional: `source_checkout` (defaults to the checkout the tool runs from),
`artifacts_dir`, `previous_release_dir`, and `overrides` — the only way to accept a
remote base that differs from the derived one (`{"public_base": {"commit": …,
"reason": …}}`). Everything else comes from `scripts/release/defaults.json`
(repository, URLs, toolchain paths, checker pin, release search roots).

## 3. Derived values

`relay_release.config.derive` reads every release folder under the search roots
(`work\releases`, `work`, `D:\Projects\relay-archive\work`), keeps those whose
`publication-receipt.json` says Published, and takes the newest by build sequence
that is older than the current one. From it: previous tag, previous public commit,
public base, pages base, previous sequence, previous metadata version, previous
payload and frozen inputs, previous lint baseline, previous large targets. Then
read-only checks: `git ls-remote` (previous tag → previous public commit, `main` →
public base, `gh-pages` → pages base, new tag absent) and `gh api` GET (newest
published release is the previous tag). A mismatch stops the step; only an
`overrides` entry with a reason accepts it.

Hashes are always computed from bytes; nothing per release is typed.

## 4. Folder layout

```
work\releases\<tag>\                  (outside the repository; small; keep forever)
  release.json  release-notes.md  update-notes.txt  release-review.json
  receipts\                           every JSON the old scripts wrote, same names
    verify\                           inputs-*.json, source-guard-*.json, *-test-results.json,
                                      lint-audit.json, verification.json, …
    steps\<step>.json                 inputs/outputs digests per step (staleness)
D:\Projects\relay-release-artifacts\<tag>\   (large; reproducible from receipts + source)
  verify\{candidate,logs,temp,…}  payload\  assets\  installer\  staged-generation-N\
  public-source-overlay\  public-pages-overlay\  public-docs-overlay\  extracted-smoke\
```

Old folders (`relay-release-20261107-1` + `-verify`) are read through a legacy
layout, so `replay`, `status` and the derivation work on them unchanged.

## 5. Subcommands

`python scripts/release/release.py <command> --release <folder>`

| Command | Old scripts | Writes |
| --- | --- | --- |
| `verify [--part P]` | `verify.ps1`, `run-*.sh`, `summarize-tests.py`, `audit-lint.py`, `finalize-verification.py`, `make-release-validation.py` | verify receipts, `verification.json`, `release-validation.json`, `inputs-Build.json` |
| `package` | `assemble.py`, `bind.py`, `verify-package.py` (+ notices regeneration) | `package.json`, `release-config.json`, `package-verification.json` |
| `source` | `prepare-public-source.py`, `deploy-reviewed-public.py source` (dry) | `source-overlay-allowlist.json`, `source-overlay-manifest.json`, `source-input-mapping.json`, `public-source-projected-proof.json` |
| `feed` | `stage-signed-feed.py`, `prepare-pages-plan.py`, `deploy-reviewed-public.py feed` (dry) | `staged-generation-N/publication.json`, `retired-*.json`, `pages-size-plan.json`, `docs-plan.json`, `server-deployment-plan.json` |
| `seal` | `seal-ready.py` | `release-ready.json` |
| `publish [--execute]` | `authorized-preflight.py`, `deploy-reviewed-public.py`, `publish-release.py`, `verify-retained-downloads.py`, `check-pages.py`, `verify-live-site.py`, `verify-native-feed.ps1`, `publish-docs.py` | the same receipts as before, stage by stage |
| `finalize` | `finalize-publication.py` | `publication-receipt.json` (final) |
| `status` | — | nothing; shows each step as done / stale / missing |
| `replay` | — | nothing; re-validates a finished release from its receipts |
| `pages-plan` | — | nothing; retirement dry run against a gh-pages checkout |

Every step records the digests of what it read and wrote in `receipts\steps\`. A
step whose inputs are unchanged is a no-op; a step whose upstream outputs were edited
by hand refuses; a local step whose inputs changed re-runs and moves its old outputs
to `superseded\`; the feed step never re-stages silently (a metadata version is
consumed) and needs `--new-generation`.

## 6. Pages diet

Rule (in `pages.py`, unit-tested): keep every installer the signed `targets.json`
(current and newly staged) references, plus the newest N by build sequence
(`pages.keep_latest_installers`, default 3); every other installer under
`updates/beta/targets/windows-x64/beta/` is retired — tracked files through
`retired-page-targets.json` (`git rm` in the feed commit), large mirrors by leaving
them out of `.github/large-targets.json`. Before anything is retired the tool
finds, by `gh api` GET, a published release whose `Relay-Setup.exe` asset has the
same size and `sha256:` digest; the feed step additionally downloads the asset and
hashes it. A file without that proof is kept, and the plan says so. The plan
reports the tracked bytes, the materialized large bytes and the projected site.

## 7. Results on 21 November

- **Unit tests:** `python -m unittest discover -s scripts/release/tests -t scripts/release/tests`
  runs 36 tests (config derivation, allowlist rule and overlay plan, receipt
  validation, step records and staleness, native-check gate, retirement selection,
  Pages plan on a throw-away git repository, Pages run decisions). With
  `RELAY_RELEASE_REPLAY=1` a 37th test runs the full replay below. All pass.
- **Replay of 7 November** (`release.py replay …\relay-release-20261107-1`): 51 checks,
  0 failed; report in `docs/release-tool-20261121/replay-20261107.json`. Reproduced
  from the 6 November receipts alone: previous tag, previous public commit `7f1b682`,
  public base `39bf269`, pages base `5d970e9`, metadata version 22, monotonic
  sequence. `git ls-remote` confirms both tags; `gh api compare` confirms each base
  was the direct parent of what was pushed; `gh api` confirms the asset digests.
  Every receipt validates against the bytes on disk (assets, payload, staged feed,
  overlays, logs). This checkout's bytes equal the frozen 4,846-file input map, so
  the source guards, both lint audits (with the re-collected locationless evidence),
  the client fingerprint `da43e3ac…` and the source projection were re-run:
  `source-input-mapping.json` and `public-source-projected-proof.json` came out
  byte-identical, the derived 56-path overlay equals the hand-written allowlist, and
  the only paths a person still had to name were the five docs. `index.html`,
  `docs-plan.json`, `.github/large-targets.json` and `retired-large-targets.json`
  were rebuilt from the bases and match. The `publish-updates.py` hash that used to
  be typed equals its frozen input entry; the policy identity `c0b6644b…` is computed
  from `policy.json`.
- **Notices:** `generate-notices.py` regenerated `THIRD_PARTY_NOTICES.txt`
  byte-identical (`b01048d8…`) on this checkout (it needs its upstream-license cache
  or network, so the check does not use `--offline`).
- **Pages retirement dry run** (`release.py pages-plan --verify api
  --next-installer-bytes 114053980`, gh-pages `d08474e`, read-only; full plan in
  `docs/release-tool-20261121/pages-retirement-dry-run.json`):

  | State | Site |
  | --- | --- |
  | Now (548 MB tracked + 433 MB mirrors) | 980.5 MB |
  | After retiring the 7 tracked installers of 28 Oct – 2 Nov, each proven to be the `Relay-Setup.exe` asset of its release | 432.6 MB |
  | At the next release (adds a ~114 MB installer; the 4 Nov mirror rotates out; N = 3) | **387.4 MB** |

  The 400 MB target is met at the next release with the default N = 3. Meeting it
  before then would take N = 2 (273 MB), a decision listed below.

## 8. Cutting the next release

1. Create `C:\Important\Projects\2026-10-03\can\work\releases\<tag>\` with
   `release.json`, `release-notes.md`, `update-notes.txt` (the signed in-app notes,
   at most 4 KiB) and, after review, `release-review.json` (`reviewer`,
   `blocking_findings`, `publication_scripts_reviewed`,
   `source_overlay_allowlist_sha256`).
2. From the source checkout being released, with `R` set to that folder:

```powershell
python scripts\release\release.py status --release $R
```
```powershell
python scripts\release\release.py verify --release $R
```
```powershell
python scripts\release\release.py package --release $R
```
```powershell
python scripts\release\release.py source --release $R
```

3. Review `receipts\source-overlay-allowlist.json` and put its sha256 in
   `release-review.json`.

```powershell
python scripts\release\release.py feed --release $R
```
```powershell
python scripts\release\release.py seal --release $R
```
```powershell
python scripts\release\release.py publish --release $R
```
```powershell
python scripts\release\release.py publish --release $R --execute --reason "<who approved>"
```
```powershell
python scripts\release\release.py finalize --release $R
```

`verify` takes hours (full suites and release builds); `verify --part Tests` runs a
single part. The first `publish` is the read-only preflight. `publish --execute`
resumes after an interruption (finished stages are checked and skipped); `--only
<stage>` runs one stage.

## 9. Still a human decision

- Tag, title, both notes, the two commit messages, which docs go public
  (`source.publish_paths`), and the review itself.
- `server.mode` / `iroh.mode` and the schema of a fresh server (checked against the
  source and the previous release, not chosen by the tool).
- Any path outside the base rule (`source.allowlist_additions`); a semantic change
  outside the rule stops `source` until someone lists it.
- When remote `main` or `gh-pages` moved after the previous release:
  `overrides.public_base` / `overrides.pages_base` with a reason, after reading what
  landed.
- `pages.keep_latest_installers` (3; 2 reaches 273 MB at once).
- Re-staging the feed (`feed --new-generation` consumes a metadata version).
- Approving `publish --execute`, and deploying a fresh server to the home host (the
  tool never does).
- A failed Pages build (as opposed to a failed deploy job) is not retried.

## 10. Not verified here (needs a real release)

- `verify`, `package` (launcher, Inno installer, smoke test), feed staging (opens
  the publisher store) and every `publish --execute` stage were not run. Their code
  is a parametrised port of the scripts that published 7 November, and the replay
  validated the receipts those scripts wrote, but the ports are unexercised.
- The automatic rerun of a failed deploy job: the decision rule is unit-tested;
  `gh run rerun --failed` has not been issued. The API does not say "timeout", so
  the rule is "build succeeded, the deploy step failed, fewer than 3 attempts".
- Retirement in a real feed commit (`git rm` of tracked installers) and the Pages
  workflow building the slimmer site; the `download` proof mode (hashing each
  retired asset's download) ran only in unit tests.
- Retiring tracked files shrinks the deployed site, not the `gh-pages` history.
- Live-site and native-updater checks against a new deployment.

## 11. Deliberate differences from the old scripts

1. Nothing per release is typed: bases, previous tag/commit/sequence/payload,
   metadata version and tool hashes are derived or computed and checked against the
   remote; `release-config.json` is written by the tool (with a `derived` block).
2. `pages_base` comes from the previous receipts and the gh-pages checkout must
   equal it (the old plan script copied whatever the checkout held).
3. One path rule (`allowlist.py`); the stricter `crates/<name>/Cargo.toml` form wins;
   the overlay allowlist is derived from the diff and bound by digest in the review;
   changed notices, lockfiles and installer inputs are published without listing.
4. Receipts live in `work\releases\<tag>\receipts`, binaries in
   `D:\Projects\relay-release-artifacts\<tag>`; step records make each step
   idempotent and refuse edited or stale inputs.
5. Each test run writes a fresh `*-selected.txt`; earlier logs move to
   `logs\attempts\` (the old harness appended across reruns).
6. The harness no longer touches the installed Relay (an unused path check on
   `dist\relay-iroh.exe` was dropped).
7. Editing `scripts/release/**` after verification does not stale the source (it is
   not shipped); `seal` records the tool digest and `publish` refuses if the tool
   changed after sealing.
8. The notices check regenerates `THIRD_PARTY_NOTICES.txt` and compares; the typed
   iroh `Cargo.lock` hash became "iroh-helper inputs equal the previous release's"
   for a reused helper.
9. The native updater check runs only after a live-site receipt that validates for
   this feed commit and run, and records which live check it followed.
10. Pages retirement covers tracked installers as well as mirrors, keeps anything the
    signed metadata references, and retires nothing without a matching release asset.
11. Public commits are authored by whoever runs the release (their git config), not a
    hard-coded "Codex" identity.
12. `pages-size-plan.json` reports the size after retirement and carries the full
    plan; `retired-large-targets-verified.json` also lists retired page targets.
13. An installer under 100 MiB is committed as an ordinary tracked target instead of
    being refused.
14. The notes files have fixed names and the signed notes must equal
    `update-notes.txt`. The locationless lint evidence scans `.rs` files in Python
    instead of calling `rg`.
