# Implementation repair — progress

Living status for `implementation-repair-plan-2026-07-28.md`. **The plan is the
plan; this file is what has actually happened.** Read both before starting a
stage: the plan says what to do and in what order, this says where the tree is
and what the last session changed underneath you.

Update this file at the end of every stage.

## 1. Status

| Stage | State | Commits |
|---|---|---|
| S1 — Runtime identity registry | **done** | `055e894` |
| S2 — Independent hardening | **done** | `5629736`, `70ea4be` |
| S3 — ABI manifest, MIR representation, fault values | **done** | `8a30db1`, `7075017`, `91b64c1`, `1f7eca7` |
| S4 — Object layout and heap provenance | **done** | `dfe42b6` |
| S5 — Root-set completeness, native RAII roots | **done** | `7911337` |
| S6 — Allocation pacing, effect metadata, heap lifecycle | **done** | `7182d56`, `ce08ae3`, `b384df9`, `968af35`, `3b5bfb7` |
| S7 — Descriptor totality, typed collections, fault representation | **part done** — RT-06, RT-17, RT-18 closed; P0-11, RT-09, RT-07, RT-10, RT-11 remain | `d014067`, `c00aab7` |
| S8 … S21 | not started | |

Also closed out of order: **DBG-01** (`3836b74`), a P0 the plan schedules in
S10. It fell out of S1.

Baseline at `136ce4b` was **928 passed, 0 failed, 149 ignored**.
Now: **1026 passed, 0 failed, 123 ignored**. `just ci` is green.

Twenty-six of the audit's ignored regressions are un-ignored and passing. The
five added this session (S6's remaining exit criteria, then two of S7's):

| Test | File |
|---|---|
| `checked_int_add_is_an_automatic_gc_safepoint` | `praxis-runtime/src/abi.rs` |
| `repeated_collection_reuses_dead_object_storage` | `praxis-runtime/src/heap.rs` |
| `dropping_heap_finalizes_live_owned_payloads` | `praxis-runtime/src/heap.rs` |
| `alloc_char_rejects_values_that_only_become_valid_after_truncation` | `praxis-runtime/src/abi.rs` |
| `setting_none_cannot_create_a_pending_fault` | `praxis-runtime/src/context.rs` |

Two tests were **rewritten**, not merely un-ignored, because this session
inverted their premise:

- `reset_restores_collection_pacing` grew the threshold by calling
  `collect_with`, and an explicit collection no longer grows it (RT-04). It now
  drives a real paced collection through the new test-only
  `Heap::maybe_collect_with`.
- `setting_none_cannot_create_a_pending_fault` was built on
  `fault.set(FaultKind::None)`, which no longer compiles (RT-17 made `set` take
  a `RaisedFault`). It now pins the property where it still lives —
  `RaisedFault::new` rejecting `None`, every other kind round-tripping.

Thirteen findings-without-gates got one, all checked against the unfixed code:

| Test | Pins |
|---|---|
| `predicate_wrappers_return_bool_singletons_and_allocate_nothing` | RT-03's other 24 sites |
| `every_scalar_boxing_wrapper_paces_the_collector` | P0-08b, verified to fail against the unfixed `praxis_text_len` |
| `a_reclaimed_block_is_reused_for_the_next_object_of_its_layout` | RT-01's re-heading |
| `reset_discards_the_free_list` | RT-01 vs. arena teardown |
| `dropping_heap_finalizes_reachable_payloads_too` | RT-02 — rooting must not exempt |
| `resetting_then_dropping_finalizes_each_payload_once` | RT-02 double-finalize |
| `a_snapshot_may_be_dropped_after_the_runtime_it_names` | H8 (in `crash_snapshot.rs`) |
| `an_explicit_collection_does_not_grow_the_pacing_threshold` | RT-04 half two |
| `pacing_charges_the_bytes_a_payload_owns` | RT-04 half one |
| `a_source_slice_text_is_charged_nothing_beyond_its_block` | RT-04, the double-count it must not do |
| `alloc_char_rejects_a_negative_code_point` | RT-18, the other end of the range |
| `alloc_text_reports_invalid_utf8_as_its_own_fault_kind` | RT-17's second `None` caller |
| `an_out_of_range_or_non_boundary_slice_is_unconstructible` | RT-06 (in `text.rs`) |

