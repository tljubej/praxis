# Eight defects the book found, and eleven comments that lied

Writing [the book](../book/) meant running the compiler against every claim in
the design document and every ADR — several hundred small programs, each one
written to make a documented rule observable. That is a different exercise from
the test suite, which asserts what the implementation was built to do. This is
what fell out of asking instead what it *says* it does.

Everything below was reproduced by hand against `target/release/praxis` at
`8fd8e15` (2026-08-05). No item is a report forwarded from somewhere; each
carries the command that produces it and the output that came back. Paths in the
transcripts are shortened for reading — the compiler prints whatever path it was
given.

Nothing here is fixed. The defects are ordered by what they cost a user, not by
how hard they are.

## Reproducing

Build once:

```bash
cargo build --release -p praxis-cli
```

Then each repro below is a `.px` file and one command. None needs input on stdin
unless it says so.

---

## 1. `praxis check` exits 0 on programs `run` refuses

**Severity: high.** This is the failure mode [ADR-130](../decisions/130-a-matchs-coverage-is-analysis-answer-and-the-pattern-is-built-once.md)
was written to close, still open for every code it did not move. ADR-130's own
statement of the problem — "a file could check clean and fail to run" — is still
true of at least three diagnostics.

`praxis check` routes through `praxis_lsp::query::Snapshot::diagnostics`
(`crates/praxis-lsp/src/query.rs:148`), which runs parse and analyze. It never
runs HIR lowering. Diagnostics that lowering is the sole emitter of are therefore
invisible to `check` **and to the editor**, which publishes the same
`snapshot.diagnostics()` (`crates/praxis-lsp/src/server.rs`).

### `Y013` — an integer literal out of range

```praxis
var x = 99999999999999999999999
out(x)
```

```console
$ praxis check big.px
$ echo $?
0
$ praxis run big.px --debug never
error[Y013]: `99999999999999999999999` is outside the range of `Int`

  big.px:1:9
$ echo $?
1
```

### `Y125` — a refutable `for` binding

```praxis
struct Point { x: Int, y: Int }
var pts = [Point{x: 0, y: 1}]
for Point { x: 0, y } in pts {
    out(y)
}
```

```console
$ praxis check y125.px
$ echo $?
0
$ praxis run y125.px --debug never
error[Y125]: a `for` binding must match every item, and a literal pattern does not

  y125.px:3:5
```

`Y124` (a payload named past its arity) and `Y099` are in the same position by
construction; `crates/praxis-hir/src/pattern.rs:20` names `Y122`, `Y124` and
`Y013` as this builder's, and `lower.rs:1041` and `:1644` name `Y099` and `Y125`
as lowering's.

