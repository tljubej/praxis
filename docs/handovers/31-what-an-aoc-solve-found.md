# Three bugs and nine gaps three AoC solves found

**Date:** 2026-08-06
**Status:** **eleven of twelve are closed outright; only 6 is closed in part,
and it says so where it stands.** Item 3 is now closed in full: its two missing
`to_text` rows exist and §8.1's interpolation is implemented. Item 6's
diagnostic is fixed and its ergonomics complaint is not. Every other entry is
closed as written. Each keeps its reproduction and gains a **Fixed** line naming
the change and the gate.
**Build:** reproduced by hand against `target/release/praxis` and
`target/debug/praxis` at `f049c64` (2026-08-06). Both behaved identically. The
transcripts below are what the compiler *did*; run them against the current
binary and you get the new answer, which is the point of leaving them in.

This is what fell out of solving three real puzzles end to end — AoC 2025 day 7
("Laboratories"), day 11 ("Reactor") and day 12 ("Christmas Tree Farm") —
rather than writing programs to make a documented rule observable, which is what
[30](./30-eight-defects-the-book-found.md) did.

All three solves went well.
[`tests/aoc-corpus/aoc2025_day07.px`](../../tests/aoc-corpus/aoc2025_day07.px) is 39 lines, was right
the first time it ran, and answers both parts in 20ms.
[`tests/aoc-corpus/aoc2025_day12.px`](../../tests/aoc-corpus/aoc2025_day12.px) is a 2D packing
decision procedure — two sound bounds plus a complete backtracking search — and
its parser, the part that had to read one file in two unrelated formats, worked
on the first attempt.
[`tests/aoc-corpus/aoc2025_day11.px`](../../tests/aoc-corpus/aoc2025_day11.px) is memoized path
counting over a 648-node DAG, and it **turned up nothing new at all** — which is
the most encouraging line in this document. Everything it needed already worked:
a `(Text, Int)` tuple as a `Map` key for the memo, `Map` subscript beside
`contains`, 2.07e17 paths counted without a thought about the integer type.

The good parts are worth naming, because they are what the rest of this document
is measured against. `read grid(char)` absorbed day 7's input with no parsing
code, reported a ragged row with a byte offset, and took a trailing blank line
and CRLF without comment. Day 12's

```praxis
read sections(
    s0: block(`{i:int}:`, rows: grid(char)), … s5: block(…),
    regions: lines(`{w:int}x{h:int}: {counts:ws(int)}`),
)
```

is the entire parser for a six-shape-blocks-then-a-thousand-region-lines file —
two unrelated formats, one expression, and no hand-written scanning anywhere in
the program. Two composition rules carry it: `{counts:ws(int)}`, a capture body
that is a whole parser expression
([ADR-072](../decisions/072-a-template-capture-body-is-a-parser-expression.md)),
and `block`'s header-template-then-`grid` split
([ADR-090](../decisions/090-a-block-item-is-offered-its-own-lines.md)), which
reaches a level deeper into the file than the program first asked it to — see
item 10. The diagnostics were the best part of all three loops: every deliberate
mistake came back pointed and carrying its fix.

So the list below is short, and the ordering is by what it costs a user, not by
how hard it is. **Items 1 and 11 are the two that stop a program**, and both are
`check`-clean-then-`run`-fails — the asymmetry
[ADR-133](../decisions/133-every-diagnostic-a-well-formed-program-can-earn-is-analysiss.md)
was written to close, still open in two more places. Item 9 is a smaller defect.
Items 2–4, 8, 10 and 12 are missing rows — of which **12 is the one to do
first**, because ADR-107 already built everything downstream of the syntax and
`match` on a `Char` is unwritable until it lands. Items 5–6 are decisions worth
revisiting. Item 7 is two comments that describe code that is not there — and it
is the same file as item 1, which is not a coincidence.

Items 1 and 11 were both found by *deliberately* writing something the three
solutions had not: item 1 by currying a closure, item 11 by stripping every type
annotation off working programs. Neither is exotic — an unannotated helper that
indexes a nested collection is ordinary code — but neither is what a program
written once and left alone happens to contain.

| # | What it was | Severity | Where it went |
|---|---|---|---|
| 1 | a closure whose body *is* a closure loses its transitive captures | **high** — silent wrong answer, panic, or SIGSEGV | [ADR-027](../decisions/027-closures.md) amended |
| 2 | no reverse iteration: `(0..n).rev()` is `Y110` and `5..0` is silently empty | medium | [ADR-145](../decisions/145-a-reversal-needs-the-whole-sequence-so-it-is-a-barrier.md) |
| 3 | no `Int.to_text()`, no `Char.to_text()`, no interpolation | medium (known) | [ADR-143](../decisions/143-the-to-text-family-is-int-float-and-char.md) for the two rows, then [ADR-147](../decisions/147-a-hole-renders-anything-because-the-program-wrote-the-hole.md) for interpolation — **closed in full** |
| 4 | no `join` on `Vec[Char]` — a grid row cannot be rendered back as a line | medium | [ADR-144](../decisions/144-a-sequence-of-text-joins-and-a-sequence-of-char-becomes-one.md) |
| 5 | `Set[Int]` and `Map[Int, V]` order lexicographically on the rendered form | low, but silent | [ADR-138](../decisions/138-a-container-orders-by-the-value-and-not-by-its-printing.md) |
| 6 | recursion has no spelling that does not thread state — day 12's is 10 parameters | low | [ADR-068](../decisions/068-a-function-does-not-capture.md) amended — the *diagnostic* half only |
| 7 | `capture.rs`'s walker comments describe a guard the code does not have | low | with item 1 |
| 8 | no collection can be built at a size: no sized `Grid`, no `Vec(n, fill)` | medium | ADR-146 |
| 9 | a `for` binding has no name **or type** in the crash snapshot — it prints `?` | low | [ADR-139](../decisions/139-a-pattern-name-is-a-name-in-the-frame.md) |
| 10 | `sections` cannot express "a repeated group, then a fixed one" — `repeated` is tail-only | low | [ADR-140](../decisions/140-a-counted-repeated-is-bounded-so-something-can-follow-it.md) |
| 11 | a catalog method on a value *derived* from an unannotated parameter is an ICE | **high** — `check` clean, `run` panics | [ADR-137](../decisions/137-a-deferred-receiver-resolves-in-rounds-and-the-channel-runs-to-a-fixpoint.md) |
| 12 | no character literal: `"#"[0]` is the spelling, and `match` on a `Char` is unwritable | medium | [ADR-141](../decisions/141-a-character-is-one-token-and-a-literal-is-a-load.md) |

**One entry is closed only in part, and it is worth being plain about.**