(Earlier nine, from S3/S5: `closure_capture_indices_never_flow_through_gc_locals`,
`call_result_locals_retain_their_inferred_static_types`,
`pipeline_runtime_call_destinations_retain_vec_and_unit_types`,
`automatic_gc_roots_the_ambient_input_buffer`,
`automatic_gc_roots_parse_failure_partial_values`,
`automatic_gc_roots_runtime_owned_crash_snapshots`,
`nested_allocating_helpers_root_intermediate_results`,
`bool_and_unit_abi_allocations_reuse_runtime_singletons`,
`reset_restores_collection_pacing`; their no-gate findings were P0-02, P0-03
and DBG-04. Earlier three: `overaligned_payload_accessor_matches_initialized_address`,
`foreign_heap_root_cannot_delay_reclamation`,
`fault_epilogue_returns_the_valid_unit_sentinel`. Earlier nine:
`builtin_type_ids_are_globally_unique`,
`regression_unicode_identifier_may_start_with_a_unicode_scalar`,
`regression_in_is_classified_consistently_with_the_keyword_table`,
`regression_postfix_forms_may_be_interleaved`,
`regression_formatting_does_not_delete_comments`,
`for_continue_targets_the_increment_block_not_the_header`,
`missing_explicit_input_file_is_a_usage_error`, `float_sign_of_zero_is_zero`,
`regression_runtime_scalar_descriptors_recover_their_actual_types`. Search by
test name — line numbers throughout the plan have all moved.)

## 2. Foundations: what actually landed

Partial foundations are the main trap for a fresh context.

**F1 — built-in type identity: landed, minus `compare` semantics.**
`BuiltinTypeId` is the registry; `TypeDescriptor::builtin::<P>` is the only
constructor for a built-in. `compare: Option<CompareFn>` **exists on every
descriptor and is `None` everywhere** — the field's shape is settled, its
semantics are design decision D3 and are not. See ADR-038.

**F3 — identifier class: predicate half only.** `praxis-syntax/src/ident.rs`
has `is_ident_start` / `is_ident_continue` / `is_ident`, and the lexer uses
them. **Not done:** the `Ident` newtype; `_` lexing as `UNDERSCORE` (D7 + S12);
`praxis-input-parser/src/scan.rs` `split_capture`'s independent ASCII rule
(IP-04, S19); `praxis-debugger/src/evaluate.rs` `sanitize_name`, which still
*rewrites* invalid names to `_x` non-injectively (DBG-03, S12) — its bug-pinning
test `sanitize_rejects_digit_leading_and_punct` is still green and still needs
rewriting, per §8.2.

**F4 — ABI manifest: landed whole.** `praxis_stdlib::abi` is 137 rows, one per
`praxis_*` wrapper: linker name, `AbiKind` params, `AbiRet`, and an `Effect`
(`Pure` / `Faults` / `Allocates` / `AllocatesAndFaults`).
`praxis_runtime::abi::address` is the one symbol→address table.
`RuntimeSymbol::from_name` + `address` is all `symbols::resolve` is now. See §3
for what this means for a stage that adds a symbol (it is now two lines).

**F6 — GcHeader repack: landed whole.** See ADR-039 and §3.

**F7 — composite root set, native RAII roots and the `Safepoint` token: landed
whole.** `RuntimeRoots` is the only thing `Heap::collect`/`maybe_collect` takes,
`NativeScope`/`Rooted` are how native code roots what it holds, and
`Heap::alloc`/`alloc_with` take a `Safepoint` minted only by `Heap::pace`. See
ADR-040 and §3. **Not done:** `ScopedVec` — no site needed it once the payload
accessors took `Rooted`, and the parser interpreter (the one place the plan
wants it) is S20.

**F16 — MirType and raw-word elimination: landed, minus `TupleShape`.**
`MirType::{Known, Opaque}` is `Local.ty`, `AllocKind::Tuple.ty` and
`AllocKind::Collection.args`; `Inst::LoadCapture` carries the capture index as
an immediate; `alloc_empty_vec` is an ordinary `AllocKind::Collection`. **Not
done:** `TupleShape` (the validated `Box<[Type]>` that makes arity < 2
unrepresentable) — the fused `enumerate`/`zip` tuples have no real type until
MIR-05 supplies one in S21, so they are `MirType::Opaque` and the backend keeps
its degenerate empty-schema path. `MirType::expect_known`/`MirTypeError` are
also not written: their only consumer is F17's verifier (S9).

