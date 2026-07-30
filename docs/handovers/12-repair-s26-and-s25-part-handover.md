# Repair session handover — S26 closed, and S25 half done

**Date:** 2026-07-30
**Tree:** `bb3bc43` · **Suite:** 1409 passed, 0 failed, 38 ignored · `just ci` green

`implementation-repair-progress.md` is still the living status and this file does
not replace it — read §1 and §4 there first. This is the session-scoped note: what
landed, where to start, and the things that would otherwise have to be
rediscovered.

## What landed

| Stage | Rows | Commits |
|---|---|---|
| **S26** — declaration, pattern and inference gaps | REP-03 + REP-04, REP-14 (closing the stage) | `b3bb6f5`, `4b7d763`, docs `fd4ba67` |
| **S25** — grammar completion | REP-07, REP-08 | `c74e062`, `bb3bc43` |

**S26 is closed and D17 is answered**, so **D16 is the only repair decision still
open**. Two new ADRs: **062** (an iterated parameter is generic in the iterable and
monomorphic in its element) and **063** (a self-referring type declaration is
reported).

**Three new findings, one of them a P0.** **REP-15** (six of the nine iterables
have no `for` lowering) is the P0. **REP-16** (there is no `m[key]` syntax at all)
and **REP-17** (a trailing comma in an argument list is parsed as an extra
argument) came from measuring S25's own acceptance criterion, and they change it:
**§3.3's representative program needs both**, so S25's exit list is six rows and
not four. All three are registered in the plan's §4.1 and are `unscheduled`.

Four of §4.1's seventeen rows remain in S25 — **REP-09, REP-10, REP-16, REP-17** —
plus REP-15.

## Where to start

**REP-17, then REP-16, then REP-09** — that is what §3.3's representative program
still needs, measured directly against this tree rather than taken from the plan.
REP-17 is one `if` in `parse_arg_list` and is the cheapest thing left in the
repair; **REP-16 is the big one** and is scoped below. The program's other halves
already work: `read lines(…)` with record captures, `segment.x2`, `sign`/`abs`/
`max`, `0..=distance`, the tuple literal, `continue`, `!diagonals && dx != 0 && dy
!= 0`, and `counts.values().count(|n| n >= 2)`.

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

### REP-16, scoped against this tree

`m[key]` is a `P001` at the `[` and `counts[key] += 1` a `P002`: the postfix loop
in `crates/praxis-parser/src/parse.rs` has arms for `L_PAREN` and `DOT` and none
for `L_BRACK`. Four pieces, and a decision:

1. **The read form.** §4.7 is explicit about the semantics and they are not
   `.get`'s: "Indexing a missing map key **faults** instead of returning an
   option… the user chooses between explicit absence with `.get` and
   assertion-like access with indexing." So it is a distinct runtime call, not
   sugar for `get`.
2. **The compound-assign form.** `counts[key] += 1` is what §3.3 writes, and §6.2
   adds "`Counter[T]` behaves as a map whose absent values read as zero" — so the
   `Counter` case is a read-or-zero plus a store, not a fault.
3. **An lvalue in the assignment grammar.** Assignment is statement-level
   (`is_assignment_op`), and its target is a name today. A subscript target is the
   first thing that is not.
4. **A per-collection lowering**, as `for` has: `Vec`, `Deque`, `Map`, `Counter`,
   `Grid` and `Text` all index, and each has its own symbol.

**The decision is how far to go.** §6.2 also writes `distance[key] min= candidate`
and `best[key] max= score` — two assignment operators that do not exist — and says
"for `min=` and `max=`, an absent entry accepts the first value". The read and `+=`
are what §3.3 needs; `min=`/`max=` are §6.2's and can be their own row.

### REP-08, as landed (kept for the reasoning, not as work)

The scoping guess above it was right about the node kind and wrong to treat the
lexer as an open question — it is the load-bearing half. **A digit run whose
immediately preceding *token* is a bare `DOT` is an index and takes no fraction**,
or `t.0.1` folds its `0.1` into one `FloatLit` and a nested tuple stays unreadable
even though the parser accepts `p.0`. It must be the token and not the source byte:
in `1.5..2.5` the byte before `2` is a `.` too, and that one was consumed into a
`DOT2`.

It is its own node at every level — `TUPLE_INDEX_EXPR`, `Expr::TupleIndex`,
`TypedExpr::TupleIndex`, `Inst::LoadTupleElem` — and adding the variant sent the
compiler to **seven** exhaustive walks, not the four the guess listed: F20's child
walker and MIR's `verify` and `liveness` were the extra three.
`RuntimeSymbol::TupleGet` did already exist with no MIR caller, so there was no new
runtime code.

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
   `Y124` (S26 part 1); both are back-recorded, along with `N006` (REP-14) and
   `Y019` (REP-08). **`Y020` is the next free code in the `Y0xx` user block and
   `N007` in `N0xx`.** Declaration mistakes go in the Name category — that is the
   ADR's own rule, and it is why REP-14 did not spend a `Y0xx` code.
   **A report a stage wants `praxis check` to see must be emitted in inference**:
   `check` does not run lowering, so `Y112`'s emitter is clean under `check` and
   fails under `run`, which is REP-12's asymmetry. `Y018` and `Y019` are both in
   inference for that reason.

4. **`&&` and `||` are one MIR function now** (`lower_short_circuit`), with the
   skipping side's answer flipped. If you add a third short-circuiting form, that
   is where it goes. And the operands **join** with `Bool` rather than unifying, so
   a divergent one is absorbed — without that, `false && panic("x")` reported
   "expected Never, found Bool".

## Four places the plan was wrong, and what replaced it

- **S25's ordering note says REP-07 + REP-08 + REP-09 are what stand between
  §3.3's representative program and the compiler.** Measured at `bb3bc43`, with
  REP-07 and REP-08 landed, it also needs **REP-16** (`counts[point] += 1` — there
  is no subscript syntax at all) and **REP-17** (the trailing comma in its
  three-line `max(…)` call is parsed as a third argument). Both are now in §4.1.
  This is the stage's *acceptance criterion*, so the correction is not cosmetic —
  it is two more rows before the stage can close.

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
