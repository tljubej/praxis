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

## Running programs: the `.px` / `.in` / `.out` triple

Every `.px` program anywhere under this tree is executed by
`crates/praxis-cli/tests/corpus.rs`, which walks the directories rather than
listing them, so a new program is covered as soon as it lands. Each one needs:

| File | Required | What |
|---|---|---|
| `name.px` | yes | the program |
| `name.out` | **yes** | its expected stdout, exactly as `praxis run` prints it |
| `name.in` | only if it `read`s | the input, passed as `--input` |

A program with no `.out` fails the test rather than being skipped — a skipped
fixture is what the test exists to prevent. Until this test existed nothing ran
the corpus at all, and `day02_grid_of_char.px` sat calling a `Grid` method that
does not exist: `praxis check` reported nothing (the method is resolved at
lowering) while `praxis run` exited 1 (REP-12).