No other foundation has been started. **F11** (the `praxis-repr` crate, the
total `Type ⇄ TypeDescriptor` bridge) is next on the critical path: P0-11 in S7
is what it exists for.

## 3. Things that changed under you

Mechanical consequences a fresh context will hit immediately. Items from earlier
sessions are kept — they are still true.

### From this session

**`Heap::alloc` and `alloc_with` take a `Safepoint` as their first argument.**
The only producer is `Heap::pace(&RuntimeRoots)`, which performs the
`maybe_collect` — so obtaining the token *is* the pacing, and "allocate without
pacing" is unwritable on this path. The token is neither `Copy` nor `Clone`: one
token, one allocation. A wrapper that allocates twice paces twice.

**A `praxis_*` wrapper allocates through `gc_alloc` / `gc_alloc_with`** (abi.rs),
which pace and allocate in one step. If you add an allocating wrapper, use them.

**`Heap::alloc_unpaced` / `alloc_with_unpaced` are the named back door**,
`pub(crate)`, for the two callers that must not pace: the host's
`Runtime::alloc_*` helpers and `parser.rs`. **Do not reach for them from a new
runtime wrapper.** S20's IPR-14 removes the parser's use. In-crate tests use
them too — a test has no `RuntimeContext` to pace against.

**A `Bool` answer comes from `bool_ref(ctx, b)`**, which reads
`ctx.true_ref`/`ctx.false_ref`. `Heap::alloc_immortal` now takes an
`ImmortalWitness` only `immortal.rs` can construct, so `Immortals::new` is the
one place an immortal is minted.

**Swept storage is reused.** `Heap` keeps a free list keyed on `BlockLayout`
(the whole `[header|payload]` block's size and align, not the payload's own).
`BlockLayout::of` is the one calculation `alloc_raw` lays a block out with and
`sweep` files it under. Consequence worth knowing: **a stale `GcRef` can now
name a *live* object of a different type**, where before it named poisoned
storage. That is inherent to a reclaiming non-moving allocator and is why
poisoning (S4) had to land first (H7); it also means a rooting bug now
manifests as type confusion rather than as a clean panic.

**`Heap` has a `Drop`, and a `GcRef` does not outlive the heap.** It finalizes
every still-live payload, so reading a `GcRef` after the `Runtime` drops is now
a visible use-after-free. Both `take_crash_snapshot` consumers were audited
(H8): `Repl` declares `snapshot` before `session`, so the snapshot dies first,
and the debugger replaces its snapshot while the runtime is alive.
`CrashSnapshot`/`ParseDetail` hold `GcRef`s but have no `Drop` that
dereferences one.

**Pacing charges what an object costs, not its block.** `TypeDescriptor` gained
`owned_bytes: Option<OwnedBytesFn>`, set with the `const` builder
`.with_owned_bytes(f)` on the thirteen owning descriptors; scalars and
`VarCell` leave it `None`. A 64 KiB `Text` now reaches the 64 KiB threshold on
its own. A source-slice `Text` charges nothing (it borrows its owner).
**Post-allocation growth is still uncharged** — a `push` that reallocates the
spine — but the elements pushed are themselves paced allocations.

**Only a paced collection grows the threshold.** `Heap::collect` /
`Runtime::collect_now` no longer double it. Any test that grew the threshold by
calling `collect_with` is now wrong; `reset_restores_collection_pacing` was
rewritten for exactly this, and `#[cfg(test)] Heap::maybe_collect_with` is how
an in-crate test drives the paced path.

**`RUNTIME_ABI_VERSION` is 11**, bumped by RT-17's `Fault` repack. S6's half
needed no bump at all: `Heap` gained a field and `TypeDescriptor` gained a
field, but generated code holds the `Heap` pointer without dereferencing it and
passes descriptors by pointer without reading their fields — the only thing it
reads out of the runtime's own types is `GcHeader::payload_offset_for`, which is
unchanged. **S7's one bump (H17) is therefore spent.**

**`Fault` is one field wide, and `Fault::set` takes a `RaisedFault`.** The
`pending: bool` is gone — `is_pending()` is `kind != None`, so the two cannot
disagree. `RaisedFault` is a `FaultKind` proven not to be `None`, with an
associated constant per raisable kind (`RaisedFault::INT_OVERFLOW`, …);
`RaisedFault::new` is the only fallible route in. `Fault.kind` is private; use
`kind()`. **`FaultKind` gained `InvalidChar` and `InvalidText`** — a `match` over
it is two arms longer.

