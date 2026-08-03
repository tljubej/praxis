# ADR-102: A check is a branch, not a call — and the check itself stays

**Date:** 2026-08-02
**Status:** accepted — implemented
**Milestone:** post-M11 performance (handover 21 finding §3.4)
**Cashes in:** ADR-017's Consequences bullet, verbatim. Amends the
`raise_if_nonzero` comment and `praxis_raise_int_overflow_if`'s doc, both of
which stated the opposite trade. ADR-080's guard discipline, ADR-088's
observation rule, ADR-039's layout authority and ADR-044 decision 6 are
preserved; where the change touches their edges it is recorded below.

## Context

The arithmetic core is already right: `Inst::IntBinOp` emits an inline `iadd`
and an inline overflow predicate (§4.12). Everything *around* it was an
out-of-line call into Rust, each callee wrapped in `std::panic::catch_unwind` by
`abi_guard!`. Per binary operation on `Int`, generated code emitted:

- two `praxis_int_load` calls, to read the two operands' payloads;
- one **unconditional** `praxis_raise_int_overflow_if` call, whose argument was
  the predicate and which did nothing on the overwhelming majority of
  executions;
- one `praxis_check_fault` call, to read one word out of the context.

Handover 21 measured the consequence rather than inferring it: sampling
`collatz` — the benchmark whose inner loop is arithmetic and nothing else — put
**only 19% of runtime inside JIT-generated code**. `praxis_int_load`,
`praxis_check_fault` and the two raise wrappers were together the largest
identifiable block of the rest.

The cost is not the guard. ADR-080's own Consequences say `catch_unwind` "adds a
landing pad and inhibits some inlining. The happy path is unaffected at
runtime", and on a table-driven-unwind target that is accurate: what is left in
the callee is a `__rust_try` region boundary and a `Result` discriminant. **The
guard is not the expensive part of a guarded call; the call is** — a `bl`, a
frame, and a full caller-saved-register clobber at every site, which at
`opt_level = "none"` forces a spill and reload of every live value in the loop.

ADR-017 anticipated this exactly, and the sentence is worth quoting because it
is the whole authority for what follows:

> Every faultable operation is a runtime call (not inline Cranelift
> arithmetic); **a later optimization can inline the checked op and branch
> directly.**

This is that optimization.

## Decision

Three sites in `crates/praxis-codegen-cranelift/src/lower.rs` stop emitting a
call and start emitting a branch. **No `abi_guard!` is removed, no
`#[no_mangle]` wrapper is deleted, and no manifest row changes.** Every wrapper
still exists and is still what the cold path calls — which is what makes each
refusal below byte-for-byte what it was.

### 1. `Inst::ExtractScalar` is an inline load behind an inline descriptor check

```text
have = load.i64  [src + GcHeader::DESCRIPTOR_OFFSET]
want = iconst.i64 &scalars::INT
ok   = icmp eq have, want
       brif ok, fast, slow
fast:  v = load.i64 [src + payload_offset_for(align_of::<IntPayload>())]
slow:  (cold) v = call praxis_int_load(ctx, src)     ; refuses; does not return
```

with `uload8` + compare-against-zero for `Bool`, `uload32` for `Char`, and the
same eight-byte load for `Float` (whose bit pattern is what `praxis_float_load`
returns). `ScalarKind::Byte` gets **no** inline form: its `load_symbol()` is
`IntLoad`, an eight-byte read of a one-byte payload chosen defensively while
nothing emitted it, and inlining that would be REP-37 by construction.

**The check is not an optimization to be traded away later, and this is the part
of the ADR that matters.** The static type is *not* a proof that this value is
an `Int`, and there is no version of this change that could rely on one:

- `Scalar` locals are `MirType::Opaque` by construction, and the `src` here is
  frequently a `Gc` local that lowering allocated as `Opaque` too (ADR-044
  decision 5 keeps `OpaqueAtDescriptorSite` off for exactly this reason).
