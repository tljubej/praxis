# ADR-006: Milestone 1 fuzz gate is a `proptest` property test

**Date:** 2026-07-23 · **Status:** accepted

## Context

Milestone 1 acceptance (§19) requires "no panic on fuzzed token streams." Two
approaches were considered: a `cargo-fuzz` (libFuzzer) target with
coverage-guided fuzzing, or a `proptest`/quickcheck property test feeding
arbitrary input to the lexer and parser.

## Decision

Use a `proptest` property test as the M1 gate: feed arbitrary byte/character
strings to the lexer and parser and assert only that they do not panic (and
that valid prefixes round-trip where applicable). `proptest` is a dev-dependency
of `praxis-parser`.

## Reason

- The acceptance criterion is a negative one ("no panic"); a property test
  satisfies it directly and deterministically on every `cargo test` run.
- `proptest` runs on stable Rust with no extra toolchain, lives in-crate, and
  needs no separate `fuzz/` workspace or CI wiring for M1.
- `cargo-fuzz` remains available later for coverage-guided fuzzing once the
  parser is richer; it can be added without changing this gate.

## Consequences

- One dev-dependency (`proptest`) on `praxis-parser`.
- The property test runs as part of `just ci`; shrinking helps localize any
  panic-triggering input that appears during development.
- This is an M1 stand-in, not a commitment against adding `cargo-fuzz` later.