**Item 6 is the diagnostic only.** The handover attached no proposal for the language question — three
individually defensible rules meeting at a corner — and none was invented. What
landed is the cheap fix the entry itself named: `N007` no longer offers a
closure to a *recursive* `fn`, because `N001` would refuse it. The ergonomics
complaint — ten parameters of transport in day 12's `search` — stands
unanswered, and §6 below says so.

Two things the round found that the handover did not know:

- **Item 12's downstream was not "already built".** ADR-107 said the work was
  "a lexer, a parser arm, and `lower_lit`. Nothing else," and this document
  repeated it. MIR's `Lit::Char` pattern arm was an unconditional jump to
  success, so a `Char` pattern would have matched *every* value — a silent wrong
  answer sitting directly on item 12's headline gate — and `pattern.rs` typed a
  `Char` pattern as whatever the scrutinee was. Both were found because the fix
  was written against a test that had to fail first.
- **Item 11 had a second defect at the same panic.** An *uncalled* function whose
  receiver nothing pins also reached MIR and ICEd, with no chaining involved
  (`fn f(v) { v.len() }` beside `out(1)`). ADR-093 said `monomorphize` dropped
  such bodies; it does not, because ADR-057's pin makes them monotypes.

## Reproducing

```bash
cargo build --release -p praxis-cli
```

Each repro is a `.px` file and one command. None needs input on stdin.

---

## 1. A closure whose body *is* a closure loses its transitive captures

**Fixed.** The four-line early return at `capture.rs:140` is gone, and the token
scan's "declared outside `closure_range`" test — which was already the correct
predicate — is now the only one. Nothing in lowering or codegen needed changing:
once the outer closure captures `base`, `base` *is* a local of the synthetic
outer function by the time the inner literal is allocated, and the prologue
keys on the same `SymbolId`.

Two shapes this entry did not list were broken the same way and are fixed by
the same deletion: `|a| |b| { … }` (a body that is a closure whose own body is a
block), and `[1, 2, 3].map(|x| |y| x + y + base)` — a curried closure inside a
pipeline, which is the one most likely to bite.

The `Unit` that made all three failure modes is now unrepresentable rather than
merely unreached: MIR's closure-allocation arm used to fill a missing capture
slot with `lower_lit_gc(&Lit::Unit)`, and it panics instead. It never fires
across the workspace suite or the book.

**Gate**, as proposed: `crates/praxis-cli/tests/run.rs::a_curried_closure_prints_the_same_thing_with_and_without_braces`
asserts the two spellings produce byte-identical stdout, and that it is
`<closure:1>\n11\n`. Comparing the two runs is the point — a fix that stopped
the panic without restoring the capture would still print two different `N`.
Also: eight capture-analysis unit tests, three lowering tests asserting the
capture's *type* is `Int` and not `Unit`, and six JIT value tests including the
silent-`Text` mode and a cell shared across three frames.
[ADR-027](../decisions/027-closures.md) gains the transitive rule, and
[functions.md](../book/src/language/functions.md) a section with a runnable
example.

**Severity: high.** `praxis check` is clean. It breaks at run time, three
different ways depending on what was captured, and one of them is a silent wrong
answer for a program the type checker accepted.

```praxis
var base = 10
var mk = |a| |b| b + base
out(mk)
out(mk(5)(1))
```

```console
$ praxis run curried.px --input /dev/null
<closure:0>

thread 'main' panicked at crates/praxis-runtime/src/abi.rs:1055:5:
int_payload wants a `Int` payload; this value is a `Unit` (REP-56)
internal error: a panic escaped the runtime wrapper `praxis_int_load`
```

Expected `<closure:1>` and `11`.

**The language diagnoses its own bug on the first line of output.** `out` on a
closure prints `<closure:N>` where `N` is how many bindings it captured
([functions.md](../book/src/language/functions.md)). `mk` prints `<closure:0>`:
the outer closure captured nothing, when it must capture `base` in order to
have one to hand to the closure it returns. The inner closure is then built with
one environment slot filled out of an empty environment, and reads back `Unit`.

### Three failure modes, one root

The captured binding's type decides which one you get.

| Captured binding | What happens |
|---|---|
| `Int`, never reassigned | the Rust panic above. It escapes as `internal error: a panic escaped the runtime wrapper`, **not** as a Praxis fault, so the crash debugger never engages |
| `Int`, reassigned (so `CaptureKind` is a cell) | **SIGSEGV, exit 139** — a `Unit` dereferenced as a `VarCell` pointer |
| `Text` | **silently wrong.** No panic, no fault, no signal |

The third is the one to fix first:

```praxis
var base = "hello"
var mk = |a| |b| base
var got = mk(0)(0)
out(got)
out(got.len())
```

```console
$ praxis run silent.px --input /dev/null
Unit
0
```

`got` is `Text` at check time and `Unit` at run time. That is a type-safety
hole, and nothing in the output says so.

The reassigned-binding variant, for completeness:

```praxis
var base = 10
base = 20
var mk = |a| |b| b + base
out("before")
out(mk(5)(1))
out("after")
```

```console
$ praxis run segv.px --input /dev/null
before
$ echo $?
139
```

### The boundary is `Expr::Closure` as the body, not nesting

This matters for triage, because it is narrower than "nested closures are
broken" and it is why the bug survived: **the shapes an ordinary program writes
are all on the working side.**

Working:

```praxis
var limit = 3
out([[1, 2, 3, 4], [5, 1, 2, 9]].map(|r| r.filter(|n| n < limit)))   // [[1, 2], [1, 2]]
```

```praxis
fn adder(n: Int) -> (Int) -> Int { |x| x + n }                        // a `fn` cannot
out(adder(5)(2))                                                      // capture at all
```

```praxis
var base = 10
var mk = |a| { var inner = |b| b + base ; inner(a) }                  // 15
out(mk(5))
```

Broken — and three levels (`|a| |b| |c| c + base`) fail the same way:

```praxis
var base = 10
var mk = |a| |b| b + base
out(mk(5)(1))
```

The nested-pipeline case is the one worth calling out: `map(|r| r.filter(|n| n <
limit))` captures `limit` across two closure levels and is **correct**. So is
[`tests/aoc-corpus/aoc2025_day07.px`](../../tests/aoc-corpus/aoc2025_day07.px), whose part 1 does the
same thing with `width`. The bug needs a closure returned *from a closure*, with
the inner body naming something declared outside both.

### Root cause: the early return at `capture.rs:140`

`crates/praxis-hir/src/capture.rs` has two paths through `walk`, and they
disagree about what a nested closure means.

```rust
// A nested closure manages its own captures — do not descend into it.
if matches!(expr, Expr::Closure(_)) {
    return;
}
// Scan every token in the subtree.
for child in expr.syntax().descendants_with_tokens() { … }
```

