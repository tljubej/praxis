# ADR-100: The shadow stack is one contiguous region, and the recursion limit is what keeps it in bounds

**Date:** 2026-08-02
**Status:** accepted — implemented
**Milestone:** post-M11 performance (handover 21 finding §3.3)
**Amends:** ADR-019 decisions 1 and 3, and its Consequences bullet on
prologue/epilogue overhead. ADR-012's `RootSet` seam, ADR-016's liveness pass,
ADR-033's snapshot ordering, ADR-040's pacing and MIR-16's two-spill split are
all preserved; where the change touches their edges it is recorded below.

## Context

ADR-019 made a shadow frame an **object**: a `#[repr(C)]` struct with a
`parent` pointer, a `slot_count`, and a fixed `[*mut GcHeader; MAX_SHADOW_SLOTS]`
array, `Box`ed by `praxis_push_shadow_frame` in every generated prologue and
freed by `praxis_pop_shadow_frame` in every epilogue.

`MAX_SHADOW_SLOTS` is 192, so that struct is 1552 bytes, and `ShadowFrame::new`
initialized `slots: [null_mut(); 192]` — **1536 bytes memset to null on every
Praxis call, whatever the function's real slot count**, then freed on return.
Handover 21 measured it: dropping the constant from 192 to 24, which removes
only part of the memset and nothing else, took a 5M-iteration no-op-call
benchmark from 1.43 s to 1.27 s. The full cost is ~32 ns per call, and the
memset is only the visible half — each direction was also an extern call
wrapped in `abi_guard!`, so every Praxis call carried two `catch_unwind`
landing pads, one `malloc` and one `free` that existed solely to hold roots.

ADR-019's own Consequences bullet is the sentence this ADR is answering:

> Every function now has a prologue/epilogue overhead (two extern calls + frame
> allocation). This is acceptable for a puzzle-solving language; a future
> optimization can elide frames for functions with no safepoints.

It anticipated the direction. This is that future optimization, arriving as a
**cheaper frame** rather than an elided one — which is the better trade, because
eliding depends on a whole-function property ("no safepoints") that most
interesting functions do not have, while making the frame cheap helps all of
them.

The collector side was no better. `<ShadowFrame as RootSet>::push_roots`
recursed the parent chain and called a `live_refs()` that built a fresh
`Vec<GcRef>` **per frame, on every collection**. A collection taken 8000 frames
deep was 8000 mallocs and 8000 levels of native recursion *inside* `Heap::mark`.

## Decision

**One region, not N boxes.** The `Runtime` owns a `SlotStack<*mut GcHeader>`:
a `Box<[*mut GcHeader]>` of `SHADOW_STACK_SLOTS` slots, sized once at
`Runtime::new()` and never resized, plus a separately boxed `#[repr(C)]`
`{ top, base, limit }` header. `RuntimeContext.roots: *mut ShadowFrame` becomes
`shadow: *mut SlotStackHeader<*mut GcHeader>` — same position, same width, so
every other generated-code-read offset is unchanged (§11.6) — and points at that
header for the life of the runtime.

A frame stops being an object. It is the run of slots between the `top` a
function found on entry and the `top` it left behind. The prologue loads the
header, loads `top`, zeroes exactly `slot_count` slots, stores the bumped `top`,
and keeps the old `top` as this frame's base. The epilogue stores that base back
into `top`. Both directions are a handful of instructions with **no call, no
`catch_unwind`, no `malloc`, no `free`**, and the spill's address becomes
`frame_base + slot*8` — one store with the slot index as the store's own
displacement, where ADR-019 needed an `iadd_imm_s` first because the slot array
sat at a fixed offset *inside* the frame. At `opt_level = "none"` (handover 21
§3.7) Cranelift does not fold that add into the store, so every spilled root was
paying for it.

`praxis_push_shadow_frame` and `praxis_pop_shadow_frame` are deleted from the
manifest, from `praxis_runtime::abi::address`, and from the runtime. Both matches
are exhaustive, so the removal is compiler-checked. `RUNTIME_ABI_VERSION` goes
14 → 15: a runtime of the previous version would read a stack header as a frame,
taking `top` for a `parent` pointer.

### The collector scans `[base, top)`

`impl RootSet for SlotStackHeader<*mut GcHeader>` is one linear pass over a
contiguous slice, skipping nulls. This yields **exactly** the set the chain walk
yielded: each frame occupies exactly its own `slot_count` slots and the frames
partition `[base, top)`, so the concatenation is the union of every live frame's
`slots[..slot_count]`. What it does not do is allocate a `Vec` per frame or
recurse per frame.

Slots *above* `top` may still hold pointers a popped frame wrote. That needs no
invariant to be safe: they are outside `[base, top)`, so they are never scanned,
and the next push zeroes exactly the run it claims before any safepoint can read
it.

