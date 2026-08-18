# ADR-155: A release is a resumable walk of a computed order, and the version is set by a script because cargo cannot inherit it

**Date:** 2026-08-19 · **Status:** accepted

## Context

The workspace is fifteen publishable crates and one that excludes itself. Three
properties of crates.io shape what releasing them can look like.

**Order is mandatory.** The registry rejects a package whose dependency is not
already on it, so the crates go up in topological order. The graph is not the
obvious one: cargo strips a path-only dev-dependency from the manifest a package
ships but keeps one that carries a version, and the registry then checks that it
exists. `praxis-ast` therefore waits for `praxis-parser`, which is a
dev-dependency of it and nothing else.

**A first release is metered in hours.** New crate *names* are limited to a
burst of five and then one per ten minutes. Fifteen unpublished names is roughly
a hundred minutes of waiting, most of a release spent doing nothing, with plenty
of opportunity to be interrupted. Later releases do not pay it — a new version
of a name the registry knows is limited at one per minute.

**Nothing can be taken back.** A version yanks but never deletes and a name is
never freed, so a wrong dependency version is permanent.

That last one bites here specifically. Every crate takes its version from
`workspace.package.version`, but cargo has no way to inherit a version into a
*dependency specification* — a `[workspace.dependencies]` entry has to write
`version = "x.y.z"` out, because a dependency carrying only a path is rejected.
The release number therefore appears sixteen times in the root manifest, and
fifteen of them are easy to miss. Missing them is not a build error and not a
test failure: the workspace still compiles, because a path dependency wins
locally. It goes wrong only on the registry, where `praxis-cli 0.2.0` is
published depending on `praxis-hir 0.1.0` — resolvable, wrong, and permanent.

## Decision

- `scripts/publish.sh` is the release. It has three modes — `--plan`,
  `--dry-run`, and publish — and the `justfile` only names them, per ADR-002.
- **The order is computed from `cargo metadata` on every run**, counting normal
  and build dependencies always and a dev-dependency only when it carries a
  version. There is no list of crates anywhere in the repository.
- **Every crate is checked against the registry and skipped if it is already
  there.** An interrupted release is resumed by running the script again.
- A rate-limit refusal is waited out rather than failed on, and it is recognised
  from the registry's own words in cargo's output. Any other failure stops the
  run.
- `scripts/set-version.sh` sets all sixteen versions together, and
  `publish.sh` refuses to publish if they ever disagree.
- `.github/workflows/release.yml` is `workflow_dispatch` only and calls the same
  script.

## Reason

- A computed order cannot go stale. A hand-written one goes stale the first time
  a crate gains a dependency, and the failure arrives partway into an upload
  that cannot be undone.
- Skipping is what makes the rate limit survivable. The alternative to resuming
  is editing a list of the crates that are left, by hand, two hours into a
  release — which is where mistakes come from.
- The version check turns the one silent, permanent failure into a refusal
  before anything is uploaded.
- The release workflow is manual, not tag-triggered. A tag push is too easy to
  do by accident for an action nothing can reverse.

## Consequences

- `python3` is a release-time dependency, for the dependency-graph walk and the
  version rewrite. It is not needed to build or test Praxis.
- `just publish-dry` is deliberately not part of `just ci`: it is a second full
  compile of the workspace from fifteen extracted tarballs, and ADR-002's gate
  is already long enough.
- A stale `target/package/` from an interrupted run makes the dry run fail with
  errors describing code that is no longer in the tree — cargo verifies each
  tarball against the others and does not always notice the tree moved beneath
  it. `rm -rf target/package` is the fix. A dry-run failure naming a symbol that
  does not exist anywhere is this, not a defect.
- The registry's copy of `praxis-test-support` is nothing: it sets
  `publish = false`, and the nine crates that test with it depend on it without
  a version, so cargo drops it from what they ship.
