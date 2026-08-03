# ADR-107: A small `Char` is one object per code point, and there is no character literal

**Date:** 2026-08-03
**Status:** accepted — implemented
**Milestone:** post-M11 performance (handover 23, P-4 — "`Char` interning")
**Amends:** ADR-100 Decision 1's bolded "**Only `Int`.**", which named the
second scalar that passes its own argument and did not take it. Extends
ADR-011's non-moving-address argument to a fourth immortal kind, and owes an ABI
bump — v19, shared with ADR-105's, because one release is one bump (see the
version history on `RUNTIME_ABI_VERSION`).

## Context

Every `Char` in the language was a fresh 32-byte heap block, and Praxis is a
language for reading character grids.

Five call sites boxed one, through four doors:

- **`praxis_text_get`** — `t[i]`, and *also* every step of `for c in t`, because
  `iter_plan` lowers a `Text` iteration to the `praxis_text_len` /
  `praxis_text_get` pair the subscript already calls (ADR-086, §4.13). This is
  the largest one for real code: a program that walks a line of input allocates
  once per character.
- **`Rt::alloc_char`** in the parser interpreter, reached from the `char` atomic
  and from `one_of`. `read grid(char)` runs the `char` atomic **once per cell**,
  so a 140×140 AoC map allocated 19,600 `Char`s of which at most 128 had
  distinct values.
- **`checked_alloc_char`**, the one door for `praxis_alloc_char` and
  `praxis_int_to_char`.
- **`default_cell`**, a `Grid[Char]`'s NUL fill.

Handover 23 names only the two parser sites. `praxis_text_get` is the one that
was missing and is probably the largest, because it is the whole of `for c in
line` as well as every subscript.

None of it bought anything. §4.3 reserves the cure in so many words: *"This
uniform model is normative even if later optimizations intern small integers,
use tagged pointers, or eliminate allocations through escape analysis. Such
optimizations must preserve reference and aliasing semantics."* `Unit` and
`Bool` have been interned singletons since ABI v10 and small `Int`s since v15.

The condition is *preserve reference and aliasing semantics*, and for `Char`
there are none to preserve. That is ADR-100's argument, and every leg of it was
re-checked against `Char` rather than inherited:

- **There is no identity operator.** `praxis_hir`'s `BinOp` is arithmetic, the
  six comparisons and the two logical connectives; `UnaryOp` is `Neg`/`Not`.
  There is no `is`, no `===`, no `ref_eq`.
- **`==` on `Char` compares payloads.** `compare_kind` answers
  `CompareVia::Scalar(ScalarKind::Char)`, which lowers to `Inst::IntCmp` over
  extracted code points. The structural fallback, `praxis_struct_eq`, has no
  pointer fast path either — it checks descriptor identity and dispatches
  `char_equals`.
- **Keyed collections are structural.** `DynamicKey::eq` *does* open with a
  pointer comparison, and `char_equals` is a reflexive `u32 ==`, so sharing an
  object can only make that fast path fire more often, never change the answer.
- **A `Char` payload is never written after allocation.** `Inst::StoreScalar`
  has no builder site and the backend's arm for it is a documented no-op.
- **Nothing hashes an address into anything language-visible.** `impl Hash for
  GcRef` exists and has no users.

## Decision

### 1. The runtime interns `0..=127`, minted once, in the one place immortals are minted

`Immortals::new` builds a `Box<[GcRef]>` of `SMALL_CHAR_COUNT` immortal `Char`s
after the `small_ints` loop, through the same `ImmortalWitness` seal. RT-03 — an
immortal is minted exactly once, and only from `immortal.rs` — is preserved
rather than worked around (ADR-040 Decision 4): nothing else may mint one, and
`char_ref` only reads the table. `Box<[GcRef]>` and not `[GcRef; N]` for
ADR-100's reason reaching a different consumer: the parser interpreter reads the
table through a raw pointer parked in the context, and that pointer must survive
a move of the `Runtime` that owns the `Immortals`.

**The ceiling is ASCII, and the number that decides it is the rung.** A `Char`
block is `{16 + 4 = 20, align 8}`, which rounds to the 24-byte rung of ADR-103's
ladder — the same rung an `Int` takes. So:

| range | objects | resident arena |
|---|---:|---:|
| ASCII `0..=127` | 128 | **3 KiB** |
| Latin-1 `0..=255` | 256 | 6 KiB |
| BMP, minus surrogates | 63,488 | 1.45 MiB |
| every scalar value | 1,112,064 | 25.4 MiB |

3 KiB buys the whole population. Praxis reads AoC-shaped input, and a code point
a UTF-8 `Text` stores in one byte is exactly a code point ≤ 127 — a grid of
`#`/`.`, a line of digits, every letter of an English word, every character of
every program in `tests/aoc-corpus/`. A `Char` above the range is what the
allocator is for. The BMP would be 1.45 MiB of permanently resident arena on a
change whose sibling work is explicitly about the memory ceiling (handover 21
§3.6), to cover glyphs no Praxis program has yet read.

