# ADR-002: CI runs `just ci`, with a minimal GitHub Actions wrapper

**Date:** 2026-07-23 · **Status:** accepted

## Context

Milestone 0's deliverables include CI for formatting, linting, and tests, and
the project owner wanted it minimal and easily runnable locally. The classic
failure mode is drift: a hosted workflow that runs slightly different commands
than a developer runs at the keyboard, so green locally / red on CI (or vice
versa).

## Decision

- The `justfile` at the repo root is the **single source of truth** for the
  quality gate. The aggregate recipe is `just ci` = `fmt-check` + `clippy` +
  `test`.
- The hosted workflow (`.github/workflows/ci.yml`) does no logic of its own: it
  installs `just` and runs `just ci`.
- `just fmt` (which modifies files) is intentionally **not** part of `just ci`,
  so CI only ever verifies.

## Reason

- Zero drift by construction — hosted CI and local developers invoke the same
  commands.
- `just` recipes double as documentation of how to run each check.
- Adding `just` to CI is one `cargo install just` step; no matrix, caching, or
  cross-OS strategy yet (deferred per the milestone's "minimal" goal).

## Consequences

- Developers run `cargo install just` once.
- The hosted workflow stays ~20 lines; all quality policy lives in the
  `justfile`, `rustfmt.toml`, and `clippy.toml`.

## Amendment (2026-07-31): doctests are disabled, so `just test` is the whole gate

`cargo test --workspace` ran a doctest target per library crate, and that is
**not** free when a crate has no doctests: `cargo test` invokes `rustdoc --test`,
and rustdoc has to analyze the whole crate — parse, expand, resolve, typeck —
before it can discover how many doctests are in it. Finding zero costs what
finding some costs. None of that work is shared with the compilation cargo just
did, and none of it is cached, so it re-ran on every invocation: ~6s per crate
warm (12s for `praxis-hir`), **95s of the suite** to execute the one doctest the
workspace contained.

Every library crate now sets `doctest = false`. The consequence worth knowing is
that a `///` example is still *compiled* by `cargo doc` and is **never executed**
— so an assertion in one proves nothing, and belongs in a unit test. That is the
cost of the decision, and it is stated in the README and the `justfile` rather
than left for someone to discover from a doctest that silently never ran.

This does not change the "one command is the gate" rule: `just ci` still runs
everything CI runs. It runs less, on purpose.
