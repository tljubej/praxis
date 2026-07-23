# ADR-013: Scalar + Vec[T] descriptors in M3; other collection descriptors in M5

**Date:** 2026-07-23 · **Status:** accepted

## Context

The M3 deliverables (§19) name descriptors for `Unit`, `Bool`, `Int`, `Byte`,
`Char`, and `Text`. §11.2 maps the collection types to Rust collections
(`Vec[T] -> Vec<GcRef>`, etc.), and their *method surface* (`push`, `get`, …)
is an M5 deliverable. M3's acceptance criterion "GC stress tests preserve
nested references" requires *some* composite type whose `trace` callback
forwards nested `GcRef`s, otherwise the headline collector test cannot be
exercised — scalars are all leaves (their `trace` is a no-op).

Separately, §4.3 allows `Text` to be either an owned UTF-8 payload or a
"source-slice metadata" (§12.6: `owner: GcRef, start, length`). Source slices
are produced by the input-parser, an M6 deliverable.

## Decision

- Implement descriptors for the **six named scalars** (Unit, Bool, Int, Byte,
  Char, Text) plus a **minimal `Vec[T]` descriptor** that stores
  `Vec<GcRef>` and forwards nested references through `trace`. `Vec[T]` gets
  `alloc`/`trace`/`drop_value`/`format`/`equals` but **no** method runtime
  wrappers (`push`/`get`/…) — those are M5.
- Defer `Deque`, `MinHeap`/`MaxHeap`, `Grid`, `Map`, `Set`, `Counter`
  descriptors to M5.
- Ship **owned `Text` only** in M3 (`Box<str>` payload). `TextSlice` lands in
  M6 when the input-parser produces source slices.

## Reason

- `Vec[T]` is the smallest composite that proves the nested-tracing path the
  GC stress test targets, without building M5's full method surface.
- Owned-only `Text` keeps M3's Text `trace` a no-op (same as scalars); the
  nested-reference guarantee is instead proven by `Vec[T]` (and `Vec` of
  `Vec`). Splitting the slice layout out avoids designing the input-parser
  ABI prematurely.
- Rule 20.2 (vertical slices): each descriptor lands with executable behavior
  (allocate, trace, format, collect).

## Consequences

- M3 cannot construct a source-slice `Text` that traces an owner; this is
  fine because `Vec[T]` covers the nested-trace test. When M6 adds slices,
  the `TEXT` descriptor either splits per-layout or grows a discriminant — a
  decision for M6, recorded here so it is not a silent deviation.
- `Vec[T]`'s payload records its element descriptor (§11.2: "the static type
  `T` … recorded in the collection object's type descriptor"), so `format`
  and `equals` dispatch element-wise without a type switch.