ADR-012's stated seam — "The `RootSet` trait is the seam the M4 shadow-stack
plugs into" — is preserved verbatim. Only the implementor changed.

### Why a `Box<[T]>` and not a `Vec`

Generated code holds a frame's base pointer in a Cranelift `Variable` for the
whole duration of a call, and the header's address for the whole program. A
`Vec` that reallocated on growth would invalidate both — a use-after-free
reachable from any program that recursed deep enough to trigger the growth, and
one that would present as a corrupted root set rather than as a crash. A
`Box<[T]>` allocated at its final size cannot move.

### The depth guard survives, and moves before the push

The tempting simplification is to delete `MAX_RECURSION_DEPTH` and treat a full
shadow stack as the definition of stack overflow. **That is wrong, and the
counterexample is decisive: a function with zero `Gc` locals consumes zero
slots.** It can recurse without limit while `top` never moves. What
`MAX_RECURSION_DEPTH` bounds is the *native* stack — "chosen with headroom under
the native stack's abort threshold" — which the shadow stack knows nothing
about.

The implication runs the other way, and that is the useful one. **The depth
bound implies a shadow-stack bound.** Every prologue now enforces
`depth >= MAX_RECURSION_DEPTH` *before* it pushes anything, and every frame is
`<= MAX_SHADOW_SLOTS` wide by the `SlotCount` type, so

```text
top - base  <=  MAX_RECURSION_DEPTH * MAX_SHADOW_SLOTS       always
```

`SHADOW_STACK_SLOTS` is that product plus one frame of headroom (8001 × 192 =
1,536,192 slots = 12.29 MiB), so **shadow-stack exhaustion is unrepresentable
rather than handled**: no inline bounds check, one branch fewer in the hottest
path in the language, and a `const` block that fails the *build* rather than a
test run if the reservation is ever cut loose from the two constants. That is
ADR-040's P0-08c discipline applied to a different fact.

The guard moving before the push is not only about the sizing argument. It also
means the over-limit path pushed nothing, so it pops nothing: ADR-019's third
`emit_pop_shadow_frame` call site is **deleted**, not rewritten, and the one
`return_` in a generated function that is not preceded by an epilogue is exactly
the one path that skipped the prologue. Observable behaviour is identical —
bodies still execute at nesting levels 1..8000 and the call at 8001 still faults
`StackOverflow` — with one fewer frame ever pushed.

### Absolutes, not increments

The depth counter moves inline with the rest: one load in the entry block, a
store of `depth + 1` in the prologue, and a store of the **saved pre-bump value**
in the epilogue. Same for `top`: the epilogue restores the saved base rather
than subtracting the frame's width. Restoring an absolute cannot underflow —
the deleted pop helper needed a `saturating_sub` for exactly that reason — and
is self-healing: an imbalance introduced below this frame is corrected here
rather than propagated upward.

### Sizing, and what it actually costs

12.29 MiB is *virtual address space*. `vec![null_mut(); N].into_boxed_slice()`
hits std's `IsZero` specialization for raw pointers, so it is one `alloc_zeroed`
— an `mmap` of fresh zero pages on macOS and Linux, faulted in only as deep as
the program actually recurses. Measured max RSS on the benchmark suite is
unchanged.

## Measurements

Apple M2 Pro, release build, `/usr/bin/time -p`, three runs per configuration,
minimum reported, the two binaries run interleaved. Baseline is the tree with
handover 21 §3.1 (the free-list fix) already landed.

| | before | after | |
|---|---:|---:|---|
| 5M × no-op call | 0.71 s | 0.41 s | **−42%** |
| `tree` @ 60 | 2.13 s | 1.74 s | **−18%** |
| `primes` @ 300,000 | 0.57 s | 0.55 s | −4% |
| `collatz` @ 60,000 | 0.80 s | 0.81 s | — |
| 10M × `i = i + 1` | 0.31 s | 0.31 s | — |

The no-op-call figure is the one this change is about. Its floor (the same
program at size 0 — compile, start, everything before the loop) is under 10 ms
either way, so the 0.30 s is 5M calls' worth: **60 ns off every Praxis call**,
against the ~32 ns handover 21 attributed to the memset alone. The rest is the
two extern calls, their two `catch_unwind` landing pads, the `malloc`, the
`free`, and the address computation the spill no longer needs.

The two loops with no calls in them are the control, and neither moves.
`collatz` is flat for the same reason: its inner loop is one function's
arithmetic, and what it was paying for is what §3.1 already addressed. `tree`
and the no-op benchmark are the call-dominated ones, and they are where the win
is.

Max RSS is unchanged — `primes` 892.91 → 892.93 MiB, `tree` 1076.45 → 1076.56
MiB — which is the 12.29 MiB reservation being virtual, observed rather than
assumed.

## Consequences

