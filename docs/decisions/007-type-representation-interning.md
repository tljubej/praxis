# ADR-007: Type representation is an interned arena

**Date:** 2026-07-23 · **Status:** accepted

## Context

Milestone 2 builds an HM-style inference engine (§5). The representation of
types has to support unification (linking type variables), generalization, and
cheap copying (every expression and binding carries a type). The obvious
textbook representation is `Rc<RefCell<TypeEnum>>`, but that threads shared
mutability through the whole inference engine and makes the *lifecycle state*
of a variable (fresh / linked / generalized) an implicit convention.

## Decision

Represent types as an **interned arena** (`TypeDb`): every `Type` is a copyable
`u32` handle into a `Vec<Slot>`. Type variables live *in* the arena as `Slot`s,
so unification links them by mutating a slot. A `VarState` enum (`Unbound` /
`Linked` / `Generalized`) makes each phase representable rather than signaled by
convention — directly serving the workspace rule "make illegal states
unrepresentable." This is the chalk/rustc idiom.

## Reason

- `Type` is `Copy`, so bindings/expressions/constraints carry types by value —
  no refcounting, no borrow-checker fights across the inference walk.
- Variable lifecycle is explicit: a `Generalized` slot cannot be unified, a
  `Linked` slot is followed via `prune`, an `Unbound` slot carries its binding
  level.
- The arena is the single mutable focal point; the rest of inference takes
  `&mut TypeDb`, which is easier to reason about than scattered `Rc<RefCell<>>`.

## Consequences

- A `Type` is only meaningful inside the `TypeDb` that minted it. `Analysis`
  carries the `TypeDb` alongside the symbols/refs so consumers (hover,
  diagnostics) can render types. This is a deliberate coupling, not a leak.
- `praxis-types` reuses `ScalarType`/`CollectionCtor` from `praxis-stdlib`
  (rule 20.3) and adds the inference layer on top; `Unit` is its own variant,
  mirroring how `TypePattern` splits `Unit` from `Scalar`.
- A distinct name (`Type`) from the runtime's `TypeId` (descriptor id) keeps the
  static and runtime type identities from being confused.
