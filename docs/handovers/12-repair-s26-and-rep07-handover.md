# Repair session handover — S26 closed, and S25 opened

**Date:** 2026-07-30
**Tree:** `c74e062` · **Suite:** 1406 passed, 0 failed, 38 ignored · `just ci` green

`implementation-repair-progress.md` is still the living status and this file does
not replace it — read §1 and §4 there first. This is the session-scoped note: what
landed, where to start, and the things that would otherwise have to be
rediscovered.

## What landed

| Stage | Rows | Commits |
|---|---|---|
| **S26** — declaration, pattern and inference gaps | REP-03 + REP-04, REP-14 (closing the stage) | `b3bb6f5`, `4b7d763`, docs `fd4ba67` |
| **S25** — grammar completion | REP-07 (of four) | `c74e062` |

**S26 is closed and D17 is answered**, so **D16 is the only repair decision still
open**. Two new ADRs: **062** (an iterated parameter is generic in the iterable and
monomorphic in its element) and **063** (a self-referring type declaration is
reported).

**One new finding, and it is a P0: REP-15.** It is registered in the plan's §4.1
and is deliberately `unscheduled` — see below.

Three of §4.1's fifteen rows remain: **REP-08, REP-09, REP-10** (the rest of S25),
plus REP-15.

## Where to start

**REP-08 — `p.0`, tuple element access.** It is next in S25's own ordering, it is
the other half of what §3.3's representative program needs, and it is scoped below.
After it, REP-09 (`Counter[(Int, Int)]()`) is what completes that program — which
is the stage's acceptance criterion and the most visible thing left in the repair.