- Where a type *is* known it has been wrong, three times. **REP-56** is a
  program that passes `praxis check` and emits `ExtractScalar { scalar: Int }`
  against a value whose descriptor is `Unit` — zero bytes wide — so a release
  build answered an ASLR-varying number off an eight-byte out-of-bounds read
  while a debug build aborted. **REP-49** is `Lit::Bool` sharing the `Lit::Int`
  arm, so `match b { true => … }` read a one-byte payload at eight bytes.
  **REP-37** is `ClosureOracle::is_goal` doing the same to a `Bool`.

So the check survives inlining, and it survives it in the form `int_payload`'s
doc insists on: **unconditional, in every profile**. That doc's argument is that
"a `debug_assert` is not a bound: it is compiled out of a release build", and
that the check costs only a never-taken branch because `scalar_type_mismatch` is
`#[cold] #[inline(never)]`. The inlined form preserves that argument
*exactly* — what moves is only which never-taken branch it is: to a Cranelift
cold block instead of into a `#[cold]` callee.

And because that cold block calls the wrapper rather than reimplementing it, the
refusal is unchanged end to end: `praxis_int_load` re-runs `read_scalar`, fails,
panics, `abi_guard!` catches it, `panic_fault_is_observable` finds the manifest
row is `Effect::Pure`, and it prints the message and aborts. Same message, same
exit.

The check buys a second thing that is easy to miss. The payload offset is folded
from `GcHeader::payload_offset_for`, which is correct *only because* the header
records what the allocator computed **from that descriptor's alignment**
(ADR-039 decision 1). Proving the descriptor is therefore also what makes the
constant offset the offset the allocator actually used. ADR-039 decision 3's
poisoning then falls out for free: a swept header has a null descriptor, fails
the comparison, and routes to the wrapper, whose `GcHeader::descriptor()` panics
"descriptor read from a poisoned (swept) GcHeader".

