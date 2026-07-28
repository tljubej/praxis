# ADR-045: Ordering is scalar, total in containers, and IEEE in the source language

**Date:** 2026-07-29
**Status:** Accepted
**Milestone:** Repair (stage S10 — P0-12)
**Answers:** design decision **D3** (NaN ordering; whether composites are
orderable)
**Amends:** ADR-026's "scalars and ordering ops stay [native]" sentence;
ADR-037's Float comparison note; §5.4 `SupportsOrd`

## Context

`TypeDescriptor::compare` has existed since S1 with the shape of the answer and
none of its content: `None` on all twenty-one descriptors, because two questions
were open (the plan's D3).

Meanwhile every ordering in the implementation read a payload as an `i64`:

- `<` on `Text` loaded the first eight bytes of a `TextPayload` — a `Box<str>`
  fat-pointer half, or an enum discriminant — and compared *addresses*.
- `<` on `Char` loaded eight bytes from a four-byte, four-byte-aligned payload:
  a misaligned read four bytes past the object.
- `HeapEntry::cmp` read every element as `i64`, so a `MinHeap[Float]` ordered
  `-2.0` *after* `-1.0` (sign-magnitude bit patterns compare backwards for
  negatives).
- `Map`/`Set`/`Counter` formatting had no order to sort by at all, which is why
  RT-16 sorts *rendered strings* and prints `10` before `9`.

And nothing checked orderability: `supports_ord` was written in M5 and never
called, so `true < false` and `(1, 2) < (1, 3)` compiled and produced whatever
the `i64` read happened to say.

## Decision 1: only scalars are orderable, and `Bool`/`Unit` are not among them

`Int`, `Byte`, `Char`, `Float` and `Text` have an ordering. Nothing else does:
tuples, records, enums, collections and functions are **rejected at compile
time** with `Y006` ("values of type `T` cannot be ordered").

`supports_ord` used to answer *yes* for a tuple of orderable elements, a
collection of them, and a record whose fields were all orderable. That is a
defensible language design — lexicographic products are conventional — but it
was never a *lowering*: MIR had one integer compare and no structural one, so
the type checker's optimism was the enabling half of P0-12. Between admitting
composites with a semantics nobody had chosen and rejecting them until someone
does, this stage rejects. `v.sort()` on a `Vec[(Int, Int)]` is a diagnostic
today and a design decision (with a `praxis_value_cmp` that recurses) whenever a
milestone wants it.

`Bool` and `Unit` stay non-orderable, unchanged from M5: no total order over
them is defined, and `a < b` on booleans is nearly always a mistyped `&&`.

## Decision 2: NaN sorts last inside containers, and `<` keeps IEEE semantics

These are two different operations and this ADR keeps them apart.

**The source-level operators** `<`, `>`, `<=`, `>=` on `Float` are IEEE-754
(§4.12, ADR-037): NaN is unordered, so all four are `false` when either operand
is NaN, exactly as `==` is. This does not change — it is `Inst::FloatCmp`, one
Cranelift `fcmp`, and it agrees with every other language a Praxis user has met.

**The descriptor's `compare` callback** is the ordering a *container* imposes —
a heap's `Ord`, a sort, a deterministic rendering. A container needs a **total**
order: `BinaryHeap` with an inconsistent `Ord` does not merely order oddly, it
breaks its own sift invariants. So `FLOAT.compare`:

- orders finite values numerically;
- treats `-0.0` and `+0.0` as **equal**, agreeing with `FLOAT.equals`;
- places **NaN greater than everything, and equal to itself**.

Placing NaN last is the choice a total order has to make somewhere, and last is
where the reader expects the junk. The one incoherence is deliberate and is the
only one: `compare` says `NaN == NaN` where `equals` says it does not. A `Float`
NaN in a `MinHeap` therefore sinks to the bottom instead of poisoning the heap's
structure; a NaN compared with `<` in source is still `false`.

Rust's `f64::total_cmp` was the alternative and is rejected: it splits `-0.0`
from `+0.0`, which would disagree with `equals` for *ordinary* values, not just
for NaN.

`Text` compares lexicographically by UTF-8 bytes, which for UTF-8 is exactly
code-point order. `Char` compares by Unicode scalar value. `Int` is signed,
`Byte` unsigned. No callback reinterprets a payload as anything but its own
type — that is the whole finding.

## Decision 3: descriptor identity gates every callback dispatch

`praxis_value_cmp(ctx, a, b)` and `praxis_struct_eq(ctx, a, b)` both read `a`'s
descriptor *and* `b`'s, and require pointer identity (ADR-038) before calling a
callback. A mismatch raises `FaultKind::TypeMismatch` for the comparison and
answers "not equal" for equality; neither runs a callback on a foreign layout.

This is RT-09's rule (`DynamicKey`, S7) applied to the other two dispatch sites.
Well-typed code cannot reach it — the operands have been unified — so it costs a
pointer compare and buys the property that a *miscompile* is a fault rather than
a type confusion.

`HeapEntry::cmp` does the same and, having no fault channel, answers `Equal`
when the two entries' descriptors differ or the descriptor has no `compare`. A
heap is homogeneous, so all-`Equal` is a consistent (if useless) total order:
the heap degrades to a bag rather than corrupting itself.

## Consequences

- `TypeDescriptor::compare` is populated on five descriptors and stays `None` on
  the other sixteen. `is_orderable()` is now a real question with a real answer.
- Ordering a `Text` is a runtime call (`praxis_value_cmp`), not an inline
  compare. Ordering an `Int`, `Char` or `Float` is still a scalar instruction —
  `Char` now through `ScalarKind::Char` (`praxis_char_load`), which is what
  removes the misaligned eight-byte read.
- `Map`/`Set`/`Counter` formatting can now sort by key rather than by rendered
  string. It deliberately does **not** yet: RT-16 shipped the determinism and
  `maps::write_sorted` is one function, but changing what `{10: a, 9: b}` prints
  is a user-visible output change that belongs with the sort/`Ordered` work, not
  with a bug-fix stage. The debt is recorded here rather than in a comment.
- `numeric_scalars_are_orderable` (capability.rs) keeps its meaning; the two
  tuple assertions beside it invert, and say why in place.