**`praxis_alloc_char` range-checks before narrowing.** `value as u32` truncated,
so `0x1_0000_0041` silently became `'A'` and a negative wrapped into range. Both
now raise `FaultKind::InvalidChar`.

**A source-slice `Text` is a validated `SourceSlice`.** `TextPayload::Slice`
holds one, its fields are private, and `SourceSlice::new` is the only
constructor — it rejects a range past the owner, an overflowing length, and ends
that split a multi-byte scalar. `Runtime::alloc_text_slice` and the parser's now
return `Option<GcRef>` and are `unsafe` (they read the owner's payload to
validate). `text_bytes` has no clamp and `text_str` no `unwrap_or("")`: both
were how a bad range became a *different, plausible* `Text` rather than a
failure.

### From S5 and the first half of S6

**`Heap::collect` and `maybe_collect` take a `&RuntimeRoots`, not a `&dyn
RootSet`.** In-crate tests use `Heap::collect_with` / `Runtime::collect_with`
(both `#[cfg(test)]`); a host that wants to force a collection calls
`Runtime::collect_now()`, which takes no root set on purpose. `RootSet` and
`RootScope` still exist and are still what `ShadowFrame` / `CrashSnapshot` /
`NativeRootFrame` implement — only the *entry point* is sealed.

**`RuntimeRoots::from_context` is exhaustive over five arms**, and its
`push_roots` destructures the struct rather than reading fields, so adding an
owner to `RuntimeContext` without rooting it is a compile error. If you add a
field that can hold a `GcRef`, that is where it goes.

**Native code roots through `NativeScope`.** `unsafe { NativeScope::new(ctx) }`
pushes a frame onto `ctx.native_roots` and pops it on `Drop`; `scope.root(r)`
takes `&self` (the roots are behind a `RefCell`) so several `Rooted` may be live
at once. Each scope costs one `Box`; if that shows up in a profile, S6 is the
stage that revisits the allocation path.

**`*_payload_mut` takes a `Rooted<'s>` and returns `&'s mut T`.** All nine
accessors and their 26 call sites. If you add a wrapper that needs one, open a
scope — that is the point, not an inconvenience: the old `&'static mut` said
the payload outlives the program.

**`RuntimeContext` gained three fields** — `native_roots` (S5), then `true_ref`
and `false_ref` (S6) — all appended, so generated-code offsets are unchanged.
**`RUNTIME_ABI_VERSION` / `COMPILER_EXPECTED_ABI_VERSION` are 10.**

**`praxis_alloc_bool` / `praxis_alloc_unit` return cached singletons** off the
context and allocate nothing. **`Heap::reset` resets the pacing counter and
threshold.** **`Runtime::heap_mut` is gone** — there is no safe route to
`&mut Heap` from a `Runtime`, which is what made `Heap::reset` dangerous.

**Automatic collection now happens with no generated frame on the stack.** That
is the whole point of P0-06, and it is the thing most likely to surprise: any
test that allocated through the `praxis_*` wrappers and assumed the pacing
counter kept climbing is now wrong. `maybe_collect_runs_under_pressure` was
rewritten for exactly this.

