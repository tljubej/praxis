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
| S6 … S21 | not started | |

Also closed out of order: **DBG-01** (`3836b74`), a P0 the plan schedules in
S10. It fell out of S1.

Baseline at `136ce4b` was **928 passed, 0 failed, 149 ignored**.
Now: **1004 passed, 0 failed, 130 ignored**. `just ci` is green.

Nineteen of the audit's ignored regressions are un-ignored and passing. The
seven added this session (S3's and S5's remaining exit criteria):

| Test | File |
|---|---|
| `closure_capture_indices_never_flow_through_gc_locals` | `praxis-mir/src/build.rs` |
| `call_result_locals_retain_their_inferred_static_types` | `praxis-mir/src/build.rs` |
| `pipeline_runtime_call_destinations_retain_vec_and_unit_types` | `praxis-mir/src/build.rs` |
| `automatic_gc_roots_the_ambient_input_buffer` | `praxis-runtime/src/abi.rs` |
| `automatic_gc_roots_parse_failure_partial_values` | `praxis-runtime/src/abi.rs` |
| `automatic_gc_roots_runtime_owned_crash_snapshots` | `praxis-runtime/src/abi.rs` |
| `nested_allocating_helpers_root_intermediate_results` | `praxis-runtime/src/abi.rs` |

Three of these were rewritten rather than merely un-ignored, each checked
against the unfixed code first so it is a real gate:
`pipeline_runtime_call_destinations_retain_vec_and_unit_types` (the Vec half
moved from a `Call` to an `Alloc`), `nested_allocating_helpers_root_intermediate_results`
(now calls the real `praxis_grid_positions` over a 40×40 grid, because the
sketch it inlined no longer models the fix), and — outside the four —
`maybe_collect_runs_under_pressure`, whose premise the P0-06 fix inverted.

Findings with no gating test that got one this session: **P0-02**
(`an_opaque_local_carries_neither_a_descriptor_nor_a_type_id`,
`an_opaque_element_type_resolves_to_no_descriptor`,
`an_opaque_tuple_type_yields_an_empty_schema` in
`praxis-codegen-cranelift/src/lower.rs`), **P0-03**
(`a_closure_prologue_reads_its_captures_by_immediate_index`), and **DBG-04**
(`snapshot_values_survive_a_collection_inside_the_evaluated_expression` in
`praxis-debugger/src/evaluate.rs` — it drives `evaluate` with the heap already
past its threshold and reads back a snapshot *temp*, the value that is never a
`__p_expr` parameter and so the one a generated prologue does not mask).

(Earlier three: `overaligned_payload_accessor_matches_initialized_address`,
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

**F7 — composite root set and native RAII roots: landed, minus `Safepoint`.**
`RuntimeRoots` is the only thing `Heap::collect`/`maybe_collect` takes, and
`NativeScope`/`Rooted` are how native code roots what it holds. **Not done:**
the `#[must_use] Safepoint` token on `Heap::alloc_with` — minting one requires
the wrapper to hold a `RuntimeRoots`, which means calling `maybe_collect`, which
*is* P0-08b. It belongs to S6, with the 14 unpaced wrappers, not here (H2).
`ScopedVec` is also unwritten: no site needed it once the payload accessors took
`Rooted`, and the parser interpreter (the one place the plan wants it) is S20.

**F16 — MirType and raw-word elimination: landed, minus `TupleShape`.**
`MirType::{Known, Opaque}` is `Local.ty`, `AllocKind::Tuple.ty` and
`AllocKind::Collection.args`; `Inst::LoadCapture` carries the capture index as
an immediate; `alloc_empty_vec` is an ordinary `AllocKind::Collection`. **Not
done:** `TupleShape` (the validated `Box<[Type]>` that makes arity < 2
unrepresentable) — the fused `enumerate`/`zip` tuples have no real type until
MIR-05 supplies one in S21, so they are `MirType::Opaque` and the backend keeps
its degenerate empty-schema path. `MirType::expect_known`/`MirTypeError` are
also not written: their only consumer is F17's verifier (S9).

No other foundation has been started. F7 (the composite root set) is next on the
critical path and is what S5 is mostly made of.

## 3. Things that changed under you

Mechanical consequences a fresh context will hit immediately. Items from earlier
sessions are kept — they are still true.

### From this session

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

**`ctx.native_roots` is a new `RuntimeContext` field** (appended, so
generated-code offsets are unchanged) and **`RUNTIME_ABI_VERSION` /
`COMPILER_EXPECTED_ABI_VERSION` are 9.**

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

**`RUNTIME_ABI_VERSION` / `COMPILER_EXPECTED_ABI_VERSION` are 8.** Bumped once,
for the header repack (H17). S5's `native_roots` change bumps to 9.

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
`AllocatesAndFaults`). **S6's P0-08b should treat the manifest as the intent and
make the wrappers match it**, not the reverse: 14 wrappers currently gc-allocate
without calling `maybe_collect`.

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