`analyze(body, closure_range, …)` passes the closure's **body** to `walk`.

- When the body *is* the nested closure — `|a| |b| …` — the first branch fires
  and the function returns having recorded nothing. `<closure:0>`.
- When the body merely *contains* one — a block, a method call, anything — the
  token scan runs over `descendants_with_tokens()`, which includes the nested
  closure's tokens, and `record_free_var` correctly keeps every symbol whose
  declaration is outside `closure_range`. That is the right answer, and it is
  why every working case above works.

The token scan's test is already the correct one: a symbol declared outside the
*outer* closure is a capture of the outer closure no matter which nested closure
references it — that is exactly what makes the inner one's environment fillable.
The early return is the only thing standing between the broken shape and the
working one.

**One-brace user workaround**, which also confirms the diagnosis:

```praxis
var base = 10
var mk = |a| { |b| b + base }
out(mk)          // <closure:1>
out(mk(5)(1))    // 11
```

**A suggested gate**, since `<closure:N>` makes the defect assertable without
reaching into the compiler: `|a| |b| b + base` and `|a| { |b| b + base }` must
print the same `N` and the same answer. A fix that only stops the panic without
restoring the capture would still fail it.

---

## 2. No reverse iteration

**Fixed** as `reversed()`, a **barrier** on the generic `Iterable` receiver —
one row covering `Range`, `Vec`, `Set`, `Text`, `Map` and the rest, so
`(0..n).reversed()` and `xs.reversed()` are the same row. Reversal cannot answer
its first element before it has seen its last, so a fused `classify_link` arm
would be unsound off the source; the ADR says so and names the peephole as an
explicit non-goal. It carries **no** capability bound, which is its own claim:
`sorted` reads `compare` and `unique` reads `hash`, reversal reads nothing, so a
`Vec` of closures reverses where `sorted()` is `Y006`.

`5..0` stays silently empty, deliberately: no analysis pass has ever emitted a
`Severity::Warning`, two book examples exist to demonstrate the rule and would
earn the lint, and the real gap was the missing spelling.
`jit::a_countdown_is_a_reversed_range` carries both halves.
[ADR-145](../decisions/145-a-reversal-needs-the-whole-sequence-so-it-is-a-barrier.md).

**Severity: medium.** `Range` has no `rev`, no sequence has a `reversed`, and a
descending range is empty rather than an error:

```console
$ praxis check rev.px
error[Y110]: no method `rev` on type `Range` taking 0 argument(s)

  rev.px:1:17
  1 | for y in (0..5).rev() { out(y) }
    |                 ^^^ no method `rev` on type `Range` taking 0 argument(s)
```

```praxis
for y in 5..0 { out(y) }
out("done")            // prints only `done`
```

Bottom-up dynamic programming is one of the most common AoC shapes, and it has
no spelling but manual bookkeeping. From the day 7 solution:

```praxis
var y = height - 1
while y >= sy {
    …
    y = y - 1
}
```

That is the only place in the program where the mechanism is visible instead of
the puzzle. `(0..n).rev()` as a `Range` stage, or `Vec.reversed()`, removes it.
Of the two, the `Range` one is what this shape actually wants.

Nothing here is wrong — `5..0` being empty is Rust's answer too — but it means
the mistake is silent, which raises the value of having the right spelling
available.

---

## 3. No `Int.to_text()`, no `Char.to_text()`, no interpolation

**Fixed.** `Int.to_text()` and `Char.to_text()` exist, closing the family at
Int/Float/Char. The load-bearing part is the refactor that came first:
`scalars::write_int` and `scalars::write_char` were extracted from the existing
`int_format`/`char_format` (mirroring the `write_float`/`float_format` pair that
already existed), so each descriptor's `format` callback and its `to_text`
wrapper call **one** function. `out(n)` and `n.to_text()` disagreeing is not
representable rather than merely tested, and the runtime tests assert against
`GcRef::format` — `out`'s own path — not against literals.

**Interpolation landed too, so this entry is closed in full.** `out("n = {n}")`
renders the value; the braces are no longer literal text.
[ADR-147](../decisions/147-a-hole-renders-anything-because-the-program-wrote-the-hole.md)
is the decision, and two things about it were settled by the user rather than
derived, so they are recorded here as decisions and not as consequences:

- **A hole may hold any type**, rendered exactly as `out` renders it. `"{v}"` on
  a `Vec[Int]` is `[1, 2, 3]`. That is structural, not tested-and-found-to-agree:
  a hole lowers to `praxis_value_to_text`, which calls `GcRef::format` — the same
  function `praxis_write_stdout` calls — so there is one renderer and two
  callers, which is exactly what ADR-143 decision 2 bought for the three scalar
  rows.
- **A hole may hold a full expression**, not only a bare name: `{a + b}`,
  `{p.0}`, `{xs.len()}`, `{m["k"]}`.

**The ADR-085 entanglement was settled on purpose rather than by accident**,
which is what ADR-143 decision 4 asked for in so many words. ADR-085 decision 2
refused an implicit conversion to `Text` for `+`, and it **still refuses**:
`"n = " + n` is still `Y001`. The reconciliation is that a hole is a rendering
site the program wrote, where `+` coercing would render values the program never
asked to render — so the two are complements. Both halves are pinned from one
source file by
`infer_tests::text_plus_an_int_is_still_y001_beside_a_hole_that_renders_it`,
so an edit that "unifies" them by relaxing `+` fails a test whose name says what
it broke.

The cost estimate in ADR-143 decision 5 was accurate about the parts and wrong
about which part mattered. The one that *decided the design* was `capture.rs`:
because capture analysis looks token **ranges** up in the resolver's map, a hole
had to be a real subtree with real tokens, which rules out the cheap
"lex the literal whole, re-parse holes later" implementation. That
implementation compiles, passes every other test, and leaves
`var f = |_| "{outer}"` capturing nothing — a silent wrong answer at run time.
`capture::a_hole_in_a_closure_body_captures_the_name_it_holds` is the gate.

The proposed desugar to `"a" + n.to_text() + "b"` was **not** taken, because it
bounds a hole to the four types with a `to_text()` row. See ADR-147 decision 2.
[ADR-143](../decisions/143-the-to-text-family-is-int-float-and-char.md),
amending [ADR-086](../decisions/086-a-text-subscript-answers-a-char.md), and
ADR-147 amending both ADR-143 and ADR-085.

**Severity: medium.** Already known and written down in
`crates/praxis-stdlib/src/builtins.rs:797` and
[text.md](../book/src/language/text.md); this is a note that a real solve hit it
constantly rather than a new finding.