**A MIR local's `ty` is a `MirType`, not a `Type`.** `MirType::known() ->
Option<Type>` is the only reader, so every consumer decides what to do with the
absence. `Function::new_local`, `Builder::alloc_gc`/`alloc_temp`,
`AllocKind::Tuple.ty` and `AllocKind::Collection.args` all take it. A site that
has no type says `MirType::Opaque`; there is no longer any way to write
`Type(0)` and mean "unknown".

**Most call and allocation destinations now carry their real type.** The
`TypedExpr` being lowered already had one at far more sites than the plan
assumed — call/method-call/indirect-call results, tuple/record/enum
allocations, match results, field reads, `for` items and bindings, the parser
input and result, the closure-self param, and the pipeline result Vec and push
Units. What is still `Opaque`: pipeline accumulators, fused-loop items, the
fused `enumerate`/`zip` tuples, the `VarCell` slot, and every `Scalar` slot.

**`Opaque` resolves to nothing in the backend, not to something wrong.**
`collection_element_descriptor_for(db, args, i)` returns null (which every
`praxis_*_new` wrapper already reads as "unknown element type"), `tuple_schema_for`
returns the empty schema, and `build_debug_local_metas` emits a null descriptor
plus `praxis_runtime::debug::NO_STATIC_TYPE` (`u32::MAX`) — the debugger's
`type_str` already omits both. `descriptor_for_type` still takes a
`praxis_types::Type` and keeps its `_ => INT` fallback on the `Known` path (H9);
P0-11 in S7 makes that exhaustive.

**Closure captures load by immediate.** `Inst::LoadCapture { dst, closure,
index: u32 }` replaces `ConstInt` + `MoveGc` + `Call(ClosureCapture)`, and there
is no `check_fault` after it (the wrapper is `Effect::Pure`). Adding an
instruction touches four exhaustive matches: `ir.rs`, `liveness.rs` defs and
uses, and `lower_inst`. It is deliberately **not** in `safepoint_roots_slot`.

**`MoveGc` is `Gc` → `Gc`.** Documented, and true at every site — but not yet
*enforced*: the check is F17's verifier (S9). The gate today is
`closure_capture_indices_never_flow_through_gc_locals`, which now lowers a
closure program and a pipeline program.

**`GcHeader` is 24 bytes and its fields are private.** `descriptor`, `size`,
`payload_offset`, `mark`, `_pad`, `heap_id`. Use the accessors; construct with
`GcHeader::new` (allocator only) or `GcHeader::detached()` (tests). **The
payload no longer starts at `size_of::<GcHeader>()`** — it starts at
`GcHeader::payload_offset_for(payload_align)`, which is the single layout
authority and the only thing generated code may use to reach a payload.

**Every allocation carries a `HeapId`.** `Heap::mark` skips any reference whose
`heap_id` is not this heap's — including a swept one, since `Heap::sweep`
poisons (`descriptor = null; heap_id = 0`) before unregistering.
`GcHeader::descriptor()` now **panics** on a poisoned header rather than
dereferencing null. `Heap::reset` mints a fresh `HeapId`. `Heap::owns(GcRef)` is
public.

**A runtime symbol is a `RuntimeSymbol`, not a string.**
`CallTarget::Runtime(RuntimeSymbol)`, `TypedExpr::MethodCall.lowering_symbol:
Option<RuntimeSymbol>` (`None` = intrinsic; the empty-string sentinel is gone),
`MethodLowering::RuntimeSymbol(RuntimeSymbol)`. **Adding a runtime symbol is now
exactly two edits**: a row in `crates/praxis-stdlib/src/abi.rs` and an arm in
`praxis_runtime::abi::address`. Both are exhaustive; miss either and it does not
compile. H16 is discharged.

**`MethodEntry.allocates` is deleted.** Use `MethodEntry::allocates()`, which
reads the manifest. Note the correction it forced: `Vec.len` declared
`allocates: false`, but `praxis_vec_len` boxes its result and can collect. Any
safepoint reasoning that trusted the old field was wrong for the `*_len` family.

**Codegen derives every runtime signature.** `signature_for(sym, module)` from
the manifest row; `call_symbol` checks arity and narrows each argument to the
declared width. The twelve `*_sig()` builders, `runtime_funcref` and
`call_runtime_by_name` are gone. Call convention comes from
`module.isa().default_call_conv()`, pointer width from
`module.target_config().pointer_type()`. `const GC: types::Type = I64` still
exists for the value channel and is untouched.

**`RunnableFunction` and `MainEntry` are `fn(*mut RuntimeContext) -> GcRef`.**
The trailing phantom `GcRef` is gone (a zero-parameter `main` never had it), so
callers no longer allocate a Unit to fill it. `evaluate.rs`'s `null_sentinel()`
is deleted with it.

**`Int` arithmetic is lowered natively; two new raise wrappers exist.**
`Inst::IntBinOp` emits `iadd`/`isub`/`imul`/`sdiv`/`srem` on the scalar channel
with the overflow predicate computed inline, then calls
`praxis_raise_int_overflow_if` / `praxis_raise_div_by_zero_if` **with the
predicate** rather than branching around the call. Consequences worth knowing:

- Arithmetic no longer allocates, so an arithmetic site is not a safepoint.
  Anything that assumed `IntBinOp` needed a root spill is now over-spilling.
- `sdiv`/`srem` *trap* (process abort) on a zero divisor and on `i64::MIN / -1`,
  so the lowering substitutes a divisor of `1` in both cases and reports the
  fault. Do not remove that substitution.
- The raise wrappers are unconditional calls. If a later stage wants the
  branch-around form instead, `Inst::CheckFault`'s arm already shows how to
  split a Cranelift block mid-MIR-block.

**Fault epilogues return `ctx.unit_ref`, not `iconst 0`.** `UNIT_REF_OFFSET` in
`lower.rs` is the load. Both fault exits (the `Terminator::Fault` block and the
stack-overflow guard) use it.

**Effect classification is a judgement, and it is recorded per row.**
`Allocates` means "may trigger a collection", so a wrapper that only hands back
an immortal singleton is `Pure` however "alloc" its name reads —
`praxis_alloc_bool`, `praxis_alloc_unit` and the six `praxis_int_*` comparisons
are all `Pure`. The rows were derived by call-graph analysis over `abi.rs` and
spot-corrected (`praxis_run_parser` reaches the parser interpreter and is
`AllocatesAndFaults`). P0-08b took the manifest as the intent and made the
wrappers match it, in both directions: the fourteen that allocated without
pacing now pace, and the `Pure` predicate wrappers now really do allocate
nothing. A new row's `Effect` is a claim about the wrapper you are about to
write — write the wrapper to it.

### From earlier sessions

**Descriptors are `static`, and three fields are private.**
`praxis_runtime::scalars::INT` and its twenty siblings are `static
TypeDescriptor` — call sites take `&scalars::INT`. `id`, `size` and `align` are
private: use `.id()`, `.size()`, `.align()`. `ptr::eq` on two descriptors is
authoritative type identity; ADR-038 supersedes ADR-028's "compare by TypeId,
not by pointer".

**Descriptor ids were renumbered.** They follow `BuiltinTypeId`'s declaration
order. Any ADR quoting a specific number is describing a variant, not a literal.

**`SyntaxKind::from_raw_u16`** is the total conversion; `kind_from_raw` calls it.

**The lexer emits an `ERROR` token for an unclassifiable character**, so a bad
character yields two diagnostics rather than one.

**`SourceMap` stores `Arc<SourceFile>` and `FileView` has no lifetime
parameter.**

**`Jit::check_target(pointer, endianness)`** rejects a non-i64-pointer or
big-endian target at `Jit::new`.

**`lower_for` emits an extra block.** The index increment has its own block and
is `continue`'s target.

**The formatter preserves comments.** When F8 adds
`Token::preceded_by_newline`, `fmt::starts_new_line` should read that instead of
re-deriving it.

**`praxis_float_sign` no longer uses `f64::signum`.** Both zeros give `0.0`.

## 4. Where to start

**Finish S7.** Five findings remain, and the first needs a decision.

- **P0-11** (descriptor totality) is the stage, and it is **blocked on D9**:
  what does the JIT do when `descriptor_for_type` returns `Err` — fail the
  compile with a diagnostic, or fall back and reintroduce the bug? The plan's
  own S7 text annotates the first as "(correct)". `descriptor_for_type`
  (`praxis-codegen-cranelift/src/lower.rs`) still takes a `praxis_types::Type`
  and keeps its `_ => INT` fallback on the `Known` path; that fallback *is* the
  finding. P0-11 is mostly **F11**, the new `praxis-repr` crate holding the
  total, bidirectional `Type ⇄ TypeDescriptor` bridge — read §3.1's F11 before
  starting.
- **P0-11 must precede RT-10 and RT-11.** Tightening collection and tuple
  equality first would surface the mislabelled `INT` descriptors as a cascade
  of new failures instead of the intended assertions.
- **P0-11 inverts currently-passing tests.** The Vec-adopts-first-push
  descriptor assertions near
  `praxis-codegen-cranelift/tests/adversarial_audit.rs:284` assert the
  behaviour it removes — budget to rewrite them, not just unignore (plan §8.2,
  H18).
- **RT-09** (`DynamicKey::eq` omits descriptor identity, so it can run a
  callback against the wrong payload layout and disagree with `Hash`). Gated by
  `dynamic_keys_with_different_descriptors_are_never_equal`
  (`praxis-runtime/src/dynamic_key.rs`). Independent of F11 — this one is
  reachable today.
- **RT-07** (negative or overflowing `Grid`/`BitSet` extents, and neighbour
  arithmetic that overflows, cast toward huge `usize` values and panic or OOM
  across `extern "C"`). No gating test; the plan wants one asserting they
  *fault* rather than panic or allocate absurdly. Also independent of F11.
- **S7's ABI bump is spent** (11, for RT-17's `Fault` repack). If RT-13-style
  signature work turns out to be needed here rather than in S18, it shares that
  bump — do not add a second (H17).

Re-read §6 of the plan first. The hazards that still bind: **H3**, **H9**,
**H10**, **H15**, **H17**. **H1, H2, H4, H6, H7, H8 and H16 are discharged.**

**H15 became live in S6.** `Heap::drop` now runs finalizers, and record and
tuple payloads hold `*const RecordSchema` / `*const TupleSchema`. It is safe
today only because those schemas are `Box::leak`ed and never freed. S8's
generation-arena reclamation must not change that without also fixing drop
order: `DebugSession` declares `jit` *before* `runtime`, so the arena would go
first and heap teardown would dereference freed schemas.


## 5. Design decisions still open

**D14 is answered** — see ADR-040. The `Safepoint` token shipped with a named,
`pub(crate)` `Heap::alloc_unpaced` back door for the host helpers and the
parser interpreter (the plan's option 2), rather than landing IPR-14 out of
stage order (option 1) or deferring the token (option 3). **D9 now blocks S7
outright**, alongside D1, D3 and D5, none of which have been answered.

| | Decision | Blocks |
|---|---|---|
| D9 | What the JIT does when `descriptor_for_type` returns `Err` — diagnose, or fall back and reintroduce the bug | S7 |
| D3 | NaN ordering, and whether Text/tuples/records/collections are orderable at all. **This is what `TypeDescriptor::compare` is waiting for** | S10, blocking P0-12 |
| D7 | After `_` lexes as `UNDERSCORE`, is it still legal in `let _ = f()`, `fn g(_)`, `\|_\| 0`? | S12 |
| D8 | Exactly where a newline terminates an expression | S12 |
| D13 | Diagnostic-code allocation for the whole block, before S13 starts | S13/S16 |
| D2 | Loop break-value semantics | S14 |
| D4 | Hashability of mutable collections as Map keys | S17 |
| D5 | The 15 phantom prelude names: implement or delete | S17 |
| D6 | `CollectionCtor::Range`: delete or implement | S17 |
| D1 | `Map.get` / `Grid.find` — `Option[V]` or V-with-Unit. Source-visible | S18 |
| D10 | How much parser-expression grammar a template capture body may contain | S19 |
| D11 | `grid(int)` granularity; greediness of `text`/`word` | S20 |
| D12 | Panic-across-FFI policy — should precede RT-06, RT-07 and the parser findings | cross-cutting |

## 6. Corrections to the plan

Things the plan states that are no longer or were not quite true.

- **DBG-01 is closed** (`3836b74`), not open in S10. **DBG-02** — collection
  element types defaulting to `Int` — is untouched and still needs F11.
- **§8.4 is stale for P0-05 and P0-14.** Both now have standing gates in the
  ordinary suite. A Miri job is still worth adding.
- **F1's `compare` is declared but unpopulated.** S10 does not need to touch 21
  descriptors, but it does need to answer D3 before it can populate one.
- **Line numbers throughout the plan have moved.** Search by test name.
- **F6's "batch the ABI bump with F7 and F18" is not what happened.** H17's
  one-bump-per-stage rule and S5's own exit criterion ("bump exactly once here
  for the native_roots field") both say per stage, so S4 bumped to 8 on its own.
  S5 bumped to 9, S6 to 10. **S6's second half needed no bump**: `Heap` gained a
  field and `TypeDescriptor` gained a field, but generated code holds the `Heap`
  pointer without dereferencing it and passes descriptors by pointer without
  reading their fields. Check what generated code actually reads before spending
  a stage's bump.
- **F6 says `Heap::mark` should debug-panic on a foreign root.** It skips
  instead. A panic would fail `foreign_heap_root_cannot_delay_reclamation`
  (which expects the collection to complete), and it would crash the debugger's
  second-heap configuration, which is the one place this actually arises.
  Reasoning in ADR-039.
- **F4's `AbiRet` sketch has three variants.** It needs a fourth, `Ptr`:
  `praxis_closure_fn_ptr`, `praxis_push_shadow_frame` and
  `praxis_push_debug_frame` return pointers, not `GcRef`s.
- **F4 lists `MethodLowering::Intrinsic(Intrinsic)`.** `Intrinsic` is still
  `&'static str`; only the `RuntimeSymbol` arm was typed. A real `Intrinsic`
  enum is worth doing when S21 touches the pipeline lowerings.
