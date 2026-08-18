# praxis-debugger

Crash snapshots and the interactive crash REPL for
[Praxis](https://github.com/tljubej/praxis).

Praxis has no exceptions and no error handling. An index out of bounds, a
missing key, an overflow, a mismatched `read` or a failed assertion stops the
program — and hands you the wreckage instead of a stack trace:

```text
error: program faulted: index out of bounds

Backtrace:
#0   window_sum
#1   <entry>

  locals:
    values: Vec[Int] = [12, 7, 41]
    start: Int = 1
  temps:
    <tmp#6: Int> @ "values[start + 1]" = 41
    <tmp#9: Int> @ "start + 2" = 3
    <tmp#10: Int> @ "values[start + 2]" = <uninit>
```

Those are not only the locals. They are the intermediate values of the faulting
expression, each labelled with the source text it came from, with `<uninit>`
marking the one the fault stopped from being assigned.

On a terminal you get a full-screen debugger instead of an exit code —
backtrace, source, locals and transcript at once. You walk the frames, evaluate
expressions against the captured state, then fix the file and `reload` without
re-entering your input.

## The second way in

A `:bp` marker stops a program that has **not** faulted, and the same REPL and
TUI serve it: the snapshot is the same deep copy of the same debug chain, so the
questions and their answers are the same. What differs is which commands the
situation supports. A stopped program has frames to return to, so it gains
`continue`; it is in the middle of using its own runtime, so it loses everything
that would execute.

## What it provides

- `render` — the snapshot rendering, and the noninteractive fallback for when
  stdout is not a terminal.
- `repl` — `bt`, `frame`, `up`, `down`, `locals`, `p EXPR`, `type EXPR`,
  `source`, `input`, `parser`, `heap`, `restart`, `reload`, `continue`, `help`,
  `quit`.
- `evaluate`, `purity` — the read-only expression evaluator. Nothing it runs can
  change the captured state.
- `tui` — the full-screen view, on [`ratatui`](https://crates.io/crates/ratatui).
- `session` — the live compile/run state `restart` and `reload` reach.

## Part of Praxis

Praxis is a small, statically typed, garbage-collected language for Advent of
Code-style puzzles: the input parser is part of the language, types are inferred
rather than written, and a program that falls over hands you its state instead
of a stack trace.

To *use* the language, install [`praxis-cli`](https://crates.io/crates/praxis-cli)
— it provides the `praxis` binary. The
[repository](https://github.com/tljubej/praxis) has the book, the design
document and the decision records.

This crate is one stage of that compiler, published so the pipeline is
inspectable and so `praxis-cli` can be built from the registry. Its API tracks
what the compiler needs and is not a stable platform for outside consumers.

Praxis was written with large language models against a human design. The
repository's README says what that means for the license.

Licensed under either of Apache License 2.0 or the MIT license, at your option.
