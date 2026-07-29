# ADR-047: A scheme owns its binders, a level can only be lowered, and a declaration group predeclares its signatures

**Date:** 2026-07-29
**Status:** Accepted
**Milestone:** Repair (stage S11 — F10 in part, TY-01, TY-03, TY-22)
**Amends:** ADR-008's account of generalization (the arena no longer records
quantification); the `VarState` lifecycle in ADR-007

## Context

Three defects in the inference core turned out to be one shape: **a fact was
recorded somewhere that could not hold it.**

**TY-03 — quantification was recorded twice.** A `Scheme` carried a
`quantified: Vec<VarId>`, *and* `generalize` flipped each of those variables'
arena slots to `VarState::Generalized`. The arena is global; a scheme is not. So
`Scheme::monotype(t)` — which has no binders by construction — could have its
body's variables flipped to `Generalized` by a *later* `generalize` of an
unrelated binding. The monotype then contained a variable it did not list,
nothing would substitute it, and `unify` had no arm that could link it: the
monotype silently stopped unifying with anything.

**TY-01 — the level-lowering comparison was reversed.** ADR-008's rule is
Pottier's `level(w) := min(level(w), level(v))`: when a variable is linked to a
type, every variable *inside* that type is pulled out to the linked variable's
level, so an inner generalization cannot quantify something the outer
environment still reaches. The code read

```rust
if level < self.min_level { /* write min_level back */ }
```

which **raised** older variables into the inner scope instead of lowering inner
ones out of it — the exact soundness hole the rule exists to close.

**TY-22 — inference had no forward declarations.** Name resolution has been
two-pass since M2, so `fn first() { later("wrong") }` *resolves* `later` even
when it is declared below. Inference minted a function's signature placeholder
only when it reached the declaration, so at the call site the symbol had no
scheme and the call was simply not checked. A forward call could pass any
arguments.

The three are coupled. TY-01's own analysis says so, and the coupling is
mechanical: correcting `lower_levels` makes the recursion placeholder — minted
at the *outer* level and then unified with the derived function type — clamp
every parameter and result out to level zero, so no signature can generalize.
Fixing the comparison alone turns every function in the language monomorphic.

## Decision 1: `Level` is a newtype whose only mutator is `clamp_to`

```rust
pub struct Level(u32);
impl Level {
    pub const OUTERMOST: Level;
    pub const fn deeper(self) -> Level;
    pub fn clamp_to(&mut self, outer: Level);      // self.0 = min(self.0, outer.0)
    pub const fn is_deeper_than(self, site: Level) -> bool;
}
```

The bare `u32` was compared by hand at each of its three uses and one was
backwards. `clamp_to` is monotone-decreasing by construction, so the reversed
form is not merely fixed — it is unwritable. `is_deeper_than` names the
generalization test so the other comparison reads as what it means.

## Decision 2: `VarState::Generalized` is deleted; a scheme owns its binders

`VarState` is `Unbound { level } | Linked { target }`. There is no third state,
because "this variable is quantified by some scheme" is not a fact about the
arena — it is a fact about a scheme, and only the scheme that quantifies it
knows.

`Scheme`'s fields are private (`binders`, `body`), reached through
`binders()` / `body()`. `generalize` **mutates nothing**: it collects the
variables deeper than the binding site and returns them with the body it was
given. `instantiate` substitutes by **binder membership** alone; it used to also
require the arena to say `Generalized`, which is a fact about *some* scheme
rather than this one, so a variable this scheme bound could be skipped because
another had not marked it.

Two consequences fall out:

- `instantiate_with_mapping` is now trivial to offer (the fresh variable per
  binder, in binder order) — MONO-01 needs exactly that, and it was previously
  hidden inside the walk.
- Rendering needs the binder set. `render_scheme` passes it down, so a variable
  the scheme quantifies prints `T` and one it does not prints `?T`. `render` on
  a bare type has no scheme, so every unbound variable prints `?T` — which is
  the honest answer, and the same one it gave before for anything not yet
  generalized. `render_in_scheme` is the third case, for a sub-type of a known
  scheme.

## Decision 3: the signature placeholder lives at the declaration-group level

`infer_declaration_group` runs in two phases:

1. Enter one level for the group. Mint a monomorphic signature placeholder for
   **every** `fn` in it and attach `Scheme::monotype(placeholder)` to its
   symbol.
2. Infer every statement in source order. Each `fn` body is inferred a level
   deeper, unified with its placeholder, and generalized at the level the group
   was entered *from* (`TypeDb::generalize_at`) — the group's own level is still
   open for the signatures declared after it.

A forward call now unifies against the very variable the later declaration will
resolve, so a disagreement is a diagnostic (TY-22). And the placeholder is
deeper than the generalization site, so the corrected `lower_levels` clamps a
function's parameters to the placeholder's level rather than to level zero, and
signatures still generalize (TY-01).

A function's scheme is replaced by the generalized one as soon as its *own*
declaration is inferred, so uses after the declaration are polymorphic — which
is what keeps `fn id(x) { x }` usable at `Int` and `Text` in the same program.

## Consequences

**Mutual recursion is checked but not yet properly generalized.** `fn a() { b() }`
above `fn b() { a() }` now unifies both directions against real placeholders,
which is strictly better than the previous silence. But `a` generalizes before
`b`'s body has constrained the shared variables, so a genuinely mutually
recursive pair can generalize too early. Doing this correctly needs
dependency-ordered binding groups (SCCs over the call graph), which is F19's
`DeclGroup` driver in S13. This ADR moves the placeholder to the group level;
F19 decides what the groups *are*.

**Top-level statements are inferred one level deeper than before.** Levels are
only ever compared relatively, so nothing observable changed — but a future
reader comparing absolute levels against ADR-008 should know the root is now the
group level, not level zero.

**F10 landed in part.** The constraint channel (`Capability`, `Constraint`,
`TypeDb::take_dischargeable`, and the single `capability::check`) is the other
half of F10 and did **not** land: its only consumers are S17's TY-25…TY-34 and
RT-08, and unused surface is what this repair has consistently declined to add.
`Level` and scheme-owned binders are the half S11 needs, and they are what F10's
`ORDER` note calls the prerequisite for TY-01, TY-03 and TY-22.

## Alternatives considered

**Keep `Generalized` as a guard against unifying a scheme body directly.** It
did have that effect: a quantified variable could not be linked, because `unify`
had no arm for it. Rejected — that protection was a side effect of the bug, and
the level discipline is the real guarantee: a quantified variable is deeper than
any live environment binding, so nothing in scope can name it. TY-01 is what
makes that guarantee true.

**Generalize the whole declaration group at group exit** (the textbook
treatment). Rejected for S11: it makes every function's uses monomorphic *within
the group*, so a program that calls `id` at two types in `main` stops compiling.
The right fix is per-SCC groups, which is F19/S13.

**Give `Level` a general setter and fix `lower_levels` separately.** Rejected:
the newtype's entire value is that the reversed comparison cannot be written,
and a setter is that comparison with extra steps.