```console
$ praxis check label.px
error[Y110]: no method `to_text` on type `Int` taking 0 argument(s)
```

Every labelled debug line is two calls:

```praxis
out("splits:")
out(splits)
```

The documented `Int` workaround, `n.to_float().to_text()`, renders `1660` as
`1660.0`, so it is not one. `Float.to_text()` existing alone is the part that
surprises — the catalog has a `to_text` column with one row in it.

This is the item that showed up most often per hour of writing, which is worth
recording even though the decision to defer it is already made.

---

## 4. No `join`, so a grid row cannot be rendered back as a line

**Fixed**, and the surface is not the one this entry proposed — which is the
interesting part. `join` on both `Vec[Text]` and `Vec[Char]` is **not
buildable**: a generic `Iterable.join/1` beside a concrete `Vec[Char].join/1` is
`AmbiguousWithIterable`, a build-time panic, and two `Iterable` rows differing
only in their item bound compiled and passed every existing test while producing
a silent insertion-order precedence rule — exactly what ADR-127 decision 6
refuses. That state is now a catalog build failure of its own
(`MethodCatalogError::AmbiguousIterablePair`).

So it is two rows under two names: `join(Text)` on `Iterable` with the item
bounded to `Text`, and `to_text()` on a concrete `Vec[T]` with `T` bounded to
`Char`. `out(grid.row(y).to_text())` renders the line; `[1, 2].join(",")` is
`expected Text, found Int`, which is what keeps `join` from being a back door
around ADR-143.
[ADR-144](../decisions/144-a-sequence-of-text-joins-and-a-sequence-of-char-becomes-one.md).

**Severity: medium**, and it compounds with 3.

```console
$ praxis check join.px
error[Y110]: no method `join` on type `Vec[Char]` taking 1 argument(s)
```

`out(grid.row(y))` prints `[., ., |]`. There is no way to get `..|`.

Drawing the grid is how you debug a grid puzzle, and day 7 is a puzzle whose
statement is nine consecutive pictures of a grid. Watching the beams fill in is
the obvious way to check the simulation, and it is not writable — `Vec[Char] →
Text` has no route, with or without a separator. `join(Text)` on `Vec[Text]` and
`Vec[Char]` would cover it; `Vec[Char].to_text()` would cover the grid case
alone.

---

## 5. `Set[Int]` and `Map[Int, V]` order lexicographically on the rendered form

**Fixed.** `{2, 9, 10, 100}`, and `s` now agrees with `s.sorted()`.

The comments this entry quotes were stale in a way worth recording: `compare` was
**not** `None` on every descriptor — it was already populated on `Int`, `Byte`,
`Char`, `Float` and `Text`, and `None` only on composites. So the work was to use
it at ADR-066's three named sites and to give the composites one. It is now
carried by exactly the eleven descriptors a `Map` key or `Set` member can be —
the scalars, `Text`, `Range`, `Record`, `Tuple` and `Enum`, the last three
element-wise — and `None` on the nine collections, `Closure` and `VarCell`, none
of which can ever be a key. That is deliberately a **wider** set than the source
language's `<`: a tuple has a container order and still has no `<`, so
`(1, 2) < (1, 3)` is still `Y006`.

Determinism is unchanged and is still the property being bought: the order is
total, NaN sorts last and ties with itself, and keys of different descriptors
order by descriptor first.
[ADR-138](../decisions/138-a-container-orders-by-the-value-and-not-by-its-printing.md),
discharging the debt [ADR-045](../decisions/045-ordering-semantics-and-the-compare-callback.md)
recorded and removing a leg from ADR-083's and ADR-086's arguments (both
survive on the others).

**Severity: low, but it is silent.**

```praxis
var s = Set()
s.insert(9) ; s.insert(10) ; s.insert(100) ; s.insert(2)
out(s)
out(s.sorted())

var m = Map()
m[9] = "nine" ; m[10] = "ten" ; m[2] = "two"
out(m.keys())
```

```text
{10, 100, 2, 9}
[2, 9, 10, 100]
[10, 2, 9]
```

This is documented — [control-flow.md](../book/src/language/control-flow.md) and
[ADR-066](../decisions/066-a-for-iterates-a-snapshot.md) both say "ascending by
rendered member" — and the guarantee it buys is real and worth keeping: Rust
randomizes hash order per process, so without a fixed sort key two runs of one
program disagree. Determinism is the correctness property; the *rendered form*
is only how it is currently obtained.

**ADR-066 already says so and already names the way out**, which makes this a
scheduling note rather than a new decision:

> The rendered-form sort keys are still D3's to replace; `write_sorted`,
> `ordered_entries` and `ordered_members` are the three places that change when
> `TypeDescriptor::compare` is populated.

So the three call sites are known and the blocker is one unpopulated descriptor
field. What this solve adds is a reason to move it up: the failure has **no
symptom**. A program that walks a `dijkstra` result in key order to find the
nearest reachable node gets `10` before `2` and a wrong answer, with nothing
printed to suggest one — and the collections it bites are exactly the ones a
graph puzzle keys on `Int`.

Until `TypeDescriptor::compare` lands, the cheap half is the word *ascending*,
which sits one line away from `sorted()` — numeric, same collection — in the
book's own table.

---

## 6. Recursion has no spelling that does not thread state

**Partly fixed, and only the part this entry proposed.** No language change was
invented: the three rules still meet at the same corner, and day 12's `search`
still has ten parameters of which five are transport. That complaint is open.

What landed is the cheap fix named in the last paragraph above. `N007`'s advice
is now conditional on recursion:

```console
$ praxis check countdown.px
error[N007]: `countdown` cannot use `step`: a function does not capture the bindings around it (pass `step` as a parameter)

help: `countdown` calls itself, so a closure is not the way out: a closure cannot name itself (`N001`)
```

A mutual cycle says "calls itself through `pong`", naming the other members the
way `N006` already names the other declarations in a type cycle. A
non-recursive `fn` is unchanged, byte for byte, and still names both ways out.
No new diagnostic code was spent.

Recursion is decided on a call graph the resolver builds as it walks, off the
single `resolve_name_ref` funnel. The fact is not available at the instant
`N007` is emitted, so the report is pushed at the use site — preserving source
order — and only its *wording* is settled at the end of resolution.
[ADR-068](../decisions/068-a-function-does-not-capture.md) is amended.

**Severity: low.** Three individually well-argued rules meet at a corner:

- a `fn` does not capture — `N007`, which says so and suggests a closure;
- a closure cannot name itself — `N001`, so recursion needs a `fn`;
- therefore a *recursive* function's state must be a parameter.

The recursive form of day 7 part 2 is a two-parameter algorithm — `(x, y)` —
with a five-parameter signature:

