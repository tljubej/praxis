# ADR-053: A `loop` is the value its `break`s carry, and it is the only loop that has one

**Date:** 2026-07-29
**Status:** Accepted — both halves implemented
**Milestone:** Repair (stage S14 — TY-21, which closes the stage)
**Answers:** the plan's **D2**

## Context

`loop { break 42 }` was `Unit`. Inference gave every `loop` the unit type without
looking at its `break`s, HIR lowering hard-coded `ty: self.unit`, and the MIR
builder lowered a `break` value *for effect* and then yielded a `Unit` literal —
three passes agreeing on an answer none of them had computed. `break 42` was
accepted and its value discarded at every level.

Fixing it needs two decisions the audit does not make, which the plan raises as
**D2**:

1. **What is a `loop` no `break` leaves?** `loop { }` runs forever; a loop exited
   only by `return` leaves the function rather than the loop. Neither produces a
   value, and the type has to say something about that.
2. **May a `while` or a `for` carry a value out?** A `while` leaves when its
   condition fails and a `for` when its sequence runs out. On those paths there
   is no `break` and therefore no value.

TY-19 landed the machinery both answers need: `Never` is a real type
(`TypeData::Never`), and `TypeDb::join` absorbs it wherever two branches meet.
`join_all` already seeds with `Never`.

## Decision 1: a `loop` is the join of its `break` values, and `Never` when it has none

`Inferer::loops` is a stack of loop contexts, innermost last. Each frame carries
a **flavour** and the **join of every `break` value seen so far**, seeded with
`Never`. `infer_break` joins its value into the top frame; `infer_loop` pops the
frame and answers with what it accumulated.

- `loop { break 42 }` is `Int`.
- `loop { break }` is `Unit`: a bare `break` leaves the loop with nothing, so it
  contributes `Unit` — which makes mixing `break` and `break 1` in one loop the
  `Y001` it should be, rather than a coincidence that happens to work.
- `loop { }` is **`Never`**, not `Unit`. It produces no value, which is the
  bottom type's whole job, and it is what makes `if c { 1 } else { loop { } }` an
  `Int` rather than a mismatch. It agrees with `panic()`, which is already
  `Never`, and with a loop exited only by `return`.

The alternative — `Unit` for an unbroken loop — was rejected because it is a
lie with consequences: `Unit` is a value, so a diverging loop would have to
*disagree* with every branch it sits beside, and the very absorption TY-19 landed
would not apply to the one construct that most obviously never returns.

A loop's **body** type is discarded, as it always was. A loop repeats its body;
it does not produce it.

## Decision 2: only a `loop` is an expression loop

A `break` carrying a value inside a `while` or a `for` is **`Y017`**.

Those two loops have a `Unit`-valued exit path the compiler cannot fill: nothing
in `while c { break 1 }` says what the loop produces when `c` is false. The
alternatives are all worse than rejecting it — invent a value (there is none to
invent), make the type `Option[T]` (a language change, and a surprising one), or
give the non-`break` exit `Unit` and join (which makes every such loop a
mismatch reported at a confusing place). The plan recommends the rejection; this
takes it.

This is why the loop stack carries a flavour rather than a depth: the report has
to distinguish "there is no loop here" (`Y012`, TY-20) from "this loop cannot
carry a value", and the check must be on what the loop *is*, not on the keyword
text at the `break`.

`Y017` is an amendment to ADR-051, which listed TY-21 among the findings needing
no code.

## Consequences

- **Three passes now compute the same join, and each has to.** Inference reports;
  the HIR lowerer records the answer on `TypedExpr::Loop.ty` (reading the body's
  type would answer what the loop repeats); the MIR builder gives a
  value-producing loop a **result slot** every `break` writes before jumping,
  exactly as `lower_if` gives an `if` one its branches write. This is the same
  shape TY-19 needed in `lower_if`, and the same reason: inference agreeing is
  not enough, because the typed tree is what the backend reads. F15's per-node
  type map (S15) is what eventually collapses the three into one.
- **A `Never`-valued loop gets no result slot.** `Never` has no runtime
  representation, so a local of that type would fail the compile at its
  descriptor site (D9/ADR-042). The exit block of such a loop is unreachable, so
  there is nothing to hold.
- **Both loop boundaries are function boundaries.** A closure clears the loop
  stack and restores it, in inference and in lowering alike: a `break` inside a
  closure belongs to no loop outside it (TY-20 makes it `Y012`), and it must not
  contribute a value to one either.
- **The MIR builder no longer tolerates a missing loop context.** `lower_break`,
  `lower_continue` and the fused pipeline's `break_loop`/`continue_loop` read the
  stack with an `expect`: inference reports `Y012` and no MIR is built for a
  program that has one. That is the plan's fourth S14 exit criterion.
- **`while` and `for` still get a frame** they then discard, so a `break` inside
  one is never attributed to a `loop` further out.
- **Nothing in the corpus changes.** Every `.px` under `tests/` and every CLI
  fixture still reports exactly what it is meant to.