**This table was written against a 24-byte `GcHeader` and is on its second set of
numbers**, which is worth recording rather than quietly correcting. ADR-109
landed the same day and took the header to 16 bytes; every figure above moved,
and `the_table_costs_three_kibibytes_of_permanently_resident_arena` is what
caught it, because it derives the rung from `BlockLayout::of(&CHAR)` rather than
restating a constant. That is the codebase's rule about pinning derivations doing
its job across two changes that did not know about each other.

**The claim that it costs zero additional pages did not survive that**, and it is
the one conclusion here that inverted. It rested on a class-1 page holding
`(32768 - 448) / 32 = 1010` blocks, so the 1,281 interned `Int`s spilled onto a
second immortal page with 737 free. At 24-byte blocks and a 584-byte page header
a page holds 1,340, so the `Int`s now fit on **one** page with 57 free — and the
128 `Char`s force a second. One extra 32 KiB page is attributable to this change.
Total immortal pages is still three, so `Heap`'s "there are three of them" comment
is still true; but the free ride is gone, and 3 KiB of objects costs 32 KiB of
page. That is still the right trade at this range and would not be at the BMP.

**Widening past `0xD7FF` would stop being one decision.** The surrogate range
`0xD800..=0xDFFF` holds no scalar values, so a table over the BMP would carry
either a hole its index arithmetic had to know about or 2,048 immortals whose
payloads violate `Char`'s own invariant — and an immortal is never reclaimed, so
that is permanent. `0..=127` needs no second rule beside `index_of`, and
`every_interned_code_point_is_a_valid_unicode_scalar` is what makes a future
widening fail in `small_char.rs` rather than in the heap.

**`Float` and `Text` are still out**, for ADR-100's reasons unchanged:
`float_equals` is IEEE so NaN ≠ NaN, which is the reflexivity leg `DynamicKey`'s
fast path rests on; and `TextPayload::Owned(Box<str>)` is not `Copy`, so an
immortal `Text` leaks its `Box<str>` at teardown (RT-02).

### 2. There is no `GcConst::Char`, because there is no character literal

This is the whole compiler-side change: nothing. Handover 23 calls this change
"exactly the shape ADR-100 built for `Int`"; it is the runtime half of that shape
and only the runtime half. ADR-100's Decision 4 turned an
in-range `Int` *literal* into `Inst::ConstGc` — two loads instead of a call, a
guard, a pacing check and a full-frame spill. `Char` has no analogue and cannot
have one, because the language has no way to write a character.

`Lit::Char` exists in `praxis-hir`'s `Lit` and is **constructed nowhere in the
tree** — it is only ever matched, and `lower_lit_gc`'s arm for it already says
so. `tests/aoc-corpus/day04_word_search_lines.px` spells a character as
`"X"[0]`, which is `praxis_text_get`. So `GcConst` deliberately keeps its three
variants, generated code never reads the new table, and the interning lives
entirely inside the runtime.

Recorded rather than left implicit, because the natural next question on reading
Decision 1 is "where is the `ConstGc` half", and the answer is a fact about the
language rather than an omission. If character-literal syntax is ever added, a
`GcConst::Char` becomes correct on the same day and `small_char::index_of` is
already the compile-time predicate it would ask — which is why that function is
a `const fn` even though only the runtime calls it today.

### 3. Every door goes through `char_ref`, and it paces on both arms

`char_ref` is `int_ref`'s shape. All four boxing sites reach it: three directly,
and `praxis_alloc_char`/`praxis_int_to_char` through `checked_alloc_char`, whose
whole reason for existing is that a rule stated at two doors goes stale at one.
`praxis_text_get` does **not** go through `checked_alloc_char` — its `ch` is a
Rust `char`, so `ch as u32` is a valid scalar by construction and the range check
would be checking something already proved.

It takes the safepoint on the interned path too, and that is deliberate.
`TextGet`, `AllocChar` and `IntToChar` are all `AllocatesAndFaults` in the
manifest, which is generated code's contract that the call site is a GC
safepoint. A `for c in line` loop over ASCII touches nothing else that bumps the
pacing counter, and the counter is the collector's only trigger — an early return
would make such a loop run arbitrarily long with no collection at all. So the
token is minted and then explicitly dropped: `Safepoint` is `#[must_use]`, so
which of the two happened is stated rather than implied.

Every manifest row is unchanged. `AllocChar` and `IntToChar` still validate and
still allocate out of range; `TextGet` still faults on an out-of-bounds index.