```praxis
fn timelines(m: Grid[Char], sp: Char, memo: Map[(Int, Int), Int], x: Int, y: Int) -> Int
```

`m`, `sp` and `memo` are threaded through every call — the memo tops out at one
entry per cell, 20,022 of them on this input — purely to work around the absence
of capture. It works, and the answer is right; it is the program looking least
like the problem.

Day 12 is where this stops being cosmetic. Its packing search is **ten
parameters**, repeated verbatim at each of three recursive call sites:

```praxis
fn search(
    w: Int, h: Int, occ: Vec[Bool],
    orients: Vec[Vec[Vec[(Int, Int)]]], sizes: Vec[Int],
    counts: Vec[Int], remaining: Int, cells_left: Int, waste: Int, pos: Int,
) -> Bool
```

Five of those vary as the search descends — `counts`, `remaining`,
`cells_left`, `waste`, `pos`. The other five, `w`, `h`, `occ`, `orients` and
`sizes`, are fixed for the whole search and are parameters **only** because a
`fn` cannot capture. Half the signature, and half of every call, is transport.

Day 11's memoized path count is the same ratio at a smaller size — seven
parameters, of which `adj`, `first`, `second` and `target` never change:

```praxis
fn count_via_both(
    adj: Map[Text, Vec[Text]], memo: Map[(Text, Int), Int],
    node: Text, seen: Int,
    first: Text, second: Text, target: Text,
) -> Int
```

Three programs, three recursive functions, and in each one the majority of the
parameter list is invariant. That is the pattern worth weighing against whatever
the fix costs: it is not one awkward signature, it is the shape every recursion
in this language takes.

No proposal attached; the three rules are each defensible. Recording it because
`N007`'s `help:` offers a closure as the way out, and for a recursive function —
which is every use that hits this — a closure is not one, because `N001` forbids
it naming itself. The help text could say so, and that is a cheap fix
independent of whatever the real answer is.

---

## 7. `capture.rs`'s walker comments describe a guard that is not there

**Fixed with item 1**, and the entry was right that the comments were where the
bug hid — they described the guard that would have made the *working* cases
broken too.

One correction to this entry: `walk` was never "the recursive walker". It never
called itself; it was a single flat token scan over `descendants_with_tokens()`
with one early return bolted on the front. The rewritten doc says "flat token
scan", because a reader who believes it recurses will look for the bug in the
wrong shape. `_inside_nested_closure` is deleted.

**Severity: low on its own**, listed because it is the same file as item 1 and
reads like the place the bug hid.

`crates/praxis-hir/src/capture.rs:125`:

> The recursive walker. Descends into every expression kind *except* nested
> closures (their captures are their own).

It descends into nested closures in every case but one — that is the token scan
at line 148, and it is the behaviour that makes correct programs correct.

`crates/praxis-hir/src/capture.rs:150`:

> Skip tokens inside a nested closure (their captures are their own). A nested
> closure's subtree is excluded by the early return above for direct closures,
> but a nested closure may appear as a descendant of a non-closure expr; guard
> by checking the token is not within any CLOSURE_EXPR descendant other than via
> the outer closure.

There is no such check in the loop. The comment describes the guard that item
1's early return half-implements, and had that guard actually been written, the
working cases in item 1 would be broken too.

Both comments describe an intent — *a nested closure's captures are entirely its
own* — that is wrong. A nested closure's captures that resolve outside the
enclosing closure are **also** the enclosing closure's, and must be, because the
enclosing environment is where the inner one is filled from. Fixing item 1 means
fixing that sentence first.

Also in the file, and probably worth taking with it: `_inside_nested_closure` at
line 167, a `#[allow(dead_code)]` empty function retained "as a note for a
future tighter analysis". The tighter analysis it anticipates is the one that
would make item 1 worse.

---

## 8. No collection can be built at a size

**Fixed.** `Vec(n, fill)` and `Grid(w, h, fill)` exist, and day 12's occupancy
board is now the real `Grid[T]` rather than a hand-rolled `Vec[Bool]` — the
program answers byte-identically, which is the evidence that the feature is the
one this entry asked for.