- **The prologue no longer null-checks the context.** `praxis_push_shadow_frame`
  returned null for a null `ctx` and `praxis_pop_shadow_frame` returned early;
  inline code cannot do that without paying for it on every call, and does not.
  `Runtime::context()` is the only producer of a wired context and always sets
  `shadow` non-null. `RuntimeContext::placeholder` is documented as not-yet-wired
  test scaffolding, and now says explicitly that generated code must never be
  run against one. This is a real reduction in defensive behaviour and is named
  here rather than glossed. The test that asserted the old wrapper's null
  behaviour is deleted, because its subject is.
- **`Runtime::collect_now` improves.** It mints a fresh context, which under
  ADR-019 carried `roots: null` and therefore could not see the shadow chain at
  all. Every context now points at the one stack, so a collection taken from the
  host sees the frames that are actually on it.
- **The stack-overflow crash snapshot is one frame shallower.** The guard runs
  before the push, so `praxis_snapshot_debug_chain` on that path sees the
  caller's chain rather than the overflowing frame's. Same fault, same kind, one
  fewer line in a `bt`. ADR-033 decision 1 is otherwise untouched: the snapshot
  is still taken by the innermost fault epilogue, still before any pop, still
  idempotent.
- **An over-wide frame is now unconstructible rather than rejected.** `SlotCount`
  is the only way to name a frame width and cannot hold more than
  `MAX_SHADOW_SLOTS`; the backend turns the `None` into a diagnostic naming the
  function. The `#[should_panic]` test that pinned the old runtime `assert!` is
  replaced by a non-panicking type check, which is the invariant getting
  stronger, not weaker.
- **`MAX_SHADOW_SLOTS = 192` stops costing per-call time.** Under ADR-019 it was
  the width of every allocation, so it was a tax on every call in the language.
  It is now a cap on one frame's width, and its only cost is 64 KiB of
  reservation per unit of it. Keeping 192 (M8 raised it from 64 for AoC-style
  graph programs) is nearly free.
- **Two `abi_guard!` wrappers are gone.** ADR-080's property is unaffected — it
  is a property of the *set* of `#[no_mangle]` entry points, and its
  source-scanning test still passes. Inline Cranelift cannot panic, so the two
  removed guards protected nothing that still exists. One side effect: every
  `#[no_mangle]` wrapper in `praxis-runtime` is now named by the manifest, so
  the "unmanifested entry point" case in
  `a_panic_dummy_is_only_returned_where_a_fault_check_can_follow` no longer has
  a real example and is stated in prose there.
- **Nothing about *when* a collection may run changed.** ADR-040's `Safepoint`
  token, `gc_alloc`/`gc_alloc_with` as the wrappers' only route, and
  `alloc_unpaced` as the one named back door are untouched. `Heap::pace` still
  takes the whole `RuntimeRoots` and still mints the token. This is a change to
  what the collector *reads*, not to when it runs, and it should not be read as
  a pacing change.
- **The capacity corner cannot be reached by a test, and the reason is worth
  knowing.** A function wide enough to fill the reservation has a native frame
  to match, and 8000 of those exhaust the thread's stack long before the shadow
  stack's 12.29 MiB — `MAX_RECURSION_DEPTH` is a call count, not a byte budget.
  So the corner is closed by the arithmetic and the `const` block; the tests
  show the mechanism working at a width and depth a real program could reach.

## What this offers finding §3.2

The runtime type and the codegen are generic from the start, because §3.2 — the
crash debugger's per-call bookkeeping — wants the same mechanism a second time:

```rust
#[repr(C)] pub struct SlotStackHeader<T: Copy> { top: *mut T, base: *mut T, limit: *mut T }
pub struct SlotStack<T: Copy> { header: Box<SlotStackHeader<T>>, slots: Box<[T]> }
```

with `emit_slot_stack_push(builder, ctx_val, ctx_field_offset, count,
slot_bytes, cfg) -> Value` and `emit_slot_stack_pop(builder, ctx_val,
ctx_field_offset, base)` on the codegen side, both parameterised by the
`RuntimeContext` field offset. A second stack is one more field and the same two
calls.

The instantiation this recommends is `SlotStack<Option<GcRef>>` for the debug
frame's value slots: `Option<GcRef>` is one machine word by the `NonNull` niche
(F18), so a zeroed backing store *is* a run of `None`, and because `gc_slot`
already makes "a local's shadow slot index doubles as its debug-local index" the
two stacks are index-parallel by construction.

**The constraint §3.2 must respect is MIR-16.** The debug set is deliberately
over-approximate while the GC set is exact — "There used to be a single
`emit_spill` writing one root list into both frames, which is why the two frames
could not disagree, and why making the GC root set exact would have silently
emptied the debugger's view." A design that reads debug values *out of* the
shadow stack re-couples the two sets and loses locals that are dead for GC but
still interesting to a human. A parallel `SlotStack` keeps MIR-16 as written and
still deletes the debug frame's mallocs.
