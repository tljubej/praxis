# Repair session handover — §3.3 runs

**Date:** 2026-07-30
**Tree:** `b495e93` · **Suite:** 1435 passed, 0 failed, 38 ignored · `just ci` green

`implementation-repair-progress.md` is still the living status and this file does
not replace it — read §1 and §4 there first. This is the session-scoped note: what
landed, where to start, and the things that would otherwise have to be
rediscovered.

## What landed

| Row | Commit | ADR |
|---|---|---|
| **REP-16** — a subscript is a catalog row | `0ee29b1` | **064** |
| **REP-09** — a type constructor's brackets are type arguments | `2f9bcbd` | **065** |
| **REP-18** + **REP-20** — `values()`/`count(pred)`, and a template literal that begins with a space | `b495e93` | — |

**S25's acceptance criterion is met: §3.3's representative program compiles and
runs.** It is `tests/aoc-corpus/s33_representative_program.px` and answers `5` and
`12` — the AoC-2021-day-5 sample answers. Nothing else in the repair can claim
this, and it is the reason to prefer REP-15 next.

**Four new findings, all registered in the plan's §4.1: REP-18, REP-19, REP-20,
REP-21.** Two are fixed (18, 20); two are `unscheduled` (19, 21). The register is
now **REP-01 … REP-21**.

## Where to start

**REP-15**, the P0 — six of the nine iterable collections have no `for` lowering.
It is unchanged by this session and is now unambiguously the most severe thing in
the tree; §4 of the progress doc and §4.1 of the plan both scope it. It needs a
decision (the iteration protocol, and whether `for (k, v) in m` destructures)
before it needs code.

Two things this session found are worth weighing against it:

- **REP-19 (P1)** — **a top-level statement is analyzed and then never executed.**
  ```
  out(1)
  let x = 2
  out(x)
  ```
  passes `praxis check` and prints **nothing**. `TypedModule.items` holds only
  `fn` declarations, `lower_module` emits only those, and `run.rs` calls the `main`
  *function*. A file with no `fn main` at all is "error: no `main` function to run"
  after a clean check.

  **§3.3 is written entirely at top level, and so is §4.2's `let width =
  grid.width`** — the design doc's own programs are the ones this silences. It is
  also why the trailing `main()` call every corpus program ends with is decorative:
  it never runs either. The corpus copy of §3.3 wraps its body in `fn main()` and
  says so in a comment.

  The fix is a synthetic entry point — lower the file's top-level statements into a
  `main`, or into a block that runs before it — and the decision it needs is what
  happens when the file *also* declares `fn main`. Note the interaction with
  **N005**: a `fn` inside a `fn` is rejected, so "wrap everything in `main`" cannot
  be a literal source transformation; the top-level `fn` items have to stay where
  they are.

- **REP-21 (P3)** — `min=`/`max=`. Cheap and nearly free: `praxis_map_update_min`
  and `praxis_map_update_max` **already exist in the runtime with no caller**, and
  `praxis_stdlib::catalog` already reserves the row names `[]min=` and `[]max=`
  (ADR-064 documents why they are separate rows and not a read-modify-write). What
  is left is the grammar, and it has to be contextual: `min` is an *identifier*, so
  `min=` is two tokens, and a lexer fusion would break `var min=0`. The rule is "at
  an assignment position, an `Ident` spelling `min`/`max` immediately followed by
  `EQ`".

## Five things worth not rediscovering

1. **A subscript is a catalog row, and that was the load-bearing decision**
   (ADR-064). Dispatch on receiver shape *and arity* is what §5.7's table already
   is, so putting `[]`/`[]=` in it inherits the bidirectional argument inference,
   the `HasMethod` deferral (so `fn first(m, k) { m[k] }` infers), TY-31's bounds,
   TY-32's invariants and monomorphization. **The names are not identifiers on
   purpose** — the parser accepts only an `Ident` after `.`, so `m.[](k)` cannot be
   written and the subscript grammar is the rows' only caller. A gate asserts that
   property rather than trusting it.

   One hole this opened and closed: `resolve_deferred_method` deliberately leaves a
   *missing method* alone because lowering owns `Y110`. A subscript has no report at
   lowering, so a deferred `m[k]` that resolved to a `Set` would have been accepted
   and then silently dropped. It reports `Y020` at the use site now, and the gate
   for it is the assertion that failed first when the test was written.

2. **`Ident [ … ]` is ambiguous and only a name table can decide it** (ADR-065).
   The brackets are identical and the contents are too: `Int` is a legal
   expression, `(Int, Int)` a legal tuple of two names. "Brackets followed by `(`"
   is the tempting rule and it silently reparses `m[k](7)`, which §4.10 makes
   legal. So `praxis-parser` holds `TYPE_CONSTRUCTOR_NAMES` — the ten §6.1
   collections plus `Option` — and `the_parsers_type_constructors_are_the_compilers`
   keeps it from drifting from `praxis-hir`'s `is_type_ctor_name`. The cost is that
   a binding shadowing a constructor's name cannot be subscripted; that is the
   whole price.

3. **A `HashMap`'s iteration order is a correctness problem here, not a tidiness
   one.** RT-16 fixed *formatting* by sorting on the rendered entry. `keys()` and
   `values()` return values, so the same randomization would change a program's
   **answer** between runs. `maps::ordered_entries` is that rule hoisted, and the
   two accessors are index-aligned because they share it. When D3 lands and
   `TypeDescriptor::compare` is populated, `write_sorted` and `ordered_entries` are
   the two places that change.

4. **The previous handover was wrong that `counts.values().count(…)` works.**
   Neither half existed: `values` was in no catalog row (and appears nowhere in the
   design doc except that one line), and `count` was defined only at arity zero.
   The claim survived three sessions because the program failed *earlier* every
   time it was measured — which is the general lesson from this session: **each of
   REP-16, REP-09, REP-18, REP-20 and REP-19 was invisible until the one before it
   landed.** Measuring an acceptance criterion once tells you the first thing
   missing, not the list.

   The corollary for REP-18: a **second arity** of `count` is not D16. The method
   catalog has always been keyed by `(receiver, name, arity)`, so two arities of one
   method name were always legal there. D16 is about a *prelude function*.

5. **A compound store must not be desugared.** `m[k] += v` → `m[k] = m[k] + v`
   names the receiver and every index twice, and MIR lowers each `TypedExpr` where
   it stands — so `m[f()] += 1` would call `f` twice. `TypedStmt::IndexAssign`
   carries the pieces once, and two gates hold the line: one counts the
   instructions, one observes a logging index *and* a logging receiver. If a later
   change makes this look like duplication worth collapsing, that is what it costs.

## Two smaller notes

- **`stmt_exprs` now exists** in `praxis-hir`, beside F20's `TypedExpr::children`
  and for the same reason: three walks over `TypedStmt` (MIR's closure collection,
  MIR's function-value collection, the debugger's purity check) named the fields by
  hand, and `IndexAssign` is the first statement with three expressions. Two test
  walkers use it too. If you add a statement variant, that is the one place to add
  its sub-expressions.
- **`peek_text` returns `Option<&'t str>`.** The elided lifetime tied it to the
  parser borrow, so making a second decision from the same token text was a borrow
  error — a trap rather than a design, since the text outlives the parser.

## The `praxis check` sweep

Done, over `crates/praxis-cli/tests/fixtures`: `bad_byte.px`, `parse_error.px` and
`type_error.px` fail, and they are the three intentional negative fixtures. The
`tests/` corpus is executed by `every_corpus_program_runs_and_prints_the_answer_it_documents`
(REP-12), so it needs no hand sweep — and it gained two programs this session:
`rep16_subscript_counting.px` and `s33_representative_program.px`.

## One asymmetry to keep in mind

`praxis check` exits **0** on a program whose only mistake is a `Y110` (no such
method), because `Y110` is emitted at *lowering* and `check` does not run it. That
is REP-12's asymmetry, still present, and it is why every diagnostic this session
spent (`Y020`, `Y021`) is emitted in **inference**. It bit twice during the session
— a clean `check` followed by a failing `run` — so it is worth knowing before you
trust a green `check`.
