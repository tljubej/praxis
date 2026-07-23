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