`Inst::EnumTag` inlines a payload read today with **no** check at all. It is a
weaker precedent than it looks: what licenses it is ADR-091 (a variant pattern's
enum is the scrutinee's, so the static type reaches the read), and that is the
same class of argument REP-56 falsified here. Whether the tag read should
acquire this check is a real question and is left open below.

### 2. The overflow report is a `brif` to a cold block

`raise_if_nonzero` called `praxis_raise_int_overflow_if` unconditionally, at
five sites in the `Inst::IntBinOp` arm. Its comment stated the choice:

> The call is unconditional and the wrapper decides: an arithmetic site stays
> one basic block, and the wrapper allocates nothing, so the site is not a GC
> safepoint and needs no root spill.

Both clauses were true. **The second is fully preserved** — the cold block calls
the same `Effect::Faults` wrapper, which still allocates nothing, so the site is
still not a safepoint and still spills no roots. The first is the one that was
mispriced. What a single basic block buys is not having to keep values live
across a CFG edge; but **a branch does not clobber registers and a call does**,
and the call forced exactly the spill-and-reload the single block was meant to
avoid. The hot path also gets *shorter*: the `ushr_imm_u` and the three
`uextend`s that existed only to shape an `i64` argument are gone with the
argument.

The wrapper's signature is unchanged and the cold block passes a constant `1`,
which is honest — it is reached only when the predicate held — and keeps
`if condition != 0` a true statement rather than dead code. Adding an
unconditional `praxis_raise_int_overflow` to mirror
`praxis_raise_stack_overflow` would be tidier and costs two manifest rows, two
address arms and two doc rewrites; it is registered below, not ridden in here.

### 3. `Inst::CheckFault` is two loads and a branch

```text
spill_debug(...)                              ; unchanged — see below
slot = load.i64 [ctx + offset_of!(RuntimeContext, pending_fault)]
kind = load.i32 [slot + Fault::KIND_OFFSET]
       brif kind, on_fault, fallthrough
```

Neither load tests for null, and neither needs to. ADR-017's Consequences state
the invariant — "`pending_fault` is always non-null once wired; the pending
state lives in the `Fault` slot, not in pointer-nullness" — `Runtime::context`
is the only producer of a context generated code sees, and
`RuntimeContext::placeholder`, the one null-wiring constructor, is `unsafe` and
test-only. That was prose; `a_wired_context_has_a_fault_slot` makes it a gate.
Nor is `ctx` itself a new assumption: the prologue's recursion guard already
loads through it unconditionally (ADR-101).

The bare branch on the loaded word *is* `Fault::is_pending()`, because
`FaultKind::None` is 0 and no other kind is — the enum gives every variant an
explicit discriminant and Rust rejects an enum that assigns one twice, so
pinning `None == 0` is the whole of it.

`spill.spill_debug(...)` stays, verbatim and ahead of the loads. Its comment is
unchanged in force: `CheckFault` is a debugger (not GC) safepoint, and without
it a snapshot taken on the fault path sees `<uninit>` for operands computed
since the last GC safepoint — the `0` divisor in `x / 0`, for instance.

`praxis_check_fault` keeps its `#[no_mangle]`, its `abi_guard!`, its manifest row
and its address arm. It simply stops having a generated call site. Its two null
tests are now the *difference* between it and the inline form, and are why it
remains the right thing for a host with a possibly-unwired context to call.

### Offsets are exported and derived, never written out

`GcHeader::DESCRIPTOR_OFFSET`, `Fault::KIND_OFFSET` and `Fault::KIND_SIZE` are
minted with `offset_of!`/`size_of!` in the modules that own the private fields,
because ADR-039 decision 1 made `GcHeader`'s fields private and RT-17 made
`Fault.kind` private. The backend derives `PENDING_FAULT_OFFSET` the same way
from the public `RuntimeContext`. And `KIND_SIZE` is asserted against the load
width in a `const _` block, so a `#[repr(u8)]` on `FaultKind` is a build failure
rather than a three-bytes-of-something-else read.

### `praxis_float_load` was the one unchecked scalar reader, and now is not

It read `let p: *mut f64 = r.payload::<f64>();` and dereferenced `p` — no
`read_scalar`, unlike `int_payload`, `praxis_bool_load` and `praxis_char_load`.
It was green under
`every_scalar_payload_read_goes_through_the_bounded_reader` on a *spelling*:
that gate forbade the literal `*r.payload::<f64>()`, and a `let` breaks the
spelling without breaking the defect. It now goes through `float_payload`, and
the gate now forbids the bare call rather than one phrasing of the dereference.

This had to land with the inline path rather than after it: the inline check and
its own cold fallback must agree about what a wrong descriptor means, and a
fallback that read anyway would have made "the refusal is unchanged" false for
`Float`.

### `RUNTIME_ABI_VERSION` 16 → 17

No layout, calling convention or wrapper signature changed. What changed is the
*set of things generated code depends on*, which is the v12 precedent exactly:
a meaning change with no layout change. Generated code now reads `Fault.kind`
and `GcHeader.descriptor` directly, so repacking either — or `FaultKind` — is
now a generated-code change. A v17-compiled program run against a runtime whose
`Fault` still carried v10's `pending: bool` would read that bool as the kind.

## What was deliberately *not* done

**`Inst::Alloc` / `Inst::Materialize` keep their calls.** The handover lists an
inline allocation fast path as the fourth site, and it is the largest remaining
one — for `AllocKind::Int` the wrapper reaches `gc_alloc` → `safepoint` →
`RuntimeRoots::from_context` → `Heap::pace` → `maybe_collect` → `alloc_raw`.
Two reasons to defer it, and the second is the one that would outlive a
re-measurement:

1. **Its shape is not final.** Finding §3.6 replaces the free list, deletes the
   `live` registry, moves `size`, `payload_offset` and `heap_id` into per-page
   metadata and takes `GcHeader` from 24 bytes to 8. Every offset an inline
   allocator would bake in changes. Inlining before §3.6 means writing the
   inline allocator twice.
2. **It is the one change that would put a `Heap` invariant into generated
   code.** ADR-040's `Safepoint` token exists so that "allocate on the paced
   path without pacing" *has no spelling*: obtaining the token is the pacing. An
   inline allocator spells it. There is a sound framing — the inline path is
   exactly the branch on which `maybe_collect` would have returned `false`, so
   it still bumps `bytes_since_collect`, still tests the threshold, and hands
   off to the wrapper the moment either the size class is empty **or** the
   threshold is reached, forging no token because the token is permission to
   *collect* and the inline path never collects. That framing is defensible and
   it is an amendment to ADR-040, which deserves its own decision record rather
   than a paragraph in this one.

**`opt_level` stays `"none"`.** Finding §3.7 was re-tested against this tree,
which was the whole point of doing §3.4 first, and the negative result held: no
benchmark moved by more than laptop noise, and the compile-time floor grew by up
to 4.7 ms. See handover 21 §3.7, which now records both measurements, and the
comment at `Jit::in_generation`.

## Measurements

Apple M2 Pro, release build, three runs per configuration, minimum reported, the
two binaries saved aside and run **interleaved** (A,B,A,B,A,B) because the
laptop drifts. Baseline is the tree with §3.1, §3.5 and §3.3 already landed —
not `fd70374`, or the attributions would double-count.

| | before | after | |
|---|---:|---:|---|
| `primes` @ 300,000 | 0.300 s | 0.194 s | **−35%** |
| `collatz` @ 60,000 | 0.315 s | 0.230 s | **−27%** |
| 10M × `i = i + 1` | 0.390 s | 0.303 s | **−22%** |
| 5M × no-op call | 0.367 s | 0.335 s | −9% |
| `vm` @ 400,000 | 1.276 s | 1.209 s | −5% |
| `pipeline` @ 200,000 | 1.452 s | 1.366 s | −6% |
| `tree` @ 60 | 1.462 s | 1.381 s | −6% |
| `mandelbrot` @ 200 | 0.934 s | 0.889 s | −5% |
| `bfs` @ 80 | 5.366 s | 5.163 s | −4% |
| `hashwork` @ 800,000 | 0.708 s | 0.706 s | — |

The three arithmetic-dominated workloads are where the change is, and they are
the three the sites were chosen from. `hashwork` is the honest control: its time
is inside `praxis_map_*` wrappers, which this does not touch at all.

**The direct evidence is the profile, not the timings.** `/usr/bin/sample` at
1 ms over a 3 s window on `collatz`, before and after:

- before: `praxis_int_load`, `praxis_check_fault`,
  `praxis_raise_int_overflow_if` and `praxis_raise_div_by_zero_if` appear on 35
  distinct stacks, with `praxis_int_load` the single heaviest leaf.
- after: **zero** occurrences of any of the four.

No timing number can make that assertion on its own, and it is the one that says
the calls are gone rather than merely cheaper.

## Consequences

- **A `lower_inst` arm can now split the current Cranelift block.** This was
  already true of `Inst::CheckFault`, and it is now true of `ExtractScalar` and
  of every checked `IntBinOp`. The consequence to know is that `blocks[blk_idx]`
  is the MIR block's **entry** only: an instruction lowered later in the same
  MIR block may land in a different Cranelift block. `lower_terminator` emits
  through `builder.ins()` rather than a block handle, so it is correct today;
  a future change that assumes "MIR block N is Cranelift block N throughout"
  breaks.
- **The IR-shape tests are the gate, and they had to be.** "The instruction is
  the fact": a behavioural test cannot tell an inline load from a call to a
  wrapper that performs the same load, so no test that *runs* a program can see
  this change — or see it being undone. Six tests in `lower.rs` read the emitted
  Cranelift text: the descriptor `icmp` is present and in the hot block, the
  fast path calls nothing, the cold block is marked cold and is the one that
  calls, `Bool` reads one byte and `Char` four *at the payload displacement*,
  `Byte` has no inline form, and the pending-fault load claims neither
  `readonly` nor `can_move`.
- **The inline check and the wrapper prove the same descriptor, and that is
  tested.** `the_inline_check_proves_exactly_what_the_wrapper_would` asserts the
  descriptor `inline_scalar_load_of` compares against is the one behind
  `scalars::…_PAYLOAD`, and that the alignment it folds is the descriptor's. If
  they diverged, the site would hold two contradictory notions of what the value
  is, and one of them would be a read.
- **ADR-088 is untouched, and the reason is worth stating precisely.** Its rule
  is positional *within a MIR block* (`verify::check_fault_observed`), and this
  emits no MIR. Every diamond converges before the `CheckFault` lowering, so the
  check runs on the raising path and the non-raising path alike.
  `an_overflow_diverts_before_the_next_statement_runs` proves it behaviourally
  by a sharper route than "it faulted": the statement after the overflow is a
  division by zero, and `set_fault` overwrites unconditionally, so the observed
  kind names the last operation that ran.

  > **The convergence half is amended by
  > [ADR-117](./117-a-raise-that-branches-is-its-own-observation.md)
  > (2026-08-03), and the conclusion held.** A diamond whose `Inst::CheckFault`
  > has been folded into it does *not* converge — its cold block jumps straight
  > to the fault epilogue and the check emits nothing. What survives is the
  > weaker sentence ADR-117 states in its place: *on the raising path, control
  > reaches the fault epilogue before any instruction after the raise executes.*
  > The sentence above is that rule seen in the one case where the raise is
  > observed by a later read rather than by its own branch. ADR-088 is still
  > untouched, still for the reason given here, and
  > `an_overflow_diverts_before_the_next_statement_runs` still passes unedited.
- **ADR-080 loses nothing.** No guard is removed and no entry point is added or
  deleted, so `every_no_mangle_wrapper_is_behind_the_panic_guard` and
  `every_manifest_symbol_resolves_to_a_distinct_address` are untouched. This
  removed call *sites*, not guards. There is no class of wrapper for which
  ADR-080's proof could be waived, either: the obvious candidate — `Effect::Pure`
  with no allocation and no user code reachable — contains `praxis_int_load`,
  which **panics by design**, and REP-56 is the bug report proving the panic is
  reachable from a `praxis check`-clean program.
- **The manifest rows for `IntLoad`, `BoolLoad`, `CharLoad`, `FloatLoad` and
  `CheckFault` are kept although generated code no longer calls three of them.**
  `ScalarKind::load_symbol` is read by `Inst::fault_reason` to decide whether an
  `ExtractScalar` needs a `CheckFault`; deleting a row would leave it with
  nothing to return and would move the verifier's answer. Keeping the row and
  simply not emitting the call is what lets the backend and the verifier go on
  stating one fact once (MIR-10).
- **`Overflow::Bounded` is unaffected.** Bounded sites emit bare arithmetic and
  no raise at all, so only the `Checked` path gains a branch. ADR-044 decision 6
  and ADR-088 §3's use of it as the precedent for a site claim the backend
  actually reads both stand.
- **A host that hand-built a `RuntimeContext` with a null `pending_fault` and
  called generated code now faults the process** where it previously ran a
  program that could never observe a fault. Not reachable today —
  `Runtime::context` is the only producer and `placeholder` is `unsafe` and
  test-only — and named here rather than glossed, because it is a real
  reduction in defensive behaviour. Making the field `NonNull` is the
  unrepresentable-states version and is left open below.

## Open questions

- **Should `Inst::EnumTag` acquire the same descriptor check?** It inlines a
  payload read with none, justified by ADR-091 and by `EnumPayload` being
  8-aligned — the first of which is the class of argument REP-56 falsified. The
  cost would be one load, one compare and a never-taken branch per `match` arm,
  but it needs a cold-path callee, and `praxis_enum_tag`'s manifest row is
  `Effect::Allocates`, so calling it from a cold block would make the site a
  nominal safepoint with no root spill. Its own item, not a rider on this one.
- **Should `RuntimeContext.pending_fault` become non-nullable in the type
  system?** The inline load depends on an invariant that is prose plus a
  constructor convention plus, now, a test. `NonNull<Fault>` would make the null
  state unrepresentable, but `placeholder` needs a null and the struct is
  `#[repr(C)]`, so it would have to be `Option<NonNull<Fault>>` relying on the
  niche — the trick `DebugLocal.value` already uses (F18). That is a `#[repr(C)]`
  semantics change and wants its own decision.
- **Should the unconditional `praxis_raise_int_overflow` /
  `praxis_raise_div_by_zero` wrappers be added?** Passing a constant `1` to a
  wrapper named `…_if` is honest but silly, and
  `praxis_raise_stack_overflow` / `praxis_raise_empty_collection` are the
  existing unconditional precedent.
- **Is `collatz`'s remaining per-op cost dominated by `praxis_alloc_int` or by
  the debug frame?** That decides whether the deferred alloc fast path or
  finding §3.2 is next, and it can only be answered by re-profiling against this
  tree — not from the handover's pre-§3.4 numbers.
