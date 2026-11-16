# The release pipeline

Three workflows of GitHub Actions and two scripts, after imprimeur's. None of the workflows holds
a check of its own: the checks are in the `Makefile`, the release in `scripts/release.py`, and the
fetch of dbt in `scripts/fetch-dbt`. Nothing has run on GitHub yet: the repository has no remote.

## The workflow CI

`.github/workflows/ci.yml` runs on a push to any branch, and on a pull request from a fork,
which it checks at the pull request's own commit, with the whole history. A newer run of the
same reference cancels the one under way. Its permission is `contents: read`. Each job runs on
`ubuntu-24.04`, but one, with Nix from `cache.nixos.org` and this repository's own Actions
cache, and has 30 minutes:

| Job | Runs |
|---|---|
| `static` | `make check-static`: fmt, clippy, `public`, Ruff and the Python tests, actionlint; and off `main`, `make commits BASE=origin/main` |
| `test` | `make test` |
| `macos` | On `macos-15`, `make test`. Its failure fails neither CI nor a release |
| `dbt-oss` | dbt-oss 2.0.11 fetched, the network cut checked, then `make conformance` |
| `dbt` | The same with the proprietary dbt 2.0.9, then `make live` with the network cut. Only on a push, and only when the repository's variable `DBT_LSP_PLUS_PROPRIETARY` is `true`; its failure fails neither CI nor a release |

Each job holds its own steps rather than a part of a matrix, since a job's `if:` cannot read a
matrix, and the proprietary build's job must be gated on the variable. The jobs run side by side,
and none stops the others. The `Makefile`'s cap of four workers stays, as GitHub's runners have
no more.

### The network cut

Each dbt job runs `sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0`, as that
runner's AppArmor otherwise refuses `unshare -r`, then `unshare -rn true`, so that a job without
the cut fails rather than passes: the corpus runs each parse under `unshare -rn` when it can, and
says "network namespace: no" when it cannot. The live test is built with the network, then runs
under `unshare -rn`, as `dbt lsp` runs as the user's command gives it, with no flag of the proxy's.

### dbt

`scripts/fetch-dbt BUILD DIR` downloads the release archive that the owner's Nix packages pin,
checks its SHA-256 before reading it, and writes its one file named `dbt` as `DIR/dbt`, in
`$RUNNER_TEMP`, never the checkout. PyPI has no wheel of either build at these versions. No cache
holds the binary, as a fork's pull request can restore a base branch's caches, and the proprietary
build stays out of the flake. The proprietary build runs under dbt Labs' licence: whether that
allows it on GitHub's runners is the owner's reading, hence the variable.

### What `continue-on-error` hides

A job with `continue-on-error` that fails leaves the run's conclusion `success`, so no failure is
notified and CD goes on: a failure of `macos` or `dbt` shows on the run's page only. CD also waits
for both before each release.

## The workflow Weekly

`.github/workflows/weekly.yml` runs the corpus on the latest stable dbt each Monday at 06:17
UTC, off the hour when GitHub drops scheduled runs first, and by hand (`workflow_dispatch`).
GitHub schedules the copy on the default branch only: a branch's copy runs by hand.
It is apart from CI, so that it neither cancels nor is cancelled by a push's run, and counts
for neither CI nor CD. Its permission is `contents: read`.

| Job | Runs |
|---|---|
| `dbt-oss` | `scripts/fetch-dbt --latest oss`: the highest release `vX.Y.Z` of GitHub's `dbt-labs/dbt`, compared as numbers, not a draft nor a pre-release, that holds the Linux archive, checked by GitHub's SHA-256 of it; then the network cut and `make conformance` |
| `dbt` | `scripts/fetch-dbt --latest proprietary`: the stable tag of dbt Labs' `versions.json`, as dbt Labs' installer reads it, checked by the `.sha256` that dbt Labs publishes beside the archive; then the corpus, and the live test built, then run with the network cut. Only when the repository's variable `DBT_LSP_PLUS_PROPRIETARY` is `true` |

- No `continue-on-error`: a drift fails the run, and GitHub tells the user who last changed
  its schedule, if they have Actions notifications on.
- Caches are restored only, and the checkout keeps no credentials: the workflow itself never
  saves a cache. The build of dbt it fetches runs as the runner's user, as CI's pinned builds
  do, so a hostile build, not merely a broken one, could tamper with the rest of its job, and
  read the job's read-only token, which the Nix installer writes into `/etc/nix/nix.conf`.
- Each job's summary holds the build it fetched (`BUILD VERSION URL SHA256`), the corpus's
  line, and the corpus's failure whole: what drifted (the divergences to register, and those
  registered that no longer diverge, a changed answer showing in both), or which parse failed
  and how; with the proprietary build, the live test's result and failure too. The live test
  runs whatever the corpus said, once the build is fetched and the network cut.
- dbt-oss's latest is trusted through GitHub's digest of its asset; a newer release that holds
  the Linux archive without one fails the run rather than being skipped. The proprietary
  latest is trusted through TLS to dbt Labs' CDN and the digest dbt Labs publishes on the same
  origin, which catches a corrupt download but not a compromised origin. No signature exists.
  Canary and preview builds are not run.