**What a fix has to do.** ADR-130's shape is the precedent: build the thing once,
during analysis, and let the answer be analysis's. The pattern builder already
runs in analysis for coverage (`pattern.rs:21` — "the coverage pass runs this
builder with a sink it throws away"). The sink being thrown away is the bug: the
diagnostics it produces are real and should reach `Snapshot::diagnostics`. `Y013`
on a literal is the same story from a different caller.

**Do not** fix this by making `check` lower. The point of ADR-097 is that `check`
and the editor run the same query; a fix that only helps the CLI reintroduces
exactly the divergence ADR-097 removed.

The book documents the current behaviour honestly
([tooling/diagnostics.md](../book/src/tooling/diagnostics.md) has a run-only
table). **That table should be deleted when this is fixed**, and
`docs/book/examples/tooling/lowering-only.px` with it.

---

## 2. `reduce` with a mistyped closure body panics the compiler

**Severity: high.** An ICE on a program a user could plausibly write.

```praxis
var r = ["ab", "c"].reduce(|a, b| a.len())
out(r)
```

```console
$ praxis run reduce.px --debug never
thread 'main' panicked at crates/praxis-mir/src/build.rs:1437:17:
internal compiler error: the pipeline recognizer declined `len`, and it carries
no runtime symbol. Every intrinsic row must be classified by `classify_link` or
`classify_sink` ... and every unresolved method call must have been reported by
inference and dropped before here (ADR-093).
```

The panic's own message names both possibilities and neither is what happened:
inference *did* resolve `a.len()` — `a` is `Text` there — and `len` *is*
classified. What reaches `build.rs:1437` is a resolved intrinsic in a position
the recognizer does not expect, inside a `reduce` closure rather than a pipeline
stage.

The signature the chapter documents is right, and the neighbouring error path
works:

```console
$ praxis check reduce-bool.px      # [1,2,3].reduce(|a, b| a > b)
error[Y001]: expected (Int, Int) -> Int, found (Int, Int) -> Bool
```

So the gap is narrow: a `reduce` whose closure body calls a method on the
accumulator at a type that does not unify reaches MIR instead of being stopped by
inference. Anything that makes the mismatch a `Y001` before lowering closes it.

**Site:** `crates/praxis-mir/src/build.rs:1437`, and whichever inference path
lets the closure body through. The assertion at `build.rs:1437` is correct and
should stay — it caught this.

---

## 3. A one-element tuple aborts MIR verification

**Severity: medium.** Rejected at the wrong stage, and the rejection is an abort.

```praxis
var t = (1,)
out(t + 1)
```

```console
$ praxis run tuple1.px --debug never
internal error: MIR verification failed (1 problems):
  <entry>: block 0 inst 3: ExtractScalar reads a Int payload out of LocalId(3),
  which every definition proves is a Tuple
```

`(1,)` parses as a one-element tuple (`crates/praxis-parser/src/parse.rs`,
`parse_paren`) and types as `Int` (`crates/praxis-hir/src/decl.rs`,
`tuple_or_degenerate`). The two disagree, and the verifier is what notices.

The other spellings are each wrong in their own way: `out((1,))` prints `(1)`,
and `t.0` is a type error.

**Decide first what `(1,)` is**, then make both sides agree. Either it is a
one-element tuple everywhere (and `t.0` works, and it prints `(1,)`), or the
trailing comma is not tuple syntax at arity one and the parser rejects it. The
verifier is doing its job here; do not weaken it.

---

## 4. `out` accepts a function value and prints `Unit`

**Severity: medium.** A silently wrong answer, which is worse than a fault.

`pi` is a **nullary function**, not a constant:

```console
$ praxis run pi.px         # out(pi())
3.141592653589793
$ praxis check pi3.px      # var x = pi ; out(x + 1.0)
error[Y001]: expected Float, found () -> Float
```

So far so good — the type is visible and the error is right. But:

```console
$ praxis run pi_bare.px    # out(pi)
Unit
```

`out(pi)` type-checks and prints `Unit`. A function value flowed into `out` and
was rendered as if it were the unit value. Two things to settle:

- **`out` should refuse a function value**, or render it as one. Printing `Unit`
  is neither.
- **`pi`'s prelude entry is documented as a constant** —
  `crates/praxis-stdlib/src/prelude.rs:31` reads `"The constant π as a Float."`
  It is a function; the entry should say so, or `pi` should become a real
  constant. The doc string is what hover shows, so this one is user-visible in
  the editor.

---

## 5. `let x = 1` suggests `Set`

**Severity: low, but it is the first thing an old-example reader hits.** `let` was
retired by [ADR-125](../decisions/125-a-binding-is-a-binding-and-the-compiler-decides-its-storage.md)
and every document written before it opens with one.

```console
$ praxis check oldlet.px
error[N001]: `let` is not defined

  oldlet.px:1:1
  1 | let x = 1
    | ^^^ `let` is not defined
help: did you mean `Set`?
```

The suggestion is the budget working as designed: `crates/praxis-source/src/suggest.rs:25`
spends `max(1, len/3)`, `let` is three characters, so the budget is 1 — and `Set`
is one edit away. The rule is right in general (it is rustc's); the outcome here
is not.

Two candidate fixes, and the second is better:

1. Raise the floor so three-letter names draw no suggestion. This costs real
   suggestions elsewhere.
2. **Recognize `let` specifically.** It is a retired keyword, not a typo, and it
   deserves the message it actually needs: `` `let` was replaced by `var` `` with
   a machine-applicable fix, which is exactly the shape
   [ADR-132](../decisions/132-a-code-action-is-a-diagnostics-machine-applicable-suggestion.md)
   established. `let` is still a legal identifier (`var let = 5` compiles and
   prints), so the special case belongs at the point where `N001` is raised for a
   name in statement position, not in the lexer.

---

## 6. The faulting instruction's destination renders as `Unit`, not `<uninit>`

**Severity: low.** Cosmetic, but it is in the debugger's most-read output and it
reads as a value that was computed.

```praxis
var numbers = [12, 7, 41]

fn window_sum(values, start) {
    values[start] + values[start + 1] + values[start + 2]
}

out(window_sum(numbers, 1))
```

```console
$ praxis run crash.px --debug never
error: program faulted: index out of bounds
...
  temps:
    <tmp#9: Int> @ "start + 2" = 3
    <tmp#10: Int> @ "values[start + 2]" = Unit
    <tmp#11: Int> @ "values[start] + ... + values[start + 2]" = <uninit>
```

`tmp#10` is the destination of the subscript that faulted. It was never written,
exactly like `tmp#11` — but it prints `Unit` while `tmp#11` prints `<uninit>`.

`crates/praxis-debugger/src/render.rs:224` renders `<uninit>` when
`local.value` is `None`. So `tmp#10`'s slot is `Some(_)` holding something that
formats as `Unit` — a slot pre-initialized to the unit singleton rather than left
empty, or a stale spill. The renderer is probably not where the fix goes; find
who wrote that slot.

A reader debugging this program has to know that `Unit` here means "nothing
happened", which is the one thing the debugger exists to make obvious.

---

## 7. A `help:` names a method that does not exist

**Severity: low.** The help is on the single most common mistake in a puzzle
program, and half of it sends the reader somewhere that does not exist.

```praxis
var raw = "12"
var count: Int = raw
out(count)
```

```console
$ praxis check text-to-number.px
error[Y001]: expected Int, found Text

  text-to-number.px:2:18
  2 | var count: Int = raw
    |                  ^^^ expected Int, found Text

help: this is `Text`; call `.int()` on it (or use `read lines(int)`)
```

**Site:** `crates/praxis-hir/src/infer.rs:938`.

`Text` has no `int` method — its own rows are `len`, `is_empty` and `get`, plus
the subscript and the pipeline combinators. `raw.int()` reports `Y110`. The
parenthesized half of the help is the half that works.

Either add the `Text.int()` the help promises, or rewrite the help to name what
exists. The book currently documents the wart
([types/errors.md](../book/src/types/errors.md), "Text and numbers") and that
passage should be deleted when this is fixed.

---

## 8. The CLI advertises two commands and misfiles them under Milestone 0

**Severity: low.** Two separate wrongnesses in the same surface.

```console
$ praxis --help
  watch  Keep the program and input alive, recompile on source changes. (Later milestone)
  repl   Start an ordinary interactive REPL session. (Later milestone)

$ praxis watch prog.px
error: `praxis watch` `prog.px` is not implemented yet (planned for Milestone 0)
$ echo $?
2
```

- The milestone is **hardcoded to `0`** at both call sites —
  `crates/praxis-cli/src/main.rs:94` and `:95` pass `0` to `not_implemented`.
  Milestone 0 completed long ago, so the message points at the past. `watch` is
  §19 M-later and `repl` has no milestone; pass the real number or drop the
  clause.
- The design document's `watch` invocation (§3.1) shows `--input`, and the clap
  `Command::Watch` variant declares only `file` — there is no `--input` to
  accept. Worth settling when `watch` is built rather than after.

Separately, `--help` leaks implementation markers into user-facing text, because
the clap doc comments *are* the implementation notes:

```text
run    Parse, type-check, JIT-compile, and run the program. (Milestone 4+)
lsp    Start the language server over stdio (§15, M11). ...
--input  ... (§7.1, M6)
--debug  ... (§9.6, M10)
```

A reader of `--help` has no idea what `§7.1` or `M6` is. The notes are worth
keeping; they belong in a `//` comment beside the doc comment, not in it.

---

## Comments and documents that are wrong

Each of these was checked against the behaviour it describes. None is a code
defect — they are all one-line edits, and each one is a trap for whoever reads it
next. They are grouped by where the truth is.

| Where | What it says | What is true |
|---|---|---|
| `crates/praxis-stdlib/src/prelude.rs:54-56` | sequence `find`/`position` "answer an `Int` index with a `-1` miss sentinel" | They answer `Option[T]` / `Option[Int]`. The catalog rows say so and a run prints `Some(4)` / `Some(2)`. [ADR-082](../decisions/082-find-answers-the-element-and-a-miss-is-none.md) is the one that changed it. |
| `crates/praxis-stdlib/src/prelude.rs:31` | `pi` is "The constant π as a Float" | It is a nullary function. See defect 4. |
| `crates/praxis-hir/src/infer.rs:3622` | "`v.lenght()` is offered `len`" | It is not. `lenght` is six characters, so the budget is 2, and the distance to `len` is 3. Running it prints `Y110` with no help line. The surrounding claim — that candidates are the receiver's rows and not the whole catalog — is correct; only the example is wrong. |
| `crates/praxis-parser/src/lex.rs:232-235` | "A trailing dot with no following digit (`2.`) is a valid float iff the integer part is nonempty" | The code requires a digit after the dot. `2.` lexes as `IntLit` then `DOT`, so `var x = 2.` followed by `out(x)` parses as a method call on `2` and reports `Y110`. |
| `crates/praxis-parser/src/parse.rs:1314` | "Empty `()`: a degenerate paren expr; type checking rejects it" | It does not. `var u = ()` then `out(u)` prints `Unit`. |
| `crates/praxis-mir/src/forward.rs:725` | `Lit::Char` "is synthesized by the input parser alone" | Nothing in the tree constructs `Lit::Char`; grep finds only match arms. |
| `crates/praxis-source/src/diagnostic.rs:255` and `:267` (doc comments on `NoTupleElement` and `NotIndexable`) | `Y112` "is a lowering diagnostic — `praxis check` never runs lowering", and `Y110` likewise | Both are reported by `check` today: `v.nope()` gives `Y110` and `v.nope` gives `Y112`, from `check`. [ADR-093](../decisions/093-a-method-that-cannot-resolve-is-reported-at-check.md) moved `Y110`. The reasoning these comments give for their *own* code is still sound; only the contrast they draw is stale. |
| `docs/decisions/093-*.md:154` and the doc comment on `unknown_method` (`crates/praxis-hir/src/diagnostics.rs:461`) | a leaked receiver prints `?a` | The renderer produces `?T`. |
| `docs/decisions/061-*.md:12`, `docs/decisions/068-*.md:10,14` | examples written `let f = double`, `let x = 1` | `let` was retired by ADR-125. A reader who follows a link from current documentation lands on the retired spelling. |
| `docs/decisions/086-*.md:113-115` (the "Why not `Char.to_text()`" aside) | quotes §4.13 — "building a `Text` out of a number is not yet possible in any spelling" | Half true now. `Float.to_text()` exists and prints `5.0`; `Int.to_text()` still reports `Y110`. The aside's argument survives, but the sentence it leans on does not. |
| `benchmarks/praxis/vm.px` (header) | "the least arithmetic-shaped of the seven" | There are eight programs in `benchmarks/praxis/`. |

`docs/decisions/129-*.md` also describes its 7.3 MiB floor as "`praxis run` on a
size-0 program … including the JIT", but a size-0 program exits at "no statements
to run and no `main` function" and never reaches the JIT. The number may be
measuring something other than what it names; it could not be reproduced on an
arm64 darwin host.

---

## After you fix one

Two gates, and the second is the one people forget.

```bash
just ci
./docs/book/examples/verify.sh
```

The second re-runs every example in the book — 381 of them — and diffs each
against the output printed in the chapter that quotes it. **Several of these
defects are currently documented as behaviour**, so fixing one is *expected* to
turn that gate red. That is the signal working, not a regression. When it fires:

1. Read the diff. Confirm the change is the one you meant to make.
2. `./docs/book/examples/verify.sh --bless <area>` to rewrite the expectation.
3. **Fix the prose too.** The examples that exist only to document a defect are
   named in the defect entries above, and the chapters that describe the wart in
   words will not be caught by any gate.
