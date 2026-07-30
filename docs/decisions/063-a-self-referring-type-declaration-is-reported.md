# ADR-063: A self-referring type declaration is reported, and a declaration behind one is not

**Date:** 2026-07-30
**Status:** Accepted — implemented
**Milestone:** Repair (stage S26 — REP-14; answers the plan's **D17**)

## Context

```praxis
struct Node { next: Node, value: Int }

fn main() -> Int {
    let n = Node { next: 7, value: 1 }
    n.value
}
```

`praxis check` was clean and `praxis run` printed `1`.

The declaration pass registers types in dependency order (ADR-052 decision 3): a
declaration is *ready* when no annotation inside it names a type still pending.
A declaration in a cycle never becomes ready, so the loop stalled and the
remainder was registered anyway — and `resolve_or_fresh` gave the recursive member
a **fresh type variable**, as every unresolvable annotation gets.

ADR-052 recorded that as a deliberate non-decision, and it was right that
*supporting* recursive types is a language feature out of the repair's scope. What
it did not see is that a fresh variable is not a neutral fallback: **a variable
unifies with everything.** So every recursive declaration had exactly one member
the type checker did not check, and `next: 7` was accepted for a field declared
`Node`. That is REP-14, and it is the defect that survives either answer to D17.

`struct Node { children: Vec[Node] }` is the same defect with a smaller hole — the
outer `Vec` shape survives and only its element is the variable, so
`children: 5` is caught and `children: some_vec_of_text` is not.

## Decision 1: report it — `N006`

D17's recommended answer, taken. A cycle member is reported at its **name**, once,
and the report supersedes ADR-052's silence:

```text
error[N006]: `Node` refers to itself, and a self-referring type is not supported
  1 | struct Node { next: Node, value: Int }
    |        ^^^^
```

```text
error[N006]: `A` refers to itself through `B`, and a self-referring type is not supported
error[N006]: `B` refers to itself through `A`, and a self-referring type is not supported
```

Naming the *other* members is what tells a mutual pair apart from two independent
self-references, and it is the whole content of the message for a three-cycle.

**`N006` is in the Name category, and ADR-051 already said where.** "Declaration
mistakes go in the Name category" is that ADR's own rule, and `N004` (duplicate
declaration) and `N005` (nested function) are the precedents: the mistake is what
was *declared*, and there is no pair of types to have failed to unify. So this
does not spend `Y019`, which stays free. ADR-051 is amended with the row.

The wording says the feature is missing, not that the values are impossible. Every
Praxis field holds a reference, so `struct Node { children: Vec[Node] }` describes
a perfectly ordinary tree and "a type cannot contain itself" would be a false
statement about the runtime. What is absent is equi- or iso-recursive types in the
type system, and saying "is not supported" is the honest form of that.

## Decision 2: the member stays a fresh variable

The declaration has been reported. Making the member an error type, or `Never`, or
refusing to register the declaration at all, would turn one report into one report
per *use* — `Y001` at every field access and every literal, about a type the user
already knows is rejected. That is the cascade `N004` deliberately avoids ("one
`N004`, no cascade of `N001`s"), and the gate asserts the count: one diagnostic for
the file above, not two.

So `Node { next: 7 }` is still accepted *after* the `N006`. The program does not
compile, which is the property that matters.

## Decision 3: a declaration that merely waits behind a cycle is not the mistake

This is the half the finding does not mention and the more consequential one.

```praxis
struct C { a: A }      // ← not recursive, and not the mistake
struct A { b: B }
struct B { a: A }
```

`C` is written above the cycle, so when the readiness loop stalled `C` was in the
remainder too, and "register what is left, in source order" gave `C.a` the same
fresh variable. `C { a: "not an A" }` compiled and ran.

Two changes together fix it:

- The stall computes which of the remaining declarations can reach **themselves**
  through the mention graph — `self_referring`, a plain depth-first reachability
  per node over a graph with one node per type declaration in the file. Only those
  are reported.
- The readiness loop **resumes** after declaring the reported members, so
  everything that was only waiting behind them is registered in dependency order
  as usual. `C.a` is a real `A`, and a `Text` in it is a `Y001`.

The graph is built only at the stall, and only for the remainder. ADR-052 rejected
"a topological sort with an explicit cycle report" because it needs an edge set and
a decision about what a cycle means; D17 is that decision, and the edge set is
needed for exactly the declarations the rounds could not order.

**The loop terminates.** At a stall, every remaining declaration mentions another
remaining one, so following mentions stays inside a finite set and must revisit a
node: there is a cycle, and at least one node reaches itself. So each pass through
the outer loop removes at least one declaration. `self_referring`'s emptiness is
still handled — by declaring the remainder and returning — so that a future change
to `mentions` cannot turn a wrong answer into a hang.

## Consequences

- **`N006` is spent; `Y019` is not.** The `Y0xx` user block is still contiguous
  through `Y018`, and the `N0xx` block through `N006`.
- **`a_type_declaration_cycle_still_analyzes` is amended, not replaced.** Its
  assertions still hold — the pass returns and the rest of the file is inferred —
  but its comment stated the defect ("registers what is left in source order,
  exactly as an unresolvable annotation has always been handled"). It now also
  asserts one `N006` per cycle member, without which it passed equally well
  against the silence.
- **TY-10's gate is untouched**, which is the exit criterion's own requirement: a
  non-recursive forward reference still resolves, in both directions, and the new
  gate re-asserts it because this is the code that could break it.
- **A field or variant named like its type is not a self-reference.** Only
  annotation tokens are in `type_refs`, so `struct Node { node: Int }` and
  `enum E { E }` are clean. That was already true of the readiness check; it is now
  load-bearing for a report as well.
- **ADR-052's cycle paragraph is amended in place** rather than left to be read as
  current. Supporting recursive types is still out of scope; the *silence* is what
  this supersedes.
- **A recursive type is now a hard error, so any corpus program that declared one
  stops compiling.** The `praxis check` sweep over `crates/praxis-cli/tests/fixtures`
  and the `tests/` corpus found none — no program in the tree declares a recursive
  type, which is unsurprising: the member was unusable.
