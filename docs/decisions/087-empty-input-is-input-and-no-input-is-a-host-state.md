# ADR-087: Empty input is input, and "no input" is a host state

**Date:** 2026-08-01
**Status:** Accepted — implemented
**Milestone:** Repair (REP-60)

## Context

§7.10 says when the input buffer is read: "The first `read` lazily reads standard
input once into an immutable GC-managed source buffer; later `read` expressions
reuse it." It does not say what happens when that read answers **zero bytes**,
and three sites had each answered the question the same way by omission:

```rust
if !bytes.is_empty()      { (*ctx).input_source = text }   // praxis_get_input
if !t.is_empty()          { ctx.input_source = … }          // praxis-cli's --input arm
if !self.input_text.is_empty() { ctx.input_source = … }     // DebugSession::rerun_main
```

A buffer that is not installed leaves `input_source` at the immortal `Unit`
singleton, and `praxis_run_parser`'s §6.3 descriptor guard then faults
`ParseFailed` *before* the parser interpreter runs. That fault is raised before
`Input::new`, before `clear_parse_detail`, and before `record_fail`, so
`ParseDetail` stays empty and the renderer — which prints the offset and
`expected` lines only `if let Some(fail)` — prints nothing. The user saw:

```
error: program faulted: input parse mismatch
```

and no more. No offset, no `expected`, no `actual`, and no mention of the input
being empty.

The `--input` arm had already been repaired, which is what made the question
unavoidable: the *same* user request answered two different ways depending on
which flag carried it.

| invocation | stdout | stderr | rc |
|---|---|---|---|
| `praxis run --input empty.in e.px` | `0` | — | 0 |
| `: \| praxis run e.px` | — | `program faulted: input parse mismatch` | 1 |

`praxis_get_input`'s own doc comment stated the second row **as the design**:
"Empty input leaves `input_source` at the immortal Unit, which is the state 'no
input' has always had". So the decision was either that sentence is the contract
and the CLI's repair violated it, or that sentence describes a defect.

## The two candidate rules

1. **Two states.** "No input" is a state a program can be in, distinct from
   "empty input". A `read` in that state is an error, and the `--input` repair
   was wrong to erase the distinction for zero-byte files.
2. **One state for the program.** A reader that answers zero bytes has given
   *empty input*. The buffer's existence follows from the `read`, not from the
   input's length.

## Decision

**Rule 2, with a boundary: a reader that answers zero bytes has given empty
input; only a host that installs neither a buffer nor a reader has "no input",
and that is an embedder state, not a program state.**

Four arguments, in the order of their weight.

**§7.11 makes the old behaviour a contract violation regardless of how the state
question is answered.** §7.11 lists what a mismatch creates: "a runtime fault
containing: input span / parser span / expected description / actual preview /
parser path / partial root value". Six fields; the empty-stdin fault carried
zero. If "no input" were a legitimate program state it would still owe a
§7.11-conformant fault — and it cannot pay, because a fault raised before any
buffer exists has no input span to name. A state that cannot satisfy §7.11 is not
a state §7 has.

**§7.10 is unconditional.** It says the first `read` reads standard input into a
buffer. Not "into a buffer if there is anything to put in it". §8.3 says the same
thing from the diagnostic end: every input diagnostic it shows is anchored at an
offset inside a buffer, so a bufferless state has no §8.3 rendering at all.

**Consistency has already forced it.** The `--input` half decided that a
zero-byte file is input. A user cannot be expected to know that `< /dev/null` and
`--input /dev/null` are different questions.

**A puzzle program wants exactly this.** `read lines(int)` over nothing is `[]` by
`split_lines`'s own rule — the program answers, and the answer is right. A program
that requires content now says `at input offset 0..0: expected int`, which tells
the user they forgot to pipe their input. `input parse mismatch` told them
nothing they could act on.

### The boundary, and why it stays

The host keeps its second state and must. `run_main_no_input` in the codegen
crate's `jit.rs` installs neither a buffer nor a reader, and
`adv_read_against_non_text_input_faults_cleanly` is the gate that a `read` there
does not segfault — §6.3's host-safety gap. After this change that path is
unchanged: `take_input_reader()` answers `None`, `input_source` stays `Unit`, and
`praxis_run_parser`'s descriptor guard remains exactly as it was, as the net
under that state. No `praxis run` reaches it.

So the guard is **not** softened and no `FaultKind` is minted for "no input".
`ParseFailed` there is a host bug being contained, not a parse being reported —
which is why the guard now also **clears** the parse detail before returning. It
used to be the one entry into the parser that did not, so a host reaching it
after an earlier mismatch printed *that* mismatch's offset and expectation for a
parse that never ran. Clearing is the whole repair; fabricating a `ParseFail`
there would be worse than saying nothing, because with no buffer there is no
input span and an invented `expected` would make an embedder's host bug read as a
parse failure at an offset that does not exist.

### Where the rule lives

At `praxis_get_input`, which is the enforcement site. `input.rs`,
`praxis-cli/src/run.rs`, `praxis-debugger/src/session.rs` and
`praxis-cli/tests/corpus.rs` cite it rather than restate it — the previous
arrangement had the rule written out in four places, three of which were about to
go stale and one of which (`praxis_get_input`'s) stated the defect as the design.

## Consequences

- **`praxis run` of a reading program at a terminal now prints an answer computed
  from empty input instead of faulting.** `lazy_stdin::read` answers `Vec::new()`
  for a terminal stdin, so the reader gives zero bytes and the rule applies. That
  is a wrong answer where there used to be a useless one. It is not a new
  asymmetry — `--input /dev/null` already behaved this way — and separating a
  terminal from an empty pipe is a third state through a different door
  (`IsTerminal`), worth its own row if it is worth a warning. Deliberately not
  solved here.
- **A `/dev/null` sweep loses a signal.** Any harness that used "a `read` program
  faults under `/dev/null`" as evidence stops seeing it. Nothing in `crates/`
  asserted it, so no test breaks; the comment in `praxis-cli/tests/corpus.rs` that
  leaned on it does, and is corrected.
- **§9.7 is repaired on the path that was already supposed to be finished.** The
  debugger's `rerun_main` carried the same guard, so a `restart` after a
  zero-byte-input fault re-ran with *no* buffer: the second banner was contentless
  and `input` answered "(no input context — not a parse failure)" about a run that
  had failed to parse. A restart now sees the same empty input the first run saw.
- **No ABI change.** No field, no signature, no new `FaultKind`;
  `RUNTIME_ABI_VERSION` stays 14.
- **One new allocation**, the one the `--input` path already made: a zero-length
  `Text` stored straight into the `input_source` root with no allocation between.
