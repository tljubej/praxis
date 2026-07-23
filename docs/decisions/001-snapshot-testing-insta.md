# ADR-001: Snapshot testing library is `insta`

**Date:** 2026-07-23 · **Status:** accepted

## Context

The design (§17.2) calls for golden-tree tests with `.stderr`/snapshot files and
a "bless mode" for updating them. This is the pattern every diagnostic, golden
syntax-tree, and UI test will follow, so the library choice is load-bearing.

## Decision

Standardize on [`insta`](https://crates.io/crates/insta) via `praxis-test-support`.

## Reason

- Mature, de-facto standard for Rust compiler/tooling diagnostic snapshots.
- Stores golden output in separate `.snap` files, matching the §17.2 "golden
  tree" / `.stderr` layout.
- `cargo insta review` provides a diff-driven accept flow — this is the "bless
  mode" deliverable, with no custom machinery to maintain.
- Pending updates land in `.snap.new` files (gitignored), so a forgotten
  acceptance can never accidentally rot the tree.

## Consequences

- One dev dependency (`insta`) on `praxis-test-support`.
- All diagnostic / golden-tree tests call helpers in `praxis-test-support`
  rather than `insta` directly, so the choice stays swappable.