- **P0-13's exit criterion "a symbol absent from the manifest fails to compile"
  is now structural**, not a test: `CallTarget::Runtime` takes a `RuntimeSymbol`,
  so an absent symbol is unspellable. The tests that exist assert the property
  that replaced the bug — signatures derived from rows, narrow params declared
  narrow, void wrappers with no result.
- **P0-08's raise symbols take a predicate.** The plan says "two non-allocating
  raise symbols"; they are `praxis_raise_int_overflow_if` and
  `praxis_raise_div_by_zero_if`, each `(ctx, condition: i64) -> void`. Taking
  the condition instead of branching around the call keeps an arithmetic site
  in one basic block, which is what let native lowering land without touching
  the block bookkeeping. If a future stage prefers the branch form, nothing
  about the fault protocol changes.
- **The parser interpreter still cannot collect, and that is now enforced by a
  name.** H1 lists `parser.rs`'s unrooted `Vec<GcRef>` intermediates alongside
  the grid helpers, but the two are not in the same state: the grid helpers call
  `praxis_*` wrappers (which pace), while `parser.rs` allocates through
  `heap_ref(ctx).alloc_unpaced` / `alloc_with_unpaced`, which is the only route
  that skips pacing. Its intermediates are latent, not live. **IPR-14 (S20) must
  give them `NativeScope`s in the same commit that moves those nine call sites
  to the paced `alloc`/`alloc_with`** — that is when H1's mechanism actually
  arrives for the parser, and when the back door loses its second caller.
