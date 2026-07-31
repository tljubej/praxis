# ADR-080: Totality is the contract at the ABI boundary, and `catch_unwind` is the proof

**Date:** 2026-07-31
**Status:** Accepted — implemented
**Milestone:** Repair (stage S20 — D12)

## Context

§9.2 and §10.4 both state the requirement: runtime wrappers must prevent panic
escape and translate unexpected panics into internal faults. Nothing implemented
it as a policy. The repair had been closing it wrapper by wrapper —
ADR-041's `GridExtent` and `BitIndex`, ADR-046's validated constructors, RT-06's
`SourceSlice`, S20's own `Input`, `csv_tokens` and `region_str` — and ADR-041's
Consequences said so explicitly: "this is not a general panic-across-FFI policy;
D12 remains open."

The cost of leaving it open is not theoretical and it is not a crash. A Rust
panic unwinding out of `extern "C"` into Cranelift frames is undefined behaviour.
The damage does not surface at the wrapper; it surfaces later, somewhere else,
as something that looks like a different bug.

S20 found two live instances while doing other work. `region_offset_of` called
`slice::windows(0)` for any CSV field that trimmed to nothing, which `"10,20,"`
reaches. `preview_around` computed `start = offset - 24` and `end = min(offset +
24, len)` and sliced `&input[start..end]`, an inverted range for any offset past
the end of the buffer — reachable because a ragged grid's fill is parsed against
its own buffer, so a failure there carries an offset that means nothing to the
input the preview is taken from. Both were found by reading, which is exactly the
method that does not scale to 164 entry points.

## Decision 1: totality first, and a guard at every boundary

Both, with totality primary.

Per-wrapper totality stays the discipline: a wrapper validates its arguments and
reports a bad one through the fault protocol. Validated newtypes and fallible
constructors are how a wrapper is made total, and four landed ADRs already do it
that way.

On top of that, every `#[no_mangle] extern "C" fn` in `praxis-runtime` has its
body inside `abi_guard!`, which is `catch_unwind` plus the fault protocol.

The guard should never fire. It exists because a contract that cannot be checked
is a hope, and because the failure mode of forgetting is not a message.

**Alternative rejected: totality alone.** It contradicts §9.2 and §10.4, which
mandate translating unexpected panics — the word *unexpected* is the admission
that the discipline will occasionally be wrong.

**Alternative rejected: `panic=abort`.** It deletes M10's crash report, which is
the feature the milestone before this repair shipped, and it deletes it precisely
on the inputs a user most wants it for.

**Alternative rejected: the guard alone.** It makes every wrapper's argument
handling somebody else's problem, and it is what turns a validated newtype into
"the guard will catch it".

## Decision 2: the coverage is gated by a test that reads the source

`every_no_mangle_wrapper_is_behind_the_panic_guard` scans the four files that
declare entry points and fails, naming the function, if any `#[no_mangle]`'s body
does not open with `abi_guard!`.

Read as source text on purpose. The property is "every entry point is wrapped",
which is a property of the *set* of entry points; a test that called them one by
one would be a test of the ones somebody remembered. A new wrapper that forgets
is now a failing test instead of a latent abort. (Verified by removing one guard
and watching the test name it.)

## Decision 3: a caught panic is `FaultKind::Panic` with a message

Not a new `FaultKind::Internal`.

A new `FaultKind` variant is a `#[repr(C)]` layout change that costs an ABI bump
— ADR-075 settled that, and spent one for it. S20 changes no `#[repr(C)]` type
generated code reads and has no other reason to spend one.

`Panic` plus a message naming the wrapper carries strictly *more* information for
the crash report (§9.4) than a bare `Internal` kind would: "a panic escaped
`praxis_grid_set`" says where. The message is prefixed "internal error", which is
what distinguishes it from a program's own `panic(value)`.

The wrapper returns the defined dummy §10.4 already specifies, chosen per return
type by an `AbiSentinel` impl: `ctx.unit_ref` for a `GcRef` (integer zero would
be an invalid reference, not a dummy), zero for an `i64`, null for a raw pointer.
A null context has no fault slot to write and no `Unit` to return, so it aborts —
still better than unwinding into generated frames.

## Consequences

- **164 wrappers changed shape.** The bodies are unchanged; each is one
  indentation level deeper inside `abi_guard!(name, ctx, { … })`.
- **Cost.** `catch_unwind` adds a landing pad and inhibits some inlining. The
  happy path is unaffected at runtime.
- **`preview_around` clamps** rather than panicking on an out-of-buffer offset,
  and `region_offset_of` is deleted rather than guarded — the guard is a backstop,
  and a reachable panic is still a bug to fix.
- **This closes D12**, which the progress doc has carried as open since S7.
