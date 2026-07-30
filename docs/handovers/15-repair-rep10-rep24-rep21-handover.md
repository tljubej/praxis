# Repair session handover — the register is closed

**Date:** 2026-07-30
**Tree:** `0066a6f` · **Suite:** 1458 passed, 0 failed, 38 ignored · `just ci` green

`implementation-repair-progress.md` is still the living status and this file does
not replace it — read §1 and §4 there first. This is the session-scoped note: what
landed, where to start, and the things that would otherwise have to be
rediscovered.

## What landed

| Row | Commit | ADR |
|---|---|---|
| **REP-10** (P2, S25's last scheduled row) — a record and a tuple are patterns | `ca7385e` | **069** |
| **REP-24** (P2, **new**) — a declaration's members take a line break | `33b386f` | — |
| **REP-21** (P3, the one unscheduled row) — `min=`/`max=` map updates | `8833cb7` | **070** |
| **REP-25** (P2, **new**) — a `for` binding is a pattern | `0066a6f` | — |

**Every row in the plan's §4.1 register is now done, and S25 is closed.** What is
left of the repair is **S18…S21, which were never started**; S18 is `D1`-blocked
and D1 is still open.

**Two findings were registered this session — REP-24 and REP-25** — and both
landed in it. REP-25 is `for (k, v) in m`, which ADR-066 decision 3 had deferred
to REP-10: once REP-10's grammar existed, the binding *position* was all that was
left, and it needed no new syntax.

REP-24 was found while writing REP-10's corpus program: §4.5's own
`struct Point {\n x: Int\n y: Int\n}` and §4.6's `enum Tile { Empty\n Wall\n … }`
are both `P001`, because a declaration's members had to be comma-separated. **The
defect had shaped the tests rather than being caught by them** — every declaration
in the corpus and in the suite is written on one line.

## Where to start

**D1, then S18.** The decision is `Map.get`/`Grid.find` answering `Option[V]`
versus `V`-with-`Unit`; two `#[ignore]`d tests in `praxis-runtime/src/abi.rs` name
it directly, and `min`/`max` on an empty sequence answering `0` is the same
question from the other end. S18 also owns **RT-13** (F12's runtime half), which
must land in one commit with codegen and spends the ABI bump 12 → 13 — the first
one available. Whoever spends it should add the **two missing fault kinds** at the
same time: four raises currently borrow `InvalidSize` for "an empty range"
(ADR-058, ADR-059) and "an argument this algorithm has no answer for" (ADR-060).

## Eight things worth not rediscovering

1. **The exhaustiveness checker was already ready for REP-10, and S16 said so.**
   Maranget's matrix handles a `Closed` signature with one constructor — `Bool`'s
   is one constructor larger — so records and tuples needed two `Ctor` rows and no
   new case. Their signature was `Open` *because no pattern could name them*, not
   because the checker could not see into them. If a future composite gets a
   pattern, that is the whole checker-side change.

2. **`TypedPattern::sub_patterns` is written once now, and three walks wanted it.**
   The usefulness matrix asks it twice and MIR's decision tree once, and each of
   them named `EnumVariant` by hand. That is exactly how a new composite pattern
   becomes a silent catch-all in all three at the same time — the failure mode
   HIR-06 already paid for once.

3. **A wildcard component is not read at all now.** `emit_subpattern_tests` used to
   emit the load and then jump past it, so `Some` read a payload nothing used and a
   three-field record pattern naming one field read all three. Only a `Wildcard`
   may be skipped: a `Bind` matches anything too, but it needs the value.

4. **A record pattern's head is `N001` where a variant pattern's is `Y122`, and the
   difference is real.** A variant name is ambiguous with a binding (`match x { y
   => … }`), so an unresolved one is left to the type level. `Name {` in pattern
   position is nothing else, so the ordinary name-reference path is right — and it
   is what a record *literal*'s head already does.

5. **`min=` could not be a lexer token, and the plan was right that the rule is
   contextual.** `min` and `max` are prelude helpers §3.3's own program calls
   (ADR-058), so a lexer rule claiming `min=` takes the name away. The parser
   decides it after a complete expression in statement position, and **adjacency is
   checked against the raw token stream** — `nth_kind` skips trivia, which is
   precisely the difference between `min=` and `min =`.

6. **The `=` of `min=` must not be a direct child of `PLACE_ASSIGN_STMT`.** It is
   wrapped in an `UPDATE_OP` node, or every existing walk that looks for the
   assignment operator finds the `=` and reads the update as a plain store —
   silently, which is the difference between "keep the smaller value" and
   "overwrite it". `PlaceAssignStmt::op()` answers a `PlaceAssignOp` now and **the
   token accessor was removed rather than kept beside it**; that is what turned the
   two call sites into a compile error.

7. **`min=`/`max=` needed no MIR change, and that is the shape to reuse.** They
   lower as a *non-compound* store — no read — whose `set` symbol is the update
   wrapper, and MIR already emits `set(receiver, indices…, value)`. Everything else
   (the `HasMethod` deferral, bounds, monomorphization) came from ADR-064's row
   dispatch for free. The row's receiver is `Map[K, V: Int]` because the wrappers
   compare through `int_payload`, and the **bound** — not a literal `Int` argument —
   is what pins an unresolved `Map()` instead of reporting (TY-31's rule).

8. **A `for` binding needed no new grammar and no new MIR shape.**
   `TypedExpr::For.binding` became a `TypedPattern`; a bare name is a `Bind` that
   lowers to exactly the slot it always did (same `LocalDebugKind::User`, same
   debug metadata), and `bind_components` is `emit_pattern_test` with the testing
   half removed — no branch, because a binding position is irrefutable. That last
   word is the load-bearing one: `for Some(n) in v` would silently skip every
   `None`, so a refutable pattern is `Y125` and the check recurses.

## Two things noticed and not chased

Neither is in the register, because neither was reproduced against a stated
contract.

- **An anonymous structural record has no pattern.** A `read lines(...)` yields
  `{ from: Int, to: Int, weight: Int }` and REP-10's record pattern is *nominal*:
  it names a `struct`, and `Edge { from, to }` against a structural record is a
  `Y001`. `rep21_min_max_updates.px` has a comment saying so where it would
  otherwise have used a pattern. A spelling would have to be name-less
  (`{ from, to }`), which §4.5 does not write.
- **One unreproduced test failure.** A single `just ci` run reported a failure in
  `praxis-runtime`'s lib tests with no test named in the captured output; eight
  subsequent runs of that crate (lib and full) and two full `just ci` runs were
  clean. Not chased further, and recorded here rather than dropped: if
  `praxis-runtime` fails once in a future session, this is not the first time.

## The `praxis check` sweep

Done, over `crates/praxis-cli/tests/fixtures`: `bad_byte.px`, `parse_error.px` and
`type_error.px` fail, and they are the three intentional negative fixtures. The
`tests/` corpus is executed by
`every_corpus_program_runs_and_prints_the_answer_it_documents` (REP-12) and gained
two programs this session: `rep10_record_and_tuple_patterns.px` (both composites,
with literal sub-patterns that select an arm, declarations written in §4.5's and
§4.6's own style, and a destructuring `for` header) and `rep21_min_max_updates.px`
(a relaxation that reaches every node for the first time — the shape a
read-modify-write cannot express).