**S6** (weight 27): allocation pacing, effect metadata, heap lifecycle —
P0-08b, P0-08c, RT-01 … RT-05. S5 was its hard barrier and is done, so H2 no
longer binds.

- **P0-08b**: 14 wrappers gc-allocate without calling `maybe_collect`. The
  manifest is the intent — make the wrappers match the `Effect` rows, not the
  reverse (see §3's effect-classification note). The durable form is F7's
  `#[must_use] Safepoint` token on `Heap::alloc_with`: it can only be minted
  from a `RuntimeRoots`, so "allocate without pacing" stops being writable.
- **H7 binds**: RT-01's free list must not land before S4's sweep poisoning —
  which it has, so this is satisfied; re-read it anyway before reusing storage.
- **H8 binds**: RT-02's `Heap` `Drop` needs the snapshot-ownership audit in the
  same change. `take_crash_snapshot` has callers in `praxis-cli/src/run.rs` and
  `praxis-debugger/src/repl.rs`.
- RT-05 is "delete `Runtime::heap_mut().reset()`", not a live blocker.
- One ABI bump for the stage if RT-03 (`true_ref`/`false_ref`) lands (H17).

Then **S7** (weight 23): descriptor totality (P0-11), typed collection
construction, fault representation. Note P0-11 is what makes the `Known` arm of
`descriptor_for_type` exhaustive and what turns `alloc_empty_vec`'s element
descriptor honest — §8.2 warns its Vec-adopts-first-push assertions near
`adversarial_audit.rs` invert.

Re-read §6 of the plan first. The hazards that still bind: **H3**, **H7**,
**H8**, **H9**, **H10**, **H17**. **H1, H2, H4, H6 and H16 are discharged.**

## 5. Design decisions still open

None have been answered. Three block a stage outright (D1, D3, D5).

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
  S5 bumps to 9.
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
- **The parser interpreter cannot collect today, so S5 did not touch it.**
  H1 lists `parser.rs`'s unrooted `Vec<GcRef>` intermediates alongside the grid
  helpers, but the two are not in the same state: the grid helpers call
  `praxis_*` wrappers (which pace), while `parser.rs` allocates only through
  `heap_ref(ctx).alloc*`, which never calls `maybe_collect`. Its intermediates
  are latent, not live. **IPR-14 (S20) must give them `NativeScope`s in the same
  commit that adds its safepoints** — that is when H1's mechanism actually
  arrives for the parser.
- **P0-08 raised a question the plan does not answer.** The two loop-increment
  `Inst::IntBinOp` sites in `build.rs` (the `for`-loop index bump) are *not*
  followed by a `check_fault`. An overflow there now sets a pending fault that
  is only observed at the next check. That is harmless (faults are sticky and
  the index would have to reach `i64::MAX`), but a MIR verifier rule "every
  faulting instruction is followed by a CheckFault" — MIR-10, S9 — would flag
  it, and the right answer is probably to mark the increment non-faulting
  rather than to add a check.