**But REP-15 is more severe, and choosing is a real decision.** Six of the nine
iterable collections have no `for` lowering at all: `for x in s` over a `Set`, `for
i in b` over a `BitSet`, `for kv in c` over a `Counter` and `for kv in m` over a
`Map` each pass `praxis check` and **kill the process**; a `MinHeap` over
`[3, 1, 2]` sums to `4349199564`, which is worse — a silently wrong answer out of a
program that reports nothing. MIR's `get_symbol_for` has arms for `Vec`, `Deque`
and `Range` and defaults everything else to `VecGet`, and the runtime has **no
indexed accessor** for the other six to select (`MapGet`/`CounterGet` are keyed
lookups; `GridGet` takes `(x, y)`). So it is not a missing match arm — it needs an
iteration protocol and a `for` lowering that uses it, which is D6's `Range` slice
again at six times the width, plus a decision (the protocol, *and* whether
`for (k, v) in m` destructures, which is REP-10's other half). `capability::iter_item`
has claimed all nine are iterable since M8 and nothing ever lowered six of them; no
test ran a `for` over one.

### REP-08, scoped against this tree

`p.0` is a `P001` "expected name after `.`" — the postfix loop at
`crates/praxis-parser/src/parse.rs:814` requires an `Ident` after `DOT`. The pieces:

1. **Lexer** — check first whether `t.0.1` lexes as `t`, `.`, `0`, `.`, `1` or as
   `t`, `.`, `0.1` (a float). `eat_number` already has the `DOT2` carve-out for
   ranges; a `.` *after* a digit run may need the same treatment.
2. **Parser** — accept an `IntLit` after `DOT` in the postfix loop.
3. **Node kind** — recommend a **new** one rather than an `IntLit` inside
   `FIELD_EXPR`. Tuple indexing is by position and the index must be a literal;
   a record field is by name. ADR-061's `TypedExpr::FnValue` is the precedent for
   "its own node, not a flag", and the reason is the same: the four exhaustive
   matches (`mono::resolve_expr`, MIR's `lower_expr_gc` and `expr_static_type`, the
   debugger's purity walk) then make the addition a compile error rather than a
   silently skipped slot.
4. **Inference** — the receiver must be a `Tuple`, the index in range, the result
   the element's type. `p.0` on a non-tuple is a **named diagnostic** per the exit
   criterion, and `Y112` ("no field on this type") is the wrong one to reuse — the
   receiver has no field *positions*, not a missing name. **`Y019` is free** in the
   `Y0xx` user block; amend ADR-051 before spending it.
5. **HIR** — `TypedExpr::FieldGet` already carries a `field_idx`, so the typed tree
   needs only the receiver's *kind* to differ.
6. **MIR + codegen** — `Inst::LoadField` hard-codes `RuntimeSymbol::RecordField` at
   `crates/praxis-codegen-cranelift/src/lower.rs:1258`. **`RuntimeSymbol::TupleGet`
   already exists** (`praxis_tuple_get(ctx, tuple, idx) -> GcRef`, `Pure`) and has
   no MIR caller today, so this is a second instruction (or a discriminated
   `LoadField`) and no new runtime work.

## Four things worth not rediscovering

1. **ADR-062's asymmetry is load-bearing, and it is not arbitrary.** A `for` over
   an unannotated parameter leaves the **iterator quantified** and **pins the
   item**. Both halves are forced by lowering: MIR picks `len`/`get` from the
   iterator's *static ctor*, so one clone per iterable kind is the only way those
   symbols can be right; and monomorphization substitutes a clone's types from the
   call site's **argument types** and never runs the constraint channel, so an item
   variable only the channel can resolve would reach MIR unbound. `pin_to_level`
   now has two callers and its doc comment says why each is there. If you touch
   either, that is the reasoning to preserve.

2. **`Iterable` is the second capability discharged by *resolving*.**
   `Inferer::resolve_deferred_iterable` sits beside `resolve_deferred_method` and
   **unifies** the item; `capability::check` still answers only the yes/no, on
   purpose — its failure shape is `Err(offending type)`, and `Vec[Text]` *is*
   iterable, so `Y005`'s wording would be a lie. A wrong element type is a `Y001`
   at the use site with the `for` as its note.

3. **ADR-051 is accurate again.** It had not been amended for `Y018` (S24) or
   `Y124` (S26 part 1); both are back-recorded, along with the new `N006`.
   **`Y019` is the next free code in the `Y0xx` user block and `N007` in `N0xx`.**
   Declaration mistakes go in the Name category — that is the ADR's own rule, and
   it is why REP-14 did not spend `Y019`.

4. **`&&` and `||` are one MIR function now** (`lower_short_circuit`), with the
   skipping side's answer flipped. If you add a third short-circuiting form, that
   is where it goes. And the operands **join** with `Bool` rather than unifying, so
   a divergent one is absorbed — without that, `false && panic("x")` reported
   "expected Never, found Bool".

## Three places the plan was wrong, and what replaced it

- **REP-07's row says "There is no `&&` or `||`."** `||` already worked, end to
  end: lexed as `PIPE2`, bound at `bp(1, 2)`, typed in `infer_bin`, and lowered by
  `lower_logical_or` with a real short circuit. The stage's REP-07 work was `&&`
  alone — plus a `Never` bug `||` had carried since it was written and that only
  `&&`'s arrival made visible.
- **S26's row for REP-14 says the defect is "the silence".** The silence is the
  visible part; the defect is that a fresh variable **unifies with everything**, so
  `struct Node { next: Node, value: Int }` accepted `Node { next: 7, value: 1 }`
  and ran it. And the row does not mention the collateral case at all: a
  declaration that merely *waited behind* a cycle had the same unchecked member,
  and it is not the mistake — the readiness loop resumes past the reported members
  now so it gets a real type.
- **REP-03's row says the symptom is "a legal program rejected".** That is one of
  two. When nothing pins the element, nothing reports: the loop variable was typed
  as the *collection*, so `fn copy(vs) { var o = Vec()\n for v in vs { o.push(v) }\n
  o }` inferred `Vec[Vec[Int]]` and **faulted at run time** out of a program
  `praxis check` accepted.

## One weak test found in passing, not fixed

`a_range_binds_looser_than_the_arithmetic_in_its_bounds` (`parse.rs`) asserts that
`1..2 == 3..4` is "two ranges, one comparison" by **counting** `RANGE_EXPR` and
`BIN_EXPR` nodes. With `..` at `bp(3, 4)` and comparison at `bp(7, 8)` the actual
parse is `(1..(2 == 3))..4`, which has the same counts. The comment claims a shape
the assertion cannot see. REP-07's new gate uses a `shape()` helper that erases
parentheses and compares whole construct lists, which is the form that can — worth
converting the older test to it when something next touches that table.
