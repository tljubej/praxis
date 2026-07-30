# ADR-075: The two owed fault kinds are paid, and one of the three debts is settled differently

**Date:** 2026-07-31
**Status:** Accepted — implemented
**Milestone:** Repair (stage S18)

## Context

Three S17 ADRs each wanted a new `FaultKind`, each declined to spend a second
ABI bump for it (hazard H17: one per stage), and each wrote the debt down:

- **ADR-058 Decision 4** — `clamp(v, low, high)` with `low > high` names an empty
  range, so it faults; the kind is `InvalidSize` and "a dedicated empty-range
  kind is owed to the next stage that spends a bump."
- **ADR-059 Decision 6** — `praxis_range_len` faults when the count does not fit
  an `Int`; the kind is `InvalidSize` and "a dedicated empty-range kind is owed
  to whichever stage next spends one, and **these two are its cases**."
- **ADR-060 Decision 5** — a negative edge weight and a negative heuristic fault;
  the kind is `InvalidSize` twice more, under the heading "an answer the walk
  cannot compute is a fault, not a wrong number", and "the next stage that spends
  a bump should give this family a kind of its own."

ADR-056 D2 had already argued why borrowing is bad: "a fault kind that had to
borrow another's name would be the RT-17 mistake again." S18 spends a bump for
RT-13 (ADR-074), and a `FaultKind` variant is additive within that same version.

## Decision 1: `FaultKind::EmptyRange = 14`

A range with no members was asked for a member. `clamp`'s inverted range is its
case: the inclusive range `low..=high` with `low > high` is empty, so there is no
operand to hand back and no answer that is not invented.

`InvalidSize`'s doc narrows back to what it meant before S17 borrowed it — a
negative `Grid` extent, a cell count that overflows or exceeds
`GridExtent::MAX_CELLS`, and a `BitSet` member outside `BitIndex`'s range. Those
three stay.

## Decision 2: `FaultKind::NoAnswer = 15`

An argument this algorithm has no answer for. ADR-060's two cases are its cases:
a negative edge weight, which Dijkstra and A\* cannot honour because they settle a
state the first time they pop it and never reconsider; and a negative heuristic,
which makes `f = g + h` decrease along a path.

The name is ADR-060's own heading. Neither `InvalidSize` nor `TypeMismatch` fits,
and the reason is worth writing down: the operand is a well-formed `Int`, the
graph is a well-formed graph, and the walk is a well-formed walk. What is absent
is *a correct answer this algorithm could produce*. `InvalidSize` would say the
argument was malformed, which it is not.

## Decision 3: `praxis_range_len` does **not** join `EmptyRange` — ADR-059 is overruled

This is a deliberate departure from ADR-059, recorded here rather than done
silently.

ADR-059 assigned `praxis_range_len`'s refusal to the empty-range kind, on the
grounds that it is "a range the runtime cannot honour". The trigger is that
`end - start` does not fit an `Int`, which happens for exactly the widest ranges:
`Int::MIN..Int::MAX` holds `2^64 - 1` integers.

Two reasons to put it elsewhere:

1. **The name would contradict the input.** `Int::MIN..Int::MAX` is the *fullest*
   range expressible. A fault reading "empty range" over it is a message that is
   simply false about the value that raised it, and a fault kind whose job is to
   tell the programmer what happened cannot afford that. ADR-058 chose
   `InvalidSize` for `clamp` partly because "it already means an argument the
   runtime cannot honour" — a fit argued from meaning, which is the same standard
   applied here and reaching the other conclusion.
2. **The workspace already answers this shape.** "A result with no `Int`" is
   `IntOverflow` in `praxis_int_gcd` (`gcd(Int::MIN, Int::MIN)` is `2^63`), in
   `praxis_int_lcm`, and in ADR-060's own path-cost row, whose justification
   reads "ADR-058's rule at the one place a walk does arithmetic the program did
   not write". `range.len()` is arithmetic the program did not write either.

So `praxis_range_len` raises `IntOverflow`, and `EmptyRange` is `clamp`'s alone
for now. TY-34's descending `a..b` — which D6 answers as *empty* — remains its
natural second case, exactly as ADR-058 said.

There was no test for `praxis_range_len`'s fault at all. Re-pointing a kind with
no gate under it is how a kind drifts, so the re-point comes with one, plus a
range one narrower to show the refusal is the edge and not the rule.

ADR-058, ADR-059 and ADR-060 each carry a note pointing here.

## Consequences

- **No second ABI bump.** `EmptyRange` and `NoAnswer` are appended after
  `AssertFailed = 13` and land inside v14's window, which ADR-074 opened.
  `RaisedFault` gains two consts and `Display` two arms.
- **Four raises move and two stay.** `praxis_int_clamp` → `EmptyRange`;
  `graph.rs`'s two negative-weight raises and its negative-heuristic raise →
  `NoAnswer`; `praxis_range_len` → `IntOverflow`. The `BitSet` member and `Grid`
  extent raises keep `InvalidSize`, and their assertions keep asserting it.
- **`FaultKind` is `#[repr(C)]` and read across the ABI**, so this is a layout
  fact and not only a naming one — which is precisely why it needed a bump and
  why three ADRs deferred it.
- **No new diagnostic code.** A fault kind is a runtime report, not a compile
  diagnostic; ADR-051 is unchanged.
