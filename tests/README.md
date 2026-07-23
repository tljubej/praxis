# Project-wide test suites

This directory holds the cross-crate integration test suites referenced in the
technical design (§14 and §17.1). Each subdirectory is filled in by the
milestone that introduces the corresponding capability. Until then they hold
only this README (so the layout is in place but the tree is not littered with
empty `.gitkeep` markers).

| Directory | Contents | First filled at |
|---|---|---|
| `ui/` | UI / diagnostic snapshot tests (`.px` source + `.stderr` expected) | Milestone 1 |
| `parser/` | Lossless parser / syntax-tree golden tests | Milestone 1 |
| `typecheck/` | Inference and capability-failure snapshots | Milestone 2 |
| `run-pass/` | Programs that compile and execute to a known output | Milestone 4 |
| `run-fault/` | Programs that execute and fault with a known diagnostic | Milestone 4 |
| `input-parsers/` | Parser-constructor and fixture tests (§17.3) | Milestone 6 |
| `aoc-corpus/` | Handcrafted format-equivalent AoC fixtures (§17.3) | Milestone 6 |

Crate-local unit and integration tests (e.g. the `praxis check` acceptance test)
live under each crate's own `tests/` directory. The suites here are reserved for
tests that span the whole compiler pipeline.