- **S6's exit criteria were sharper than the plan's list.** The plan names five
  ignored tests; three were already un-ignored before this session, and P0-08b,
  P0-08c and RT-04's second half had no gating test at all (plan §8.3 says so
  for RT-05 and P0-08c only). Ten new gates were written; §1 lists them.
- **P0-08b is "fourteen wrappers", but RT-03 was twenty-four more.** The audit
  scoped RT-03 as "Bool/Unit ABI *helpers*"; in practice `praxis_alloc_bool` and
  `praxis_alloc_unit` were two of twenty-six sites calling `alloc_immortal`, and
  the other twenty-four — every comparison, `contains`, `is_empty` — are the
  ones a program calls in a loop. A stage that reads a finding's one-line
  summary as its extent will under-fix it.
- **RT-17's gating test could not survive its own fix.**
  `setting_none_cannot_create_a_pending_fault` asserts a *runtime* property —
  `set(None)` leaves the fault not-pending — and the fix made the call
  unspellable, so the assertion has no code to run against. §8.2's list of
  bug-pinning tests does not include it, but it belongs there in spirit: when a
  fix promotes a runtime check to a compile-time one, expect the gate to need
  rewriting rather than un-ignoring.
- **RT-06 needed a newtype, not a validating constructor.** Enum *variant*
  fields cannot be private in Rust, so a `TextPayload::slice(...) -> Option<Self>`
  associated function leaves `TextPayload::Slice { .. }` just as spellable as
  before. The validated `SourceSlice` struct is what actually closes it — worth
  remembering wherever the plan says "make X unconstructible" about a variant
  (F14's `TextSlice`, IP-10's `NonEmptySeparator`).
- **P0-08c's exit criterion is partly obsolete.** "an assert that the effect
  table covers every symbol registered in symbols.rs" — `symbols.rs` no longer
  has a list (F4 deleted it; `resolve` is `from_name` + `address`), and
  `MethodEntry.allocates` was already deleted. What remains, and what landed, is
  a `const` block over `RuntimeSymbol::ALL` in `praxis-stdlib/src/abi.rs`.
  `every_manifest_symbol_resolves_to_a_distinct_address`
  (`praxis-codegen-cranelift/src/symbols.rs`) already covered the other half.
- **P0-08 raised a question the plan does not answer.** The two loop-increment
  `Inst::IntBinOp` sites in `build.rs` (the `for`-loop index bump) are *not*
  followed by a `check_fault`. An overflow there now sets a pending fault that
  is only observed at the next check. That is harmless (faults are sticky and
  the index would have to reach `i64::MAX`), but a MIR verifier rule "every
  faulting instruction is followed by a CheckFault" — MIR-10, S9 — would flag
  it, and the right answer is probably to mark the increment non-faulting
  rather than to add a check.
