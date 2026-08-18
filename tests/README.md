# Project-wide test suites

The cross-crate suites live here: tests that span the whole compiler pipeline
rather than one crate's own surface. Crate-local unit and integration tests
(the `praxis check` acceptance test, for instance) stay under each crate's own
`tests/` directory.

| Directory | Contents |
|---|---|
| `input-parsers/` | Parser-constructor and fixture tests for the `read` DSL |
| `aoc-corpus/` | Handcrafted format-equivalent Advent of Code fixtures |

## Running programs: the `.px` / `.in` / `.out` triple

Every `.px` program anywhere under this tree is executed by
`crates/praxis-cli/tests/corpus.rs`, which walks the directories rather than
listing them, so a new program is covered as soon as it lands. Each one needs:

| File | Required | What |
|---|---|---|
| `name.px` | yes | the program |
| `name.out` | **yes** | its expected stdout, exactly as `praxis run` prints it |
| `name.in` | only if it `read`s | the input, passed as `--input` |

**A program with no `.out` fails the test rather than being skipped.** A skipped
fixture is exactly what this test exists to prevent: `praxis check` and
`praxis run` do not report the same set of mistakes — a method that does not
exist is resolved at lowering, so `check` passes it and `run` exits 1 — and a
fixture nobody executes is a fixture that proves nothing.
