# ADR-040: A `Safepoint` token gates allocation, with one named unpaced route

**Date:** 2026-07-28
**Status:** Accepted
**Milestone:** Repair (foundation F7's remaining half, stage S6 — P0-08b, P0-08c, RT-03)
**Answers:** design decision D14
**Amends:** ADR-011's allocation description (`Heap::alloc` now takes a pacing
token); ADR-017's wrapper contract (a wrapper reaches the heap through
`gc_alloc`/`gc_alloc_with`, never `Heap::alloc` directly)

## Context

Fourteen `praxis_*` wrappers gc-allocated without ever calling `maybe_collect`
(P0-08b). They were not the exotic ones: `praxis_alloc_text`, the whole `.len()`
family, `Grid` extents, `Text.get`, `enum_tag` and every checked-arithmetic
wrapper. A program whose allocation pressure came from any of those — a
text-processing loop is the obvious case — could run arbitrarily long with the
collector never offered a turn, because the *other* wrappers it happened not to
call were the only ones pacing.

Adding `maybe_collect` to those fourteen closes the instance. It does not close
the class: the next wrapper someone writes has the same chance of omitting it,
and the reviewer has the same chance of not noticing. The audit's own proposal —
move `maybe_collect` inside `Heap::alloc` — cannot work, because `Heap` is
context-agnostic and has no route to the root set.

The blocker (D14) was that F7's `#[must_use] Safepoint` token, which *does*
close the class, cannot apply uniformly. `Heap::alloc*` is also the parser
interpreter's allocator, and requiring a token there would make the parser pace
— which is IPR-14, and hazard H1 forbids it until the parser's intermediates are
rooted (S20). Pacing the parser today converts a memory-growth bug into a
use-after-free.

## Decision 1: `Heap::alloc`/`alloc_with` take a `Safepoint`, minted by `Heap::pace`

```rust
#[must_use] pub struct Safepoint<'a>(PhantomData<&'a Heap>);   // private field

impl Heap {
    pub fn pace(&self, roots: &RuntimeRoots<'_>) -> Safepoint<'_>;   // performs maybe_collect
    pub fn alloc<T: Copy>(&self, _: Safepoint<'_>, …) -> GcRef;
    pub unsafe fn alloc_with(&self, _: Safepoint<'_>, …) -> GcRef;
}
```

Obtaining the token *is* the pacing, and `pace` is its only producer because the
tuple field is private to `heap.rs`. `pace` in turn takes a `RuntimeRoots`,
which is constructible only from a live `RuntimeContext` and is exhaustive over
the runtime's owners (ADR-039's companion, P0-06) — so an allocation on this
path has both given the collector its chance *and* given it the whole root set.

The token is deliberately neither `Copy` nor `Clone`: one token, one allocation.
A wrapper that allocates twice paces twice.

**Deviation from the F7 sketch:** the plan puts the producer on
`RuntimeRoots::pace()`. Putting it on `Heap::pace(&roots)` is the same invariant
with a tighter constructor — `RuntimeRoots` lives in `roots.rs`, so a producer
there would need a `pub(crate)` constructor for `Safepoint` and any module could
then mint one.

## Decision 2: `gc_alloc` / `gc_alloc_with` are the wrappers' only route

`abi.rs` grew two helpers that pace and allocate in one step, and every one of
the ~50 allocating wrappers now calls them. The token means even a wrapper that
reached `Heap::alloc` directly would be correct — it would have to come through
`pace` to get the argument — so the helpers are for brevity, not for safety.

## Decision 3: `alloc_unpaced` is the named back door, and it is `pub(crate)`

Two callers legitimately cannot pace, and they are the only ones:

* **the host's `Runtime::alloc_*` helpers.** The host holds results in Rust
  locals no root set can see, so a collection *there* would reclaim the value
  being returned.
* **the parser interpreter.** Its intermediates are unrooted; H1. IPR-14 (S20)
  gives them `NativeScope`s and moves `parser.rs` to the paced path in the same
  commit that adds its safepoints, at which point this route loses its second
  caller.

This is option 2 of the three D14 offered, chosen over option 1 (land IPR-14
early) because IPR-14 is a careful rewrite across 1,600 lines of recursive
interpreter and does not belong in a stage about pacing, and over option 3
(defer the token) because that closes the instance and leaves the class open.
The honest cost is that the escape hatch exists and Rust cannot restrict it to
two modules; the mitigation is that it is `pub(crate)`, named for what it does,
and documents its two callers at the definition.

## Decision 4: an immortal is minted only by `Immortals::new`

`Heap::alloc_immortal` now takes an `ImmortalWitness` whose field is private to
`immortal.rs`. Twenty-four wrappers were minting a fresh immortal per call — a
`Bool` per comparison, per `contains`, per `is_empty` — which is unregistered
arena storage no collection can ever reclaim (RT-03, of which only
`praxis_alloc_bool`/`praxis_alloc_unit` had been fixed). They now read
`ctx.true_ref`/`ctx.false_ref`.

The restriction is load-bearing beyond the leak: an immortal is invisible to
sweep *and* to `Heap`'s `Drop`, so every immortal payload must be `Copy` and
must be allocated exactly once at startup. Confining the constructor is what
keeps that argument true as `Drop` lands (RT-02).

## Consequences

* "Allocate without pacing" is unwritable on the paced path, and *is* writable —
  once, by name — on the two paths that must not pace.
* The `Effect::Pure` rows for the predicate wrappers became honest: they now
  really do allocate nothing, so their call sites really are not safepoints.
* Collection now happens at strictly more points than before. Any test that
  allocated through `Text`, `.len()` or checked arithmetic and assumed the heap
  only grew is now wrong.
* P0-08c is discharged as a `const` block over `RuntimeSymbol::ALL` in
  `praxis-stdlib`: a classification error fails the build rather than a test
  run. `MethodEntry.allocates` was already deleted in favour of the manifest.