The difficulty was never the runtime. It is that this is an **arity overload**,
and [ADR-089](../decisions/089-a-name-has-one-signature.md) ("a name has one
signature") forbids those in so many words. ADR-146 narrows rather than reverses
it, on ADR-089's own two grounds: the shape is chosen on the argument *count* — a
syntactic fact available before any argument is typed, so the inference
circularity ADR-089 names cannot arise — and a collection constructor has no
function value at all, so `var f = Vec` is still `Y022` and ADR-061 does not
reopen. The exception is a **closed two-row table**, and a test asserts it is
exactly two: the other seven constructors gain nothing and still report `Y024`
naming zero arguments.

A negative or oversized extent is a `size or extent out of range` fault, not a
`check`-time refusal — a size is an ordinary `Int` whose value is not known until
it runs. `AllocKind::Collection` carries a `CollectionInit` whose *variant is the
arity*, so an operand list of the wrong length is unspellable rather than merely
untested.

One consequence worth naming: `praxis_grid_filled` deliberately does not call
`default_cell`, which makes `Grid[Vec[Int]]` constructible from source for the
first time. When the fill is itself a collection every cell is the *same*
collection, because a binding names an object rather than owning it —
`Grid(2, 2, Vec())` is four names for one `Vec`, and the book says so beside the
constructor. ADR-146.

**Severity: medium.** There is no sized `Grid`, no `Vec(n, fill)`, and no
`repeat`. [grid-and-graphs.md](../book/src/language/grid-and-graphs.md) states
the `Grid` half plainly:

> `Grid()` exists as a prelude name and builds a 0×0 grid, which is not useful
> for much: it takes no arguments, so there is no way to ask for a sized one,
> and there is no `to_vec`-style `to_grid` on any sequence. A grid is something
> you read, and then index, mutate and rotate.

"A grid is something you read" is true of an *input* grid and false of a
**working** grid, which is the one an algorithm allocates for itself: an
occupancy board, a visited mask, a distance table, a DP row. Day 12's packing
search needs a W×H board that is not in the input and never will be, so the
`Grid` type — subscripts, `contains`, bounds behaviour, `neighbors4` — is
unavailable to it, and the board is a `Vec[Bool]` with `y * w + x` written out
at all four use sites:

```praxis
if occ[ny * w + nx] { return false }
occ[(y + c.1) * w + (x + c.0)] = v
```

That is the language's own `Grid[T]` re-implemented by hand, minus the bounds
checking, in the one program that most wanted the real thing.

The `Vec` half costs a loop before it costs anything else — filling the board is

```praxis
var occ = Vec()
var k = 0
while k < w * h {
    occ.push(false)
    k = k + 1
}
```

with no `Vec(n, fill)` to say it in one line. `(0..n).map(|_| false)` does work
and is what day 7 used for its DP row, which makes the omission of a direct
spelling harder to justify rather than easier — the capability is there, only
the name is missing.

Two rows would close this: `Grid(w, h, fill)` and `Vec(n, fill)`. The first is
the one that matters, because it is the difference between using `Grid[T]` and
reimplementing it.

---

## 9. A `for` binding has no name or type in the crash snapshot

**Fixed**, and it was two defects as this entry suspected. Every
pattern-introduced binding now renders `name: Type = value`:

```text
  locals:
    pairs: Vec[(Int, Int)] = [(1, 2), (3, 4)]
    acc: Int = 87
    a: Int = 3
    b: Int = 4
    opt: Option[Int] = Some(77)
    payload: Int = 77
```

`payload` — absent before, and the harder half, since absent is not the same
failure as unnamed — is there. And the destructuring `for`'s three anonymous
rows are now two: `a` and `b` are named, and the *scrutinee* is gone rather than
renamed. It is a compiler temp, not a user binding, so suppressing it is what
ADR-125's model actually implies; naming it would have invented a binding the
program never wrote.
[ADR-139](../decisions/139-a-pattern-name-is-a-name-in-the-frame.md).

**Severity: low**, but it is in the crash debugger, which is a headline feature
(§9), and the value it hides is one of the most common things to want at a
fault.

```praxis
var xs = [1, 2, 3]
var total = 0
for item in xs {
    total = total + item
}
out(xs[9])
```

```console
$ praxis run forname.px --input /dev/null
error: program faulted: index out of bounds
…
  locals:
    xs: Vec[Int] = [1, 2, 3]
    total: Int = 6
    ? = 3
```

`item` is the `? = 3`. Every other local prints `name: Type = value`; a `for`
binding prints neither its name nor its type, only its last value.

[ADR-125](../decisions/125-a-binding-is-a-binding-and-the-compiler-decides-its-storage.md)
is explicit that a `for` variable is a binding like any other — "a parameter, a
`for` variable and a name a pattern introduces included" — so this is the
snapshot disagreeing with the binding model, not a `for` variable being special.
Day 12 hit it with two loops live at once, and the dump had two anonymous `?`
rows in it with nothing to tell them apart:

```text
    orients: Vec[Vec[Vec[(Int, Int)]]] = <uninit>
    ? = <uninit>
    cells: Vec[(Int, Int)] = <uninit>
    …
    ? = <uninit>
```

### It is every pattern-introduced binding, and a `match` arm's is worse

The `for` variable is not a special case. Checking the rest of ADR-125's
sentence:

```praxis
var pairs = [(1, 2), (3, 4)]
var acc = 0
for (a, b) in pairs { acc = acc + a + b }
var opt = Some(77)
match opt {
    Some(payload) => { acc = acc + payload }
    None => {}
}
out(pairs[9])
```

```text
  locals:
    pairs: Vec[(Int, Int)] = [(1, 2), (3, 4)]
    acc: Int = 87
    ? = (3, 4)
    ? = 3
    ? = 4
    opt: Option[Int] = Some(77)
```

Three anonymous rows for one destructuring `for`: the tuple it is walking, then
`a`, then `b`. And `payload` — bound to `77`, and the arm did run, since `acc` is
`87` — **does not appear at all**.

So the rule the snapshot actually implements is "a binding with a declaration
statement keeps its name":

| Binding | In the snapshot |
|---|---|
| `var` | `name: Type = value` |
| `fn` parameter | `name: Type = value` — verified separately, both params present |
| `for` variable | `? = value` |
| `for` destructuring | `? = value` per element, plus one for the scrutinee |
| `match` arm payload | absent |

Which is exactly the distinction ADR-125 says does not exist. The fix is
presumably one map from slot to symbol that the pattern lowering does not
populate; the `match` row may be a second, separate thing, since absent is not
the same failure as unnamed.

---

## 10. `sections` cannot express "a repeated group, then a fixed one"

**Fixed** as the counted `repeated(P, N)` this entry proposed. Day 12's file is
now writable as what it is:

```praxis
read sections(
    shapes: repeated(block(`{i:int}:`, rows: grid(char)), 6),
    regions: lines(`{w:int}x{h:int}: {counts:ws(int)}`),
)
```

`I028` is unchanged in force and narrower in scope — it now fires only for the
**unbounded** form, whose argument was always greed rather than position, and
its message says so and names the counted form as the way out. `N` is a literal
of at least 1, because the parser plan is built when the program is compiled;
too few sections left is a parse fault, as too few for a fixed field already
was. [ADR-140](../decisions/140-a-counted-repeated-is-bounded-so-something-can-follow-it.md),
amending [ADR-073](../decisions/073-a-constructor-call-is-a-shape-checked-before-it-is-built.md).

**Severity: low**, and the rule behind it is right. Recording it because it is
the only place in three puzzles where the input-parser DSL could not say what
the input was, and the workaround is copy-paste.

Day 12's file is six shape sections and then one section of region lines. The
parser that says that is:

```praxis
read sections(
    shapes: repeated(block(`{i:int}:`, rows: grid(char))),
    regions: lines(`{w:int}x{h:int}: {counts:ws(int)}`),
)
```

```console
$ praxis check tail.px
error[I028]: a `repeated(...)` tail may appear only as the final named argument (§7.5): it consumes every remaining section, so nothing can follow it
```

The diagnostic is correct and explains itself: a tail that takes *every*
remaining section cannot be followed by anything. No argument with the rule as
stated. The gap is that `repeated` is the only repetition the DSL has, and it is
unconditionally greedy — there is no bounded form, so a group with a known count
that is **not** last has no spelling at all. The file is written out six times
instead:

```praxis
    s0: block(`{i:int}:`, rows: grid(char)),
    s1: block(`{i:int}:`, rows: grid(char)),
    …
    s5: block(`{i:int}:`, rows: grid(char)),
```

and then six more lines downstream to collect the fields back into the `Vec` the
program wanted in the first place, because six named record fields are not a
sequence.

A counted `repeated(P, 6)` would close it: bounded, so something can follow, and
the "may only be last" rule stays exactly as it is for the unbounded form.

### What this item is *not*

The first version of day 12 parsed the shape sections as `lines(word)` and then
hand-walked the `Text` rows looking for `#`, in a 14-line `shape_cells`
function. That was **not** a limitation of the DSL and it should not be read as
one — `block` composes with `grid` exactly as documented, the header line and
the diagram each get their own parser, and the shape's cells come straight off
the result:

```praxis
s0: block(`{i:int}:`, rows: grid(char)),
…
var cells = data.s0.rows.find_all("#"[0])   // Vec[(Int, Int)]
```

That is the whole of `shape_cells`, and the function is gone. The DSL reached
one level further into this input than the program initially asked it to, which
is worth writing down next to the one thing it genuinely could not do.

---

## 11. A method on a value *derived* from an unannotated parameter is an ICE

**Fixed**, and the diagnosis in this entry is one level off in a way worth
correcting: the derived receiver **was** pinned all along — `require_method`
pins the result variable too — and the constraint **was** made. What was missing
was that it was never looked at again.

`discharge_constraints` drained one snapshot: `for c in self.db.take_dischargeable()`.
But `HasMethod`, `Iterable` and `HasField` are discharged by *producing* a type,
and that unification is the only thing that can make the **next** link of a
chain dischargeable — and the next link is not in the batch already handed back,
because when the batch was taken its receiver was still a variable. So `t[i]`
resolved and `t[i][j]` was dropped on the floor. The channel now runs to a
fixpoint, with the termination argument written into the doc comment rather than
an iteration cap.

All seven rows of the table above now compile and answer. Two consequences
beyond the entry: a `Y110` that was being *swallowed* is now reported at `check`
(`fn f(v) { v[0].len() }` on a `Vec[Int]`), which is ADR-133's rule reaching one
more code; and a second, distinct defect at the same panic was found — an
**uncalled** body whose receiver nothing pinned also ICEd, with no chaining
involved. ADR-093 blamed `monomorphize` for dropping such bodies; it does not,
because ADR-057's pin makes them monotypes. They lower to an unconditional
`panic` now, which is sound because the body is unreachable by construction.

**Negative gate held**: `fn total(values) { values.sum() }` at `Vec[Int]` and
`Vec[Float]` is still `Y001`, byte-identical to generalization.md's transcript,
and `fn id(x) { x }` still renders `forall T. (T) -> T`. The fix changes *when* a
constraint is examined and nothing about *which* variables are pinned.
[ADR-137](../decisions/137-a-deferred-receiver-resolves-in-rounds-and-the-channel-runs-to-a-fixpoint.md).

**Severity: high.** `praxis check` exits 0. `praxis run` panics with an
`internal compiler error` before the program produces anything. The program is
well-typed — adding one annotation compiles it and it answers correctly — so
this is inference failing to resolve something it can resolve, not a program
that deserved a diagnostic.

Two lines:

```praxis
fn pick(t, i, j) { t[i][j] }
out(pick([[7, 8]], 0, 0))
```

```console
$ praxis check pick.px
$ echo $?
0
$ praxis run pick.px --input /dev/null
thread 'main' panicked at crates/praxis-mir/src/build.rs:1437:17:
internal compiler error: the pipeline recognizer declined `[]`, and it carries
no runtime symbol. Every intrinsic row must be classified by `classify_link` or
`classify_sink` …, and every unresolved method call must have been reported by
inference and dropped before here (ADR-093).
```

Expected `7`. The assertion states the invariant it is enforcing, and the
invariant is the one being broken: the unresolved call was **not** reported by
inference.

### The rule

What matters is whether the receiver is the parameter itself or something
derived from it. Every case below is a single call site at one concrete type,
so nothing here is asking for polymorphism.

| Body of `fn f(v)` | Result |
|---|---|
| `v.len()` — method on the parameter | **works** |
| `v[0]` — one subscript, result returned | **works** |
| `v[0].map(\|x\| x * 2)` — a *pipeline stage* on a derived value | **works** |
| `v[0][1]` — subscript of a subscript | **ICE** |
| `v[0].len()` — catalog method on a subscript result | **ICE** |
| `v.get(0).len()` — catalog method on a method result | **ICE** |
| `for row in v { row.len() }` — catalog method on the `for` item | **ICE** |

Binding the intermediate first does not help — `var row = t[i]` then `row[j]`
ICEs identically — so it is not about expression chaining. Annotating the
parameter fixes every row: `fn pick(t: Vec[Vec[Int]], i, j)` compiles and prints
`7`.

The pipeline row is the useful clue, and the panic message points at it: a stage
like `map` is recognized by the pipeline recognizer and never needs a catalog
row, so it survives. `[]` is offered to the same recognizer, declined, and then
has nothing to fall back on.

### Why the corpus never hit it

Day 11's two recursive functions are fully inferrable with **no** annotations —
`Map[Text, Vec[Text]]` and a `Map[(Text, Int), Int]` memo, both reconstructed
from use, both answering correctly. That program calls methods on its parameters
and never on a derived value, which is the whole difference.

Day 12 does, and it is where this surfaced: stripping the annotations from
[`tests/aoc-corpus/aoc2025_day12.px`](../../tests/aoc-corpus/aoc2025_day12.px) makes it ICE, and
bisecting the ten parameters of `search` shows exactly one restores it —

```praxis
orients: Vec[Vec[Vec[(Int, Int)]]],
```

— the parameter the body double-subscripts (`orients[s]` then `shape[o]`). The
other nine can all stay bare.

### What it is not

`fn total(values) { values.sum() }` refusing a second element type is
**deliberate** — [ADR-057](../decisions/057-a-capability-requirement-rides-on-the-scheme-that-quantified-it.md)
decision 5, documented in
[generalization.md](../book/src/types/generalization.md) under "two things that
deliberately do not generalize", because there is one lowered body per source
function and monomorphization clones a body whose method calls are already
resolved. That is a `Y001` at the second call site, which is a diagnostic and
not a crash, and it is not this.

This item is the *same pinning mechanism failing to run at all* one level down.
Discharge pins a receiver when the receiver is the parameter; when it is a
subscript result, a method result, or a `for` item, the receiver is never pinned,
inference emits nothing, and MIR panics. The `for` row is worth calling out
separately because
[ADR-062](../decisions/062-an-iterated-parameter-is-generic-in-the-iterable-and-not-its-element.md)
says an iterated parameter's *item* is pinned "for the same reason a method
receiver is" — and the book's example for it does arithmetic on the item rather
than calling a method, so the combination that breaks is exactly the one the
example does not cover.

### Suggested gates

Both of these are two-line programs and neither needs input:

- `fn pick(t, i, j) { t[i][j] }` with one call site must print `7`, not panic.
- `fn f(v) { for row in v { out(row.len()) } }` with one call site must print
  the row lengths — this is ADR-062's own claim, tested with a method instead
  of arithmetic.

And a negative gate worth keeping beside them, so a fix does not go too far:
`fn total(values) { values.sum() }` called at `Vec[Int]` and `Vec[Float]` must
still be `Y001` per ADR-057.

---

## 12. There is no character literal, and `"#"[0]` is not a substitute

**Fixed.** `'#'` is a token, an expression and a pattern, with a text literal's
escapes plus `\'`, and no `\x`/`\u{…}` in either. `''` and `'ab'` are lex
errors (`T007`) where they are written — which is the `"##"[0]` row of the table
above closed at the front end — and the too-long case offers a machine-applicable
rewrite to a text literal.

**This entry's claim that everything downstream was already built was wrong, and
it inherited that from ADR-107.** Two of the three things it missed would have
been silent:

- MIR's `Lit::Char` pattern arm was `Terminator::Jump { target: on_success }`.
  Landing the syntax alone would have made `match c { '#' => …, '.' => … }`
  compile, coverage-check clean, and return the first arm **for every
  character**. That is a silent wrong answer sitting on this item's headline
  gate, and it is invisible to `check`.
- `pattern.rs` typed a `Char` pattern as *whatever the scrutinee was*, so
  `match n { 'a' => … }` over an `Int` would have type-checked.
- There was no `GcConst::Char`. Without it `'#'` lowers to `praxis_alloc_char`
  plus a `CheckFault` — **slower than the `"#"[0]` it replaces**, so the folding
  half was required rather than polish.

Exhaustiveness genuinely needed no code: `Char` already falls to
`Signature::Open` like `Int`, and `LitKey::render` already printed a witness as
`'x'` — the proposed syntax, written years ahead of it.

**All three gates pass.** `match c { '#' => …, '.' => … , _ => … }` compiles and
is coverage-checked (and a two-arm version is `Y120`, byte-identical to `Int`'s
message); `'#'` is `Inst::ConstGc` — two loads via a new `SMALL_CHARS_OFFSET` —
where `"#"[0]` emitted a `TextGet` call; and `''`/`'ab'` are lexical errors.
`'é'` correctly still allocates. Both AoC solves now write `'^'`, `'S'` and
`'#'`, with byte-identical output.
[ADR-141](../decisions/141-a-character-is-one-token-and-a-literal-is-a-load.md),
answering ADR-086's D19 and superseding ADR-107's decision 2.

**Severity: medium.** Raised by the maintainer on reading the solutions. It is
the most-written awkwardness in a language whose stated purpose includes reading
character grids, it costs more than syntax, and — the reason it leads this
entry — **ADR-107 already built the compiler half and is waiting on the
syntax.**

A character is spelled as a one-character `Text` subscripted at zero
([ADR-107](../decisions/107-a-small-char-is-one-object-and-there-is-no-char-literal.md),
[ADR-086](../decisions/086-a-text-subscript-answers-a-char.md)). Across the
three solutions:

```praxis
var splitter = "^"[0]                       // day 7
var source = match manifold.find("S"[0])    // day 7
var hash = "#"[0]                           // day 12
```

### The part that is not cosmetic: you cannot `match` on a `Char`

A pattern must be a literal, and there is no character literal, so there is no
`Char` pattern:

```praxis
var c = "#"[0]
match c {
    "#" => out("wall"),
    _ => out("open"),
}
```

```console
$ praxis check match-char.px
error[Y001]: expected Char, found Text

  match-char.px:3:5
  3 |     "#" => out("wall"),
    |     ^^^ expected Char, found Text
```

`"#"` in a pattern is a `Text`, and the scrutinee is a `Char`. There is no third
thing to write. So **dispatching on a grid cell — the most common single
operation in this problem domain — has no `match` form at all**, and is an
`if`/`else if` chain against pre-bound variables:

```praxis
var wall = "#"[0]
var dot = "."[0]
if c == wall { … } else if c == dot { … } else { … }
```

which no exhaustiveness check can help with, in a compiler that gained
`match`-coverage diagnostics as the headline of M12
([ADR-130](../decisions/130-a-matchs-coverage-is-analysis-answer-and-the-pattern-is-built-once.md)).
`exhaustive.rs:428` already maps `Lit::Char` to a `LitKey::Char`. Nothing can
reach it.

### Three ways the workaround goes wrong, all of them `check`-clean

| Written | What happens |
|---|---|
| `"##"[0]` — a typo, meant one character | **silently** the first one; no diagnostic, ever |
| `""[0]` | `index out of bounds` **at run time** |
| `"#"[1]` | `index out of bounds` **at run time** |

The first is the bad one: `"##"[0]` is a well-typed program that quietly means
something the author did not write. A literal makes all three lexical errors.

### It is also a runtime call, not a constant

`"#"[0]` lowers to `praxis_text_get` and is not folded, so it re-evaluates on
every execution. Over a three-million-iteration loop comparing one character:

```text
"l"[0] written inline in the loop   0.07–0.08s
hoisted to a var before the loop    0.05s
```

Both day 7 and day 12 hoist it manually, which is a thing the author has to know
to do and which the source gives no hint about. A literal is
`Inst::ConstGc` — two loads — exactly as
[ADR-100](../decisions/100-a-small-int-is-one-object-and-a-literal-is-a-load.md)
decision 4 already did for `Int` — whose title, *"a literal is a load"*, is the
half `Char` never got.

### The compiler is already built for this

This is not a request to reverse ADR-107. Its Decision 2 is titled *"There is no
`GcConst::Char`, because there is no character literal"*, and it says:

> If character-literal syntax is ever added, a `GcConst::Char` becomes correct
> on the same day and `small_char::index_of` is already the compile-time
> predicate it would ask — which is why that function is a `const fn` even
> though only the runtime calls it today.

Every claim in that sentence still holds, checked against the tree at `f049c64`:

- **`Lit::Char` exists and is constructed nowhere.** Six mentions across
  `praxis-hir` and `praxis-mir`, every one a match arm — `pattern.rs:122`,
  `exhaustive.rs:428`, `build.rs:1738`, `build.rs:5203`, and two comments
  (`forward.rs:726`, `build.rs:1755`) that say so in as many words.
- **`small_char::index_of` is still `const fn`** (`small_char.rs:84`), still
  called only by the runtime.
- **The single quote is unclaimed.** The grammar's only string production is
  `TextLit := '"' (char | escape)* '"'`; `'` appears nowhere else in it.

So the work is a lexer token, a parser rule, one `Lit::Char` construction site,
and a `GcConst` variant. Pattern lowering, exhaustiveness and interning are done
and currently unreachable.

**Proposed spelling:** `'#'`, Rust-style, single code point, same escapes as
`Text`. `'ab'` is a lex error rather than a silent truncation, which is the
`"##"[0]` row above closed at the front end.

Three consequences fall out on the same day, and are the gates worth writing:
`match c { '#' => …, '.' => … }` compiles and is coverage-checked; `'#'` in a
loop costs two loads instead of a call; and `""[0]`'s and `"##"[0]`'s failure
modes stop being expressible.