- In a public repository, GitHub disables a scheduled workflow after 60 days without
  activity: it is enabled again under **Actions**.

What a weekly run shows beyond CI: the latest dbt's resolution, as new and stale divergences;
a changed error code, through a case's `expected-error`; the proxy's parse flags still
accepted; a need of the network under the cut; dbt writing into the project; and with the
proprietary build, the drift of what the live test checks of dbt lsp's protocol. Not: rules
that the corpus has no case for, whether the fake server is still faithful, macOS and other
targets, other adapters, builds released and superseded within a week, dbt-mode's side.

To move CI's pins to a new build, one commit changes `scripts/fetch-dbt`'s URL and SHA-256
(the weekly run's fetch line gives them) and its docstring, `tests/fixtures/conformance/divergences.txt`,
`docs/design.md`'s statement of the builds and the CI table above, checked by hand with
`make conformance` and `make live` on the new build. One registry holds one set of divergences: if the two builds
part, a later plan qualifies it by build.

## The workflow CD

`.github/workflows/cd.yml` runs in two cases, and never in a fork:

- once CI completes on `main`: it goes on only if CI's conclusion is `success`, its event `push`,
  and its commit from this repository;
- by hand on `main`, with an input `bump`: `patch`, the default, `minor` or `major`.

So an automatic release raises the patch part. Its permissions are `contents: write` and `actions:
read`, with the job's own `github.token` and no secret. Runs take one at a time: the one that runs
is never cancelled, and a newer one takes the place of the one that waits; it has 15 minutes. It
checks out the commit that CI checked, with every tag, and runs `python3 scripts/release.py`. The
runner's Python and `gh` serve: it needs no Nix.

## What is cached

CI's Nix store, keyed on the runner's system and a hash of `flake.nix` and `flake.lock`, with the
newest store of the same system restored failing that; and per job, cargo's registry and `target/`,
keyed on the job, the system, and a hash of `Cargo.lock` and `flake.lock`, which fixes rustc. Each
release rewrites `Cargo.lock`'s version line, so the next push's key is new and starts from the
newest cache of its job, then saves its own: the caches grow until GitHub evicts the oldest. A
branch reads its own caches and `main`'s, and never writes `main`'s. No third-party binary cache, no
`--accept-flake-config` and no change to `substituters`. CD has no cache.

## What the release writes

The version is the package's: `Cargo.toml`'s `[package] version` and the package's own entry in
`Cargo.lock`, the one without `source`, which must agree. The tag is `X.Y.Z`.

| Write | What it is |
|---|---|
| The version commit on `main` | Through GitHub's API, so that GitHub signs it. It sets the version in both files and changes no other byte. Its message is `Version X.Y.Z`, a blank line and `[skip ci]`, so that no CI follows it |
| The release | `X.Y.Z`, with its tag on that commit. Its notes are the table of the files changed since the release before, and the compare link. A first release has neither |

The version is written only by the pipeline. The first release raises `Cargo.toml`'s 0.1.0 by the
bump, as every other does: 0.1.1 for a push.

## What a run decides

A run reads first, and makes only the writes that are missing.

| What it finds | What it does | Status |
|---|---|---|
| `main` is at the commit | Both writes | 0 |
| `main` has moved, and the next commit is a version commit whose release exists | Nothing | 0 |
| `main` has moved, and the next commit is a version commit with no release | The release alone: a run cut short finishes | 0 |
| `main` has moved, and the next commit is no version commit | Nothing, as the newer push's run releases | 0 |
| `main` is at a version commit with no release, by hand | The release alone, and no bump: run it again for that | 0 |
| By hand, and CI has not succeeded on `main`'s head, or on its parent when the head is a version commit | Nothing | 2 |
| A version that is malformed, or that the two files do not agree on, or not found exactly once in either; the next version's tag exists; a version commit's tag is on another commit | Nothing | 2 |
| git or GitHub refuses a call, takes over 120 s, or answers something else | Nothing more. If `main` moved just after the read, GitHub refuses the commit, and the newer push's run releases | 2 |

A version commit is told by its message, its one parent, and what it changes: `Cargo.toml` and
`Cargo.lock`, and nothing else.

## What the owner sets on GitHub

- The variable `DBT_LSP_PLUS_PROPRIETARY`, to run the proprietary build.
- Private vulnerability reporting, which `.github/SECURITY.md` points to.
- Rules on `main` that let `github-actions[bot]` commit through the API.

## Raise a version by hand

Open **Actions**, then **CD**, then **Run workflow**, on `main`, and choose the `bump`. Or:

```sh
gh workflow run cd.yml --ref main -f bump=minor
```

## Keep a clone in step

Each release adds a commit to `origin/main`. So a local `main` runs `git pull --ff-only` before its
next merge, or the push of that merge is refused.

## What runs on the runner, not from Nix

The dev shell takes the C linker (`cc`) and `unshare` from the runner, as it does on a laptop: the
image's gcc on Linux, Apple's clang on macOS.