`Runtime::alloc_char` consults the table too, so the host helper and the ABI
wrapper answer the same object — `Runtime::alloc_int`'s rule, and the thing that
makes "a `Char` is interned" one fact rather than two. Its validity assert stays
*in front of* the lookup: `index_of` answers "is it interned", which for a value
above the range is `None` and says nothing at all about validity.

### 4. A context field generated code never reads still owes a version bump

`RuntimeContext` gained `small_chars: *const GcRef`, appended after
`debug_values` (§11.6: append at the end, never reorder), so every
generated-code-read offset is unchanged. Its readers are `char_ref` and the
parser interpreter's `Rt`, which holds nothing but a `*mut RuntimeContext` and so
has no other way to reach the table.

The bump is owed anyway, and it is the v9 (`native_roots`) / v13
(`fault_message`) case rather than v15's: the struct's *size* changed. A host
that built a `RuntimeContext` of the previous size and handed it to this runtime
would have the parser read past the end of it. Stating which case it is matters
because v15 — the `small_ints` field this one sits beside — was the other kind,
where generated code really does emit a load of the base, and a reader who
assumes the two are alike will look for a backend change that does not exist.

## Consequences

- **The evidence is allocation counts, not seconds, and that is a gap.** ADR-100
  led with a measured seven-row table; nothing in `benchmarks/praxis/` exercises
  `Char` at all — all eight programs are `read int` plus arithmetic, and
  `bfs.px` builds `Vec[Vec[Int]]`. What is pinned instead is the count, which is
  the property this change actually has:

  | | before | after |
  |---|---:|---:|
  | `grid(char)` over a 3×3 ASCII map | 10 objects | **1** |
  | `grid(char)` over a 5×5 ASCII map | 26 objects | **1** |
  | one `Char` per distinct ASCII code point, per program | unbounded | **128, minted once** |

  The one allocation is the `Grid` itself, whose cells live in a Rust
  `Vec<GcRef>`. A wall-clock number needs a `read grid(char)` / `for c in line`
  benchmark, a `benchmarks/sizes.json` row and prose in `report.py`; that is
  deferred, and the next item to touch this should add it rather than measure
  the existing suite and find it flat.

- **`for c in text` is still O(n²), and this change does not touch it.**
  `praxis_text_get` is `s.chars().nth(idx)` and `praxis_text_len` is
  `s.chars().count()`, both linear, and `iter_plan` lowers `for c in t` to
  exactly that pair. Interning removes the allocation, not the scan, so on a
  text-iteration shape the scan will dominate whatever this saves. The parser's
  grid path has no such scan, which is why that is where the count above comes
  from. Named here so it is not discovered as a surprise by whoever measures.

- **`small_char` is a module and not two constants in `small_int`.** The two
  ranges have different consumers: `small_int` is read by three crates *and* by
  generated code, and its `SMALL_INT_MIN`/`SMALL_INT_STRIDE` exist for the
  Cranelift lowering's compile-time element offset. `small_char` has one consumer
  crate and no backend reader, so it declares no `MIN` (the payload is unsigned
  and 0 is the floor, and a constant zero only invites an unchecked
  `code - MIN`), no `STRIDE`, and is not re-exported from the crate root.

- **No test in the suite was a false pass waiting to happen, and one new one
  was.** ADR-100 had to repair four tests *before* it could be measured, because
  each detected a collection by watching the live registry shrink and an interned
  object never enters the registry. Every `Char` analogue was checked and none
  exists — `every_scalar_boxing_wrapper_paces_the_collector` uses
  text_len/vec_len/grid extents/float_to_text, and the fixed-count assertions in
  `crash_snapshot.rs`, `text.rs` and `immortal.rs` touch no `Char`s. But the new
  pacing test falls into the trap by construction, so it interleaves an
  **unpaced** allocation (`Runtime::alloc_int`, the one helper that grows the
  heap without offering a turn) to supply pressure the collector can see while
  leaving `praxis_text_get` as the only safepoint in the loop. Verified red: with
  `char_ref`'s safepoint bypassed on the interned arm, it fails.

- **Two statements in `heap.rs` survive this, and one of them only just.**
  `claim_immortal_block`'s "there are three of them after `Immortals::new`"
  counts *pages*, and 128 more class-1 blocks land on a page that already exists,
  so it is still exactly true — which is worth knowing before anyone widens the
  range past 737 more objects. `Heap::reset`'s doc enumerates what a pre-reset
  `RuntimeContext` holds and names `small_ints`; that list is now one shorter
  than it should be. `reset` has no production caller, so this is a documentation
  debt, not a bug.

- **The knob is one constant with a named arbiter.** Raising `SMALL_CHAR_MAX`
  costs 32 bytes per code point and stays free only while the table stays in
  cache. Anyone tuning it should re-run a workload rather than reason about it —
  and should read the surrogate paragraph in Decision 1 first, because past
  `0xD7FF` the change is no longer a number.
